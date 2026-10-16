//! The settings and the help, floating over the session: `<prefix> ,` and
//! `<prefix> ?`.
//!
//! Each screen ([`crate::ui::settings`], [`crate::ui::help`]) used to take
//! the terminal, the session dissolving out under it and back in after. Here
//! it comes up in a panel in the middle of the session instead (see
//! [`crate::ui::float`]), and the session stays
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
//! whole — cut only between its sequences, however long the rest of one
//! takes to come (see [`crate::boundary::Boundary::relay_whole`]) — and on a
//! terminal with synchronized updates presents none of it but under the
//! panel: the session drawing as fast as it likes never shows through.
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
//! off it is at once. While it is up, whatever moves on it — the settings'
//! bar, whose row it leaves fades back from it as the picker's does (see
//! [`crate::ui::settings`]) — has frames at the fade's pace until it stops.
//!
//! # Where it does not float
//!
//! Without the terminal's colours — `NO_COLOR`, or a terminal that never
//! said — nothing can be taken toward a background nvmux does not know. The
//! session is drawn faint instead, which dims its text and not its
//! backgrounds, and the panel stands on a margin of blank cells; and nothing
//! fades. On a terminal too small for a panel and the session round it — each
//! screen says how big a one it needs ([`Floating::area`]) — and for a
//! session nvmux keeps no copy of, the screen takes the terminal as it always
//! did ([`Outcome::ShowSettings`], [`Outcome::ShowHelp`]).

use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{MouseButton, MouseEventKind};
use ratatui::layout::Rect;

use super::{
    pollfd, read_fd, ready, take_step, unsynced, Attachment, Outcome, FRAME_WAIT, SEQUENCE_TIMEOUT,
};
use crate::boundary::Boundary;
use crate::error::{NvmuxError, Result};
use crate::input::{Decoder, Input};
use crate::keys::Step;
use crate::palette::Palette;
use crate::shadow::{Cursor, Laid, Shade, Shadow};
use crate::term::{self, write_stdout, MouseReporting};
use crate::ui::float::{Did, Floating};
use crate::ui::{help, settings};
use crate::{card, config, fade, falloff, winch};

/// How far the whole session goes toward the background behind the panel:
/// half way. Nearer the panel, the falloff takes it further.
const BEHIND: f32 = 0.5;

/// The fewest milliseconds between two frames drawn because the session
/// drew: what it draws is repainted under the panel whole, every frame.
const SESSION_FRAME: Duration = Duration::from_millis(33);

/// The most of the session's output held while it is half way through one
/// sequence, which no frame can follow: past this — a sequence that never
/// ends — it goes out as it is, rather than be gathered for ever.
const HELD_MAX: usize = 8 << 20;

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

/// What floats over the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum What {
    /// `<prefix> ,`.
    Settings,
    /// `<prefix> ?`.
    Help,
}

impl What {
    /// What floats in place of the screen `outcome` would show full screen,
    /// if it shows one that floats.
    pub(super) fn of(outcome: Outcome) -> Option<Self> {
        match outcome {
            Outcome::ShowSettings => Some(What::Settings),
            Outcome::ShowHelp => Some(What::Help),
            Outcome::ToPicker
            | Outcome::Detached
            | Outcome::CreateNew
            | Outcome::Resettle
            | Outcome::Switch(_)
            | Outcome::ChildExited
            | Outcome::StdinClosed => None,
        }
    }

    fn open(self) -> Box<dyn Floating> {
        match self {
            What::Settings => Box::new(settings::Panel::open()),
            What::Help => Box::new(help::Panel::open()),
        }
    }

    /// The same screen full screen, where it cannot float.
    fn full_screen(self) -> Outcome {
        match self {
            What::Settings => Outcome::ShowSettings,
            What::Help => Outcome::ShowHelp,
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

/// `what` over the session until it is taken down: `None` to go on relaying
/// the session, and otherwise how the relay ends — the screen full screen
/// where it cannot float, the client gone, the terminal gone, or the clients
/// to settle for a change the settings made to `client.ui` or
/// `client.per_session` (see `main`'s `settle`).
pub(super) fn float(
    a: &mut Attachment,
    master_fd: RawFd,
    winch: &winch::Winch,
    what: What,
) -> Result<Option<Outcome>> {
    let mut panel = what.open();
    let size = term::terminal_size();
    let Some(mut area) = panel.area(Rect::new(0, 0, size.cols, size.rows)) else {
        return Ok(Some(what.full_screen()));
    };
    if !a.copy().is_some_and(|c| c.is_usable()) {
        return Ok(Some(what.full_screen()));
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
        // Frames at a frame's pace while anything moves: the panel coming or
        // going, or a row on it fading back from the bar.
        let lively = moving || panel.moving();
        let frame_due = (lively || owed).then(|| {
            let gap = if lively || !session_drew {
                fade::FRAME
            } else {
                SESSION_FRAME
            };
            last_frame.map_or(now, |last| last + gap)
        });
        let next = [frame_due, sequence_due, panel.wake_at()]
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
                    hold(a, &buf[..len], &mut held, &mut boundary);
                    owed = true;
                    session_drew = true;
                }
            }
        }

        if n > 0 && ready(&fds[2]) {
            winch.drain();
            let size = term::terminal_size();
            a.resize_to(size);
            match panel.area(Rect::new(0, 0, size.cols, size.rows)) {
                Some(fits) => {
                    area = fits;
                    owed = true;
                }
                // Too small to float any longer: the session back as it is,
                // and the screen full screen.
                None => {
                    panel.close();
                    put_back(a, master_fd, &mut held, &mut boundary)?;
                    return Ok(Some(what.full_screen()));
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
            put_back(a, master_fd, &mut held, &mut boundary)?;
            let client = config::get().client;
            let settle = client.ui != was.ui || client.per_session != was.per_session;
            return Ok(settle.then_some(Outcome::Resettle));
        }
        let lively = moving || panel.moving();
        let due = last_frame.is_none_or(|last| {
            let gap = if lively || !session_drew {
                fade::FRAME
            } else {
                SESSION_FRAME
            };
            now >= last + gap
        });
        if (lively || owed) && due {
            // A frame that could not go out is tried again a frame later.
            last_frame = Some(now);
            let there = coming.at(now);
            if paint(
                a,
                look,
                panel.as_mut(),
                area,
                there,
                &mut held,
                &mut boundary,
            )? {
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

/// What the client wrote: to its copy of the screen and its ledger now, and
/// to the terminal with the next frame — as much of it as ends between its
/// sequences.
fn hold(a: &mut Attachment, chunk: &[u8], held: &mut Vec<u8>, boundary: &mut Boundary) {
    a.ledger_saw(chunk);
    a.shadow_saw(chunk);
    held.extend(boundary.relay_whole(chunk));
}

/// A frame, `there` of the way in, if one can go out now (see
/// [`frame_bytes`]). Says whether it did.
fn paint(
    a: &mut Attachment,
    look: Look,
    panel: &mut dyn Floating,
    area: Rect,
    there: f32,
    held: &mut Vec<u8>,
    boundary: &mut Boundary,
) -> Result<bool> {
    // No frame can go out inside a sequence (see `frame_bytes`). Only one
    // that never ends keeps the terminal in it for long: past the most that
    // is held for it, out it goes as it is.
    if boundary.mid_sequence() {
        if held.len() > HELD_MAX {
            write_stdout(held)?;
            held.clear();
        }
        return Ok(false);
    }
    let text = falloff::ease(there * 2.0 - 1.0);
    let laid = (text > 0.0).then(|| {
        let mut buf = panel.draw(area);
        if let (Some(palette), true) = (look.palette(), text < 1.0) {
            fade::apply(&mut buf, palette, 1.0 - text);
        }
        Laid::from_buffer(&buf)
    });
    let Some(copy) = a.copy() else {
        return Ok(false);
    };
    let dim = falloff::ease(there * 2.0);
    let Some(out) = frame_bytes(copy, look, laid.as_ref(), area, dim, held, boundary) else {
        return Ok(false);
    };
    write_stdout(&out)?;
    held.clear();
    Ok(true)
}

/// The bytes of a frame: what the client wrote since the last, its own frame
/// closes taken out so that it is presented with this one and no sooner; the
/// mouse as the panel wants it; and the session under the panel — `laid`,
/// the session dimmed `dim` of the way in round it — painted over all of it,
/// in one synchronized update that this frame ends.
///
/// `None` while the session is half way through a sequence, where nothing of
/// nvmux's can be written: then nothing goes out at all, the session's bytes
/// included, until the rest of the sequence comes and a frame can go out
/// after it. The session's bytes never reach the terminal without a frame
/// after them, which is what keeps them from showing through the panel.
fn frame_bytes(
    copy: &mut Shadow,
    look: Look,
    laid: Option<&Laid>,
    area: Rect,
    dim: f32,
    held: &[u8],
    boundary: &mut Boundary,
) -> Option<Vec<u8>> {
    if boundary.mid_sequence() {
        return None;
    }
    let shade = |row: u16, col: u16| look.shade(row, col, area, dim);
    let frame = copy.frame_under(&shade, look.palette(), laid, Cursor::Hidden);
    let mut out = fade::SYNC_BEGIN.to_vec();
    out.extend_from_slice(&unsynced(held));
    let ours = out.len();
    out.extend_from_slice(term::mouse_reporting_bytes(MouseReporting::Motion));
    out.extend_from_slice(&frame);
    boundary.saw_own(fade::SYNC_BEGIN);
    boundary.saw_own(&out[ours..]);
    Some(out)
}

/// The session back as it is, the panel gone: what the client wrote since the
/// last frame, then every cell painted back as the session drew it, its
/// mouse, pen and cursor as it had them — one update. A client caught half
/// way through a sequence is let finish it first, for a moment, as a resume
/// lets one finish its frame ([`FRAME_WAIT`]): the paint has to go in between
/// its sequences.
fn put_back(
    a: &mut Attachment,
    master_fd: RawFd,
    held: &mut Vec<u8>,
    boundary: &mut Boundary,
) -> Result<()> {
    let deadline = Instant::now() + FRAME_WAIT;
    let mut buf = [0u8; 8192];
    while boundary.mid_sequence() || boundary.keeping() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut p = [pollfd(master_fd)];
        let ms = left.as_micros().div_ceil(1000) as libc::c_int;
        let n = unsafe { libc::poll(p.as_mut_ptr(), 1, ms) };
        if n <= 0 || !ready(&p[0]) {
            break;
        }
        match read_fd(master_fd, &mut buf) {
            Ok(len) if len > 0 => hold(a, &buf[..len], held, boundary),
            _ => break,
        }
    }
    let unfinished = boundary.give_up();
    let mouse = a
        .ledger
        .as_ref()
        .filter(|l| l.is_usable())
        .map(crate::ledger::Ledger::mouse)
        .or_else(|| a.copy().and_then(|c| c.mouse()))
        .unwrap_or(MouseReporting::Buttons);
    let out = put_back_bytes(a.copy().as_deref(), held, mouse, &unfinished);
    if let Some(copy) = a.copy() {
        copy.invalidate();
    }
    held.clear();
    write_stdout(&out)?;
    Ok(())
}

/// The bytes that put the session back: `held`, its frame closes taken out;
/// the session's own `mouse`; every cell of `copy` painted back, its pen and
/// cursor as the session left them, which ends the update; and after it
/// `unfinished`, from a client stopped half way through a sequence for longer
/// than [`put_back`] waits. That goes last, where the rest of it will follow
/// it: the copy has not drawn it yet either, and in front of the paint it
/// would have the paint written into the middle of it.
fn put_back_bytes(
    copy: Option<&Shadow>,
    held: &[u8],
    mouse: MouseReporting,
    unfinished: &[u8],
) -> Vec<u8> {
    let mut out = fade::SYNC_BEGIN.to_vec();
    out.extend_from_slice(&unsynced(held));
    out.extend_from_slice(term::mouse_reporting_bytes(mouse));
    match copy {
        Some(copy) => out.extend_from_slice(&copy.paint()),
        None => out.extend_from_slice(fade::SYNC_END),
    }
    out.extend_from_slice(unfinished);
    out
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

    fn count(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count()
    }

    /// What the session writes goes to the terminal only at the front of a
    /// frame, in the one update the frame ends: its own frame closes taken
    /// out, and a tail half way through a sequence kept back for the rest of
    /// it. So there is always a frame after it, and it never shows through.
    #[test]
    fn the_sessions_bytes_go_out_only_in_front_of_a_frame() {
        let area = Rect::new(8, 3, 64, 18);
        let look = Look::Dimmed(palette());
        let mut copy = Shadow::new(24, 80);
        let mut boundary = Boundary::new();
        let chunk = b"\x1b[?2026h\x1b[5;10Hbusy\x1b[?2026l\x1b]2;tit";
        copy.feed(chunk);
        let held = boundary.relay_whole(chunk);
        let out = frame_bytes(&mut copy, look, None, area, 1.0, &held, &mut boundary)
            .expect("a frame, between sequences");
        let front = [fade::SYNC_BEGIN, b"\x1b[?2026h\x1b[5;10Hbusy"].concat();
        assert!(
            out.starts_with(&front),
            "{:?}",
            String::from_utf8_lossy(&out)
        );
        assert_eq!(count(&out, fade::SYNC_END), 1, "the frame's own close only");
        assert!(out.ends_with(fade::SYNC_END), "which ends it");
        assert_eq!(count(&out, b"\x1b]"), 0, "the tail waits for its rest");

        let rest = b"le\x07";
        copy.feed(rest);
        let held = boundary.relay_whole(rest);
        let out = frame_bytes(&mut copy, look, None, area, 1.0, &held, &mut boundary)
            .expect("the next frame");
        let front = [fade::SYNC_BEGIN, b"\x1b]2;title\x07"].concat();
        assert!(
            out.starts_with(&front),
            "{:?}",
            String::from_utf8_lossy(&out)
        );
    }

    /// Once the terminal has had part of a sequence — a string with no end in
    /// sight, gone out unfinished — no frame can follow it, and nothing goes
    /// out at all, the session's bytes included, until the string ends.
    #[test]
    fn nothing_goes_out_half_way_through_a_sequence() {
        let area = Rect::new(8, 3, 64, 18);
        let mut copy = Shadow::new(24, 80);
        let mut boundary = Boundary::new();
        let mut held = boundary.relay_whole(b"\x1b[1;1Hbefore\x1b]52;c;");
        held.extend(boundary.relay_whole(&vec![b'A'; 128 * 1024]));
        assert!(boundary.mid_sequence());
        let frame = frame_bytes(
            &mut copy,
            Look::Faint,
            None,
            area,
            1.0,
            &held,
            &mut boundary,
        );
        assert!(frame.is_none(), "a frame inside the string");
        held.extend(boundary.relay_whole(b"\x1b\\after"));
        let out = frame_bytes(
            &mut copy,
            Look::Faint,
            None,
            area,
            1.0,
            &held,
            &mut boundary,
        )
        .expect("a frame once it has ended");
        let front = [fade::SYNC_BEGIN, b"\x1b[1;1Hbefore\x1b]52;c;AAAA"].concat();
        assert!(out.starts_with(&front));
        assert_eq!(count(&out, b"\x1b\\after"), 1);
        assert_eq!(count(&out, fade::SYNC_END), 1);
    }

    /// Put back, the session's bytes go first and the paint after them, in one
    /// update; and a tail the client has still to finish goes after the paint,
    /// where the rest of it will follow, rather than have the paint written
    /// into the middle of it.
    #[test]
    fn the_paint_that_puts_the_session_back_goes_between_its_sequences() {
        let mut copy = Shadow::new(24, 80);
        copy.feed(b"\x1b[1;1Hhello\x1b[?2026l\x1b]2;tit");
        let held = b"\x1b[1;1Hhello\x1b[?2026l";
        let out = put_back_bytes(Some(&copy), held, MouseReporting::Off, b"\x1b]2;tit");
        let front = [fade::SYNC_BEGIN, b"\x1b[1;1Hhello"].concat();
        assert!(
            out.starts_with(&front),
            "{:?}",
            String::from_utf8_lossy(&out)
        );
        assert_eq!(count(&out, fade::SYNC_END), 1, "the paint's own close only");
        let end = [fade::SYNC_END, b"\x1b]2;tit"].concat();
        assert!(out.ends_with(&end), "{:?}", String::from_utf8_lossy(&out));
    }

    /// The settings and the help float, each in place of its own screen and
    /// full screen as itself where it cannot; nothing else floats.
    #[test]
    fn the_settings_and_the_help_float_and_nothing_else() {
        for (outcome, what) in [
            (Outcome::ShowSettings, What::Settings),
            (Outcome::ShowHelp, What::Help),
        ] {
            assert_eq!(What::of(outcome), Some(what));
            assert_eq!(what.full_screen(), outcome);
        }
        for outcome in [
            Outcome::ToPicker,
            Outcome::Detached,
            Outcome::CreateNew,
            Outcome::Resettle,
            Outcome::ChildExited,
            Outcome::StdinClosed,
        ] {
            assert_eq!(What::of(outcome), None, "{outcome:?}");
        }
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
