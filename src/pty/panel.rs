//! The settings, help, picker and naming prompt floating over a session:
//! `<prefix> ,`, `<prefix> ?`, `<prefix> Space`, `<prefix> c`, and the
//! picker's `c` and `r`.
//!
//! The screen draws into a panel with round corners (see [`crate::ui::float`])
//! while the session keeps running behind it, dimmed half way to the
//! background, more near the panel by the cards' falloff ([`crate::falloff`]),
//! and blank under it. The hint row does not float: it stays on the
//! terminal's last row, full width ([`Floating::draw_hints`]), and the session
//! is blank under it too.
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
//! would ([`Glimpse`]), or nothing if it has none running. A move dips
//! through the background ([`Dip`]): the one shown fades out, and the next
//! comes up once the cursor rests.
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

/// The blank margin round the panel when there are no colours, in columns:
/// two, or a row.
const MARGIN: f32 = falloff::ASPECT;

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
        let d = float::distance(panel, col, row);
        match self {
            Look::Dimmed(_) => {
                let amount = if d == 0.0 || under_hints {
                    1.0
                } else {
                    1.0 - (1.0 - BEHIND) * (1.0 - falloff::around(d, falloff::PEAK, falloff::REACH))
                };
                let dimmed = amount * dim;
                Shade::Toward(dimmed + (1.0 - dimmed) * away)
            }
            Look::Faint if away > 0.0 => Shade::Blank,
            Look::Faint if d <= MARGIN || under_hints => Shade::Blank,
            Look::Faint => Shade::Faint,
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
    /// of back, and into `into`, a session that can come up where it is
    /// behind the panel (see [`going_into`]), instead of away to it.
    fn going(
        leave: Option<Leave>,
        picker_under: bool,
        unfloated: bool,
        into: Option<&str>,
    ) -> Self {
        if unfloated {
            return Way::Away;
        }
        match (Way::of(leave), leave) {
            (Way::Back, _) if picker_under => Way::Across(What::Picker),
            (Way::Away, Some(Leave::Switch(id))) if into == Some(id.as_str()) => Way::Into,
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

/// The session shown behind the picker, and how far it has dipped toward the
/// background to make way for the next: a move fades it out, and the next
/// comes up once the cursor has rested on its row, so a held key passes rows
/// in the dark and lends none of them.
struct Dip {
    /// What is to be shown: `None` is the session floated over.
    wanted: Option<String>,
    /// When `wanted` last changed.
    since: Instant,
    shown: Option<String>,
    /// 0 shown as it is, 1 the background.
    out: Coming,
}

impl Dip {
    fn new(shown: Option<String>, now: Instant) -> Self {
        Self {
            wanted: shown.clone(),
            since: now,
            shown,
            out: Coming::to(0.0, 0.0, Duration::ZERO, now),
        }
    }

    fn want(&mut self, id: Option<&str>, now: Instant) {
        if id != self.wanted.as_deref() {
            self.wanted = id.map(str::to_string);
            self.since = now;
        }
    }

    /// When `wanted` is due to be shown, if it is not: once out at the
    /// background, and `rest` after it was last changed.
    fn due_at(&self, rest: Duration) -> Option<Instant> {
        let out = self.out.start + self.out.length;
        (self.shown != self.wanted).then(|| out.max(self.since + rest))
    }

    /// Fade toward showing `wanted`: out while another is shown, back up once
    /// it is, and out at once if `blank`, with nothing to show. Returns
    /// whether `wanted` is now due ([`Dip::show`]).
    fn step(&mut self, blank: bool, one_way: Duration, rest: Duration, now: Instant) -> bool {
        if blank {
            self.out = Coming::to(1.0, 1.0, Duration::ZERO, now);
        } else {
            let to = if self.shown == self.wanted { 0.0 } else { 1.0 };
            if self.out.to != to {
                self.out = Coming::to(self.out.at(now), to, one_way, now);
            }
        }
        self.due_at(rest).is_some_and(|due| now >= due)
    }

    /// `wanted`, up from the background over `one_way` unless `blank`.
    fn show(&mut self, blank: bool, one_way: Duration, now: Instant) {
        self.shown = self.wanted.clone();
        let to = if blank { 1.0 } else { 0.0 };
        self.out = Coming::to(1.0, to, one_way, now);
    }
}

/// What the session area is to show as the panel goes by `going` (`None`
/// while it is up): the cursor's row while up, the session floated over on
/// the way back, the one switched into. `None` holds it as it is.
fn wanted_behind<'a>(
    going: Option<Way>,
    cursor: Option<&'a str>,
    switched: Option<&'a str>,
) -> Option<Option<&'a str>> {
    match going {
        None => Some(cursor),
        Some(Way::Back) => Some(None),
        // Even with the dip on its way to another row: the switch's is the one
        // handed over.
        Some(Way::Into) => Some(switched),
        Some(Way::Across(_) | Way::Away) => None,
    }
}

/// The session a switch from `panel` can go into where it is: the one shown,
/// if it can be handed over, or the cursor's row the dip has yet to show,
/// checked once lent.
fn going_into(panel: &dyn Floating, dip: &Dip, glimpse: &mut Option<Glimpse>) -> Option<String> {
    let Some(Leave::Switch(id)) = panel.leave() else {
        return None;
    };
    let id = id.as_str();
    let shown = dip.shown.as_deref() == Some(id);
    let coming = !shown && panel.preview() == Some(id);
    let ready = shown && glimpse.as_mut().is_some_and(|g| g.can_be_handed_over());
    (ready || coming).then(|| id.to_string())
}

/// A session shown behind the panel in place of the one it floats over (see
/// [`Dip`]): its client, lent by the session loop ([`Others::lend`]) and read
/// here as its thread would.
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
        let resized = pty_size(&self.client.master) != Some(size);
        self.client.resize_to(size);
        if let Some(copy) = self.copy.as_mut() {
            copy.resize(size.rows, size.cols);
        }
        self.client.tell_its_size(size, resized);
        if resized {
            self.come_back();
        }
    }

    /// Read what it wrote if `readable`, send it input left waiting for room
    /// if `writable`, and look at a started-ahead client's probe, as its
    /// thread would. `false` once it has gone or is to be retired: it then
    /// goes back for its thread to see to.
    fn keep(&mut self, readable: bool, writable: bool, buf: &mut [u8]) -> bool {
        if !self.client.kept_by_its_probe() {
            self.client.hang_up();
            return false;
        }
        if writable && self.client.input.send().is_err() {
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
    // The cursor's row comes up with the panel: the dip is for moves.
    let first = panel
        .preview()
        .filter(|id| *id != a.session_id)
        .map(str::to_string);
    *glimpse = first
        .as_deref()
        .and_then(|id| others.lend(id))
        .map(|client| Glimpse::open(client, size));
    let mut dip = Dip::new(first, Instant::now());
    let mut drawn_out = 0.0;
    // What the session area shows changed: the next frame paints it whole.
    let mut fresh_behind = true;

    loop {
        let now = Instant::now();
        let moving = !coming.done(now) || !away.done(now) || !dip.out.done(now);
        if !moving && (drawn_at != coming.to || drawn_away != away.to || drawn_out != dip.out.to) {
            owed = true;
        }
        // Fade-paced frames while anything moves.
        let lively = moving || panel.moving();
        let frame_due = (lively || owed)
            .then(|| last_frame.map_or(now, |last| last + gap(lively, session_drew)));
        // Nothing is shown anew on the way out with the session (see below).
        let shown_due = (!going || way != Way::Away)
            .then(|| dip.due_at(rest(look, going)))
            .flatten();
        let next = [frame_due, sequence_due, panel.wake_at(), shown_due]
            .into_iter()
            .flatten()
            .min();
        let timeout = next.map_or(super::IDLE_POLL_MS, |d| {
            let left = d.saturating_duration_since(Instant::now());
            left.as_micros()
                .div_ceil(1000)
                .min(super::IDLE_POLL_MS as u128) as libc::c_int
        });
        // Stdin is read even while the client is behind on its input: only
        // terminal replies go to it from here. Without a glimpse its fd is
        // -1, which `poll` skips.
        let mut fds = [
            pollfd(stdin_fd),
            a.master_pollfd(),
            pollfd(winch.fd()),
            glimpse
                .as_ref()
                .map_or_else(|| pollfd(-1), |g| g.client.master_pollfd()),
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
        if n > 0 && fds[1].revents & libc::POLLOUT != 0 {
            a.input.send()?;
        }

        let readable = n > 0 && ready(&fds[3]);
        let writable = n > 0 && fds[3].revents & libc::POLLOUT != 0;
        if glimpse
            .as_mut()
            .is_some_and(|g| !g.keep(readable, writable, &mut buf))
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
        // A key or mouse event, or a session newly shown, gets a frame at
        // once (see `due`).
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
            let into = going_into(panel.as_ref(), &dip, glimpse);
            way = Way::going(panel.leave(), under.is_some(), unfloated, into.as_deref());
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
        // set-aside one's under the help, settings or a prompt).
        let cursor = panel
            .preview()
            .or_else(|| under.as_ref().and_then(|picker| picker.preview()))
            .filter(|id| *id != a.session_id);
        let switched = match panel.leave() {
            Some(Leave::Switch(id)) => Some(id),
            _ => None,
        };
        let switched = switched.as_ref().map(crate::ids::Id::as_str);
        if let Some(wanted) = wanted_behind(going.then_some(way), cursor, switched) {
            dip.want(wanted, now);
        }
        let one_way = dip_length(look);
        let blank = dip.shown.is_some() && glimpse.as_mut().and_then(Glimpse::copy).is_none();
        // Nothing more is lent on the way out with the session.
        if dip.step(blank, one_way, rest(look, going), now) && !(going && way == Way::Away) {
            if let Some(gone) = glimpse.take() {
                gone.close(others);
            }
            *glimpse = dip
                .wanted
                .as_deref()
                .and_then(|id| others.lend(id))
                .map(|client| Glimpse::open(client, size));
            let blank = dip.wanted.is_some() && glimpse.as_mut().and_then(Glimpse::copy).is_none();
            dip.show(blank, one_way, now);
            fresh_behind = true;
            owed = true;
            // Its first frame, all background, goes out now, so the dip's
            // bottom is drawn.
            asked = true;
        }
        // Gone, or once lent not to be handed over: out with the session
        // instead.
        if way == Way::Into
            && dip.shown == dip.wanted
            && !glimpse.as_mut().is_some_and(|g| g.can_be_handed_over())
        {
            way = Way::Away;
            away = Coming::to(away.at(now), 1.0, fade_length(look), now);
        }
        tell_the_client(a);
        if let Some(g) = glimpse.as_mut() {
            tell_the_client(&mut g.client);
        }

        let moving = !coming.done(now) || !away.done(now) || !dip.out.done(now);
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
                        dip.out.to,
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
                        debug_assert!(matches!(
                            panel.leave(),
                            Some(Leave::Switch(id)) if id.as_str() == chosen.client.session_id
                        ));
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
            let out = dip.out.at(now);
            if paint(
                behind(a, glimpse, &held, fresh_behind),
                look,
                panel.as_mut(),
                area,
                hints,
                there,
                gone,
                out,
                &mut held,
                &mut boundary,
            )? {
                owed = false;
                session_drew = false;
                fresh_behind = false;
                drawn_at = there;
                drawn_away = gone;
                drawn_out = out;
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

/// Eased levels for a frame `there` of the way in, `away` of the way out and
/// the session behind the panel `out` of the way to the background (see
/// [`Dip`]): (panel text, session dimming, whole-session fade to the
/// background).
fn levels(there: f32, away: f32, out: f32) -> (f32, f32, f32) {
    let away = falloff::ease(away);
    let text = falloff::ease(there * 2.0 - 1.0) * (1.0 - away);
    let gone = away + (1.0 - away) * falloff::ease(out);
    (text, falloff::ease(there * 2.0), gone)
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

/// One way of a [`Dip`]: half of `[effects.fade] duration_ms` where a session
/// dissolves, else zero, a cut.
fn dip_length(look: Look) -> Duration {
    match look {
        Look::Dimmed(_) if fade::enabled() && fade::session() => {
            config::get().effects.fade.one_way()
        }
        Look::Dimmed(_) | Look::Faint => Duration::ZERO,
    }
}

/// How long the cursor rests on a row before the [`Dip`] shows it: not at all
/// once the panel is going, as the cursor no longer moves.
fn rest(look: Look, going: bool) -> Duration {
    if going {
        Duration::ZERO
    } else {
        dip_length(look)
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

/// Whether a frame may go out: a [`gap`] after `last`, or at once if `asked`,
/// for a key or mouse event or the bottom of a [`Dip`]. Otherwise the row the
/// bar just left would be part faded when first seen, a held key's glow would
/// beat with the frames, and a dip could miss the background.
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
/// `hints` fades with the panel. `copy` is the session behind it, `out` of
/// the way to the background (see [`levels`]).
#[allow(clippy::too_many_arguments)]
fn paint(
    copy: Option<&mut Shadow>,
    look: Look,
    panel: &mut dyn Floating,
    area: Rect,
    hints: Rect,
    there: f32,
    away: f32,
    out: f32,
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
    let (text, dim, away) = levels(there, away, out);
    let faded = |mut buf| {
        if let (Some(palette), true) = (look.palette(), text < 1.0) {
            fade::apply(&mut buf, palette, 1.0 - text);
        }
        buf
    };
    let laid: Vec<Laid> = (text > 0.0)
        .then(|| {
            [
                Laid::rounded(&faded(panel.draw(area)), float::RADIUS),
                Laid::from_buffer(&faded(panel.draw_hints(hints))),
            ]
        })
        .into_iter()
        .flatten()
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
    float::distance(area, column, row) > 0.0 && !hints.contains(Position::new(column, row))
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
    fn the_panels_corners_are_round() {
        let panel = Rect::new(24, 6, 72, 23);
        let look = Look::Dimmed(palette());
        let amount = |row, col| match look.shade(row, col, panel, HINTS, 1.0, 0.0) {
            Shade::Toward(t) => t,
            other => panic!("{other:?}"),
        };
        assert!(amount(6, 24) < falloff::EMPTIED, "the corner cell shows");
        assert!(amount(7, 24) < falloff::EMPTIED && amount(28, 95) < falloff::EMPTIED);
        assert_eq!(amount(6, 28), 1.0, "under it past the curve");
        assert_eq!(amount(7, 25), 1.0);
        let faint = |row, col| Look::Faint.shade(row, col, panel, HINTS, 1.0, 0.0);
        assert_eq!(faint(5, 22), Shade::Faint, "off the margin's corner");
        assert_eq!(faint(6, 23), Shade::Faint);
        assert_eq!(faint(6, 24), Shade::Blank, "in the margin");
        assert!(on_the_session(panel, HINTS, 24, 6), "a press on the corner");
        assert!(!on_the_session(panel, HINTS, 28, 6));
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
        let frame = |(coming, away): (Coming, Coming), at| levels(coming.at(at), away.at(at), 0.0);

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

    /// All the way out is also how a row with no client to show it by looks.
    #[test]
    fn the_dip_takes_the_session_and_leaves_the_panel() {
        assert_eq!(levels(1.0, 0.0, 0.0), (1.0, 1.0, 0.0));
        assert_eq!(levels(1.0, 0.0, 0.5), (1.0, 1.0, 0.5), "half way out");
        assert_eq!(levels(1.0, 0.0, 1.0), (1.0, 1.0, 1.0));
        let (text, _, away) = levels(0.75, 0.0, 1.0);
        assert_eq!(away, 1.0);
        assert_eq!(text, levels(0.75, 0.0, 0.0).0);
        assert_eq!(levels(1.0, 0.5, 1.0).2, 1.0, "on the way out too");
    }

    /// One way of the dip, as `[effects.fade] duration_ms = 200`.
    const ONE_WAY: Duration = Duration::from_millis(100);
    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_move_dips_out_then_shows_the_next_from_the_background() {
        let t0 = Instant::now();
        let mut dip = Dip::new(None, t0);
        dip.want(Some("b"), t0);
        assert!(!dip.step(false, ONE_WAY, ONE_WAY, t0));
        assert!(
            (dip.out.at(t0 + ONE_WAY / 2) - 0.5).abs() < 1e-3,
            "half out"
        );
        assert!(!dip.step(false, ONE_WAY, ONE_WAY, t0 + ONE_WAY - MS));
        assert!(dip.step(false, ONE_WAY, ONE_WAY, t0 + ONE_WAY));
        dip.show(false, ONE_WAY, t0 + ONE_WAY);
        assert_eq!(dip.shown.as_deref(), Some("b"));
        assert_eq!(dip.out.at(t0 + ONE_WAY), 1.0, "its first frame");
        assert!(!dip.step(false, ONE_WAY, ONE_WAY, t0 + ONE_WAY + MS));
        assert_eq!(dip.out.at(t0 + 2 * ONE_WAY), 0.0);
        assert_eq!(dip.due_at(ONE_WAY), None);
    }

    /// Each move restarts the rest, so rows passed over are never lent.
    #[test]
    fn a_held_key_passes_rows_in_the_dark() {
        let t0 = Instant::now();
        let mut dip = Dip::new(None, t0);
        let mut at = t0;
        for row in ["b", "c", "d", "e", "f", "g"] {
            dip.want(Some(row), at);
            assert!(!dip.step(false, ONE_WAY, ONE_WAY, at), "{row}");
            at += 30 * MS;
        }
        let last = at - 30 * MS;
        assert_eq!(dip.out.at(last), 1.0, "dark since the first one way");
        assert_eq!(dip.due_at(ONE_WAY), Some(last + ONE_WAY));
        assert!(!dip.step(false, ONE_WAY, ONE_WAY, last + ONE_WAY - MS));
        assert!(dip.step(false, ONE_WAY, ONE_WAY, last + ONE_WAY));
    }

    /// The poll wakes for it when `step` shows it, so a wake gone by always
    /// shows it and the loop cannot spin: a rest after the move, or later,
    /// when out (here a shorter rest, the fade changed meanwhile).
    #[test]
    fn a_dip_is_due_when_it_says() {
        let t0 = Instant::now();
        let mut dip = Dip::new(None, t0);
        dip.want(Some("b"), t0);
        dip.step(false, ONE_WAY, ONE_WAY, t0);
        // Back, and off again: out from part way, by about 120 ms.
        dip.want(None, t0 + 60 * MS);
        dip.step(false, ONE_WAY, ONE_WAY, t0 + 60 * MS);
        dip.want(Some("c"), t0 + 70 * MS);
        dip.step(false, ONE_WAY, ONE_WAY, t0 + 70 * MS);
        assert_eq!(dip.due_at(ONE_WAY), Some(t0 + 170 * MS));
        let out = dip.due_at(ONE_WAY / 4).expect("due");
        assert!(out > t0 + 119 * MS && out < t0 + 121 * MS, "{:?}", out - t0);
        for rest in [ONE_WAY, ONE_WAY / 4] {
            let due = dip.due_at(rest).expect("due");
            assert!(!dip.step(false, ONE_WAY, rest, due - MS), "{rest:?}");
            assert!(dip.step(false, ONE_WAY, rest, due), "{rest:?}");
        }
    }

    #[test]
    fn back_on_the_row_shown_it_comes_back_up_from_where_it_got_to() {
        let t0 = Instant::now();
        let mut dip = Dip::new(Some("b".to_string()), t0);
        dip.want(Some("c"), t0);
        dip.step(false, ONE_WAY, ONE_WAY, t0);
        let back = t0 + 40 * MS;
        dip.want(Some("b"), back);
        assert!(!dip.step(false, ONE_WAY, ONE_WAY, back));
        assert_eq!(dip.due_at(ONE_WAY), None, "nothing to show anew");
        assert!((dip.out.at(back) - 0.4).abs() < 1e-3);
        assert!(
            !dip.out.done(back + 39 * MS) && dip.out.done(back + 41 * MS),
            "as fast as it went"
        );
    }

    /// No client to show it by, or its client gone half way up.
    #[test]
    fn with_nothing_to_show_it_is_the_background_at_once() {
        let t0 = Instant::now();
        let mut dip = Dip::new(None, t0);
        dip.want(Some("b"), t0);
        dip.step(false, ONE_WAY, ONE_WAY, t0);
        assert!(dip.step(false, ONE_WAY, ONE_WAY, t0 + ONE_WAY));
        dip.show(true, ONE_WAY, t0 + ONE_WAY);
        let later = t0 + 3 * ONE_WAY;
        assert!(!dip.step(true, ONE_WAY, ONE_WAY, later));
        assert_eq!(dip.out.at(later), 1.0);

        let mut dip = Dip::new(None, t0);
        dip.want(Some("b"), t0);
        dip.step(false, ONE_WAY, ONE_WAY, t0);
        dip.step(false, ONE_WAY, ONE_WAY, t0 + ONE_WAY);
        dip.show(false, ONE_WAY, t0 + ONE_WAY);
        let half = t0 + ONE_WAY + ONE_WAY / 2;
        assert!(dip.out.at(half) < 1.0);
        dip.step(true, ONE_WAY, ONE_WAY, half);
        assert_eq!(dip.out.at(half), 1.0);
    }

    /// Going, the cursor no longer moves, so nothing waits for it to rest.
    #[test]
    fn going_or_without_the_fade_nothing_waits() {
        let t0 = Instant::now();
        let mut dip = Dip::new(Some("b".to_string()), t0);
        dip.want(Some("c"), t0);
        dip.step(false, ONE_WAY, ONE_WAY, t0);
        dip.want(None, t0 + 90 * MS);
        assert!(!dip.step(false, ONE_WAY, Duration::ZERO, t0 + 90 * MS));
        assert!(
            dip.step(false, ONE_WAY, Duration::ZERO, t0 + ONE_WAY),
            "as soon as it is out"
        );
        assert_eq!(
            dip.due_at(ONE_WAY),
            Some(t0 + 190 * MS),
            "with the panel up"
        );

        let mut cut = Dip::new(None, t0);
        cut.want(Some("b"), t0);
        assert!(cut.step(false, Duration::ZERO, Duration::ZERO, t0));
        cut.show(false, Duration::ZERO, t0);
        assert_eq!(cut.out.at(t0), 0.0, "without the fade, a cut");
    }

    /// Into, the switch's session is the one handed over even with the dip on
    /// its way to another row: it turns back rather than lend that one.
    #[test]
    fn what_the_session_area_is_to_show_as_the_panel_goes() {
        assert_eq!(wanted_behind(None, Some("c"), None), Some(Some("c")));
        assert_eq!(wanted_behind(Some(Way::Back), Some("c"), None), Some(None));
        assert_eq!(
            wanted_behind(Some(Way::Into), Some("c"), Some("b")),
            Some(Some("b"))
        );
        for way in [Way::Away, Way::Across(What::Help)] {
            assert_eq!(
                wanted_behind(Some(way), Some("c"), Some("b")),
                None,
                "{way:?}"
            );
        }
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
            let mut p = [glimpse.client.master_pollfd()];
            unsafe { libc::poll(p.as_mut_ptr(), 1, 50) };
            let writable = p[0].revents & libc::POLLOUT != 0;
            if !glimpse.keep(ready(&p[0]), writable, &mut buf) {
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
            let mut p = [glimpse.client.master_pollfd()];
            unsafe { libc::poll(p.as_mut_ptr(), 1, 50) };
            let writable = p[0].revents & libc::POLLOUT != 0;
            kept = glimpse.keep(ready(&p[0]), writable, &mut buf);
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
