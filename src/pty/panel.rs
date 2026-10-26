//! The settings, help, picker and naming prompt floating over a session:
//! `<prefix> ,`, `<prefix> ?`, `<prefix> Space`, `<prefix> c`, and the
//! picker's `c` and `r`.
//!
//! The screen draws into a panel (see [`crate::ui::float`]) while the session
//! keeps running behind it, dimmed half way to the background, more near the
//! panel by the cards' falloff ([`crate::falloff`]), and blank under it. The
//! hint row does not float: it stays on the terminal's last row, full width
//! ([`Floating::draw_hints`]), and the session is blank under it too.
//!
//! The relay draws it, not the client, so it also works with
//! `[client] ui = "nvim"`: each frame paints the session from its copy of the
//! screen (see [`crate::shadow::Shadow::frame_under`]) with the panel over
//! it. Client output is held and written at the front of the next frame, in
//! the same synchronized update, split only between sequences (see
//! [`crate::boundary::Boundary::relay_whole`]), so on a terminal with
//! synchronized updates the session never shows through. Keys go to the panel
//! via [`crate::input`]; terminal replies go to the client. Mouse reporting is
//! on while it is up; a press on the session (outside the panel and its hint
//! row) closes it, like `Esc`.
//!
//! Behind the picker is the session its cursor is on ([`Floating::preview`]):
//! its client, lent by the session loop and read here as its own thread
//! would ([`Glimpse`]), or nothing if it has none running.
//!
//! It comes and goes over nvmux's fade like a card: the session dims, then
//! the panel and its hint row rise. From the picker ([`Leave`]), a switch to
//! the session shown undims it where it is, and the relay hands its client
//! over to be relayed next; any other switch fades panel and session out
//! together. The help, settings or a prompt replace the picker over the
//! dimmed session and return to it when closed. Settings switched to float
//! from full screen, and the picker back from a full-screen prompt
//! ([`Attachment::float_the_picker`]), come up with the session out of the
//! background.
//!
//! Without the terminal's colours the session is drawn faint on a blank
//! margin, with no fade. A terminal too small ([`Floating::area`]), no copy
//! of the session, or `[style] float` off means full screen instead (its
//! [`Outcome`]); toggling `float` from the settings moves them between the
//! two ([`Attachment::float_the_settings`]).

use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{MouseButton, MouseEventKind};
use ratatui::layout::{Position, Rect};

use super::{
    pollfd, pty_size, read_fd, ready, take_step, unsynced, Attachment, Others, Outcome, PtySize,
    Target, ALT_SCREEN_ENTER, FRAME_WAIT, SEQUENCE_TIMEOUT,
};
use crate::boundary::Boundary;
use crate::error::{NvmuxError, Result};
use crate::input::{Decoder, Input};
use crate::keys::Step;
use crate::palette::Palette;
use crate::shadow::{Cursor, Laid, Shade, Shadow};
use crate::term::{self, write_stdout, MouseReporting};
use crate::ui::float::{self, Did, Floating, Leave};
use crate::ui::prompt::{self, Place};
use crate::ui::{help, picker, settings};
use crate::{card, config, fade, falloff, winch};

/// Dimming of the whole session behind the panel; the falloff adds more near
/// it.
const BEHIND: f32 = 0.5;

/// Minimum gap between frames caused only by session output, as each
/// repaints the whole session.
const SESSION_FRAME: Duration = Duration::from_millis(33);

/// Most session output held while it is mid-sequence (no frame can follow).
/// Past this, a sequence that never ends is written out as is.
const HELD_MAX: usize = 8 << 20;

/// The blank margin round the panel when there are no colours.
const MARGIN_COLS: u16 = 2;
const MARGIN_ROWS: u16 = 1;

#[derive(Debug, Clone, Copy)]
enum Look {
    /// Toward the terminal's background, through its palette.
    Dimmed(&'static Palette),
    /// Faint, with a blank margin: no colours to dim toward.
    Faint,
}

impl Look {
    fn palette(self) -> Option<&'static Palette> {
        match self {
            Look::Dimmed(palette) => Some(palette),
            Look::Faint => None,
        }
    }

    /// The shade of session cell (`row`, `col`) with the panel at `panel` and
    /// its hint row at `hints`, `dim` of the way in, and the whole session
    /// `away` of the way out. Under the hint row it is emptied as under the
    /// panel, but the row has no falloff of its own.
    fn shade(self, row: u16, col: u16, panel: Rect, hints: Rect, dim: f32, away: f32) -> Shade {
        let under_hints = hints.contains(Position::new(col, row));
        match self {
            Look::Dimmed(_) => {
                let d = falloff::distance(col, row, panel.x, panel.y, panel.width, panel.height);
                let amount = if d == 0.0 || under_hints {
                    1.0
                } else {
                    1.0 - (1.0 - BEHIND) * (1.0 - falloff::around(d, falloff::PEAK, falloff::REACH))
                };
                let dimmed = amount * dim;
                Shade::Toward(dimmed + (1.0 - dimmed) * away)
            }
            Look::Faint if away > 0.0 => Shade::Blank,
            Look::Faint => {
                let left = panel.x.saturating_sub(MARGIN_COLS);
                let top = panel.y.saturating_sub(MARGIN_ROWS);
                let inside = col >= left
                    && col < panel.right() + MARGIN_COLS
                    && row >= top
                    && row < panel.bottom() + MARGIN_ROWS;
                if inside || under_hints {
                    Shade::Blank
                } else {
                    Shade::Faint
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum What {
    /// `<prefix> ,`.
    Settings,
    /// `<prefix> ?`.
    Help,
    /// `<prefix> Space`.
    Picker,
    /// The new-session prompt: `<prefix> c` and the picker's `c`.
    Create(Place),
    /// The prompt for a session's new name: the picker's `r`.
    Rename(crate::ids::Id),
}

impl What {
    /// What floats instead of `outcome`'s full screen, if `float`.
    pub(super) fn of(outcome: Outcome, float: bool) -> Option<Self> {
        if !float {
            return None;
        }
        match outcome {
            Outcome::ShowSettings => Some(What::Settings),
            Outcome::ShowHelp => Some(What::Help),
            Outcome::ToPicker => Some(What::Picker),
            Outcome::CreateNew => Some(What::Create(Place::Last)),
            // The picker's prompts full screen, where they could not float;
            // floating, they open from the picker, not from a relay outcome.
            Outcome::CreateAfter(_) | Outcome::Rename(_) => None,
            Outcome::Detached
            | Outcome::Resettle
            | Outcome::Switch(_)
            | Outcome::ChildExited
            | Outcome::StdinClosed => None,
        }
    }

    fn open(self, behind: &str) -> Box<dyn Floating> {
        match self {
            What::Settings => Box::new(settings::Panel::open()),
            What::Help => Box::new(help::Panel::open()),
            What::Picker => Box::new(picker::Panel::open(behind)),
            What::Create(place) => Box::new(prompt::Panel::create(place)),
            What::Rename(id) => Box::new(prompt::Panel::rename(id)),
        }
    }

    fn full_screen(self) -> Outcome {
        match self {
            What::Settings => Outcome::ShowSettings,
            What::Help => Outcome::ShowHelp,
            What::Picker => Outcome::ToPicker,
            What::Create(Place::Last) => Outcome::CreateNew,
            What::Create(Place::After(after)) => Outcome::CreateAfter(after),
            What::Rename(id) => Outcome::Rename(id),
        }
    }
}

/// How a panel leaves once closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Way {
    /// Back to the session as it is.
    Back,
    /// Out together with the session, to another session or a full screen.
    Away,
    /// Out alone, replaced by another panel over the session.
    Across(What),
    /// Into the session the picker shows behind it ([`Glimpse`]), which
    /// undims where it is and is relayed next.
    Into,
}

impl Way {
    fn of(leave: Option<Leave>) -> Self {
        match leave {
            None | Some(Leave::Quit) => Way::Back,
            Some(Leave::Switch(_)) => Way::Away,
            Some(Leave::Help) => Way::Across(What::Help),
            Some(Leave::Settings) => Way::Across(What::Settings),
            Some(Leave::New(after)) => Way::Across(What::Create(Place::After(after))),
            Some(Leave::Rename(id)) => Way::Across(What::Rename(id)),
        }
    }

    /// As `leave` says (see [`Way::of`]), but out with the session if
    /// `[style] float` was turned off, across to a set-aside picker instead
    /// of back, and into the session `shown` behind the panel, if it can be
    /// handed over, instead of away to it.
    fn going(
        leave: Option<Leave>,
        picker_under: bool,
        unfloated: bool,
        shown: Option<&str>,
    ) -> Self {
        if unfloated {
            return Way::Away;
        }
        match (Way::of(leave), leave) {
            (Way::Back, _) if picker_under => Way::Across(What::Picker),
            (Way::Away, Some(Leave::Switch(id))) if shown == Some(id.as_str()) => Way::Into,
            (way, _) => way,
        }
    }
}

/// How the relay ends once a leaving panel is down; `None` when it goes on
/// (back to the session, or across).
fn ends_in(leave: Leave) -> Option<Outcome> {
    match leave {
        Leave::Switch(id) => Some(Outcome::Switch(Target::Session(id))),
        Leave::Quit => Some(Outcome::Detached),
        Leave::Help | Leave::Settings | Leave::New(_) | Leave::Rename(_) => None,
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
    /// From `from` to `to`, over `whole` scaled by the distance.
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

/// The session under the picker's cursor, shown behind the panel in place of
/// the one it floats over: its client, lent by the session loop
/// ([`Others::lend`]) and read here as its thread would.
struct Glimpse {
    client: Attachment,
    /// A started-ahead client's output parsed: its own copy waits for its
    /// first relay, which replays all of it.
    copy: Option<Shadow>,
}

impl Glimpse {
    /// Resized to `size`, as a parked pty never is. A parked client comes out
    /// of its crawl, so its screen keeps up; one started ahead keeps all it
    /// writes until its first visit, so it stays at its crawl unless it must
    /// redraw at a new size.
    fn open(client: Attachment, size: PtySize) -> Self {
        let copy = client.unshown.as_deref().map(|said| {
            let at = pty_size(&client.master).unwrap_or(size);
            let mut copy = Shadow::new(at.rows, at.cols);
            copy.feed(said);
            copy
        });
        let mut glimpse = Self { client, copy };
        glimpse.resize_to(size);
        if !glimpse.client.started_ahead() {
            glimpse.come_back();
        }
        glimpse
    }

    fn fd(&self) -> RawFd {
        self.client.master.as_raw_fd()
    }

    /// Only `Clear` ends the crawl (see `card::Command::Away`); it also drops
    /// cards left on its screen, such as an old landing card.
    fn come_back(&mut self) {
        if std::mem::take(&mut self.client.away) {
            self.client.tell_client(&card::Command::Clear);
            self.client.crawling = false;
        }
    }

    fn resize_to(&mut self, size: PtySize) {
        let was = pty_size(&self.client.master);
        let resized = was != Some(size);
        self.client.resize_to(size);
        if let Some(copy) = self.copy.as_mut() {
            copy.resize(size.rows, size.cols);
        }
        // Untold (its input full), it keeps its old size: so does the pty,
        // or its resume would find nothing to tell it.
        if !self.client.tell_its_size(size, resized) {
            if let Some(was) = was {
                let _ = self.client.master.resize(was.into());
            }
        }
        if resized {
            self.come_back();
        }
    }

    /// Read what it wrote if `readable`, and look at a started-ahead client's
    /// probe, as its thread would. `false` once it has gone or is to be
    /// retired: it then goes back for its thread to see to.
    fn keep(&mut self, readable: bool, buf: &mut [u8]) -> bool {
        if !self.client.kept_by_its_probe() {
            self.client.hang_up();
            return false;
        }
        !readable || self.read(buf)
    }

    fn read(&mut self, buf: &mut [u8]) -> bool {
        match read_fd(self.fd(), buf) {
            Ok(len) if len > 0 => {
                let chunk = &buf[..len];
                if !self.client.started_ahead() {
                    return self.client.keep_parked_output(chunk);
                }
                if let Some(copy) = self.copy.as_mut() {
                    copy.feed(chunk);
                }
                let retired = self.client.keep_unshown_output(chunk).is_some();
                if retired {
                    // As its thread would retire it: given back, it is found
                    // gone.
                    self.client.hang_up();
                }
                !retired
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) =>
            {
                true
            }
            // EOF on macOS, EIO on Linux: the client has gone.
            _ => false,
        }
    }

    fn copy(&mut self) -> Option<&mut Shadow> {
        match self.copy.as_mut() {
            Some(copy) => Some(copy),
            None => self.client.copy(),
        }
        .filter(|copy| copy.is_usable())
    }

    /// A started-ahead client's ledger has seen nothing; its copy has.
    fn mouse(&mut self) -> MouseReporting {
        match self.copy.as_ref() {
            Some(copy) => copy.mouse().unwrap_or(MouseReporting::Buttons),
            None => session_mouse(&mut self.client),
        }
    }

    /// Whether `Enter` can bring it in where it is: it has a screen, and a
    /// started-ahead client's probe has answered.
    fn can_be_handed_over(&mut self) -> bool {
        self.client.probe.is_none() && self.copy().is_some()
    }

    /// Back to the session loop as it was lent.
    fn close(mut self, others: &mut dyn Others) {
        // As after any floating screen, which it was painted under.
        if let Some(copy) = self.client.copy() {
            copy.forget_under();
        }
        others.give_back(self.client);
    }
}

/// Float `what` over the session until it closes. Returns `None` to go on
/// relaying, or how the relay ends: full screen, client or terminal gone,
/// clients to settle after `client.ui` or `client.per_session` changed (see
/// `main`'s `settle`), or where the picker went.
pub(super) fn float(
    a: &mut Attachment,
    master_fd: RawFd,
    winch: &winch::Winch,
    what: What,
    others: &mut dyn Others,
) -> Result<Option<Outcome>> {
    let mut glimpse = None;
    let floated = float_with(a, master_fd, winch, what, others, &mut glimpse);
    // However it ended, a client still lent goes back.
    if let Some(glimpse) = glimpse {
        glimpse.close(others);
    }
    floated
}

/// [`float`], showing the session under the picker's cursor in `glimpse`.
fn float_with(
    a: &mut Attachment,
    master_fd: RawFd,
    winch: &winch::Winch,
    what: What,
    others: &mut dyn Others,
    glimpse: &mut Option<Glimpse>,
) -> Result<Option<Outcome>> {
    let mut what = what;
    let mut panel = what.open(&a.session_id);
    // The picker, set aside while a panel it went across to is up.
    let mut under: Option<Box<dyn Floating>> = None;
    let mut size = term::terminal_size();
    let mut screen = Rect::new(0, 0, size.cols, size.rows);
    let Some(mut area) = panel.area(screen) else {
        return Ok(Some(what.full_screen()));
    };
    let (_, mut hints) = float::split(screen);
    if !a.copy().is_some_and(|c| c.is_usable()) {
        return Ok(Some(what.full_screen()));
    }
    // Flush a held first paint and what the hint bar kept back.
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
    // Client output since the last frame, owed to the terminal.
    let mut held: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    // Out of the background with the session if the terminal shows none of
    // it yet (settings floating as the relay brings it back; see `relay`).
    let (mut coming, mut away) = way_in(
        std::mem::take(&mut a.dark),
        fade_length(look),
        Instant::now(),
    );
    let mut going = false;
    // Going because `[style] float` was turned off.
    let mut unfloated = false;
    let mut way = Way::Back;
    // A frame is owed, for the panel or the session behind it.
    let mut owed = true;
    let mut session_drew = false;
    let mut last_frame: Option<Instant> = None;
    // Levels of the last frame drawn, so a transition always ends on a frame
    // at its target.
    let mut drawn_at = f32::NAN;
    let mut drawn_away = 0.0;
    let mut sequence_due: Option<Instant> = None;
    let stdin_fd = std::io::stdin().as_raw_fd();
    // The session under the picker's cursor, if not the one floated over,
    // whether or not a client could be lent to show it.
    let mut wanted: Option<String> = None;
    // What the session area shows changed: the next frame paints it whole.
    let mut fresh_behind = false;

    loop {
        let now = Instant::now();
        let moving = !coming.done(now) || !away.done(now);
        if !moving && (drawn_at != coming.to || drawn_away != away.to) {
            owed = true;
        }
        // Fade-paced frames while anything moves.
        let lively = moving || panel.moving();
        let frame_due = (lively || owed)
            .then(|| last_frame.map_or(now, |last| last + gap(lively, session_drew)));
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
        // Without a glimpse its fd is -1, which `poll` skips.
        let mut fds = [
            pollfd(stdin_fd),
            pollfd(master_fd),
            pollfd(winch.fd()),
            pollfd(glimpse.as_ref().map_or(-1, Glimpse::fd)),
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(NvmuxError::Io(err));
        }
        let now = Instant::now();

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

        let readable = n > 0 && ready(&fds[3]);
        if glimpse
            .as_mut()
            .is_some_and(|g| !g.keep(readable, &mut buf))
        {
            if let Some(gone) = glimpse.take() {
                gone.close(others);
            }
            fresh_behind = true;
            owed = true;
        } else if readable {
            owed = true;
            session_drew = true;
        }

        if n > 0 && ready(&fds[2]) {
            winch.drain();
            size = term::terminal_size();
            a.resize_to(size);
            if let Some(g) = glimpse.as_mut() {
                g.resize_to(size);
            }
            screen = Rect::new(0, 0, size.cols, size.rows);
            (_, hints) = float::split(screen);
            match panel.area(screen) {
                Some(fits) => {
                    area = fits;
                    owed = true;
                }
                // Too small now: restore the session and go full screen.
                None => {
                    panel.close();
                    put_back(a, master_fd, &mut held, &mut boundary, None)?;
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
        let was_going = going;
        // A key or mouse event gets a frame at once (see `due`).
        let mut asked = false;
        for input in inputs {
            match input {
                Input::Client(bytes) => {
                    take_step(a, Step::Forward(bytes))?;
                }
                // Once going, the panel takes no more input.
                _ if going => {}
                Input::Key(key) => {
                    owed = true;
                    asked = true;
                    if panel.on_key(key, now) == Did::Close {
                        going = true;
                    }
                }
                Input::Mouse(mouse) => {
                    owed = true;
                    asked = true;
                    let on_session = on_the_session(area, hints, mouse.column, mouse.row);
                    let press = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
                    going = (on_session && press) || panel.on_mouse(mouse, now) == Did::Close;
                }
            }
        }
        if panel.tick(now) {
            owed = true;
        }
        going |= panel.closing();
        // `[style] float` turned off (by these settings): this panel too goes
        // out with the session and comes back full screen.
        if !going && !config::get().style.float {
            going = true;
            unfloated = true;
        }
        // Just closed: start the way out from wherever the way in got to.
        if going && !was_going {
            let shown = glimpse
                .as_mut()
                .and_then(|g| g.can_be_handed_over().then(|| g.client.session_id.clone()));
            way = Way::going(panel.leave(), under.is_some(), unfloated, shown.as_deref());
            let there = coming.at(now);
            let length = fade_length(look);
            match way {
                // The panel goes, then the session undims.
                Way::Back | Way::Into => coming = Coming::to(there, 0.0, length, now),
                // Panel and session fade out together over what is left.
                Way::Away => away = Coming::to(away.at(now), 1.0, length, now),
                // The session finishes dimming and stays so for the next.
                Way::Across(_) => coming = Coming::to(there, 0.5, length, now),
            }
        }
        // Behind the panel, the session under the picker's cursor (the
        // set-aside one's under the help, settings or a prompt). Held once
        // going, but on the way back to the session floated over.
        let cursor = if !going {
            panel
                .preview()
                .or_else(|| under.as_ref().and_then(|picker| picker.preview()))
                .filter(|id| *id != a.session_id)
        } else if way == Way::Back {
            None
        } else {
            wanted.as_deref()
        };
        if cursor != wanted.as_deref() {
            wanted = cursor.map(str::to_string);
            if let Some(gone) = glimpse.take() {
                gone.close(others);
            }
            *glimpse = wanted
                .as_deref()
                .and_then(|id| others.lend(id))
                .map(|client| Glimpse::open(client, size));
            fresh_behind = true;
            owed = true;
        }
        // Gone before it could be handed over: out with the session instead.
        if way == Way::Into && glimpse.is_none() {
            way = Way::Away;
            away = Coming::to(away.at(now), 1.0, fade_length(look), now);
        }
        tell_the_client(a);
        if let Some(g) = glimpse.as_mut() {
            tell_the_client(&mut g.client);
        }

        let moving = !coming.done(now) || !away.done(now);
        if going && !moving {
            // Close it, unless it is the picker going across, which is set
            // aside to return to.
            let aside = what == What::Picker && matches!(way, Way::Across(_));
            if !aside {
                panel.close();
            }
            match way {
                // The next panel rises over the dimmed session: the set-aside
                // picker, if it is the one.
                Way::Across(next) => {
                    let set_aside = match next {
                        What::Picker => under.take(),
                        What::Help | What::Settings | What::Create(_) | What::Rename(_) => None,
                    };
                    let up = match set_aside {
                        Some(mut picker) => {
                            picker.back();
                            picker
                        }
                        None => next.open(&a.session_id),
                    };
                    let gone = std::mem::replace(&mut panel, up);
                    if aside {
                        under = Some(gone);
                    }
                    what = next;
                    match panel.area(screen) {
                        Some(fits) => area = fits,
                        None => {
                            put_back(a, master_fd, &mut held, &mut boundary, None)?;
                            return Ok(Some(what.full_screen()));
                        }
                    }
                    coming = Coming::to(0.5, 1.0, fade_length(look), now);
                    going = false;
                    way = Way::Back;
                    owed = true;
                    continue;
                }
                // A last all-background frame, then the session's mouse
                // mode; the relay has nothing left to dissolve.
                Way::Away => {
                    let painted = paint(
                        behind(a, glimpse, &held, fresh_behind),
                        look,
                        panel.as_mut(),
                        area,
                        hints,
                        coming.to,
                        1.0,
                        false,
                        &mut held,
                        &mut boundary,
                    )?;
                    if !painted {
                        owe_out(&mut held, &mut boundary)?;
                    }
                    write_stdout(term::mouse_reporting_bytes(session_mouse(a)))?;
                    a.dark = true;
                }
                Way::Back => put_back(a, master_fd, &mut held, &mut boundary, None)?,
                // Put back as the session floated over would be, then the
                // relay hands it over (see `relay`).
                Way::Into => {
                    if let Some(mut chosen) = glimpse.take() {
                        put_back(a, master_fd, &mut held, &mut boundary, Some(&mut chosen))?;
                        chosen.client.lit = true;
                        a.handing = Some(Box::new(chosen.client));
                    }
                }
            }
            if unfloated {
                return Ok(Some(what.full_screen()));
            }
            if let Some(ends) = panel.leave().and_then(ends_in) {
                return Ok(Some(ends));
            }
            let client = config::get().client;
            let settle = client.ui != was.ui || client.per_session != was.per_session;
            return Ok(settle.then_some(Outcome::Resettle));
        }
        let lively = moving || panel.moving();
        if (lively || owed) && due(now, last_frame, lively, session_drew, asked) {
            // A frame that could not go out is tried again a frame later.
            last_frame = Some(now);
            // Recomputed each frame: the picker's size follows its rows.
            if let Some(fits) = panel.area(screen) {
                area = fits;
            }
            let there = coming.at(now);
            let gone = away.at(now);
            // A row with no client to show it by.
            let blank = wanted.is_some() && glimpse.as_mut().and_then(Glimpse::copy).is_none();
            if paint(
                behind(a, glimpse, &held, fresh_behind),
                look,
                panel.as_mut(),
                area,
                hints,
                there,
                gone,
                blank,
                &mut held,
                &mut boundary,
            )? {
                owed = false;
                session_drew = false;
                fresh_behind = false;
                drawn_at = there;
                drawn_away = gone;
            }
        }
    }
}

/// The way in, and the session's extra fade beyond the dimming (0 none, 1
/// all), over `length`. If the terminal shows nothing of the session yet
/// (`dark`), both rise from the background together.
fn way_in(dark: bool, length: Duration, now: Instant) -> (Coming, Coming) {
    if dark {
        (
            Coming::to(1.0, 1.0, Duration::ZERO, now),
            Coming::to(1.0, 0.0, length, now),
        )
    } else {
        (
            Coming::to(0.0, 1.0, length, now),
            Coming::to(0.0, 0.0, Duration::ZERO, now),
        )
    }
}

/// Eased levels for a frame `there` of the way in and `away` of the way out:
/// (panel text, session dimming, whole-session fade to the background). With
/// nothing to show behind the panel (`blank`), the session is all background.
fn levels(there: f32, away: f32, blank: bool) -> (f32, f32, f32) {
    let away = falloff::ease(away);
    let text = falloff::ease(there * 2.0 - 1.0) * (1.0 - away);
    (
        text,
        falloff::ease(there * 2.0),
        if blank { 1.0 } else { away },
    )
}

/// The whole way in (session dims, then panel rises):
/// `[effects.fade] duration_ms` where the fade runs, else zero.
fn fade_length(look: Look) -> Duration {
    match look {
        Look::Dimmed(_) if fade::enabled() => {
            Duration::from_millis(config::get().effects.fade.duration_ms)
        }
        Look::Dimmed(_) | Look::Faint => Duration::ZERO,
    }
}

/// Minimum time between frames: [`SESSION_FRAME`] when only session output
/// is to show, else the fade's frame.
fn gap(lively: bool, session_drew: bool) -> Duration {
    if lively || !session_drew {
        fade::FRAME
    } else {
        SESSION_FRAME
    }
}

/// Whether a frame may go out: a [`gap`] after `last`, or at once for a key
/// or mouse event (`asked`). Otherwise the row the bar just left would be
/// part faded when first seen, and a held key's glow would beat with the
/// frames.
fn due(now: Instant, last: Option<Instant>, lively: bool, session_drew: bool, asked: bool) -> bool {
    asked || last.is_none_or(|last| now >= last + gap(lively, session_drew))
}

/// Send changed settings to a card-drawing client, so what it draws behind
/// the panel follows them (see [`card::Command::Settings`]).
fn tell_the_client(a: &mut Attachment) {
    let generation = config::generation();
    if a.draws_cards && a.heard != generation {
        a.heard = generation;
        a.tell_client(&card::Command::Settings(config::get().drawing()));
    }
}

/// Client output: to its copy and ledger now, and to the terminal with the
/// next frame, up to the last sequence boundary.
fn hold(a: &mut Attachment, chunk: &[u8], held: &mut Vec<u8>, boundary: &mut Boundary) {
    a.ledger_saw(chunk);
    a.shadow_saw(chunk);
    held.extend(boundary.relay_whole(chunk));
}

/// Write a frame (`there` of the way in, `away` of the way out) if one can
/// go out now (see [`frame_bytes`]); returns whether it did. The hint row at
/// `hints` fades with the panel. `copy` is the session behind it (see
/// [`levels`] for `blank`).
#[allow(clippy::too_many_arguments)]
fn paint(
    copy: Option<&mut Shadow>,
    look: Look,
    panel: &mut dyn Floating,
    area: Rect,
    hints: Rect,
    there: f32,
    away: f32,
    blank: bool,
    held: &mut Vec<u8>,
    boundary: &mut Boundary,
) -> Result<bool> {
    // No frame inside a sequence. One that never ends is written out as is
    // once past `HELD_MAX`.
    if boundary.mid_sequence() {
        if held.len() > HELD_MAX {
            write_stdout(held)?;
            held.clear();
        }
        return Ok(false);
    }
    let (text, dim, away) = levels(there, away, blank);
    let laid: Vec<Laid> = (text > 0.0)
        .then(|| [panel.draw(area), panel.draw_hints(hints)])
        .into_iter()
        .flatten()
        .map(|mut buf| {
            if let (Some(palette), true) = (look.palette(), text < 1.0) {
                fade::apply(&mut buf, palette, 1.0 - text);
            }
            Laid::from_buffer(&buf)
        })
        .collect();
    let Some(copy) = copy else {
        return Ok(false);
    };
    let Some(out) = frame_bytes(copy, look, &laid, area, hints, dim, away, held, boundary) else {
        return Ok(false);
    };
    write_stdout(&out)?;
    held.clear();
    Ok(true)
}

/// The copy to paint behind the panel: the glimpse's, else the session
/// floated over's. Painted whole if `fresh`, or if it is the glimpse's and
/// `held` goes out first: the glimpse is not fed the front session's bytes.
fn behind<'s>(
    a: &'s mut Attachment,
    glimpse: &'s mut Option<Glimpse>,
    held: &[u8],
    fresh: bool,
) -> Option<&'s mut Shadow> {
    let (copy, glimpsed) = match glimpse.as_mut().and_then(Glimpse::copy) {
        Some(copy) => (copy, true),
        None => (a.copy()?, false),
    };
    if fresh || (glimpsed && !held.is_empty()) {
        copy.invalidate();
    }
    Some(copy)
}

fn on_the_session(area: Rect, hints: Rect, column: u16, row: u16) -> bool {
    let at = Position::new(column, row);
    !area.contains(at) && !hints.contains(at)
}

/// A frame's bytes in one synchronized update: client output since the last
/// (its sync closes removed, so it shows with this frame), panel mouse
/// reporting, then the session dimmed `dim` with `laid` (the panel and its
/// hint row) over it. `None` mid-sequence, where nvmux cannot write: client
/// bytes always get a frame after them, so they never show through.
#[allow(clippy::too_many_arguments)]
fn frame_bytes(
    copy: &mut Shadow,
    look: Look,
    laid: &[Laid],
    area: Rect,
    hints: Rect,
    dim: f32,
    away: f32,
    held: &[u8],
    boundary: &mut Boundary,
) -> Option<Vec<u8>> {
    if boundary.mid_sequence() {
        return None;
    }
    let shade = |row: u16, col: u16| look.shade(row, col, area, hints, dim, away);
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

/// Restore the session, the panel gone: client output since the last frame,
/// then every cell repainted with the session's mouse, pen and cursor, in one
/// update. A client mid-sequence gets up to [`FRAME_WAIT`] to finish it (as on
/// resume), since the paint must go between its sequences.
///
/// The session is `onto`'s if given, to be handed over: then `a`, which
/// writes here no more, is not waited for, and a sequence it left unfinished
/// is dropped (its copy and ledger have it), or it would run into the next
/// session's bytes.
fn put_back(
    a: &mut Attachment,
    master_fd: RawFd,
    held: &mut Vec<u8>,
    boundary: &mut Boundary,
    mut onto: Option<&mut Glimpse>,
) -> Result<()> {
    let deadline = Instant::now() + FRAME_WAIT;
    let mut buf = [0u8; 8192];
    while onto.is_none() && (boundary.mid_sequence() || boundary.keeping()) {
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
    let mut unfinished = boundary.give_up();
    // The resume this replaces would have put the terminal on the alternate
    // screen.
    let mut enter_alt = false;
    let mouse = match onto.as_deref_mut() {
        Some(glimpse) => {
            unfinished.clear();
            enter_alt = !a.copy().is_some_and(|c| c.on_alternate_screen());
            glimpse.mouse()
        }
        None => session_mouse(a),
    };
    let copy = match onto {
        Some(glimpse) => glimpse.copy(),
        None => a.copy(),
    };
    let out = put_back_bytes(copy.as_deref(), held, mouse, enter_alt, &unfinished);
    if let Some(copy) = copy {
        copy.invalidate();
    }
    held.clear();
    write_stdout(&out)?;
    Ok(())
}

/// The session's mouse mode, from the ledger, else its copy of the screen.
fn session_mouse(a: &mut Attachment) -> MouseReporting {
    a.ledger
        .as_ref()
        .filter(|l| l.is_usable())
        .map(crate::ledger::Ledger::mouse)
        .or_else(|| a.copy().and_then(|c| c.mouse()))
        .unwrap_or(MouseReporting::Buttons)
}

/// The bytes that restore the session: `held` (sync closes removed), the
/// alternate screen if `enter_alt`, its `mouse`, and a full repaint of `copy`
/// with its pen and cursor, ending the update. Then `unfinished`, a sequence
/// the client left incomplete past [`put_back`]'s wait: last, so its rest
/// follows it and the paint is not written into its middle (the copy has not
/// drawn it either).
fn put_back_bytes(
    copy: Option<&Shadow>,
    held: &[u8],
    mouse: MouseReporting,
    enter_alt: bool,
    unfinished: &[u8],
) -> Vec<u8> {
    let mut out = fade::SYNC_BEGIN.to_vec();
    out.extend_from_slice(&unsynced(held));
    if enter_alt {
        out.extend_from_slice(ALT_SCREEN_ENTER);
    }
    out.extend_from_slice(term::mouse_reporting_bytes(mouse));
    match copy {
        Some(copy) => out.extend_from_slice(&copy.paint()),
        None => out.extend_from_slice(fade::SYNC_END),
    }
    out.extend_from_slice(unfinished);
    out
}

/// On exit with no frame to come, write held client output as is, complete
/// or not.
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

    /// The hint row of a 120 by 42 screen, below every cell the panel tests
    /// check.
    const HINTS: Rect = Rect::new(0, 41, 120, 1);

    #[test]
    fn the_session_dims_round_the_panel_by_the_cards_falloff() {
        let panel = Rect::new(24, 6, 72, 23);
        let look = Look::Dimmed(palette());
        let amount = |row, col| match look.shade(row, col, panel, HINTS, 1.0, 0.0) {
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
        assert_eq!(
            look.shade(0, 0, panel, HINTS, 0.0, 0.0),
            Shade::Toward(0.0),
            "not yet"
        );
    }

    #[test]
    fn without_colours_the_panel_stands_on_a_blank_margin() {
        let panel = Rect::new(24, 6, 72, 23);
        let shade = |row, col| Look::Faint.shade(row, col, panel, HINTS, 1.0, 0.0);
        assert_eq!(shade(10, 30), Shade::Blank);
        assert_eq!(shade(10, 22), Shade::Blank, "the margin, left");
        assert_eq!(shade(10, 97), Shade::Blank, "and right");
        assert_eq!(shade(5, 30), Shade::Blank, "the row above");
        assert_eq!(shade(29, 30), Shade::Blank, "and below");
        assert_eq!(shade(10, 21), Shade::Faint);
        assert_eq!(shade(4, 30), Shade::Faint);
    }

    #[test]
    fn the_session_empties_under_the_hint_row() {
        let panel = Rect::new(24, 6, 72, 23);
        let look = Look::Dimmed(palette());
        for col in [0, 60, 119] {
            let shade = |dim| look.shade(41, col, panel, HINTS, dim, 0.0);
            assert_eq!(shade(1.0), Shade::Toward(1.0), "column {col}");
            assert_eq!(shade(0.0), Shade::Toward(0.0), "not yet, column {col}");
            let faint = Look::Faint.shade(41, col, panel, HINTS, 1.0, 0.0);
            assert_eq!(faint, Shade::Blank, "column {col}");
        }
        assert_eq!(
            look.shade(40, 0, panel, HINTS, 1.0, 0.0),
            Shade::Toward(BEHIND),
            "the row over it, far from the panel"
        );
        assert_eq!(
            Look::Faint.shade(40, 0, panel, HINTS, 1.0, 0.0),
            Shade::Faint
        );
    }

    #[test]
    fn a_press_on_the_hint_row_is_not_on_the_session() {
        let panel = Rect::new(24, 6, 72, 23);
        assert!(on_the_session(panel, HINTS, 0, 0));
        assert!(on_the_session(panel, HINTS, 30, 40), "the row over it");
        assert!(!on_the_session(panel, HINTS, 30, 10), "the panel");
        for col in [0, 60, 119] {
            assert!(!on_the_session(panel, HINTS, col, 41), "column {col}");
        }
    }

    fn count(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count()
    }

    /// Client sync closes are removed and an incomplete tail is held back,
    /// so a frame always follows client bytes.
    #[test]
    fn the_sessions_bytes_go_out_only_in_front_of_a_frame() {
        let (area, hints) = (Rect::new(8, 3, 64, 18), Rect::new(0, 23, 80, 1));
        let look = Look::Dimmed(palette());
        let mut copy = Shadow::new(24, 80);
        let mut boundary = Boundary::new();
        let chunk = b"\x1b[?2026h\x1b[5;10Hbusy\x1b[?2026l\x1b]2;tit";
        copy.feed(chunk);
        let held = boundary.relay_whole(chunk);
        let out = frame_bytes(
            &mut copy,
            look,
            &[],
            area,
            hints,
            1.0,
            0.0,
            &held,
            &mut boundary,
        )
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
        let out = frame_bytes(
            &mut copy,
            look,
            &[],
            area,
            hints,
            1.0,
            0.0,
            &held,
            &mut boundary,
        )
        .expect("the next frame");
        let front = [fade::SYNC_BEGIN, b"\x1b]2;title\x07"].concat();
        assert!(
            out.starts_with(&front),
            "{:?}",
            String::from_utf8_lossy(&out)
        );
    }

    /// Once part of an unterminated string has gone out, nothing more goes
    /// out until it ends.
    #[test]
    fn nothing_goes_out_half_way_through_a_sequence() {
        let (area, hints) = (Rect::new(8, 3, 64, 18), Rect::new(0, 23, 80, 1));
        let mut copy = Shadow::new(24, 80);
        let mut boundary = Boundary::new();
        let mut held = boundary.relay_whole(b"\x1b[1;1Hbefore\x1b]52;c;");
        held.extend(boundary.relay_whole(&vec![b'A'; 128 * 1024]));
        assert!(boundary.mid_sequence());
        let frame = frame_bytes(
            &mut copy,
            Look::Faint,
            &[],
            area,
            hints,
            1.0,
            0.0,
            &held,
            &mut boundary,
        );
        assert!(frame.is_none(), "a frame inside the string");
        held.extend(boundary.relay_whole(b"\x1b\\after"));
        let out = frame_bytes(
            &mut copy,
            Look::Faint,
            &[],
            area,
            hints,
            1.0,
            0.0,
            &held,
            &mut boundary,
        )
        .expect("a frame once it has ended");
        let front = [fade::SYNC_BEGIN, b"\x1b[1;1Hbefore\x1b]52;c;AAAA"].concat();
        assert!(out.starts_with(&front));
        assert_eq!(count(&out, b"\x1b\\after"), 1);
        assert_eq!(count(&out, fade::SYNC_END), 1);
    }

    /// An unfinished tail goes after the paint, so the paint is not written
    /// into its middle.
    #[test]
    fn the_paint_that_puts_the_session_back_goes_between_its_sequences() {
        let mut copy = Shadow::new(24, 80);
        copy.feed(b"\x1b[1;1Hhello\x1b[?2026l\x1b]2;tit");
        let held = b"\x1b[1;1Hhello\x1b[?2026l";
        let out = put_back_bytes(Some(&copy), held, MouseReporting::Off, false, b"\x1b]2;tit");
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

    /// The picker's prompts float from the picker, so as outcomes they do
    /// not float.
    #[test]
    fn the_settings_the_help_the_picker_and_the_prompt_float_and_nothing_else() {
        for (outcome, what) in [
            (Outcome::ShowSettings, What::Settings),
            (Outcome::ShowHelp, What::Help),
            (Outcome::ToPicker, What::Picker),
            (Outcome::CreateNew, What::Create(Place::Last)),
        ] {
            assert_eq!(What::of(outcome, true), Some(what));
            assert_eq!(What::of(outcome, false), None, "{outcome:?}, off");
            assert_eq!(what.full_screen(), outcome);
        }
        let id = crate::ids::Id::new("abcd2345").expect("an id");
        for (what, outcome) in [
            (
                What::Create(Place::After(Some(id))),
                Outcome::CreateAfter(Some(id)),
            ),
            (What::Create(Place::After(None)), Outcome::CreateAfter(None)),
            (What::Rename(id), Outcome::Rename(id)),
        ] {
            assert_eq!(what.full_screen(), outcome);
        }
        for outcome in [
            Outcome::Detached,
            Outcome::CreateAfter(Some(id)),
            Outcome::Rename(id),
            Outcome::Resettle,
            Outcome::ChildExited,
            Outcome::StdinClosed,
        ] {
            assert_eq!(What::of(outcome, true), None, "{outcome:?}");
        }
    }

    #[test]
    fn going_away_takes_the_whole_session_to_the_background() {
        let panel = Rect::new(24, 6, 72, 23);
        let look = Look::Dimmed(palette());
        for (row, col) in [(0, 0), (10, 18), (10, 30), (40, 119)] {
            assert_eq!(
                look.shade(row, col, panel, HINTS, 1.0, 1.0),
                Shade::Toward(1.0)
            );
            let Shade::Toward(half) = look.shade(row, col, panel, HINTS, 1.0, 0.5) else {
                panic!("not toward");
            };
            let Shade::Toward(none) = look.shade(row, col, panel, HINTS, 1.0, 0.0) else {
                panic!("not toward");
            };
            assert!(half >= none, "({row}, {col}): {half} < {none}");
        }
        assert_eq!(
            Look::Faint.shade(0, 0, panel, HINTS, 1.0, 1.0),
            Shade::Blank
        );
    }

    /// The session is never undimmed in between: the first frame is all
    /// background, the last as an ordinary way in ends.
    #[test]
    fn out_of_the_dark_the_panel_and_the_session_come_in_together() {
        let t0 = Instant::now();
        let whole = Duration::from_millis(200);
        let panel = Rect::new(24, 6, 72, 23);
        let look = Look::Dimmed(palette());
        let frame =
            |(coming, away): (Coming, Coming), at| levels(coming.at(at), away.at(at), false);

        let dark = way_in(true, whole, t0);
        let (text, dim, away) = frame(dark, t0);
        assert_eq!(text, 0.0, "nothing of the panel yet");
        for (row, col) in [(0, 0), (10, 18), (10, 30), (40, 119)] {
            assert_eq!(
                look.shade(row, col, panel, HINTS, dim, away),
                Shade::Toward(1.0),
                "({row}, {col}) is the background"
            );
        }
        let (text, dim, away) = frame(dark, t0 + whole / 2);
        assert!(text > 0.0 && text < 1.0, "the panel half up: {text}");
        assert_eq!(dim, 1.0, "the session as dimmed as it goes, all the way");
        assert!(
            away > 0.0 && away < 1.0,
            "half out of the background: {away}"
        );
        assert_eq!(frame(dark, t0 + whole), (1.0, 1.0, 0.0), "up");
        assert_eq!(
            frame(dark, t0 + whole),
            frame(way_in(false, whole, t0), t0 + whole),
            "as an ordinary way in ends"
        );
        assert_eq!(frame(way_in(false, whole, t0), t0), (0.0, 0.0, 0.0));
    }

    #[test]
    fn the_pickers_ways_out() {
        let id = crate::ids::Id::new("abcd2345").expect("an id");
        assert_eq!(Way::of(None), Way::Back);
        assert_eq!(Way::of(Some(Leave::Quit)), Way::Back);
        assert_eq!(Way::of(Some(Leave::Switch(id))), Way::Away);
        assert_eq!(Way::of(Some(Leave::Help)), Way::Across(What::Help));
        assert_eq!(Way::of(Some(Leave::Settings)), Way::Across(What::Settings));
        assert_eq!(
            Way::of(Some(Leave::New(Some(id)))),
            Way::Across(What::Create(Place::After(Some(id))))
        );
        assert_eq!(
            Way::of(Some(Leave::New(None))),
            Way::Across(What::Create(Place::After(None)))
        );
        assert_eq!(
            Way::of(Some(Leave::Rename(id))),
            Way::Across(What::Rename(id))
        );
        assert_eq!(
            ends_in(Leave::Switch(id)),
            Some(Outcome::Switch(Target::Session(id)))
        );
        assert_eq!(ends_in(Leave::Quit), Some(Outcome::Detached));
        for leave in [
            Leave::Help,
            Leave::Settings,
            Leave::New(Some(id)),
            Leave::Rename(id),
        ] {
            assert_eq!(ends_in(leave), None, "{leave:?}");
        }
    }

    /// However they close, unless a prompt made a session (then gone to).
    /// With no picker set aside, back to the session; with `[style] float`
    /// turned off, out with the session.
    #[test]
    fn the_help_the_settings_and_the_prompts_go_back_to_the_picker() {
        assert_eq!(
            Way::going(None, true, false, None),
            Way::Across(What::Picker)
        );
        assert_eq!(Way::going(None, false, false, None), Way::Back);
        assert_eq!(Way::going(None, true, true, None), Way::Away);
        assert_eq!(Way::going(None, false, true, None), Way::Away);
        let id = crate::ids::Id::new("abcd2345").expect("an id");
        assert_eq!(
            Way::going(Some(Leave::Switch(id)), true, false, None),
            Way::Away,
            "a prompt that made a session goes to it"
        );
        for leave in [Leave::Help, Leave::Settings, Leave::Switch(id), Leave::Quit] {
            assert_eq!(
                Way::going(Some(leave), false, false, None),
                Way::of(Some(leave)),
                "{leave:?}"
            );
        }
    }

    /// Any other switch leaves as before: a row with nothing shown, or a
    /// number naming another row.
    #[test]
    fn a_switch_goes_into_the_session_shown_behind_the_picker() {
        let id = crate::ids::Id::new("abcd2345").expect("an id");
        let to = Some(Leave::Switch(id));
        assert_eq!(Way::going(to, false, false, Some("abcd2345")), Way::Into);
        assert_eq!(Way::going(to, false, false, Some("efgh6789")), Way::Away);
        assert_eq!(Way::going(to, false, false, None), Way::Away);
        assert_eq!(
            Way::going(to, false, true, Some("abcd2345")),
            Way::Away,
            "[style] float turned off"
        );
        for leave in [None, Some(Leave::Quit), Some(Leave::Help)] {
            assert_eq!(
                Way::going(leave, false, false, Some("abcd2345")),
                Way::going(leave, false, false, None),
                "{leave:?}"
            );
        }
    }

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

    #[test]
    fn a_key_is_answered_at_once_and_the_rest_at_their_pace() {
        let t0 = Instant::now();
        let soon = t0 + Duration::from_millis(1);
        assert!(
            due(soon, Some(t0), true, false, true),
            "a key, just after a frame"
        );
        assert!(
            due(soon, Some(t0), false, true, true),
            "or after the session drew"
        );
        assert!(!due(soon, Some(t0), true, false, false), "a fade waits");
        assert!(due(t0 + fade::FRAME, Some(t0), true, false, false));
        assert!(
            !due(t0 + fade::FRAME, Some(t0), false, true, false),
            "and the session longer"
        );
        assert!(due(t0 + SESSION_FRAME, Some(t0), false, true, false));
        assert!(due(t0, None, false, false, false), "the first frame");
    }

    /// A row with no client to show it by: the panel as ever, nothing behind.
    #[test]
    fn a_row_shown_blank_takes_the_session_and_leaves_the_panel() {
        assert_eq!(levels(1.0, 0.0, true), (1.0, 1.0, 1.0));
        assert_eq!(levels(1.0, 0.0, false), (1.0, 1.0, 0.0));
        let (text, _, away) = levels(0.75, 0.0, true);
        assert_eq!(away, 1.0);
        assert_eq!(text, levels(0.75, 0.0, false).0);
    }

    /// Given back what was lent, as the session loop would be.
    #[derive(Default)]
    struct Lender(Vec<Attachment>);

    impl Others for Lender {
        fn begin_next(&mut self, _: Target) -> Option<card::Card> {
            None
        }

        fn give_back(&mut self, client: Attachment) {
            self.0.push(client);
        }
    }

    /// Read a shown session's output until `done`, or give up.
    fn read_until(glimpse: &mut Glimpse, done: impl Fn(&mut Glimpse) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 8192];
        while !done(glimpse) {
            if Instant::now() > deadline {
                return false;
            }
            let mut p = [pollfd(glimpse.fd())];
            unsafe { libc::poll(p.as_mut_ptr(), 1, 50) };
            if !glimpse.keep(ready(&p[0]), &mut buf) {
                return done(glimpse);
            }
        }
        true
    }

    /// A stand-in that says what it reads of its input back as a title, in
    /// hex, once `n` bytes have come.
    fn told(n: usize) -> String {
        format!(
            r#"printf r; heard=$(head -c {n} | od -An -tx1 | tr -d ' \n'); printf '\033]0;%s\007' "$heard"; exec sleep 30"#
        )
    }

    fn title(glimpse: &Glimpse) -> Option<&[u8]> {
        glimpse
            .client
            .ledger
            .as_ref()
            .and_then(crate::ledger::Ledger::title)
    }

    /// Back from its crawl, so its screen keeps up while it is shown.
    #[test]
    fn a_parked_session_shown_behind_the_picker_is_told_it_is_back() {
        let said = card::encode(&card::Command::Clear);
        let mut client = crate::pty::kept_standin("s1", &told(said.len()));
        assert!(client.ready_for_test(), "the stand-in never got ready");
        client.draws_cards = true;
        client.away = true;
        client.crawling = true;
        let size = pty_size(&client.master).expect("its size");
        let mut glimpse = Glimpse::open(client, size);
        assert!(!glimpse.client.away && !glimpse.client.crawling);
        let mut heard = b"\x1b]0;".to_vec();
        for b in &said {
            heard.extend_from_slice(format!("{b:02x}").as_bytes());
        }
        heard.push(0x07);
        assert!(
            read_until(&mut glimpse, |g| title(g) == Some(&heard[..])),
            "{:?}",
            title(&glimpse).map(String::from_utf8_lossy)
        );
        let mut lender = Lender::default();
        glimpse.close(&mut lender);
        assert_eq!(lender.0.len(), 1, "not given back");
    }

    /// All it writes is kept for its first visit, so it stays at its crawl
    /// unless it must redraw at a new size.
    #[test]
    fn a_session_started_ahead_is_shown_from_what_it_said() {
        let mut client =
            crate::pty::kept_standin("s1", r"sleep 0.2; printf '\033[3;1Hlater'; exec sleep 30");
        client.draws_cards = true;
        client.away = true;
        client.unshown = Some(b"\x1b[?1049h\x1b[2;3Hhello".to_vec());
        let size = pty_size(&client.master).expect("its size");
        let mut glimpse = Glimpse::open(client, size);
        assert!(glimpse.client.away, "told it is back");
        let painted = |g: &mut Glimpse, text: &[u8]| {
            g.copy()
                .is_some_and(|copy| crate::test_support::contains(&copy.paint(), text))
        };
        assert!(painted(&mut glimpse, b"hello"));
        assert!(read_until(&mut glimpse, |g| painted(g, b"later")));
        let unshown = glimpse.client.unshown.as_deref().unwrap_or_default();
        assert!(unshown.starts_with(b"\x1b[?1049h\x1b[2;3Hhello"));
        assert!(unshown.ends_with(b"later"), "kept for its first visit");

        let bigger = PtySize {
            rows: size.rows + 2,
            ..size
        };
        glimpse.resize_to(bigger);
        assert!(!glimpse.client.away, "a new size drawn at once");
        assert_eq!(pty_size(&glimpse.client.master), Some(bigger));
        let shown = glimpse.copy().map(|copy| copy.size());
        assert_eq!(shown, Some((bigger.rows, bigger.cols)), "painted whole");
    }

    #[test]
    fn a_session_shown_that_ends_goes_back_to_be_seen_gone() {
        let client = crate::pty::kept_standin("s1", "printf bye; exit 0");
        let size = pty_size(&client.master).expect("its size");
        let mut glimpse = Glimpse::open(client, size);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 8192];
        let mut kept = true;
        while kept && Instant::now() < deadline {
            let mut p = [pollfd(glimpse.fd())];
            unsafe { libc::poll(p.as_mut_ptr(), 1, 50) };
            kept = glimpse.keep(ready(&p[0]), &mut buf);
        }
        assert!(!kept, "kept past its client's end");
    }

    /// The glimpse is not fed the front session's bytes that go out ahead of
    /// its frame, which would otherwise show through where it painted nothing.
    #[test]
    fn the_session_shown_is_painted_whole_after_bytes_it_was_not_fed() {
        // Its text far enough from the panel to be dimmed only half way.
        let (area, hints) = (Rect::new(24, 6, 72, 23), HINTS);
        let look = Look::Dimmed(palette());
        let mut a = crate::pty::kept_standin("s1", "exec sleep 30");
        a.screen = Some(Shadow::new(42, 120));
        let mut shown = crate::pty::kept_standin("s2", "exec sleep 30");
        let mut copy = Shadow::new(42, 120);
        copy.feed(b"\x1b[1;1Hshown");
        shown.screen = Some(copy);
        let mut glimpse = Some(Glimpse {
            client: shown,
            copy: None,
        });
        let mut boundary = Boundary::new();
        let mut frame = |held: &[u8], fresh: bool| {
            let copy = behind(&mut a, &mut glimpse, held, fresh).expect("a copy");
            frame_bytes(copy, look, &[], area, hints, 1.0, 0.0, held, &mut boundary)
                .expect("a frame")
        };
        let whole = |out: &[u8]| crate::test_support::contains(out, b"shown");
        assert!(whole(&frame(b"", false)));
        assert!(!whole(&frame(b"", false)), "nothing changed");
        assert!(whole(&frame(b"", true)), "fresh");
        assert!(whole(&frame(b"\x1b[1;1Hfront", false)), "after the front's");
    }

    /// The resume it stands in for would have entered it.
    #[test]
    fn a_session_handed_over_is_put_on_the_alternate_screen() {
        let mut copy = Shadow::new(24, 80);
        copy.feed(b"\x1b[1;1Hhello");
        let out = put_back_bytes(Some(&copy), b"held", MouseReporting::Off, true, b"");
        let front = [fade::SYNC_BEGIN, b"held", ALT_SCREEN_ENTER].concat();
        assert!(
            out.starts_with(&front),
            "{:?}",
            String::from_utf8_lossy(&out)
        );
        assert_eq!(count(&out, fade::SYNC_END), 1);
        let out = put_back_bytes(Some(&copy), b"held", MouseReporting::Off, false, b"");
        assert_eq!(count(&out, ALT_SCREEN_ENTER), 0);
    }
}
