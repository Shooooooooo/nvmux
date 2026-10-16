//! The settings, floating over the session: `<prefix> ,`.
//!
//! The settings screen ([`crate::ui::settings`]) used to take the terminal,
//! the session dissolving out under it and back in after. Here it comes up in
//! a panel in the middle of the session instead, and the session stays
//! behind it: all of it dimmed half way to the terminal's background, and
//! further the nearer it is to the panel — the cards' falloff
//! ([`crate::falloff`]), so the panel stands in a clearing the way a card in
//! the margin does — and under the panel emptied. The session goes on
//! running, and what it draws shows behind the panel, dimmed: so a change
//! that shows in the editor, `centre` say, shows as it is made.
//!
//! # Drawn by the relay
//!
//! The relay draws it, not the client, so Neovim's own client
//! (`[client] ui = "nvim"`) has it too. It paints the session from its copy
//! of the screen — the shadow, or a kept client's own copy (see
//! [`crate::shadow`]) — a cell at a time, taken toward the background through
//! the palette the terminal gave; and the panel over it, as the screen draws
//! itself into a buffer of the panel's size (see
//! [`crate::shadow::Shadow::frame_under`]). While the panel is up nothing the
//! client writes goes straight to the terminal: it goes to the copy, and to
//! the terminal with the next frame, at the front of it, inside the one
//! synchronized update that ends with the session painted under the panel
//! again. So the terminal hears everything the client said, in order and
//! whole — cut only between its sequences (see [`crate::boundary`]) — and
//! presents none of it but under the panel.
//!
//! Keys go to the panel, read by [`crate::input`]; what the terminal answers
//! the client goes on to the client, as it came. The mouse is reported while
//! the panel is up, and put back as the session had it as it goes; a press
//! outside the panel takes it down, as `Esc` does.
//!
//! # Coming and going
//!
//! In nvmux's fade, the way a card comes: over the first half the session
//! dims, and empties where the panel will be; over the second the panel
//! comes up out of the background. Going, the other way round. With the fade
//! off it is at once.
//!
//! # Where it does not float
//!
//! Without the terminal's colours — `NO_COLOR`, or a terminal that never
//! said — nothing can be taken toward a background nvmux does not know. The
//! session is drawn faint instead, which dims its text and not its
//! backgrounds, and the panel stands on a margin of blank cells; and nothing
//! fades. On a terminal too small for a panel and the session round it, and
//! for a session nvmux keeps no copy of, the settings take the screen as they
//! always did ([`Outcome::ShowSettings`]).

use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{MouseButton, MouseEventKind};
use ratatui::layout::Rect;

use super::{pollfd, read_fd, ready, take_step, unsynced, Attachment, Outcome, SEQUENCE_TIMEOUT};
use crate::boundary::Boundary;
use crate::error::{NvmuxError, Result};
use crate::input::{Decoder, Input};
use crate::keys::Step;
use crate::palette::Palette;
use crate::shadow::{Cursor, Laid, Shade};
use crate::term::{self, write_stdout, MouseReporting};
use crate::ui::settings::{self, Did, Panel};
use crate::{card, config, fade, falloff, winch};

/// How far the whole session goes toward the background behind the panel:
/// half way. Nearer the panel, the falloff takes it further.
const BEHIND: f32 = 0.5;

/// The fewest milliseconds between two frames drawn because the session
/// drew: what it draws is repainted under the panel whole, every frame.
const SESSION_FRAME: Duration = Duration::from_millis(33);

/// Without the terminal's colours, the blank cells round the panel: columns
/// either side, and rows above and below.
const MARGIN_COLS: u16 = 2;
const MARGIN_ROWS: u16 = 1;

/// How the session is drawn behind the panel.
#[derive(Debug, Clone, Copy)]
enum Look {
    /// Taken toward the terminal's background, through its palette.
    Dimmed(&'static Palette),
    /// Faint, with a blank margin: there are no colours to take it toward.
    Faint,
}

impl Look {
    fn palette(self) -> Option<&'static Palette> {
        match self {
            Look::Dimmed(palette) => Some(palette),
            Look::Faint => None,
        }
    }

    /// What becomes of the session's cell at (`row`, `col`) with the panel
    /// at `panel`, the dimming `dim` of the way in.
    fn shade(self, row: u16, col: u16, panel: Rect, dim: f32) -> Shade {
        match self {
            Look::Dimmed(_) => {
                let d = falloff::distance(col, row, panel.x, panel.y, panel.width, panel.height);
                let amount = if d == 0.0 {
                    1.0
                } else {
                    1.0 - (1.0 - BEHIND) * (1.0 - falloff::around(d, falloff::PEAK, falloff::REACH))
                };
                Shade::Toward(amount * dim)
            }
            Look::Faint => {
                let left = panel.x.saturating_sub(MARGIN_COLS);
                let top = panel.y.saturating_sub(MARGIN_ROWS);
                let inside = col >= left
                    && col < panel.right() + MARGIN_COLS
                    && row >= top
                    && row < panel.bottom() + MARGIN_ROWS;
                if inside {
                    Shade::Blank
                } else {
                    Shade::Faint
                }
            }
        }
    }
}

/// How far the panel has come: 0 the session as it is, 1 the panel up.
#[derive(Debug, Clone, Copy)]
struct Coming {
    from: f32,
    to: f32,
    start: Instant,
    length: Duration,
}

impl Coming {
    /// From where it is to `to`, over as much of `whole` as the way is long.
    fn to(from: f32, to: f32, whole: Duration, now: Instant) -> Self {
        Self {
            from,
            to,
            start: now,
            length: whole.mul_f32((to - from).abs()),
        }
    }

    fn at(&self, now: Instant) -> f32 {
        if self.length.is_zero() {
            return self.to;
        }
        let k = (now.saturating_duration_since(self.start).as_secs_f32()
            / self.length.as_secs_f32())
        .min(1.0);
        self.from + (self.to - self.from) * k
    }

    fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.start) >= self.length
    }
}

/// The settings over the session until they are taken down: `None` to go on
/// relaying the session, and otherwise how the relay ends — the settings
/// full screen where they cannot float, the client gone, the terminal gone,
/// or the clients to settle for a change of `client.ui` or
/// `client.per_session` (see `main`'s `settle`).
pub(super) fn float(
    a: &mut Attachment,
    master_fd: RawFd,
    winch: &winch::Winch,
) -> Result<Option<Outcome>> {
    let size = term::terminal_size();
    let Some(mut area) = settings::panel_area(Rect::new(0, 0, size.cols, size.rows)) else {
        return Ok(Some(Outcome::ShowSettings));
    };
    if !a.copy().is_some_and(|c| c.is_usable()) {
        return Ok(Some(Outcome::ShowSettings));
    }
    // Whatever is still owed the terminal goes first: a first paint still
    // held, and what the hint bar was keeping back.
    a.release_hold(false)?;
    a.stop_watching_the_terminal()?;
    if let Some(copy) = a.copy() {
        copy.invalidate();
    }
    let was = config::get().client;
    let look = match fade::colours() {
        Some(palette) => Look::Dimmed(palette),
        None => Look::Faint,
    };
    let mut panel = Panel::open();
    let mut decoder = Decoder::new();
    let mut boundary = Boundary::new();
    // What the client has written since the last frame, owed to the terminal.
    let mut held: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    let mut coming = Coming::to(0.0, 1.0, fade_length(look), Instant::now());
    let mut going = false;
    // A frame is owed: for the panel, or for the session behind it.
    let mut owed = true;
    let mut session_drew = false;
    let mut last_frame: Option<Instant> = None;
    // How far in the last frame drawn had the panel: the way in or out ends
    // on a frame drawn where it was going, however the frames fell.
    let mut drawn_at = f32::NAN;
    let mut sequence_due: Option<Instant> = None;
    let stdin_fd = std::io::stdin().as_raw_fd();

    loop {
        let now = Instant::now();
        let moving = !coming.done(now);
        if !moving && drawn_at != coming.to {
            owed = true;
        }
        let frame_due = (moving || owed).then(|| {
            let gap = if moving || !session_drew {
                fade::FRAME
            } else {
                SESSION_FRAME
            };
            last_frame.map_or(now, |last| last + gap)
        });
        let next = [frame_due, sequence_due, panel.wake_at(), boundary.due()]
            .into_iter()
            .flatten()
            .min();
        let timeout = next.map_or(super::IDLE_POLL_MS, |d| {
            let left = d.saturating_duration_since(Instant::now());
            left.as_micros()
                .div_ceil(1000)
                .min(super::IDLE_POLL_MS as u128) as libc::c_int
        });
        let mut fds = [pollfd(stdin_fd), pollfd(master_fd), pollfd(winch.fd())];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(NvmuxError::Io(err));
        }
        let now = Instant::now();

        // What the client writes goes to its copy of the screen now, and to
        // the terminal with the next frame.
        if n > 0 && ready(&fds[1]) {
            match read_fd(master_fd, &mut buf) {
                Ok(0) | Err(_) => {
                    let _ = owe_out(&mut held, &mut boundary);
                    return Ok(Some(Outcome::ChildExited));
                }
                Ok(len) => {
                    let chunk = &buf[..len];
                    a.ledger_saw(chunk);
                    a.shadow_saw(chunk);
                    held.extend(boundary.relay(chunk, now));
                    held.extend(boundary.release_the_sessions_close());
                    owed = true;
                    session_drew = true;
                }
            }
        }
        if boundary.due().is_some_and(|due| now >= due) {
            held.extend(boundary.give_up());
            owed = true;
        }

        if n > 0 && ready(&fds[2]) {
            winch.drain();
            let size = term::terminal_size();
            a.resize_to(size);
            match settings::panel_area(Rect::new(0, 0, size.cols, size.rows)) {
                Some(fits) => {
                    area = fits;
                    owed = true;
                }
                // Too small to float any longer: the session back as it is,
                // and the settings full screen.
                None => {
                    panel.close();
                    put_back(a, &mut held, &mut boundary)?;
                    return Ok(Some(Outcome::ShowSettings));
                }
            }
        }

        let mut inputs = Vec::new();
        if n > 0 && ready(&fds[0]) {
            match read_fd(stdin_fd, &mut buf) {
                Ok(0) => {
                    let _ = owe_out(&mut held, &mut boundary);
                    return Ok(Some(Outcome::StdinClosed));
                }
                Err(e) => return Err(NvmuxError::Io(e)),
                // Deliberately not logged, as in the relay: these are keys.
                Ok(len) => inputs.extend(decoder.feed(&buf[..len])),
            }
            sequence_due = decoder.waiting().then(|| now + SEQUENCE_TIMEOUT);
        } else if sequence_due.is_some_and(|due| now >= due) {
            sequence_due = None;
            inputs.extend(decoder.flush());
        }
        for input in inputs {
            match input {
                Input::Client(bytes) => {
                    take_step(a, Step::Forward(bytes))?;
                }
                // Once going, the panel takes nothing more.
                _ if going => {}
                Input::Key(key) => {
                    owed = true;
                    if panel.on_key(key, now) == Did::Close {
                        going = true;
                    }
                }
                Input::Mouse(mouse) => {
                    owed = true;
                    let outside = !area.contains((mouse.column, mouse.row).into());
                    let press = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
                    // A press outside takes the panel down, as `Esc` does.
                    going = (outside && press) || panel.on_mouse(mouse, now) == Did::Close;
                }
            }
            if going && coming.to > 0.0 {
                let there = coming.at(now);
                coming = Coming::to(there, 0.0, fade_length(look), now);
            }
        }
        if panel.tick(now) {
            owed = true;
        }
        tell_the_client(a);

        let moving = !coming.done(now);
        // Gone the whole way out: the session back as it is, at once.
        if going && !moving {
            panel.close();
            put_back(a, &mut held, &mut boundary)?;
            let client = config::get().client;
            let settle = client.ui != was.ui || client.per_session != was.per_session;
            return Ok(settle.then_some(Outcome::Resettle));
        }
        let due = last_frame.is_none_or(|last| {
            let gap = if moving || !session_drew {
                fade::FRAME
            } else {
                SESSION_FRAME
            };
            now >= last + gap
        });
        if (moving || owed) && due {
            // A frame that could not go out is tried again a frame later.
            last_frame = Some(now);
            let there = coming.at(now);
            if paint(a, look, &mut panel, area, there, &mut held, &mut boundary)? {
                owed = false;
                session_drew = false;
                drawn_at = there;
            }
        }
    }
}

/// How long the panel's whole way in takes, the session dimming and then the
/// panel coming: all of `[effects.fade] duration_ms`, where the fade runs.
fn fade_length(look: Look) -> Duration {
    match look {
        Look::Dimmed(_) if fade::enabled() => {
            Duration::from_millis(config::get().effects.fade.duration_ms)
        }
        Look::Dimmed(_) | Look::Faint => Duration::ZERO,
    }
}

/// A client that draws cards hears the settings as they change, so what it
/// draws behind the panel follows them (see [`card::Command::Settings`]).
fn tell_the_client(a: &mut Attachment) {
    let generation = config::generation();
    if a.draws_cards && a.heard != generation {
        a.heard = generation;
        a.tell_client(&card::Command::Settings(config::get().drawing()));
    }
}

/// A frame: what the client wrote since the last, its own frame closes taken
/// out so it is presented with this one; the mouse as the panel wants it; and
/// the session under the panel, `there` of the way in. Says whether it went
/// out: not while the session is half way through a sequence — what it owes
/// the terminal goes out on its own then, and the frame waits for the rest.
fn paint(
    a: &mut Attachment,
    look: Look,
    panel: &mut Panel,
    area: Rect,
    there: f32,
    held: &mut Vec<u8>,
    boundary: &mut Boundary,
) -> Result<bool> {
    if boundary.mid_sequence() {
        write_stdout(held)?;
        held.clear();
        return Ok(false);
    }
    let dim = falloff::ease(there * 2.0);
    let text = falloff::ease(there * 2.0 - 1.0);
    let laid = (text > 0.0).then(|| {
        let mut buf = panel.draw(area);
        if let (Some(palette), true) = (look.palette(), text < 1.0) {
            fade::apply(&mut buf, palette, 1.0 - text);
        }
        Laid::from_buffer(&buf)
    });
    let shade = |row: u16, col: u16| look.shade(row, col, area, dim);
    let Some(copy) = a.copy() else {
        return Ok(false);
    };
    let frame = copy.frame_under(&shade, look.palette(), laid.as_ref(), Cursor::Hidden);
    let mut out = fade::SYNC_BEGIN.to_vec();
    out.extend_from_slice(&unsynced(held));
    let ours = out.len();
    out.extend_from_slice(term::mouse_reporting_bytes(MouseReporting::Motion));
    out.extend_from_slice(&frame);
    write_stdout(&out)?;
    boundary.saw_own(fade::SYNC_BEGIN);
    boundary.saw_own(&out[ours..]);
    held.clear();
    Ok(true)
}

/// The session back as it is, the panel gone: what the client wrote since the
/// last frame, then every cell painted back as the session drew it, its
/// mouse, pen and cursor as it had them — one update.
fn put_back(a: &mut Attachment, held: &mut Vec<u8>, boundary: &mut Boundary) -> Result<()> {
    held.extend(boundary.give_up());
    let mouse = a
        .ledger
        .as_ref()
        .filter(|l| l.is_usable())
        .map(crate::ledger::Ledger::mouse)
        .or_else(|| a.copy().and_then(|c| c.mouse()))
        .unwrap_or(MouseReporting::Buttons);
    let mut out = fade::SYNC_BEGIN.to_vec();
    out.extend_from_slice(&unsynced(held));
    out.extend_from_slice(term::mouse_reporting_bytes(mouse));
    if let Some(copy) = a.copy() {
        out.extend_from_slice(&copy.paint());
        copy.invalidate();
    } else {
        out.extend_from_slice(fade::SYNC_END);
    }
    held.clear();
    write_stdout(&out)?;
    Ok(())
}

/// What the client wrote and the terminal has not had yet, on a way out with
/// no frame to come: as it is, whole or not.
fn owe_out(held: &mut Vec<u8>, boundary: &mut Boundary) -> std::io::Result<()> {
    held.extend(boundary.give_up());
    let out = std::mem::take(held);
    write_stdout(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> &'static Palette {
        static P: std::sync::OnceLock<Palette> = std::sync::OnceLock::new();
        P.get_or_init(crate::test_support::palette)
    }

    /// Behind the panel the whole session is half way to the background,
    /// further nearer the panel by the cards' falloff, and under it emptied;
    /// and none of it before the dimming has come in.
    #[test]
    fn the_session_dims_round_the_panel_by_the_cards_falloff() {
        let panel = Rect::new(24, 6, 72, 23);
        let look = Look::Dimmed(palette());
        let amount = |row, col| match look.shade(row, col, panel, 1.0) {
            Shade::Toward(t) => t,
            other => panic!("{other:?}"),
        };
        assert_eq!(amount(10, 30), 1.0, "under it");
        let far = amount(0, 0);
        assert!((far - BEHIND).abs() < 1e-6, "far off, half: {far}");
        let beside = amount(10, 23);
        assert!(beside > 0.9, "beside it, nearly gone: {beside}");
        let near = amount(10, 18);
        assert!(beside > near && near > far, "{beside} {near} {far}");
        assert_eq!(look.shade(0, 0, panel, 0.0), Shade::Toward(0.0), "not yet");
    }

    /// Without colours: blank under the panel and two columns and a row round
    /// it, faint everywhere else.
    #[test]
    fn without_colours_the_panel_stands_on_a_blank_margin() {
        let panel = Rect::new(24, 6, 72, 23);
        let shade = |row, col| Look::Faint.shade(row, col, panel, 1.0);
        assert_eq!(shade(10, 30), Shade::Blank);
        assert_eq!(shade(10, 22), Shade::Blank, "the margin, left");
        assert_eq!(shade(10, 97), Shade::Blank, "and right");
        assert_eq!(shade(5, 30), Shade::Blank, "the row above");
        assert_eq!(shade(29, 30), Shade::Blank, "and below");
        assert_eq!(shade(10, 21), Shade::Faint);
        assert_eq!(shade(4, 30), Shade::Faint);
    }

    /// The way in takes as much of the fade as the way is long, and ends
    /// exactly where it was going; with no fade it is there at once.
    #[test]
    fn the_panel_comes_and_goes_over_the_fade() {
        let t0 = Instant::now();
        let whole = Duration::from_millis(200);
        let coming = Coming::to(0.0, 1.0, whole, t0);
        assert_eq!(coming.at(t0), 0.0);
        assert!((coming.at(t0 + Duration::from_millis(100)) - 0.5).abs() < 1e-3);
        assert_eq!(coming.at(t0 + whole), 1.0);
        assert!(coming.done(t0 + whole));
        let back = Coming::to(0.5, 0.0, whole, t0);
        assert_eq!(
            back.length,
            Duration::from_millis(100),
            "half the way, half the time"
        );
        let at_once = Coming::to(0.0, 1.0, Duration::ZERO, t0);
        assert_eq!(at_once.at(t0), 1.0);
        assert!(at_once.done(t0));
    }
}
