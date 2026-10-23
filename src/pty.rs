//! The PTY proxy.
//!
//! nvmux runs a Neovim client (`nvmux --client <sock>`, see [`crate::client`],
//! or `nvim --server <sock> --remote-ui` with `[client] ui = "nvim"`) on a PTY
//! and sits between it and the user's terminal:
//!
//! ```text
//! stdin  -> [ prefix state machine ] -> pty master
//! stdout <-      (untouched)        <- pty master
//! ```
//!
//! **The child-to-terminal direction is never rewritten or acted on.** The
//! child negotiates directly with the real terminal, which is what makes
//! bracketed paste, kitty keys, truecolor, undercurl, titles, OSC 52 and
//! DA1/XTGETTCAP work. The shadow grid ([`crate::shadow`], for the fade) and
//! a kept client's ledger ([`crate::ledger`]) get copies; while an overlay is
//! up, the boundary ([`crate::boundary`]) may briefly hold back a trailing
//! partial sequence.
//!
//! Output is *held* only for a fading first paint ([`Hold`]), for a client
//! started ahead ([`Attachment::start_ahead`]) and, a frame at a time, under
//! a floating panel ([`panel`]). **A held byte is never reordered or
//! changed**; only synchronized-update ends are dropped, where held bytes go
//! out in one update of nvmux's.
//!
//! nvmux's own output (fade frames, the notice and hint bar, a kept client's
//! repainted screen, floating panels) is never fed to the shadow. Overlays
//! drawn during a relay go between the child's escape sequences, found by a
//! decoder rather than a lull (a session repainting at 60 fps never pauses
//! 25 ms), in one synchronized update with the session's bytes (see
//! [`Attachment::write_session_frame`]).
//!
//! **The child's exit code carries no information** (0 for a killed server, 1
//! for a detach or a failed attach); teardown is driven off the master read
//! result instead.

use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use memchr::memmem;
use pty_process::blocking::{Command, Pty};

use crate::error::{NvmuxError, Result};
use crate::handoff::HandOff;
use crate::keys::{self, Action, Pending, Prefix, Step, Wait};
use crate::ledger::Ledger;
use crate::term::write_stdout;
use crate::{announce, boundary, card, config, fade, hint, keyseq, rpc, shadow, term, winch};

mod panel;

/// A terminal size as given to `TIOCSWINSZ`. Ours rather than the pty
/// crate's `Size`, whose fields are private.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl From<PtySize> for pty_process::Size {
    fn from(size: PtySize) -> Self {
        Self::new_with_pixel(size.rows, size.cols, size.pixel_width, size.pixel_height)
    }
}

/// Neovim aborts on the 17th attached UI instead of returning an error
/// (`src/nvim/ui.c`), killing the server and the session. Refuse well before.
const MAX_UIS: usize = 8;

// Checked at compile time: getting this wrong SIGABRTs a user's session.
const _: () = assert!(MAX_UIS < 16);

/// Appended to the notice's session name while the session waits for a key;
/// cards strip it and say it their own way.
const WAITING: &str = " — press a key";

/// How the relay ended; the attachment is handed back separately by
/// [`relay`]. Outcomes that open a screen, switch or resettle leave the
/// child running.
///
/// "Cannot float" below means `[style] float` is off, the terminal is too
/// small, or nvmux keeps no copy of the session's screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// `<prefix> Space` where the picker cannot float: show it full screen.
    ToPicker,
    /// `<prefix> d`: detach and exit, leaving the session running.
    Detached,
    /// `<prefix> c` where the prompt cannot float: prompt for a name full
    /// screen and create a session. Cancelling returns to this one.
    CreateNew,
    /// `c` on the floating picker where the prompt cannot float: create a
    /// session placed after the one named (the picker's cursor row). Cancelling
    /// returns here with the picker floating again (see
    /// [`Attachment::float_the_picker`]).
    CreateAfter(Option<crate::ids::Id>),
    /// `r` on the floating picker where the prompt cannot float: rename the
    /// named session full screen, then return here with the picker floating.
    Rename(crate::ids::Id),
    /// `<prefix> ?` where the help cannot float: show it full screen.
    ShowHelp,
    /// `<prefix> ,` where the settings cannot float, or the floating settings
    /// were told not to float: show them full screen.
    ShowSettings,
    /// The settings changed `client.ui` or `client.per_session`: the session
    /// loop applies it and attaches again.
    Resettle,
    /// `<prefix> <number>`, `<prefix> n` or `<prefix> p`. A target that names
    /// nothing returns to this session.
    Switch(Target),
    /// The child exited on its own.
    ChildExited,
    /// Our stdin hit EOF. The child is still running, so this is handled like a
    /// detach; waiting on it as for a child exit would hang.
    StdinClosed,
}

impl Outcome {
    /// Whether the next relay follows with no [`crate::ui`] screen in between
    /// to clear the outgoing frame. Only a switch does.
    fn leads_straight_into_another_relay(self) -> bool {
        matches!(self, Outcome::Switch(_))
    }

    /// The session to start attaching while the outgoing one fades (see
    /// [`relay`]). A method so a test pins that only `Switch` gives one: an
    /// outcome leading into another relay must not silently start nothing.
    fn switch_target(self) -> Option<Target> {
        match self {
            Outcome::Switch(target) => Some(target),
            Outcome::ToPicker
            | Outcome::Detached
            | Outcome::CreateNew
            | Outcome::CreateAfter(_)
            | Outcome::Rename(_)
            | Outcome::ShowHelp
            | Outcome::ShowSettings
            | Outcome::Resettle
            | Outcome::ChildExited
            | Outcome::StdinClosed => None,
        }
    }
}

/// Which session a `<prefix>` switch named: by number, the neighbour in the
/// picker's order, or by id (`<prefix> Enter` on a card). It may name none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Number(u32),
    Step(crate::keys::Direction),
    Session(crate::ids::Id),
}

/// A running client, and the PTY it is talking through.
pub struct Attachment {
    pub session_id: String,
    /// The socket the client attached through (the session's own, or the local
    /// end of the SSH forward), so a resume can query the server directly.
    sock: PathBuf,
    /// What to announce when this client first takes the terminal; `None` once
    /// done. `Some` means the session changed: `spawn` runs only then, and
    /// [`Attachment::announce_on_arrival`] sets it for a returning kept client.
    announce: Option<String>,
    /// The name the picker left on screen, taken down as the session takes the
    /// terminal (see [`crate::handoff`]). Only for attaches from the picker.
    hand_off: Option<HandOff>,
    child: Child,
    master: Pty,
    /// Set by its first relay: later ones must force a repaint and restore the
    /// alternate screen, which the client will not re-enter.
    resumed: bool,
    /// Set once the child has been waited for, so `Drop` does not wait again.
    reaped: bool,
    /// The session's screen, for the fade (see [`crate::shadow`]). `None` with
    /// the fade off or `effects.fade.session` false; `screen` is used instead.
    shadow: Option<shadow::Shadow>,
    /// The client's own screen copy when there is no shadow, to repaint a kept
    /// client and to draw under floating panels.
    screen: Option<shadow::Shadow>,
    /// The first paint, held back until it can fade in. `None` once released,
    /// and always without a shadow.
    hold: Option<Hold>,
    /// The terminal parser's state, tracked only while the notice or hint bar
    /// is up. Every byte written to the terminal must be shown to it exactly
    /// once, tagged as the session's or nvmux's, or nvmux's next drawing lands
    /// inside a sequence. Except nvmux's spans around relayed writes: seen,
    /// they would read as the session holding the terminal and shut the
    /// overlay out.
    boundary: Option<boundary::Boundary>,
    /// A synchronized update nvmux opened around a relayed write and must still
    /// close. Kept open if the session's bytes stopped mid-sequence (see
    /// [`Attachment::close_span`]).
    span: bool,
    /// What the client told the terminal and will not repeat, replayed on
    /// return. `Some` exactly when clients are kept (`[client] per_session`).
    ledger: Option<Ledger>,
    /// The terminal has answered the client's DA1, the last of its startup
    /// queries (terminals answer in order). Until then it is not parked: kept
    /// without the answers, it would lack the keyboard protocol, synchronized
    /// updates and in-band sizes.
    startup_answered: bool,
    /// After in-band size reports are re-enabled: the current size, and until
    /// when the terminal's matching report is swallowed.
    expecting_a_size_report: Option<(PtySize, Instant)>,
    /// [`EXITS_RELAYED`] when parked; a change by its return means another
    /// client's exit turned off modes this one relies on.
    exits_seen: u64,
    /// A started-ahead client's output, not yet seen by anything; relayed first
    /// on its first visit.
    unshown: Option<Vec<u8>>,
    /// A started-ahead client's attach probe, until it answers.
    probe: Option<Probe>,
    /// Whether the client is nvmux's own, which draws cards.
    draws_cards: bool,
    /// Told it is out of sight (see [`Attachment::go_away`]) and not yet back.
    away: bool,
    /// While `away`, drawing at a crawl; nvmux's copy may lag by up to 1 s.
    crawling: bool,
    /// The settings generation the client last heard; if stale, a relay sends
    /// the current settings first.
    heard: u64,
    /// This relay's cards for the client; only for a client that draws cards.
    desk: Option<card::Desk>,
    /// The session's place among those `<prefix> n`/`p` step through, for the
    /// card showing where a switch landed (see [`Attachment::place_in_row`]).
    row: Option<card::Row>,
    /// The cells under that card, which this relay's fade in leaves alone: the
    /// client draws it before the screen arrives (see [`shadow::Kept`]).
    kept: shadow::Kept,
    /// The landing card left on the screen this relay handed over, for the next
    /// relay to take down.
    left_standing: Option<HandOff>,
    /// The desk's opening commands, until sent. Never during a hold: a row of
    /// names redrawn each frame would keep the paint from settling.
    opening: Vec<card::Command>,
    /// When the landing card went up; its timing counts from here.
    landed_since: Option<Instant>,
    /// The terminal shows only the background: a panel took the session away
    /// with it, or the settings or picker will float in and bring the session
    /// with them. The fade out then has nothing to dissolve.
    dark: bool,
    /// Notified on the first lull after the next relay relays anything; dropped
    /// if that relay ends first.
    painted: Option<mpsc::Sender<()>>,
    /// The settings or picker to float over the session once the next relay has
    /// its screen back.
    to_float: Option<Outcome>,
}

/// How many clients' exit sequences have been relayed (sessions that ended
/// in front). Each turns off terminal-wide modes every Neovim client sets
/// once, which a kept client returning afterwards must put back.
static EXITS_RELAYED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A session's first paint, held back so it can fade in finished.
///
/// Released once something is drawn and the client has been quiet for
/// [`HOLD_SETTLE`], or at [`HOLD_CAP`] or [`HOLD_MAX`]. The release writes the
/// held bytes through the alternate-screen entry, fades the shadow in on that
/// screen, then writes the rest in one synchronized update.
#[derive(Debug)]
struct Hold {
    bytes: Vec<u8>,
    /// When the first byte arrived.
    first: Option<Instant>,
    last: Instant,
}

/// Quiet time after which a drawn first paint is complete. Same as
/// `hint::SETTLE`: it bridges a full pty buffer's brief gaps mid-frame.
const HOLD_SETTLE: Duration = Duration::from_millis(25);

/// The longest a first paint is held after its first byte. A local session
/// settles in ~100 ms; over SSH it can take a few round trips.
const HOLD_CAP: Duration = Duration::from_millis(750);

/// Release at this size regardless (a 200×50 truecolor grid is tens of KB).
const HOLD_MAX: usize = 1 << 20;

/// The alternate-screen entry a client sends before it draws.
const ALT_SCREEN_ENTER: &[u8] = b"\x1b[?1049h";

/// Sent by a parked client only on its way out.
const ALT_SCREEN_LEAVE: &[u8] = b"\x1b[?1049l";

impl Hold {
    fn new(now: Instant) -> Self {
        Self {
            bytes: Vec::new(),
            first: None,
            last: now,
        }
    }

    fn take(&mut self, chunk: &[u8], now: Instant) {
        self.bytes.extend_from_slice(chunk);
        self.first.get_or_insert(now);
        self.last = now;
    }

    /// When the relay must next wake for the hold. Undrawn, only the cap: a
    /// lull would release nothing, and waking for it would spin `poll`.
    fn wake_at(&self, drawn: bool) -> Option<Instant> {
        let first = self.first?;
        let cap = first + HOLD_CAP;
        Some(if drawn {
            (self.last + HOLD_SETTLE).min(cap)
        } else {
            cap
        })
    }

    fn due(&self, now: Instant, drawn: bool) -> bool {
        let Some(first) = self.first else {
            return false;
        };
        now >= first + HOLD_CAP
            || self.bytes.len() >= HOLD_MAX
            || (drawn && now >= self.last + HOLD_SETTLE)
    }
}

/// Where held bytes split: just past the alternate-screen entry, or 0 if none
/// (a `TERM` without `smcup` draws on the primary screen).
fn split_at_alt_screen_entry(bytes: &[u8]) -> usize {
    memmem::find(bytes, ALT_SCREEN_ENTER).map_or(0, |i| i + ALT_SCREEN_ENTER.len())
}

/// `bytes` without synchronized-update ends, so a held paint presents as one
/// frame inside the update around it.
fn unsynced(bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    without(bytes, fade::SYNC_END)
}

/// A released first paint in one synchronized update, closed before any
/// trailing partial sequence: closing a hold released mid-sequence would cut,
/// say, a colour in two, and the terminal would print the rest as text.
fn replay(paint: &[u8]) -> Vec<u8> {
    let at = finished(paint);
    let mut out = Vec::with_capacity(paint.len() + fade::SYNC_BEGIN.len() + fade::SYNC_END.len());
    out.extend_from_slice(fade::SYNC_BEGIN);
    out.extend_from_slice(&unsynced(&paint[..at]));
    out.extend_from_slice(fade::SYNC_END);
    out.extend_from_slice(&paint[at..]);
    out
}

/// How much of a released paint is finished: up to an unclosed client frame
/// (which then goes out in the client's own update), else up to a partial
/// character or sequence.
fn finished(paint: &[u8]) -> usize {
    if let Some(open) = memmem::rfind(paint, fade::SYNC_BEGIN) {
        if memmem::find(&paint[open..], fade::SYNC_END).is_none() {
            return open;
        }
    }
    boundary::Boundary::new().relay_whole(paint).len()
}

/// `bytes` with every `needle` removed; borrowed if there is none.
fn without<'a>(bytes: &'a [u8], needle: &[u8]) -> std::borrow::Cow<'a, [u8]> {
    let mut found = memmem::find_iter(bytes, needle).peekable();
    if found.peek().is_none() {
        return std::borrow::Cow::Borrowed(bytes);
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut from = 0;
    for at in found {
        out.extend_from_slice(&bytes[from..at]);
        from = at + needle.len();
    }
    out.extend_from_slice(&bytes[from..]);
    std::borrow::Cow::Owned(out)
}

impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attachment")
            .field("session_id", &self.session_id)
            .field("resumed", &self.resumed)
            .finish_non_exhaustive()
    }
}

/// How long a hung-up client gets to exit before it is killed outright.
const REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Poll interval while reaping a hung-up client. Clients exit within a few
/// ms, and overshoot is user-visible on switches and on exit.
const REAP_STEP: Duration = Duration::from_millis(1);

/// How long the teardown paths relay a departing client's restore sequence.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// Quiet time after which a relay's session counts as painted, for the
/// `timing:` log and [`Attachment::tell_when_painted`]; bridges a heavy
/// config's redraw bursts (~100 ms).
const PAINT_SETTLED: Duration = Duration::from_millis(150);

/// A DA1 request, which a departing client sends (see
/// [`Attachment::answer_device_attributes`]).
const DA1_REQUEST: &[u8] = b"\x1b[c";

/// "VT100 with advanced video option". The content is irrelevant: a client
/// asks at shutdown only to know the terminal has caught up.
const DA1_REPLY: &[u8] = b"\x1b[?1;2c";

/// How long a resumed kept client gets to finish a frame it was caught in.
const FRAME_WAIT: Duration = Duration::from_millis(50);

/// How long a client gets to draw a requested card (or, back from a crawl,
/// its pending redraw). Normally ~1 ms.
const CARD_WAIT: Duration = Duration::from_millis(30);

/// The most backlog a resume reads after a whole frame. A parked client is
/// read throughout, so more means it is still writing.
const DRAIN_MAX: usize = 64 * 1024;

/// How long the answer to re-enabled in-band size reports is looked for.
const SIZE_REPORT_WAIT: Duration = Duration::from_secs(1);

/// DEC mode 2048's report: `CSI 48 ; rows ; cols ; height ; width t`, pixels
/// zero when unknown.
fn in_band_size_report(size: PtySize) -> Vec<u8> {
    format!(
        "\x1b[48;{};{};{};{}t",
        size.rows, size.cols, size.pixel_height, size.pixel_width
    )
    .into_bytes()
}

impl Attachment {
    /// Mark the notice to say the session is waiting for a key; otherwise it
    /// looks broken, drawing nothing until one arrives. Known only between the
    /// probe and the relay. No-op once the notice was shown.
    pub fn note_waiting_for_a_key(&mut self) {
        let Some(label) = self.announce.take() else {
            return;
        };
        // Idempotent, so a second call does not repeat the suffix.
        self.announce = Some(if label.ends_with(WAITING) {
            label
        } else {
            format!("{label}{WAITING}")
        });
    }

    /// Drop the session name from the notice, for a return from the picker to
    /// the session just left. A "waiting for a key" notice is kept.
    pub fn arrive_unannounced(&mut self) {
        if self
            .announce
            .as_deref()
            .is_some_and(|label| !label.ends_with(WAITING))
        {
            self.announce = None;
        }
    }

    /// Terminate the client; the server keeps running (killing a `--remote-ui`
    /// client never kills a `--headless --listen` server).
    pub fn terminate(self) {
        // `Drop` does the work, so every path retires a client the same way.
        drop(self);
    }

    /// Send SIGHUP and return without waiting, so the caller can spawn the
    /// replacement meanwhile; `reap` (from `Drop`) waits. Not `Child::kill`
    /// (SIGKILL), which gives no chance to restore the terminal. SIGCONT too:
    /// SIGHUP is not delivered to a stopped process (see `check_child`). Safe
    /// to repeat: nothing reaps in between (the peek in `check_child` passes
    /// `WNOWAIT`), so the pid cannot be recycled.
    pub fn hang_up(&mut self) {
        if self.reaped {
            return;
        }
        let pid = self.child.id() as libc::pid_t;
        // SAFETY: our child's pid, not yet waited for: that child or nothing.
        unsafe {
            libc::kill(pid, libc::SIGHUP);
            libc::kill(pid, libc::SIGCONT);
        }
    }

    /// Write session bytes to the terminal, then feed the shadow, which must
    /// never delay what the user sees. Across all write paths the shadow sees
    /// each session byte exactly once.
    fn write_terminal(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        write_stdout(bytes)?;
        self.shadow_saw(bytes);
        Ok(())
    }

    /// Write nvmux's own drawing over the session (notice, hint bar). Not fed
    /// to the shadow, which must keep what the client drew so overlays can be
    /// removed (see [`crate::shadow::Shadow::under`]); its diff is invalidated
    /// instead. Fed to the boundary, which tracks the terminal.
    fn write_over_session(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        write_stdout(bytes)?;
        if let Some(shadow) = self.shadow.as_mut() {
            shadow.invalidate();
        }
        if let Some(boundary) = self.boundary.as_mut() {
            boundary.saw_own(bytes);
        }
        Ok(())
    }

    /// Feed the session's bytes, relayed or not, to the shadow or `screen`.
    fn shadow_saw(&mut self, bytes: &[u8]) {
        if let Some(shadow) = self.shadow.as_mut() {
            shadow.feed(bytes);
        }
        if let Some(screen) = self.screen.as_mut() {
            screen.feed(bytes);
        }
    }

    /// Feed client output to the ledger, each byte once.
    fn ledger_saw(&mut self, bytes: &[u8]) {
        if let Some(ledger) = self.ledger.as_mut() {
            ledger.saw(bytes);
        }
    }

    /// Watch terminal input to a kept client: note when its startup queries are
    /// answered, and swallow the size report a resume triggered. That report
    /// repeats the client's current size; passed on, the server would resize to
    /// the same size, which in `input()` or `getchar()` clears to the prompt
    /// row and wipes the `:mode` repaint. Swallowing it loses nothing: a size
    /// changed while parked was sent by [`Attachment::tell_its_size`]. Other
    /// sizes (a resize since) go through.
    fn saw_input(&mut self, step: &mut Step) {
        if self.ledger.is_none() {
            return;
        }
        let Step::Forward(bytes) = step else {
            return;
        };
        if !self.startup_answered && answers_device_attributes(bytes) {
            self.startup_answered = true;
        }
        let Some((size, until)) = self.expecting_a_size_report else {
            return;
        };
        if Instant::now() > until {
            self.expecting_a_size_report = None;
        } else if let Some((at, rows, cols)) = find_size_report(bytes) {
            self.expecting_a_size_report = None;
            if (rows, cols) == (size.rows, size.cols) {
                bytes.drain(at);
            }
        }
    }

    /// Whether this client is kept (`[client] per_session`).
    pub fn is_kept(&self) -> bool {
        self.ledger.is_some()
    }

    /// Kept, and its startup queries answered.
    pub fn is_parkable(&self) -> bool {
        self.is_kept() && self.startup_answered
    }

    /// Announce the session on the next relay, for a kept client returning in
    /// place of another's. `label` comes from [`announce::label`].
    pub fn announce_on_arrival(&mut self, label: String) {
        self.announce = Some(label);
    }

    /// The session's place among those `<prefix> n`/`p` step through, and a
    /// step's direction, shown beside the name on the landing card.
    pub fn place_in_row(&mut self, row: card::Row) {
        self.row = Some(row);
    }

    /// The landing card the last relay left on screen, for the next attach to
    /// carry like the picker's name.
    pub fn take_left_standing(&mut self) -> Option<HandOff> {
        self.left_standing.take()
    }

    /// Have the client draw `card` (this switch's landing card) over the screen
    /// it is leaving, read into the shadow for the fade out. Returns the card's
    /// cells, for the fade to leave alone, and when it went up. `None` without
    /// a fade or a card-drawing client, or if it is not drawn in time.
    fn stand_landing(&mut self, card: &card::Card) -> Option<(shadow::Kept, Instant)> {
        if !self.draws_cards || !fade::enabled() || !fade::session() || self.shadow.is_none() {
            return None;
        }
        let size = term::terminal_size();
        let kept = shadow::Kept::arrival(card, size.cols, size.rows);
        if kept.is_empty() {
            return None;
        }
        self.tell_client(&card::Command::Clear);
        self.tell_client(&card::Command::Show(card.clone()));
        if self.drain_until_shown(&kept, Instant::now() + CARD_WAIT) {
            return Some((kept, Instant::now()));
        }
        tracing::debug!("the client did not draw the card a switch lands with in time");
        None
    }

    /// Read client output until the shadow shows `kept` or `deadline`; returns
    /// whether it did. Not just until any output: a client reading the return
    /// and the card apart answers the first with a frame of its own.
    fn drain_until_shown(&mut self, kept: &shadow::Kept, deadline: Instant) -> bool {
        loop {
            self.await_output(deadline.saturating_duration_since(Instant::now()));
            self.drain_unrelayed();
            if self.shadow.as_ref().is_some_and(|s| s.shows(kept)) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
        }
    }

    /// Fade the session out, leaving `kept` alone; a moving `kept` is read from
    /// the client between frames. Returns whether it faded.
    fn fade_out(&mut self, kept: &shadow::Kept) -> bool {
        let Some(mut shadow) = self.shadow.take() else {
            return false;
        };
        let faded = if kept.moves() {
            fade::fade_out_session(&mut shadow, kept, &mut |shadow, until| {
                self.read_while_fading(shadow, until, None);
            })
        } else {
            fade::fade_out_session(&mut shadow, kept, &mut fade::wait)
        };
        self.shadow = Some(shadow);
        faded.unwrap_or_else(|e| {
            tracing::debug!(error = %e, "the session's fade-out did not complete");
            false
        })
    }

    /// Between fade frames: until `until`, read client output into `shadow`,
    /// the ledger and `held` (if still owed to the terminal).
    fn read_while_fading(
        &mut self,
        shadow: &mut shadow::Shadow,
        until: Instant,
        mut held: Option<&mut Vec<u8>>,
    ) {
        let fd = self.master.as_raw_fd();
        let mut buf = [0u8; 8192];
        loop {
            let now = Instant::now();
            if now >= until {
                return;
            }
            let wait_ms = (until - now).as_micros().div_ceil(1000) as libc::c_int;
            let mut p = [pollfd(fd)];
            let n = unsafe { libc::poll(p.as_mut_ptr(), 1, wait_ms) };
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if n == 0 {
                return;
            }
            // The client is gone; the fade continues without it.
            if n < 0 || !ready(&p[0]) {
                fade::wait(shadow, until);
                return;
            }
            match read_fd(fd, &mut buf) {
                Ok(len) if len > 0 => {
                    let bytes = &buf[..len];
                    shadow.feed(bytes);
                    self.ledger_saw(bytes);
                    if let Some(held) = held.as_deref_mut() {
                        held.extend_from_slice(bytes);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                _ => {
                    fade::wait(shadow, until);
                    return;
                }
            }
        }
    }

    /// Read client output into the shadow, the ledger and `held` until the
    /// shadow shows the landing card, or `deadline`.
    fn read_until_shown(&mut self, deadline: Instant, held: &mut Vec<u8>) {
        let Some(mut shadow) = self.shadow.take() else {
            return;
        };
        while !shadow.shows(&self.kept) {
            let now = Instant::now();
            if now >= deadline {
                tracing::debug!("the client did not draw the card a switch landed with in time");
                break;
            }
            let until = deadline.min(now + Duration::from_millis(1));
            self.read_while_fading(&mut shadow, until, Some(&mut *held));
        }
        self.shadow = Some(shadow);
    }

    /// The cells this relay's fade in leaves alone: the landing card's, if the
    /// shadow shows it.
    fn standing(&self) -> shadow::Kept {
        match self.shadow.as_ref() {
            Some(shadow) if shadow.shows(&self.kept) => self.kept.clone(),
            _ => shadow::Kept::default(),
        }
    }

    /// The name the picker left on screen (see [`crate::handoff`]). The relay
    /// takes it down as the session takes the terminal; nothing may draw first.
    pub fn carry_name(&mut self, hand_off: HandOff) {
        self.hand_off = Some(hand_off);
    }

    /// The name the picker left, as an overlay on the shadow's screen.
    fn name_over(&self, shadow: &shadow::Shadow) -> Option<shadow::Over> {
        let (rows, cols) = shadow.size();
        self.hand_off.as_ref().and_then(|h| h.over(rows, cols))
    }

    /// Read pending client output into the shadow and ledger, not the terminal:
    /// on resuming a kept client it describes the screen about to be painted
    /// back, and any mouse or title change in it will not be repeated. A client
    /// caught mid-frame gets up to [`FRAME_WAIT`] to finish, or the rest would
    /// print as text; bounded by [`DRAIN_MAX`], since a busy `:terminal` never
    /// stops writing.
    fn drain_unrelayed(&mut self) {
        let fd = self.master.as_raw_fd();
        let mut buf = [0u8; 8192];
        let deadline = Instant::now() + FRAME_WAIT;
        let mut taken = 0usize;
        loop {
            let mid_frame = self.ledger.as_ref().is_some_and(|l| !l.between_frames());
            let now = Instant::now();
            if now >= deadline || (!mid_frame && taken >= DRAIN_MAX) {
                return;
            }
            let wait = if mid_frame {
                deadline - now
            } else {
                Duration::ZERO
            };
            let wait_ms = wait.as_micros().div_ceil(1000) as libc::c_int;
            let mut p = [pollfd(fd)];
            let n = unsafe { libc::poll(p.as_mut_ptr(), 1, wait_ms) };
            // A `SIGWINCH` is no reason to hand over half a frame.
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if n <= 0 || !ready(&p[0]) {
                return;
            }
            match read_fd(fd, &mut buf) {
                Ok(len) if len > 0 => {
                    self.saw_unrelayed(&buf[..len]);
                    taken += len;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                _ => return,
            }
        }
    }

    /// Client output that never reaches the terminal, still tracked by the
    /// shadow and the ledger.
    fn saw_unrelayed(&mut self, bytes: &[u8]) {
        self.shadow_saw(bytes);
        self.ledger_saw(bytes);
    }

    /// Restore what the kept client told the terminal and will not repeat
    /// (mouse, title, keyboard protocol, cursor), plus modes another client's
    /// exit turned off while it was parked (see
    /// [`crate::ledger::Ledger::put_back`]). `size` is its current size, which
    /// the terminal will report back (see [`Attachment::saw_input`]).
    fn put_back_what_it_told(&mut self, size: PtySize) {
        let Some(ledger) = self.ledger.as_ref().filter(|l| l.is_usable()) else {
            return;
        };
        term::set_mouse_reporting(ledger.mouse());
        let after_an_exit =
            EXITS_RELAYED.load(std::sync::atomic::Ordering::Relaxed) != self.exits_seen;
        self.expecting_a_size_report = ledger
            .puts_back_in_band_reports(after_an_exit)
            .then(|| (size, Instant::now() + SIZE_REPORT_WAIT));
        if let Err(e) = write_stdout(&ledger.put_back(after_an_exit)) {
            tracing::debug!(error = %e, "could not put the client's settings back");
        }
    }

    /// Tell a returning kept client a size that changed while it was parked.
    /// One with in-band resize reports on (DEC mode 2048: kitty, ghostty, foot)
    /// ignores `SIGWINCH`, so it is written the report the terminal would send
    /// (Neovim 0.11.0 and 0.12.5 take it as a size, not keys). Unchanged sizes
    /// are skipped: a same-size resize clears to the prompt row in `input()`.
    fn tell_its_size(&mut self, size: PtySize, resized: bool) {
        if resized && self.ledger.as_ref().is_some_and(Ledger::reads_size_in_band) {
            self.report_size_in_band(size);
        }
    }

    /// Write an in-band size report into the client's input; returns whether it
    /// went out whole. One `write`, never `write_all`: a split report would be
    /// read as keys, and one that does not fit is better not sent.
    fn report_size_in_band(&mut self, size: PtySize) -> bool {
        let fd = self.master.as_raw_fd();
        if !writable(fd) {
            tracing::debug!("the client's input queue is full; not reporting its size");
            return false;
        }
        let report = in_band_size_report(size);
        let n = unsafe { libc::write(fd, report.as_ptr().cast(), report.len()) };
        if n != report.len() as isize {
            tracing::debug!(wrote = n, "the in-band size report went out short");
            return false;
        }
        true
    }

    /// Send `cmd` into the client's input in one write: split, it would be read
    /// as keys. Best effort: dropped if the input queue is full.
    fn tell_client(&mut self, cmd: &card::Command) {
        let fd = self.master.as_raw_fd();
        if !writable(fd) {
            tracing::debug!("the client's input queue is full; no card");
            return;
        }
        let wire = card::encode(cmd);
        let n = unsafe { libc::write(fd, wire.as_ptr().cast(), wire.len()) };
        if n != wire.len() as isize {
            tracing::debug!(wrote = n, "a card went out short");
        }
    }

    /// Open this relay's desk for a card-drawing client and queue its opening
    /// commands. Returns whether they include a landing card.
    fn open_desk(&mut self) -> bool {
        if !self.draws_cards {
            return false;
        }
        let mut desk = card::Desk::new(&self.session_id);
        let row = self.row.take().unwrap_or_default();
        let arrival = self.announce.take().and_then(|label| {
            let waiting = label.ends_with(WAITING);
            let name = label.strip_suffix(WAITING).unwrap_or(&label);
            (waiting || !name.is_empty())
                .then(|| card::Card::arrival(card::next_id(), name, waiting).in_row(row))
        });
        let landed = arrival.is_some();
        let size = term::terminal_size();
        self.kept = arrival.as_ref().map_or_else(shadow::Kept::default, |card| {
            shadow::Kept::arrival(card, size.cols, size.rows)
        });
        self.opening = desk.open(arrival);
        // A client that missed a settings change (on the settings screen this
        // relay returns from, or while kept) is sent the current ones first.
        let generation = crate::config::generation();
        if self.heard != generation {
            self.heard = generation;
            self.opening
                .insert(0, card::Command::Settings(crate::config::get().drawing()));
        }
        self.landed_since = self.hand_off.as_ref().and_then(HandOff::up_since);
        self.desk = Some(desk);
        landed
    }

    /// Send the desk's opening commands if the input stream is between keys,
    /// with the landing card aged by how long it has been up (see
    /// [`card::Card::up_ms`]). Returns whether they went in.
    fn put_in_opening(&mut self, now: Instant) -> bool {
        if self.opening.is_empty() || !self.desk.as_ref().is_some_and(|d| d.forwarded.clear(now)) {
            return false;
        }
        let up = self
            .landed_since
            .map(|since| now.saturating_duration_since(since));
        for cmd in std::mem::take(&mut self.opening) {
            let cmd = match (cmd, up) {
                (card::Command::Show(card), Some(up)) => card::Command::Show(card.up_for(up)),
                (cmd, _) => cmd,
            };
            self.tell_client(&cmd);
        }
        true
    }

    /// Send queued cards, opening commands first; nothing during a hold.
    fn send_cards(&mut self, now: Instant) {
        if self.hold.is_some() || (!self.opening.is_empty() && !self.put_in_opening(now)) {
            return;
        }
        let Some(desk) = self.desk.as_mut() else {
            return;
        };
        for cmd in desk.ready(now) {
            self.tell_client(&cmd);
        }
    }

    /// When to retry sending queued cards. `None` while the first paint is
    /// held; its release sends them.
    fn cards_due(&self) -> Option<Instant> {
        let desk = self.desk.as_ref().filter(|_| self.hold.is_none())?;
        if self.opening.is_empty() {
            desk.retry_at()
        } else {
            desk.forwarded.retry_at()
        }
    }

    /// `<prefix> Enter`: act on the newest card with a destination (switch to
    /// its session, or show its terminal buffer in this one) and take it down.
    fn go(&mut self, now: Instant) -> Option<Outcome> {
        let (_, go) = self.desk.as_mut()?.go()?;
        self.send_cards(now);
        match go {
            card::Go::Session(id) => Some(Outcome::Switch(Target::Session(id))),
            card::Go::Buffer(buf) => {
                show_buffer(self.sock.clone(), buf);
                None
            }
        }
    }

    /// Wait up to `within` for client output, without reading it.
    fn await_output(&self, within: Duration) {
        let fd = self.master.as_raw_fd();
        let mut p = [pollfd(fd)];
        let ms = within.as_millis().min(i32::MAX as u128) as libc::c_int;
        let _ = unsafe { libc::poll(p.as_mut_ptr(), 1, ms) };
    }

    /// Pass on client output, then feed the whole chunk to the ledger, which
    /// (like the shadow) must never delay what the user sees.
    fn relay_output(&mut self, chunk: &[u8], now: Instant) -> std::io::Result<()> {
        let shown = self.show_output(chunk, now);
        self.ledger_saw(chunk);
        shown
    }

    /// Write client output to the terminal, or during a hold keep it and feed
    /// only the shadow. While an overlay is up, the boundary holds back a
    /// trailing partial sequence, so the overlay can go back up after every
    /// write.
    fn show_output(&mut self, chunk: &[u8], now: Instant) -> std::io::Result<()> {
        if let Some(hold) = self.hold.as_mut() {
            hold.take(chunk, now);
            if let Some(shadow) = self.shadow.as_mut() {
                shadow.feed(chunk);
            }
            return Ok(());
        }
        if let Some(boundary) = self.boundary.as_mut() {
            let ready = boundary.relay(chunk, now);
            // A read that was all tail: skip the write. Every shadow feed drops
            // its diff, so an empty one costs the next fade a full repaint.
            if ready.is_empty() {
                return Ok(());
            }
            let wrap = !boundary.session_brackets_its_own_frames();
            return self.write_session_frame(&ready, wrap);
        }
        self.write_terminal(chunk)
    }

    /// Relay session bytes in a synchronized update left open for the notice's
    /// frame (or the hint bar) until [`Attachment::close_span`], so the
    /// overlay stays on screen. Otherwise the terminal presents mid-repaint
    /// prefixes with the box wiped (measured: the whole box showed in 10% of
    /// frames under a busy `:terminal`). `wrap` is false for a client that
    /// brackets its own frames, where nvmux's span would end at the client's
    /// reset.
    fn write_session_frame(&mut self, bytes: &[u8], wrap: bool) -> std::io::Result<()> {
        if !wrap {
            return self.write_terminal(bytes);
        }
        let out = framed(bytes, self.span);
        write_stdout(&out)?;
        self.span = true;
        self.shadow_saw(bytes);
        Ok(())
    }

    /// Write the session's own frame close, held back so overlays could go in
    /// front of it; called every pass, right after they are drawn.
    fn release_the_sessions_close(&mut self) -> std::io::Result<()> {
        let Some(boundary) = self.boundary.as_mut() else {
            return Ok(());
        };
        let close = boundary.release_the_sessions_close();
        if close.is_empty() {
            return Ok(());
        }
        self.write_terminal(&close)
    }

    /// Close nvmux's open synchronized update unless the session is
    /// mid-sequence. Called at the end of every pass, since a terminal left
    /// inside a span shows nothing until its own timeout. Waits for a whole
    /// character: a reset between a UTF-8 lead byte and its continuation
    /// corrupts it (seen up to six times per run before this check).
    fn close_span(&mut self) -> std::io::Result<()> {
        if !self.span || self.mid_sequence() {
            return Ok(());
        }
        self.span = false;
        write_stdout(fade::SYNC_END)
    }

    /// Whether the session is mid-character or mid-sequence.
    fn mid_sequence(&self) -> bool {
        self.boundary
            .as_ref()
            .is_some_and(boundary::Boundary::mid_sequence)
    }

    /// Whether nvmux may draw over the session now: the terminal is between the
    /// session's sequences and the first paint is not held.
    fn between_sequences(&self) -> bool {
        self.hold.is_none()
            && self
                .boundary
                .as_ref()
                .is_some_and(boundary::Boundary::between_sequences)
    }

    /// When a tail held back for an overlay must be written even if unfinished.
    fn kept_due(&self) -> Option<Instant> {
        self.boundary.as_ref().and_then(boundary::Boundary::due)
    }

    /// Write out whatever the boundary is keeping, finished or not.
    fn release_kept(&mut self) -> std::io::Result<()> {
        let Some(boundary) = self.boundary.as_mut() else {
            return Ok(());
        };
        let ready = boundary.give_up();
        if ready.is_empty() {
            return Ok(());
        }
        self.write_terminal(&ready)
    }

    /// Whether the terminal's parser is tracked.
    fn watching(&self) -> bool {
        self.boundary.is_some()
    }

    /// Start tracking the parser for the hint bar, at the lull it goes up at
    /// (the session is then between sequences). No-op if already tracking.
    fn watch_the_terminal(&mut self) {
        self.boundary.get_or_insert_with(boundary::Boundary::new);
    }

    /// Stop tracking the parser once no overlay is up, flushing everything
    /// held back for it.
    fn stop_watching_the_terminal(&mut self) -> std::io::Result<()> {
        self.close_span()?;
        self.release_the_sessions_close()?;
        self.release_kept()?;
        // No next pass: close a span left open regardless, or the terminal
        // freezes until its timeout. At worst one character is cut, once.
        if std::mem::take(&mut self.span) {
            write_stdout(fade::SYNC_END)?;
        }
        self.boundary = None;
        Ok(())
    }

    /// The session's screen as nvmux keeps it: the shadow, else `screen`.
    fn copy(&mut self) -> Option<&mut shadow::Shadow> {
        self.shadow.as_mut().or(self.screen.as_mut())
    }

    /// Whether the shadow has any glyph yet.
    fn drawn(&self) -> bool {
        self.shadow
            .as_ref()
            .is_some_and(shadow::Shadow::has_contents)
    }

    /// Whether the held first paint should be released now. Checks for a hold
    /// first: [`Attachment::drawn`] scans the screen, slow when it is blank.
    fn hold_is_due(&self, now: Instant) -> bool {
        let Some(hold) = self.hold.as_ref() else {
            return false;
        };
        hold.due(now, self.drawn())
    }

    /// When the relay must next wake for the held first paint, if any.
    fn hold_wake_at(&self) -> Option<Instant> {
        self.hold.as_ref()?.wake_at(self.drawn())
    }

    /// Release the held first paint: fade the shadow in if `dissolve` and
    /// something is drawn, then write everything held as it came. Bytes through
    /// the alternate-screen entry go first, so the fade lands on the client's
    /// screen; the rest goes in one synchronized update.
    fn release_hold(&mut self, dissolve: bool) -> std::io::Result<()> {
        let Some(hold) = self.hold.take() else {
            return Ok(());
        };
        let held_for = hold.first.map(|first| first.elapsed());
        let bytes = hold.bytes;
        let drawn = self.drawn();
        let split = if dissolve && drawn {
            split_at_alt_screen_entry(&bytes)
        } else {
            0
        };
        let mut dissolved = false;
        // Client output read during the release, owed after the held bytes.
        let mut later = Vec::new();
        // Send the cards now that they cannot hold up the paint.
        let opened = dissolve && self.put_in_opening(Instant::now());
        // The picker's name comes down first either way: it is on the
        // primary screen, which the shell returns to.
        let hand_off = self.hand_off.take();
        if dissolve && drawn {
            // Opened here and closed by the fade's first frame (or the
            // replay), so the name moves screens in one presented moment.
            if let Some(h) = &hand_off {
                let mut down = fade::SYNC_BEGIN.to_vec();
                down.extend_from_slice(&h.erase());
                write_stdout(&down)?;
            }
            write_stdout(&bytes[..split])?;
            // Wait for the card to be drawn so the screen fades in with it up.
            if opened && !self.kept.is_empty() {
                self.read_until_shown(Instant::now() + CARD_WAIT, &mut later);
            }
            let kept = self.standing();
            if let Some(mut shadow) = self.shadow.take() {
                let (rows, cols) = shadow.size();
                let over = hand_off.as_ref().and_then(|h| h.over(rows, cols));
                // A moving card keeps moving: output during the fade feeds
                // the shadow for later frames and is held for the replay.
                let faded = if kept.moves() {
                    fade::fade_in_session(
                        &mut shadow,
                        over.as_ref(),
                        &kept,
                        &mut |shadow, until| {
                            self.read_while_fading(shadow, until, Some(&mut later));
                        },
                    )
                } else {
                    fade::fade_in_session(&mut shadow, over.as_ref(), &kept, &mut fade::wait)
                };
                self.shadow = Some(shadow);
                // A failed fade frame is no reason to keep the paint.
                dissolved = match faded {
                    Ok(faded) => faded,
                    Err(e) => {
                        tracing::debug!(error = %e, "the session's fade-in did not complete");
                        true
                    }
                };
            }
        } else if let Some(h) = &hand_off {
            write_stdout(&h.erase())?;
        }
        // One update regardless of the client's framing, so no stale client
        // frame shows over the fade's last one. Closed where the client
        // stopped (see `replay`).
        let mut paint = bytes[split..].to_vec();
        paint.extend_from_slice(&later);
        let mut out = std::io::stdout().lock();
        out.write_all(&replay(&paint))?;
        out.flush()?;
        drop(out);
        if let Some(shadow) = self.shadow.as_mut() {
            shadow.invalidate();
        }
        // Only the held bytes can leave the parser mid-sequence; nvmux's own
        // frames are balanced.
        if let Some(boundary) = self.boundary.as_mut() {
            boundary.saw_session(&bytes);
            boundary.saw_session(&later);
        }
        tracing::debug!(
            held_ms = held_for.map(|d| d.as_secs_f64() * 1000.0),
            bytes = bytes.len(),
            dissolved,
            "timing: first paint held"
        );
        Ok(())
    }

    /// Fade in a resumed client's last screen from the shadow. Returns whether
    /// it did; if not, the terminal is as the hand-off left it.
    fn dissolve_in(&mut self) -> bool {
        let Some(shadow) = self.shadow.as_ref().filter(|s| s.has_contents()) else {
            return false;
        };
        let over = self.name_over(shadow);
        let kept = self.standing();
        let Some(mut shadow) = self.shadow.take() else {
            return false;
        };
        // A moving card keeps moving, read into the shadow only (see `relay`).
        let faded = if kept.moves() {
            fade::fade_in_session(&mut shadow, over.as_ref(), &kept, &mut |shadow, until| {
                self.read_while_fading(shadow, until, None);
            })
        } else {
            fade::fade_in_session(&mut shadow, over.as_ref(), &kept, &mut fade::wait)
        };
        self.shadow = Some(shadow);
        match faded {
            Ok(faded) => faded,
            Err(e) => {
                tracing::debug!(error = %e, "the resumed session's fade-in did not complete");
                true
            }
        }
    }

    /// Tell the pty — and the shadow — the terminal's size.
    fn resize_to(&mut self, size: PtySize) {
        let _ = self.master.resize(size.into());
        if let Some(shadow) = self.shadow.as_mut() {
            shadow.resize(size.rows, size.cols);
        }
        if let Some(screen) = self.screen.as_mut() {
            screen.resize(size.rows, size.cols);
        }
    }

    /// After a fade in, restore the terminal's pen and cursor to where the
    /// client's last write left them (see [`shadow::Shadow::hand_back`]).
    fn hand_back_the_pen(&self) {
        let Some(shadow) = self.shadow.as_ref().filter(|s| s.is_usable()) else {
            return;
        };
        if let Err(e) = write_stdout(&shadow.hand_back()) {
            tracing::debug!(error = %e, "could not hand the client's pen back");
        }
    }

    /// Without a fade, paint a kept client's screen back at once from its copy,
    /// so the user sees the session until the server's repaint lands. Returns
    /// whether it did.
    fn paint_kept_screen(&mut self) -> bool {
        // An unkept client was not read while away, so its copy is stale.
        if !self.is_kept() {
            return false;
        }
        let Some(screen) = self
            .screen
            .as_ref()
            .filter(|s| s.is_usable() && s.has_contents())
        else {
            return false;
        };
        if let Err(e) = write_stdout(&screen.paint()) {
            tracing::debug!(error = %e, "could not paint the kept screen back");
        }
        true
    }

    /// Wait for the client to exit, sending SIGKILL after [`REAP_TIMEOUT`] so
    /// one ignoring SIGHUP cannot hang nvmux.
    fn reap(&mut self) {
        if self.reaped {
            return;
        }
        self.reaped = true;
        // Drain and discard output while waiting: a client blocked in `write`
        // on an undrained master never reaches its signal handler. The picker
        // leaves an unkept outgoing client unread, so one caught mid-repaint
        // fills the buffer and would otherwise be killed at the deadline.
        let master = self.master.as_raw_fd();
        let deadline = Instant::now() + REAP_TIMEOUT;
        // Remembered, not answered once: the request appears on one pass only,
        // when the client's input queue is likeliest to be full.
        let mut owed = false;
        while Instant::now() < deadline {
            owed |= discard_pending(master);
            if owed {
                owed = !self.answer_device_attributes();
            }
            match self.child.try_wait() {
                Ok(None) => std::thread::sleep(REAP_STEP),
                // Exited, or already reaped by someone else.
                _ => return,
            }
        }
        tracing::warn!(
            pid = self.child.id(),
            "the client ignored SIGHUP; killing it"
        );
        // SIGKILL, and nothing if the child has been waited for already.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Answer a departing client's DA1 request as its terminal would.
    ///
    /// **Saves a second per retired client.** Neovim's `tui_stop` sends
    /// `ESC [ c` and waits up to 1 s for a reply no terminal will send
    /// (measured: 1003 ms; answered, ~4 ms). Only in reply to a request
    /// actually read (an early reply is eaten by the startup handler), and only
    /// to a hung-up client (a reply split into pieces is read as keys).
    fn answer_device_attributes(&mut self) -> bool {
        // Skipped if there is no room, never waited for: blocked, `reap` would
        // miss its deadline and `drain_until_eof` would hold the terminal raw.
        // Room can vanish for a few ms while Linux moves a burst out of its tty
        // flip buffer, so those two retry.
        let fd = self.master.as_raw_fd();
        if !writable(fd) {
            tracing::debug!("the departing client's input queue is full; not answering yet");
            return false;
        }
        tracing::debug!("answering the departing client's DA1 request");
        // One `write`: `writable` promises room for one byte, and `write_all`
        // could block. A torn reply only costs the exiting client its wait.
        let n = unsafe {
            libc::write(
                fd,
                DA1_REPLY.as_ptr() as *const libc::c_void,
                DA1_REPLY.len(),
            )
        };
        if n != DA1_REPLY.len() as isize {
            tracing::debug!(wrote = n, "the DA1 answer went out short");
        }
        true
    }

    /// Relay the client's output on its way out, then stop. Unlike in
    /// [`Attachment::reap`], the restore sequence is wanted. Bounded, or a
    /// client ignoring termination would hold the terminal in raw mode.
    ///
    /// nvmux answers the DA1 request (else `<prefix> d` costs a second) and
    /// strips it from the output, an exception to the module's rule: the real
    /// terminal's answer would land in the shell as text after we exit.
    fn drain_until_eof(&mut self) {
        let fd = self.master.as_raw_fd();
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        let mut buf = [0u8; 8192];
        let mut owed = false;
        while Instant::now() < deadline {
            let mut p = [pollfd(fd)];
            let n = unsafe { libc::poll(p.as_mut_ptr(), 1, 50) };
            if n <= 0 {
                continue;
            }
            match read_fd(fd, &mut buf) {
                Ok(0) | Err(_) => return,
                Ok(len) => {
                    owed |= asks_for_device_attributes(&buf[..len]);
                    let _ = write_stdout(&without_device_attributes_request(&buf[..len]));
                    if owed {
                        owed = !self.answer_device_attributes();
                    }
                }
            }
        }
    }
}

/// Hang up and reap before the fields drop: `reap` drains the client
/// through the master.
impl Drop for Attachment {
    fn drop(&mut self) {
        self.hang_up();
        self.reap();
    }
}

impl Attachment {
    /// Park a kept client while something else is in front, telling it to draw
    /// at a crawl. `None` if no socket pair or thread could be had; it is then
    /// retired.
    pub fn park(mut self) -> Option<Parked> {
        self.exits_seen = EXITS_RELAYED.load(std::sync::atomic::Ordering::Relaxed);
        // The last fade's diff is for a terminal another session is about to
        // use; drop it now, not on a next byte an idle session may never write.
        if let Some(copy) = self.copy() {
            copy.invalidate();
        }
        self.go_away();
        self.put_aside("nvmux-parked", keep_parked)
    }

    /// Hold a never-relayed client until its session's first visit
    /// (`[client] lazy = false`), with its attach probe if still unanswered.
    /// `None` if it could not be held; it is then retired.
    ///
    /// Unlike a parked client's, its output is kept whole in
    /// [`Attachment::unshown`] and relayed first on its first visit: its
    /// startup queries are still unheard, and it accepts late answers (measured
    /// on 0.12.5: five seconds late as well as at once).
    pub fn start_ahead(mut self, probe: Option<Probe>) -> Option<Parked> {
        self.unshown = Some(Vec::new());
        self.probe = probe;
        self.go_away();
        self.put_aside("nvmux-ahead", keep_unshown)
    }

    /// Tell a card-drawing client it is out of sight, so it stops animating and
    /// draws at a crawl until a relay sends [`card::Command::Clear`]. Done
    /// here, not as the relay ends, where a switch first has it draw the
    /// landing card ([`Attachment::stand_landing`]).
    fn go_away(&mut self) {
        if self.draws_cards {
            self.tell_client(&card::Command::Away);
            self.away = true;
            self.crawling = true;
        }
    }

    /// Whether this client was started ahead and not yet relayed.
    pub fn started_ahead(&self) -> bool {
        self.unshown.is_some()
    }

    /// A started-ahead client's attach probe, if still unanswered when it was
    /// taken back; the caller waits on it as for a fresh client.
    pub fn take_probe(&mut self) -> Option<Probe> {
        self.probe.take()
    }

    /// Get notified when the session has painted on this client's next relay
    /// (`timing: session painted`); a relay ending first drops the sender.
    pub fn tell_when_painted(&mut self) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        self.painted = Some(tx);
        rx
    }

    /// Float the settings over the session once the next relay has its screen
    /// back, for full-screen settings turned to float.
    pub fn float_the_settings(&mut self) {
        self.to_float = Some(Outcome::ShowSettings);
    }

    /// Likewise for the picker, back from a full-screen rename or a cancelled
    /// create prompt.
    pub fn float_the_picker(&mut self) {
        self.to_float = Some(Outcome::ToPicker);
    }

    /// Whether the session's screen is back for a panel to float over: no held
    /// paint, and the client on the alternate screen (or drawn on the primary).
    /// Before then, panel frames would stay on the primary screen the shell
    /// returns to. `true` if the copy cannot tell.
    fn screen_is_back(&self) -> bool {
        if self.hold.is_some() {
            return false;
        }
        match self.shadow.as_ref().or(self.screen.as_ref()) {
            Some(copy) if copy.is_usable() => copy.on_alternate_screen() || copy.has_contents(),
            _ => true,
        }
    }

    pub fn sock(&self) -> &Path {
        &self.sock
    }

    /// Hand the client to a thread of its own, named `name`, running `keep`.
    fn put_aside(
        self,
        name: &str,
        keep: fn(Attachment, UnixStream) -> Option<Attachment>,
    ) -> Option<Parked> {
        let session_id = self.session_id.clone();
        let (stop, stopped) = match UnixStream::pair() {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "could not park a client; retiring it");
                return None;
            }
        };
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || keep(self, stopped));
        match thread {
            Ok(thread) => Some(Parked {
                session_id,
                stop: Some(stop),
                thread: Some(thread),
            }),
            // The client went with the failed spawn's closure: dropped, so
            // hung up and reaped.
            Err(e) => {
                tracing::warn!(error = %e, "could not park a client; retired it");
                None
            }
        }
    }
}

/// A kept client out of the front, on its own thread until wanted back.
///
/// Its server keeps sending it the screen, so its output must be read. The
/// thread owns the [`Attachment`], keeping one reader per pty as [`relay`]
/// requires, and feeds the shadow and ledger so the last screen can be
/// painted back at once. A client that exits while parked is retired on the
/// thread. Clients started ahead are held in one too.
pub struct Parked {
    session_id: String,
    /// Dropped to stop the thread: the far end then reads as hung up.
    stop: Option<UnixStream>,
    /// `Some` until joined; returns the client, or `None` if it exited.
    thread: Option<std::thread::JoinHandle<Option<Attachment>>>,
}

impl std::fmt::Debug for Parked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parked")
            .field("session_id", &self.session_id)
            .field("alive", &self.is_alive())
            .finish_non_exhaustive()
    }
}

impl Parked {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Whether the client can still be taken back.
    pub fn is_alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Take the client back; `None` if it exited. Gives up after
    /// [`UNPARK_WAIT`] if the thread is busy retiring an exiting client.
    pub fn unpark(mut self) -> Option<Attachment> {
        drop(self.stop.take());
        let attachment = self.join_within(UNPARK_WAIT)?;
        // It may have exited just before the thread noticed; handed back, it
        // would show a dead session's screen.
        let mut p = [pollfd(attachment.master.as_raw_fd())];
        let n = unsafe { libc::poll(p.as_mut_ptr(), 1, 0) };
        let hung_up = n > 0 && p[0].revents & (libc::POLLHUP | libc::POLLERR) != 0;
        let exited = peek_child(child_pid(&attachment.child)) == Peek::Gone;
        if hung_up || exited {
            tracing::debug!(id = %self.session_id, "a parked client left as it was asked back");
            return None;
        }
        Some(attachment)
    }

    /// Tell the thread to retire its client (hang up and reap) without waiting.
    /// Telling several before dropping any lets them exit in parallel.
    pub fn tell_to_retire(&mut self) {
        if let Some(stop) = self.stop.as_mut() {
            let _ = stop.write_all(b"R");
        }
    }

    /// The thread's result, if it finishes within `wait`; otherwise it is left
    /// to finish alone.
    fn join_within(&mut self, wait: Duration) -> Option<Attachment> {
        let thread = self.thread.take()?;
        let started = Instant::now();
        let deadline = started + wait;
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                tracing::debug!(id = %self.session_id, "a parked client's thread is still busy");
                return None;
            }
            // A thread in `poll` hands back in microseconds, and a 1 ms sleep
            // would be most of a switch, so spin first.
            if started.elapsed() < UNPARK_SPIN {
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        // A panic on the thread dropped, and so retired, the client.
        thread.join().ok().flatten()
    }
}

/// How long [`Parked::join_within`] spins before sleeping.
const UNPARK_SPIN: Duration = Duration::from_micros(500);

/// How long a switch waits for a parked client's thread. Only a thread busy
/// retiring an exiting client takes longer, and then counts as gone.
const UNPARK_WAIT: Duration = Duration::from_millis(50);

/// How long dropping a [`Parked`] waits for the client to be retired.
const RETIRE_WAIT: Duration = Duration::from_millis(100);

/// Retires the client, waiting at most [`RETIRE_WAIT`]. A pool tells all
/// its clients first, so they exit in parallel.
impl Drop for Parked {
    fn drop(&mut self) {
        self.tell_to_retire();
        drop(self.stop.take());
        drop(self.join_within(RETIRE_WAIT));
    }
}

/// The parked client's thread: read its output into the shadow and ledger
/// until told to stop, then hand it back, or retire it.
///
/// **A parked client that sends DA1 or leaves the alternate screen is
/// finishing**: both end Neovim's exit and its `suspend` (which `Ctrl-Z` in
/// any UI sends to every attached client). Kept, it would hang or lose its
/// startup answers for good, so it is hung up first (a suspending client then
/// finds the hangup waiting), answered and reaped (measured on 0.11.0 and
/// 0.12.5). A stopped client is retired too; only
/// [`peek_child`] is used, since [`check_child`] consumes a stop.
fn keep_parked(mut attachment: Attachment, stop: UnixStream) -> Option<Attachment> {
    let id = attachment.session_id.clone();
    let mut buf = [0u8; 8192];
    loop {
        let master = attachment.master.as_raw_fd();
        let mut fds = [pollfd(stop.as_raw_fd()), pollfd(master)];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, IDLE_POLL_MS) };
        if n < 0 {
            // A `SIGWINCH` meant for the relay can land on this thread.
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            tracing::warn!(%id, "a parked client could not be watched; retiring it");
            return None;
        }
        // Client first: read what it said before letting go, and never
        // hand back one that exited.
        if n > 0 && ready(&fds[1]) {
            match read_fd(master, &mut buf) {
                Ok(len) if len > 0 => {
                    let chunk = &buf[..len];
                    attachment.saw_unrelayed(chunk);
                    if asks_for_device_attributes(chunk) || leaves_alt_screen(chunk) {
                        tracing::debug!(%id, "a parked client is finishing; retiring it");
                        attachment.hang_up();
                        attachment.answer_device_attributes();
                        return None;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                    ) => {}
                // EOF on macOS, EIO on Linux: the client has gone.
                _ => {
                    tracing::debug!(%id, "a parked client left");
                    return None;
                }
            }
        }
        if n > 0 && ready(&fds[0]) {
            // A byte orders a retire; end of stream means the client is
            // wanted back.
            let mut order = [0u8; 1];
            return match read_fd(stop.as_raw_fd(), &mut order) {
                Ok(1) => None,
                _ => Some(attachment),
            };
        }
        if n == 0 {
            match peek_child(child_pid(&attachment.child)) {
                Peek::Running => {}
                Peek::Stopped => {
                    tracing::debug!(%id, "a parked client stopped; retiring it");
                    return None;
                }
                Peek::Gone => {
                    tracing::debug!(%id, "a parked client exited");
                    return None;
                }
            }
        }
    }
}

/// Whether this output leaves the alternate screen. Written in one flush
/// with the exit sequence, so it arrives whole.
fn leaves_alt_screen(chunk: &[u8]) -> bool {
    memmem::find(chunk, ALT_SCREEN_LEAVE).is_some()
}

/// The most a started-ahead client may write before its first visit; past
/// it, it is retired. A hold's worth, so what is kept goes out as one first
/// paint; an idle session writes a few KB, so only a busy `:terminal` gets
/// here.
pub const AHEAD_MAX: usize = HOLD_MAX;

/// How often a started-ahead client's thread checks its probe, which
/// answers over a channel `poll` cannot watch. Nothing waits on it: it only
/// bounds how soon a refused client is retired.
const PROBE_POLL_MS: libc::c_int = 100;

/// The thread of a started-ahead client: keep all its output in
/// [`Attachment::unshown`] until asked for; or retire it if told to, if it
/// is exiting, if it wrote more than [`AHEAD_MAX`], or if its probe says
/// `Blocked` or refuses (the first visit then attaches afresh). Exiting shows
/// only as leaving the alternate screen (searched across reads): DA1 is part
/// of a normal startup (asked again after the first frame on 0.12.5).
fn keep_unshown(mut attachment: Attachment, stop: UnixStream) -> Option<Attachment> {
    let id = attachment.session_id.clone();
    let mut buf = [0u8; 8192];
    loop {
        if let Some(probe) = attachment.probe.as_mut() {
            match probe.wait(Duration::ZERO) {
                None => {}
                Some(Ok(Answer::Ready)) => attachment.probe = None,
                Some(Ok(Answer::Blocked { mode })) => {
                    tracing::debug!(%id, %mode, "a client started ahead is waiting for a key; retiring it");
                    return None;
                }
                Some(Err(e)) => {
                    tracing::debug!(%id, error = %e, "a client started ahead was refused; retiring it");
                    return None;
                }
            }
        }
        let wait = if attachment.probe.is_some() {
            PROBE_POLL_MS
        } else {
            IDLE_POLL_MS
        };
        let master = attachment.master.as_raw_fd();
        let mut fds = [pollfd(stop.as_raw_fd()), pollfd(master)];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, wait) };
        if n < 0 {
            // A `SIGWINCH` meant for the relay can land on this thread.
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            tracing::warn!(%id, "a client started ahead could not be watched; retiring it");
            return None;
        }
        // The client first, as on a parked client's thread.
        if n > 0 && ready(&fds[1]) {
            match read_fd(master, &mut buf) {
                Ok(len) if len > 0 => {
                    let unshown = attachment.unshown.get_or_insert_with(Vec::new);
                    let before = unshown.len();
                    unshown.extend_from_slice(&buf[..len]);
                    let leaving =
                        leaves_alt_screen(&unshown[tail_from(before, ALT_SCREEN_LEAVE)..]);
                    let kept = unshown.len();
                    if leaving {
                        tracing::debug!(%id, "a client started ahead is finishing; retiring it");
                        attachment.hang_up();
                        attachment.answer_device_attributes();
                        return None;
                    }
                    if kept > AHEAD_MAX {
                        tracing::debug!(
                            %id,
                            bytes = kept,
                            "a client started ahead said too much before it was shown; retiring it"
                        );
                        return None;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                    ) => {}
                // EOF on macOS, EIO on Linux: the client has gone.
                _ => {
                    tracing::debug!(%id, "a client started ahead left");
                    return None;
                }
            }
        }
        if n > 0 && ready(&fds[0]) {
            // A byte orders a retire; end of stream means the client is
            // wanted.
            let mut order = [0u8; 1];
            return match read_fd(stop.as_raw_fd(), &mut order) {
                Ok(1) => None,
                _ => Some(attachment),
            };
        }
        if n == 0 {
            match peek_child(child_pid(&attachment.child)) {
                Peek::Running => {}
                Peek::Stopped => {
                    tracing::debug!(%id, "a client started ahead stopped; retiring it");
                    return None;
                }
                Peek::Gone => {
                    tracing::debug!(%id, "a client started ahead exited");
                    return None;
                }
            }
        }
    }
}

/// Where to search a buffer grown from `before` bytes for a `needle` the
/// growth may have completed.
fn tail_from(before: usize, needle: &[u8]) -> usize {
    before.saturating_sub(needle.len() - 1)
}

/// A stand-in kept client for the [`crate::pool`] tests: `sh -c script` on
/// a real pty, attached to nothing, with a ledger and no screen.
#[cfg(test)]
pub(crate) fn kept_standin(session_id: &str, script: &str) -> Attachment {
    let cmd = Command::new("sh").arg("-c").arg(script);
    let mut a = spawn_client_with(
        session_id,
        Path::new("/nvmux-test-never-read.sock"),
        "",
        cmd,
        false,
    )
    .expect("spawn sh");
    a.announce = None;
    a.shadow = None;
    a.screen = None;
    a.ledger = Some(Ledger::new());
    a.startup_answered = true;
    a
}

#[cfg(test)]
impl Attachment {
    pub(crate) fn pid_for_test(&self) -> i32 {
        child_pid(&self.child).as_raw()
    }

    /// As if its terminal had not yet answered its startup queries.
    pub(crate) fn cut_short_for_test(&mut self) {
        self.startup_answered = false;
    }

    /// Wait for a stand-in to print `r`, meaning its traps are set. Returns
    /// whether it did.
    pub(crate) fn ready_for_test(&self) -> bool {
        let fd = self.master.as_raw_fd();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 256];
        while Instant::now() < deadline {
            let mut p = [pollfd(fd)];
            unsafe { libc::poll(p.as_mut_ptr(), 1, 50) };
            if ready(&p[0]) && read_fd(fd, &mut buf).is_ok_and(|n| buf[..n].contains(&b'r')) {
                return true;
            }
        }
        false
    }
}

/// Start a client for a session and wait for the probe to pass. `sock` must
/// be reachable from this machine. Blocking, bounded by
/// [`rpc::PROBE_TIMEOUT`], for integration tests; the binary uses
/// [`spawn_client`] and [`Probe::start`] with no time limit. A session
/// waiting for a key is attached to (see [`Answer`]): the key that ends the
/// wait is pressed on this client.
pub fn spawn(session_id: &str, sock: &Path, announce: &str) -> Result<Attachment> {
    let (cmd, draws_cards) = client_command(sock);
    spawn_with(session_id, sock, announce, cmd, draws_cards)
}

/// Start a client without probing: the forking half of [`spawn`]. The
/// caller probes with [`Probe::start`] while it connects, so over SSH a
/// switch costs the longer of the two. Client output reaches the terminal
/// only through the relay, started once the probe passes. A client that
/// cannot start is reported first: that error (`nvim` missing) is the
/// useful one.
pub fn spawn_client(session_id: &str, sock: &Path, announce: &str) -> Result<Attachment> {
    let (cmd, draws_cards) = client_command(sock);
    spawn_client_with(session_id, sock, announce, cmd, draws_cards)
}

/// [`spawn_client`] with nvmux's own client run from `exe`, for tests that
/// are not that binary.
pub fn spawn_own_client(
    session_id: &str,
    sock: &Path,
    announce: &str,
    exe: &Path,
) -> Result<Attachment> {
    spawn_client_with(session_id, sock, announce, own_client(exe, sock), true)
}

/// The client command (nvmux's own, or `nvim --remote-ui` with
/// `[client] ui = "nvim"`), and whether it draws cards.
fn client_command(sock: &Path) -> (Command, bool) {
    match crate::config::get().client.ui {
        crate::config::Ui::Nvim => {
            // A `Command` inherits our environment and working directory, so
            // `TERM`, `COLORTERM` and the rest reach the client.
            let cmd = Command::new("nvim")
                .arg("--server")
                .arg(sock)
                .arg("--remote-ui");
            (cmd, false)
        }
        crate::config::Ui::Nvmux => {
            // This binary, not a `nvmux` on `$PATH`, which may differ.
            let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("nvmux"));
            (own_client(&exe, sock), true)
        }
    }
}

/// nvmux's own client, run from `exe`.
fn own_client(exe: &Path, sock: &Path) -> Command {
    let mut cmd = Command::new(exe).arg("--client").arg(sock);
    if let Some(palette) = crate::palette::get() {
        cmd = cmd.env(crate::client::PALETTE_ENV, palette.to_env());
    }
    // The settings nvmux runs on, which may not be the file's.
    cmd.env(
        crate::client::SETTINGS_ENV,
        crate::config::get().drawing().to_env(),
    )
}

/// [`spawn`] with the client command supplied, so tests can use a stand-in
/// where there is no `nvim`.
fn spawn_with(
    session_id: &str,
    sock: &Path,
    announce: &str,
    cmd: Command,
    draws_cards: bool,
) -> Result<Attachment> {
    let t_spawn = std::time::Instant::now();
    let attachment = spawn_client_with(session_id, sock, announce, cmd, draws_cards)?;
    tracing::debug!(
        ms = t_spawn.elapsed().as_secs_f64() * 1000.0,
        "timing: client spawn"
    );

    // Probe the session while the client connects.
    let t_probe = std::time::Instant::now();
    let verdict = Probe::start(session_id, sock).and_then(|mut probe| {
        // A wait that runs out gives the timeout error callers expect.
        // Dropping the probe ends its worker.
        probe
            .wait(rpc::PROBE_TIMEOUT)
            .unwrap_or(Err(NvmuxError::Rpc(crate::error::RpcError::Timeout(
                rpc::PROBE_TIMEOUT,
            ))))
    });
    match verdict {
        // A blocked session is attached to, as in the binary.
        Ok(answer) => tracing::debug!(?answer, "attach probe answered"),
        Err(e) => {
            // Explicitly, so the client is gone before the error is shown.
            drop(attachment);
            return Err(e);
        }
    }
    tracing::debug!(
        ms = t_probe.elapsed().as_secs_f64() * 1000.0,
        "timing: attach probe"
    );
    Ok(attachment)
}

/// The attach probe, running on its own thread. Its last call is deferred,
/// so a busy session (`:!make`, CPU-bound Lua) answers only when done; there
/// is no time limit, since the user can give up from `ui::attaching`. `Drop`
/// shuts the socket down under the worker (see [`rpc::Interrupt`]), or it
/// could later type `<CR>` into someone else's hit-enter prompt.
pub struct Probe {
    rx: mpsc::Receiver<Result<Answer>>,
    interrupt: rpc::Interrupt,
    /// `None` only once a test has taken it, to prove the worker ended.
    thread: Option<std::thread::JoinHandle<()>>,
}

/// What the attach probe found, for a reachable session. A session waiting
/// for a key runs with its event queue off, so deferred calls
/// (`nvim_list_uis`, the client's `nvim_ui_attach`) wait for a key; the
/// probe reports that instead of queueing behind it (see [`probe_on`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The session answered a deferred call, and has room for this client.
    Ready,
    /// Waiting for a key nvmux may not press for the user; the UI count is
    /// unknown and the first paint waits on that key.
    Blocked {
        /// The mode, as `nvim_get_mode` spells it, for the log.
        mode: String,
    },
}

impl Probe {
    /// Connect and start asking. The connect runs here, since a dead unix
    /// socket is refused at once; only the calls go to the worker.
    pub fn start(session_id: &str, sock: &Path) -> Result<Self> {
        let mut client = rpc::Client::connect_with(sock, None)?;
        let interrupt = client.interrupt()?;
        let (tx, rx) = mpsc::channel();
        let id = session_id.to_string();
        let thread = std::thread::Builder::new()
            .name("nvmux-probe".into())
            .spawn(move || {
                let verdict = probe_on(&mut client, &id);
                // A dropped receiver means the wait was given up on.
                let _ = tx.send(verdict);
            })?;
        Ok(Self {
            rx,
            interrupt,
            thread: Some(thread),
        })
    }

    /// Wait up to `timeout` for the verdict (`None`: not yet). A worker that
    /// died without one gives an error, so a poller never spins forever.
    pub fn wait(&mut self, timeout: Duration) -> Option<Result<Answer>> {
        match self.rx.recv_timeout(timeout) {
            Ok(verdict) => Some(verdict),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => Some(Err(NvmuxError::Io(
                std::io::Error::other("the attach probe ended without a verdict"),
            ))),
        }
    }

    /// Give up as `Drop` does, but return the worker so a test can prove the
    /// interrupt ended it, not only the connection.
    #[cfg(test)]
    fn abandon(mut self) -> std::thread::JoinHandle<()> {
        self.thread.take().expect("the worker is taken once")
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        // On every way out. Not joined: the shutdown unblocks the worker, and
        // nothing needs it gone first.
        self.interrupt.fire();
        if let Some(thread) = &self.thread {
            tracing::debug!(finished = thread.is_finished(), "attach probe retired");
        }
    }
}

/// Ask the server whether it can take a client, and make it ready for one.
/// `get_mode` is the reachability check: a real round trip, so a dead
/// forward or a non-Neovim peer fails it. No `api_info`: nothing here reads
/// a version, and it costs an SSH round trip per attach.
fn probe_on(client: &mut rpc::Client<UnixStream>, session_id: &str) -> Result<Answer> {
    // Both questions go out at once: over SSH each is a round trip (150 ms
    // RTT: 302 ms in series, 151 ms together). The deferred `list_uis` is
    // awaited only once the mode says the queue runs; a server waiting for a
    // key answers it only after one. Only a *key* must never be sent ahead of
    // a fresh mode reading. nvmux ends only a hit-enter prompt itself (see
    // `rpc::Mode::at_hit_enter`), re-reading the mode after each `<CR>`, since
    // prompts stack. Another attached UI loses its prompt too. Not pipelined:
    // a mode asked right behind a `<CR>` is answered before the key is
    // consumed, and would earn a second `<CR>`.
    let mut asking_mode = client.ask_mode()?;
    let counting = client.ask_uis()?;

    for _ in 0..MAX_PROMPTS {
        let mode = client.mode_reply(asking_mode)?;
        if !mode.blocking {
            return ui_count(client, session_id, counting);
        }
        if !mode.at_hit_enter() {
            // A more-prompt, a half-typed `g` or a script awaiting a key look
            // alike over RPC. Hand over the terminal: the user's first key ends
            // the wait and releases the queued `nvim_ui_attach`.
            tracing::info!(
                id = session_id,
                mode = %mode.mode,
                "attach: the session is waiting for a key nvmux must not press"
            );
            return Ok(Answer::Blocked { mode: mode.mode });
        }
        tracing::info!(
            id = session_id,
            "attach: ending the session's hit-enter prompt"
        );
        client.input("<CR>")?;
        asking_mode = client.ask_mode()?;
    }

    // Still prompting after the budget: hand over to the user, who can see
    // the prompts.
    let mode = client.mode_reply(asking_mode)?;
    if mode.blocking {
        tracing::info!(
            id = session_id,
            mode = %mode.mode,
            "attach: the session is still prompting after {MAX_PROMPTS} keys"
        );
        return Ok(Answer::Blocked { mode: mode.mode });
    }
    ui_count(client, session_id, counting)
}

/// How many hit-enter prompts one attach answers before handing over: one
/// more than the three that stacked erroring callbacks produced.
const MAX_PROMPTS: usize = 4;

/// The deferred half of the probe: whether the session has room for another
/// client, awaited only once the mode says the queue runs. Unbounded on
/// purpose: a session in `:!make` answers when done (`ui::attaching` shows a
/// spinner). Not `unwrap_or(0)`: guessing zero is how the 17th attach
/// happens. `>`, not `>=`: our own client may already be counted; `MAX_UIS`,
/// half Neovim's limit, absorbs the off-by-one either way.
fn ui_count(
    client: &mut rpc::Client<UnixStream>,
    session_id: &str,
    counting: rpc::Pending,
) -> Result<Answer> {
    let uis = client.uis_reply(counting)?;
    if uis > MAX_UIS {
        return Err(NvmuxError::Session(
            crate::error::SessionError::TooManyUis {
                id: session_id.to_string(),
                attached: uis,
            },
        ));
    }
    Ok(Answer::Ready)
}

/// Open a pty and start the client on it.
fn spawn_client_with(
    session_id: &str,
    sock: &Path,
    announce: &str,
    cmd: Command,
    draws_cards: bool,
) -> Result<Attachment> {
    // Sized before the child starts (`PtySize::default()` is 24x80). Note
    // `crossterm::size()` is (cols, rows) but `PtySize` is { rows, cols }.
    let size = term::terminal_size();
    let (master, pts) = pty_process::blocking::open()
        .map_err(|e| pty_error("could not open a pty for the client", e))?;
    master
        .resize(size.into())
        .map_err(|e| pty_error("could not size the client's pty", e))?;

    // Before the child starts, so the pty never echoes what the relay writes.
    take_input_as_written(&master);

    // nvmux runs under umask 077; the client gets the launch umask back, as
    // the session does (see `paths::restrict_umask`).
    let umask = crate::paths::launch_umask().map(|mask| mask.bits());
    // Asked here, because `sysconf` is not a call the child may make.
    let open_max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    // SAFETY: `before_exec` makes async-signal-safe calls only.
    let cmd = unsafe { cmd.pre_exec(move || before_exec(umask, open_max)) };
    // `spawn` takes and closes the slave; held open here, the master would
    // never see EOF after the child exits.
    let child = cmd
        .spawn(pts)
        .map_err(|e| pty_error("could not start the client", e))?;

    let fades = fade::enabled() && fade::session();
    let shadow = fades.then(|| shadow::Shadow::new(size.rows, size.cols));
    let kept = crate::config::get().client.per_session;

    Ok(Attachment {
        session_id: session_id.to_string(),
        sock: sock.to_path_buf(),
        announce: Some(announce.to_string()),
        hand_off: None,
        child,
        master,
        resumed: false,
        reaped: false,
        // A shadow costs a parse of all output, so only with the fade; one of
        // the two always exists, for panels to float over.
        shadow,
        screen: (!fades).then(|| shadow::Shadow::new(size.rows, size.cols)),
        hold: None,
        // Set by `relay`, which knows whether there is a notice to write.
        boundary: None,
        span: false,
        // Only for a kept client, the only kind resumed from its output.
        ledger: kept.then(Ledger::new),
        startup_answered: false,
        expecting_a_size_report: None,
        exits_seen: 0,
        unshown: None,
        probe: None,
        draws_cards,
        away: false,
        crawling: false,
        // The generation its environment was just given (see `own_client`).
        heard: crate::config::generation(),
        desk: None,
        row: None,
        kept: shadow::Kept::default(),
        left_standing: None,
        opening: Vec::new(),
        landed_since: None,
        dark: false,
        painted: None,
        to_float: None,
    })
}

/// Child setup between fork and exec: reset commonly ignored signals and
/// unblock all (both survive exec; under `nohup` every client would inherit
/// an ignored SIGHUP), mark every fd past stderr close-on-exec (not closed,
/// so std's pipe can still report a failed exec), and set our launch umask.
/// Async-signal-safe calls only, no allocation.
fn before_exec(umask: Option<libc::mode_t>, open_max: libc::c_long) -> std::io::Result<()> {
    for sig in [
        libc::SIGCHLD,
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGTERM,
        libc::SIGALRM,
    ] {
        // SAFETY: async-signal-safe, and SIG_DFL installs nothing to run.
        unsafe { libc::signal(sig, libc::SIG_DFL) };
    }
    // SAFETY: async-signal-safe, on a set of our own.
    unsafe {
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
    }
    close_on_exec_from(3, open_max);
    if let Some(mask) = umask {
        // SAFETY: async-signal-safe, and it cannot fail.
        unsafe { libc::umask(mask) };
    }
    Ok(())
}

/// The most fds [`close_on_exec_from`] checks one by one: the limit can be
/// far past any fd in use, and each is a syscall.
const CLOEXEC_SCAN_MAX: libc::c_long = 65_536;

/// Mark every fd from `low` up to `open_max` (the fd limit, read before the
/// fork) close-on-exec.
fn close_on_exec_from(low: libc::c_int, open_max: libc::c_long) {
    // One call where available: Linux 5.11 and later.
    #[cfg(target_os = "linux")]
    {
        // SAFETY: a syscall touching none of our memory; old kernels refuse.
        let marked = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                low as libc::c_uint,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        };
        if marked == 0 {
            return;
        }
    }
    let high = if open_max > 0 {
        open_max.min(CLOEXEC_SCAN_MAX)
    } else {
        CLOEXEC_SCAN_MAX
    };
    for fd in low..high as libc::c_int {
        // SAFETY: async-signal-safe; on a closed fd it fails harmlessly.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

/// Convert a pty crate error into an I/O error saying what was being done,
/// keeping the system's error kind.
fn pty_error(doing: &str, e: pty_process::Error) -> NvmuxError {
    let e = match e {
        pty_process::Error::Io(e) => e,
        e => std::io::Error::other(e.to_string()),
    };
    NvmuxError::Io(std::io::Error::new(e.kind(), format!("{doing}: {e}")))
}

/// Put the pty in raw input mode before the client starts, as it would
/// itself a few ms later. Otherwise a card written meanwhile is echoed
/// (`ESC` as `^[`) onto the primary screen, `Ctrl-C` kills the client and
/// `Enter` arrives as `Ctrl-J`. Output flags are left alone. Set through the
/// master, which works on Linux and macOS.
fn take_input_as_written(master: &Pty) {
    use nix::sys::termios::{self, SetArg};

    let fd = master.as_fd();
    let made = termios::tcgetattr(fd).and_then(|opened| {
        let mut raw = opened.clone();
        termios::cfmakeraw(&mut raw);
        raw.output_flags = opened.output_flags;
        termios::tcsetattr(fd, SetArg::TCSANOW, &raw)
    });
    if let Err(e) = made {
        tracing::warn!(error = %e, "could not make a client's input raw; it may be echoed");
    }
}

/// Relay bytes between the terminal and the child until interrupted.
///
/// One thread and one `poll` over stdin, the master and the SIGWINCH pipe:
/// two readers on one file description would split the stream, and a thread
/// blocked in `read(0)` cannot be cancelled.
///
/// `begin_next` is called once with a switch's target as soon as [`pump`]
/// returns it, so the next client's fork and probe run under this session's
/// fade out. It must only start work and must not draw: the terminal is
/// still raw with this session on it. It returns the switch's landing card,
/// if known, which both fades leave standing
/// ([`Attachment::take_left_standing`]).
pub fn relay(
    mut attachment: Attachment,
    highest_session_num: u32,
    begin_next: &mut dyn FnMut(Target) -> Option<card::Card>,
) -> Result<(Outcome, Option<Attachment>)> {
    let master_fd = attachment.master.as_raw_fd();

    let winch = winch::Winch::install()?;
    // A client that has not painted yet, whose first paint can be held.
    let fresh = !attachment.resumed;
    if attachment.resumed {
        // The client entered the alternate screen on its first relay and will
        // not re-enter it; on the primary screen, its last frame would stay
        // under the shell prompt after a detach. Not shown to the shadow,
        // whose screen is what fades back in. A picker's name comes off in the
        // same span, closed by the fade's first frame.
        match attachment.hand_off.as_ref() {
            Some(h) => term::enter_alt_screen_and_clear_after(&h.erase()),
            None => term::enter_alt_screen_and_clear(),
        }
    } else {
        // Normally done by whoever handed over the terminal; this is a
        // backstop. Skipped if the picker left a name up: the first paint
        // takes it down (see `release_hold`).
        if attachment.hand_off.is_none() {
            term::leave_alt_screen_and_clear();
        }
        // A client started ahead was sized at startup; resize it before its
        // output is read, so the shadow and its redraw match the terminal.
        // Tell it it is back first, or it would redraw at a crawl, after the
        // held paint is released.
        if attachment.started_ahead() {
            if std::mem::take(&mut attachment.away) {
                attachment.tell_client(&card::Command::Clear);
            }
            let size = term::terminal_size();
            if pty_size(&attachment.master) != Some(size) {
                attachment.resize_to(size);
            }
        }
        attachment.shadow_saw(term::HANDOVER);
    }
    let mut raw = term::RawMode::enter()?;

    // Whether the screen faded in from the shadow on a resume; lets the
    // notice go up over a quiet kept session.
    let mut dissolved = false;
    // A kept client's repaint, requested on return.
    let mut erasing: Option<Erasing> = None;
    // Cards first: clear the old ones, show the landing card. The client
    // draws it at once, so the fade in shows it.
    let landed = attachment.open_desk();
    // Sent now if the screen is drawn already or nothing will be held; a
    // fresh client's held paint sends them on release.
    if attachment.resumed || attachment.shadow.is_none() {
        attachment.put_in_opening(Instant::now());
    }

    // What the child buffered while blocked mid-write describes a screen the
    // picker has since drawn over.
    if attachment.resumed {
        let t_resume = Instant::now();
        // A kept client's backlog is the screen about to come back, so it
        // is read, not discarded, until the shadow has the requested card. A
        // client back from a crawl is likewise given time to draw what it
        // owed, so the screen does not jump later.
        if attachment.is_kept() {
            let kept = attachment.kept.clone();
            if landed && !kept.is_empty() && attachment.shadow.is_some() {
                if !attachment.drain_until_shown(&kept, Instant::now() + CARD_WAIT) {
                    tracing::debug!(
                        "the client did not draw the card its return lands with in time"
                    );
                }
            } else if landed || attachment.crawling {
                attachment.await_output(CARD_WAIT);
            }
            attachment.drain_unrelayed();
        } else {
            discard_pending(master_fd);
        }
        // Size first, then the fade in; the repaint lands after it. nvmux
        // shows the cursor itself: the last fade out hid it, and the server
        // may never repaint.
        let size = term::terminal_size();
        // Before the resize, which changes it.
        let resized = pty_size(&attachment.master) != Some(size);
        attachment.resize_to(size);
        // A kept client's repaint is requested as the fade in starts, so the
        // server redraws under it. Not if the size changed while parked (the
        // repaint could beat the resize and draw at the old size), nor if a
        // moving card stands (output during the fade reaches only the
        // shadow); then it is requested after.
        let early = attachment.is_kept() && !resized && !attachment.standing().moves();
        if early {
            erasing = kept_clients_repaint(&mut attachment);
        }
        // Settings or a picker floating over the returning session bring it
        // in from the background themselves (see `panel`), rather than
        // fading it in only to dim it again.
        attachment.dark = attachment.to_float.is_some()
            && attachment.hand_off.is_none()
            && attachment
                .shadow
                .as_ref()
                .is_some_and(shadow::Shadow::has_contents);
        dissolved = !attachment.dark && attachment.dissolve_in();
        // Off the primary screen since the switch; on the session's only if
        // the fade drew it.
        let carried = attachment.hand_off.take().is_some();
        let on_screen = attachment.dark || dissolved || attachment.paint_kept_screen();
        if !on_screen {
            // Close the span the switch opened, which no fade frame closed.
            if carried {
                term::end_span();
            }
            attachment.shadow_saw(term::RESUME);
            term::show_cursor();
        }
        tracing::debug!(
            ms = t_resume.elapsed().as_secs_f64() * 1000.0,
            on_screen,
            kept = attachment.is_kept(),
            "timing: resumed screen"
        );
        if attachment.is_kept() {
            // The ledger restores the client's terminal state, the server the
            // exact screen. Restore the pen and cursor first: the client may
            // write before the repaint, assuming neither moved.
            if dissolved {
                attachment.hand_back_the_pen();
            }
            attachment.put_back_what_it_told(size);
            attachment.tell_its_size(size, resized);
            if !early {
                erasing = kept_clients_repaint(&mut attachment);
            }
        } else {
            repaint(&mut attachment, Repaint::Resume);
        }
    }
    attachment.resumed = true;
    // Back in front as far as the client knows: told above if started
    // ahead, or by the opening's Clear if resumed.
    attachment.away = false;
    attachment.crawling = false;
    if fresh && attachment.shadow.is_some() {
        attachment.hold = Some(Hold::new(Instant::now()));
    }

    // Announced by a card the client draws (see `crate::card`). The old
    // box is kept but never armed; it was
    // `announce::Popup::arm(attachment.announce.take(), Instant::now())`.
    let popup: Option<announce::Popup> = None;
    // Only for the notice; everything written above is complete, so the
    // parser starts between sequences.
    attachment.boundary = popup.is_some().then(boundary::Boundary::new);

    let outcome = pump(
        &mut attachment,
        master_fd,
        &winch,
        highest_session_num,
        popup,
        dissolved,
        erasing,
    );
    // Cleared whether or not shown: this client may come straight back.
    attachment.announce = None;
    attachment.desk = None;
    // Only this relay notifies.
    attachment.painted = None;
    // Floats only over this relay's session.
    attachment.to_float = None;
    // Nothing the client writes reaches the terminal until it is relayed
    // again, so what the ledger sees next is news to the terminal.
    if let Some(ledger) = attachment.ledger.as_mut() {
        ledger.left_the_terminal();
    }

    // As early as possible, so the caller's work runs under the fade.
    let landing = outcome
        .as_ref()
        .ok()
        .and_then(|o| o.switch_target())
        .and_then(begin_next);

    // However the relay ended, nothing the client wrote may stay unshown.
    if let Err(e) = attachment.stop_watching_the_terminal() {
        tracing::debug!(error = %e, "could not write out what the notice was keeping");
    }
    // A relay cut short during the hold (a prefix typed at once, a child
    // that exited) releases the paint as is, with no fade.
    if let Err(e) = attachment.release_hold(false) {
        tracing::debug!(error = %e, "could not release the held first paint");
    }

    match outcome {
        Ok(
            held @ (Outcome::ToPicker
            | Outcome::CreateNew
            | Outcome::CreateAfter(_)
            | Outcome::Rename(_)
            | Outcome::ShowHelp
            | Outcome::ShowSettings
            | Outcome::Resettle
            | Outcome::Switch(_)),
        ) => {
            // Fade out before restoring modes (so a key typed meanwhile is not
            // echoed) and before the hand-off, which erases with the last
            // frame's colour. A switch first has the client draw the landing
            // card, which the fade leaves standing.
            let stood = landing
                .as_ref()
                .and_then(|card| attachment.stand_landing(card));
            let kept = stood
                .as_ref()
                .map_or_else(shadow::Kept::default, |(kept, _)| kept.clone());
            // A panel that took the session with it already faded it.
            let faded = std::mem::take(&mut attachment.dark) || attachment.fade_out(&kept);
            raw.restore();
            // Clear now, not after the next client spawns, or the old session
            // stays up meanwhile. Not shown to the shadow: a switch naming
            // this session resumes it and fades that screen back in. A card
            // the fade left standing stays for the next relay to take down.
            if held.leads_straight_into_another_relay() {
                let standing = stood.filter(|_| faded).and_then(|(kept, since)| {
                    let drawn = fade::standing(attachment.shadow.as_ref()?, &kept);
                    HandOff::card(drawn, kept.spans(), since)
                });
                match standing {
                    Some(h) => {
                        term::leave_alt_screen_and_clear_keeping(&h.show());
                        attachment.left_standing = Some(h);
                    }
                    None => term::leave_alt_screen_and_clear(),
                }
            }
            Ok((held, Some(attachment)))
        }
        Ok(Outcome::ChildExited) => {
            // The child emitted its restore; relay the rest. Parked kept
            // clients rely on modes it just turned off (see [`EXITS_RELAYED`]).
            EXITS_RELAYED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            attachment.drain_until_eof();
            raw.restore();
            attachment.reap();
            Ok((Outcome::ChildExited, None))
        }
        Ok(other @ (Outcome::Detached | Outcome::StdinClosed)) => {
            // Let the child restore the terminal; a competing reset would
            // corrupt its restore. A final reset covers a client that died
            // first.
            attachment.hang_up();
            attachment.drain_until_eof();
            raw.restore();
            attachment.reap();
            term::reset_screen();
            // After the reset: leaving the alternate screen puts the cursor
            // back on the line with the client's hangup message.
            term::erase_hung_up_clients_line();
            Ok((other, None))
        }
        Err(e) => {
            // The error goes to a shell: restore cursor and colours first, in
            // case the client died mid-frame.
            attachment.hang_up();
            attachment.drain_until_eof();
            raw.restore();
            attachment.reap();
            term::reset_screen();
            // The same hangup leaves the same stray line.
            term::erase_hung_up_clients_line();
            Err(e)
        }
    }
}

fn pump(
    attachment: &mut Attachment,
    master_fd: RawFd,
    winch: &winch::Winch,
    highest_session_num: u32,
    mut popup: Option<announce::Popup>,
    dissolved: bool,
    mut erasing: Option<Erasing>,
) -> Result<Outcome> {
    let stdin_fd = std::io::stdin().as_raw_fd();
    let mut keys = crate::config::get().keys;
    let mut prefix = Prefix::with_keys(highest_session_num, keys.prefix, keys.bind);
    let mut prefix_timeout = Duration::from_millis(keys.timeout_ms);
    // When a pending prefix, half-typed number or cut-off sequence must be
    // settled. An instant, not a per-poll timeout: a chatty child keeps
    // `poll` returning early, so a restarting timeout would never fire.
    let mut deadline: Option<Instant> = None;
    // Re-read only on a `SIGWINCH` or after a floating panel: each read
    // costs 3x the syscalls of the rest of a pass.
    let mut size = term::terminal_size();
    // The row that says what the next key does while a `<prefix>` waits. It
    // shares `erasing` (the notice's repaint) when free, since a `:mode`
    // repaints its row too.
    let mut bar = hint::Bar::new(Instant::now(), size, keys.bind);
    // `<prefix> d` with unsaved settings asks first, on the bar's row,
    // and takes the keyboard until answered.
    let mut leaving: Option<Leaving> = None;
    let mut buf = [0u8; 8192];
    // The session's redraw, timed from the moment the terminal became its,
    // for the `timing:` log and `Attachment::painted`.
    let relay_started = Instant::now();
    let mut first_byte: Option<Duration> = None;
    let mut painted_bytes = 0usize;
    let mut last_byte = Instant::now();
    let mut painted = false;

    // A started-ahead client's output so far goes first, as if just read.
    if let Some(unshown) = attachment.unshown.take().filter(|b| !b.is_empty()) {
        let now = Instant::now();
        attachment.relay_output(&unshown, now)?;
        first_byte = Some(relay_started.elapsed());
        painted_bytes += unshown.len();
        last_byte = now;
    }

    loop {
        // Float pending settings or picker once the screen is back, before
        // reading keys, so keys typed meanwhile go to them.
        if let Some(outcome) = attachment.to_float.filter(|_| attachment.screen_is_back()) {
            if let Some(outcome) = float_over(attachment, master_fd, winch, outcome)? {
                return Ok(outcome);
            }
            // Re-read what they may have changed, as after `<prefix> ,` below.
            keys = crate::config::get().keys;
            prefix = Prefix::with_keys(highest_session_num, keys.prefix, keys.bind);
            prefix_timeout = Duration::from_millis(keys.timeout_ms);
            deadline = None;
            size = term::terminal_size();
            bar = hint::Bar::new(Instant::now(), size, keys.bind);
        }

        // Every clock folds into one absolute `poll` deadline.
        let now = Instant::now();
        let next = [
            deadline,
            popup.as_ref().map(announce::Popup::wake_at),
            bar.wake_at(),
            attachment.hold_wake_at(),
            attachment.kept_due(),
            erasing.as_ref().map(|_| now + ERASE_POLL),
            attachment.cards_due(),
        ]
        .into_iter()
        .flatten()
        .min();

        // A stopped child produces no poll activity at all, so even an idle
        // wait is bounded; an armed prefix shortens it to its own deadline.
        let timeout_ms = match next {
            Some(d) => {
                // Rounded up: rounded down, the last fraction of a ms
                // before each deadline would spin in `poll`.
                let left = d.saturating_duration_since(Instant::now());
                let ms = left.as_micros().div_ceil(1000);
                ms.min(IDLE_POLL_MS as u128) as libc::c_int
            }
            None => IDLE_POLL_MS,
        };

        // The board for cards the notifier posts (see `crate::card`).
        // Without its pipe it is polled as -1, which `poll` skips.
        let board = card::board();
        let mut fds = [
            pollfd(stdin_fd),
            pollfd(master_fd),
            pollfd(winch.fd()),
            pollfd(board.wake_fd().unwrap_or(-1)),
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(NvmuxError::Io(err));
        }

        // Output first, so the screen is current before keys are acted on.
        let child_spoke = n > 0 && ready(&fds[1]);
        if child_spoke {
            match read_fd(master_fd, &mut buf) {
                // EOF on macOS, EIO on Linux: both mean the slave closed.
                Ok(0) | Err(_) => return Ok(Outcome::ChildExited),
                Ok(len) => {
                    // Byte for byte; the shadow gets a copy.
                    let now = Instant::now();
                    attachment.relay_output(&buf[..len], now)?;
                    if first_byte.is_none() {
                        first_byte = Some(relay_started.elapsed());
                    }
                    painted_bytes += len;
                    last_byte = now;
                }
            }
        }

        // Release the finished first paint before the notice below,
        // which must find the session on screen.
        let released = attachment.hold_is_due(Instant::now());
        if released {
            attachment.release_hold(true)?;
        }

        // A held tail is never held long: a client stopped mid-sequence may
        // be awaiting the terminal's answer.
        if attachment
            .kept_due()
            .is_some_and(|due| Instant::now() >= due)
        {
            attachment.release_kept()?;
        }

        // Once per relay, on the first lull after any output. After the
        // read above, so a burst ending this pass is counted first.
        if !painted && first_byte.is_some() && last_byte.elapsed() >= PAINT_SETTLED {
            painted = true;
            tracing::debug!(
                first_byte_ms = first_byte.map(|d| d.as_secs_f64() * 1000.0),
                painted_ms = (last_byte - relay_started).as_secs_f64() * 1000.0,
                bytes = painted_bytes,
                "timing: session painted"
            );
            if let Some(waiting) = attachment.painted.take() {
                let _ = waiting.send(());
            }
        }

        if n > 0 && ready(&fds[2]) {
            winch.drain();
            size = term::terminal_size();
            // Enough on its own: the kernel signals the pty's foreground group
            // and the client calls try_resize.
            attachment.resize_to(size);
        }

        // After the frame it sits on and before keys; `size` is current, so
        // each notice frame fits.
        if let Some(p) = popup.as_mut() {
            // A held paint counts as the child talking: the fade that ends
            // the hold repaints every cell and would wipe the box.
            let pass = announce::Pass {
                wrote: child_spoke || attachment.hold.is_some(),
                // Not before the client's first byte (until then the
                // terminal shows the shell's primary screen), unless the
                // screen faded in from the shadow on a kept client's
                // return.
                between: (first_byte.is_some() || dissolved) && attachment.between_sequences(),
            };
            let act = p.step(Instant::now(), pass, size, attachment.shadow.as_ref());
            match act {
                announce::Act::Idle => {}
                announce::Act::Paint(bytes) => {
                    attachment.write_over_session(&bytes)?;
                    // On a three-row terminal the box's bottom row is the
                    // bar's. Telling only the bar avoids freezing the notice
                    // while a prefix is armed.
                    bar.overdrawn();
                }
                announce::Act::Erase => {
                    // Only the server knows what the box covered. Asked from a
                    // thread so the relay keeps running (see [`Erasing`]);
                    // resized here first.
                    attachment.resize_to(size);
                    match Erasing::start(&attachment.sock) {
                        Ok(started) => erasing = Some(started),
                        // No thread: use the fallback.
                        Err(e) => {
                            tracing::debug!(error = %e, "no thread for the notice repaint");
                            apply(attachment, Repaint::Notice, Served::NONE);
                        }
                    }
                    popup = None;
                }
                announce::Act::Done => popup = None,
            }
            // Log the parser state when the notice ends: the key fact
            // for a "the box never appeared" report.
            if popup.is_none() {
                tracing::debug!(
                    between = attachment.between_sequences(),
                    "timing: attach notice over"
                );
            }
        }

        // The notice's repaint, if answered; only the fallback waits on it.
        if let Some(served) = erasing.as_ref().and_then(Erasing::answer) {
            erasing = None;
            apply(attachment, Repaint::Notice, served);
        }

        // Settle a pending prefix before reading new input, so a key just
        // after the deadline is not taken as a command. A cut-off escape
        // sequence is the opposite: waiting input may complete it, so it is
        // read first.
        let stdin_ready = n > 0 && ready(&fds[0]);
        let overdue = deadline.is_some_and(|d| Instant::now() >= d);
        if overdue && !(stdin_ready && prefix.wait() == Some(Wait::Sequence)) {
            deadline = None;
            // `timeout` may also turn a half-typed number into a switch, so
            // act on its steps.
            for step in prefix.timeout() {
                if let Some(outcome) = take_step(attachment, step)? {
                    return Ok(outcome);
                }
            }
        }

        if stdin_ready {
            match read_fd(stdin_fd, &mut buf) {
                Ok(0) => return Ok(Outcome::StdinClosed),
                Err(e) => return Err(NvmuxError::Io(e)),
                Ok(len) if leaving.is_some() => {
                    let Some(mut leave) = leaving else {
                        unreachable!("guarded");
                    };
                    match reply(&buf[..len], leave.detach) {
                        Some(Reply::Detach) => return Ok(Outcome::Detached),
                        Some(Reply::Save) => {
                            let live = config::get();
                            match config::save(&live, &config::saved(), leave.overwrite) {
                                Ok(_) => {
                                    config::set_saved((*live).clone());
                                    return Ok(Outcome::Detached);
                                }
                                Err(e) => {
                                    bar.ask(Some(leave.not_saved(&e)));
                                    leaving = Some(leave);
                                }
                            }
                        }
                        Some(Reply::Stay) => {
                            leaving = None;
                            bar.ask(None);
                        }
                        None => {}
                    }
                }
                Ok(len) => {
                    // Whether a floating panel came and went (keys may change).
                    let mut floated = false;
                    // Deliberately NOT logged: every keystroke, into a /tmp
                    // file that outlives the session.
                    for step in prefix.feed(&buf[..len]) {
                        if let Some(outcome) = take_step(attachment, step)? {
                            // `<prefix> ,`, `?`, `Space` or `c`: float the
                            // panel and return here, unless it cannot float or
                            // leads to the session loop.
                            let outcome = match float_over(attachment, master_fd, winch, outcome)? {
                                Some(outcome) => outcome,
                                None => {
                                    floated = true;
                                    continue;
                                }
                            };
                            // With unsaved settings, a detach asks first.
                            if outcome == Outcome::Detached {
                                let unsaved =
                                    crate::settings::changed(&config::get(), &config::saved())
                                        .len();
                                if unsaved > 0 {
                                    let leave = Leaving {
                                        detach: keys.bind.key(Action::Detach).unwrap_or(b'd'),
                                        overwrite: false,
                                    };
                                    bar.ask(Some(leave.question(unsaved)));
                                    leaving = Some(leave);
                                    break;
                                }
                            }
                            return Ok(outcome);
                        }
                    }
                    if floated {
                        keys = crate::config::get().keys;
                        prefix = Prefix::with_keys(highest_session_num, keys.prefix, keys.bind);
                        prefix_timeout = Duration::from_millis(keys.timeout_ms);
                        // A resize while they floated was theirs.
                        size = term::terminal_size();
                        bar = hint::Bar::new(Instant::now(), size, keys.bind);
                    }
                    // Every keystroke restarts the clock, so a number typed at
                    // a human pace is one number.
                    deadline = prefix.wait().map(|wait| {
                        Instant::now()
                            + match wait {
                                Wait::Command => prefix_timeout,
                                Wait::Sequence => SEQUENCE_TIMEOUT,
                            }
                    });
                }
            }
        }

        // The hint bar goes after the keys (earlier, it would appear a wake-up
        // late), the winch and the notice. Output or a held or just-released
        // paint means its row may be overdrawn; while the parser is tracked,
        // the bar goes back up in the very frame that drew over it. A command
        // ending the relay returned above, so `<prefix> Space` never shows it.
        let pass = hint::Pass {
            wrote: child_spoke || released || attachment.hold.is_some(),
            between: attachment
                .watching()
                .then(|| attachment.between_sequences()),
        };
        let now = Instant::now();
        let go = attachment.desk.as_ref().is_some_and(card::Desk::can_go);
        match bar.step(
            now,
            pass,
            size,
            prefix.pending(),
            go,
            attachment.shadow.as_ref(),
        ) {
            hint::Act::Idle => {}
            hint::Act::Write(bytes) => {
                // Going up: from now on, session writes over the row are
                // answered on the same pass, so even a fast-redrawn statusline
                // never shows through.
                if bar.is_up() {
                    attachment.watch_the_terminal();
                }
                attachment.write_over_session(&bytes)?;
            }
            hint::Act::Blanked(bytes) => {
                // Blanked: with no shadow, only the server knows what was under
                // the row. Request a repaint unless one is in flight.
                attachment.write_over_session(&bytes)?;
                if erasing.is_none() {
                    attachment.resize_to(size);
                    match Erasing::start(&attachment.sock) {
                        Ok(started) => erasing = Some(started),
                        Err(e) => {
                            tracing::debug!(error = %e, "no thread for the hint bar's repaint");
                            apply(attachment, Repaint::Notice, Served::NONE);
                        }
                    }
                }
            }
        }

        // With no overlay up (notice over, bar down), stop tracking the parser,
        // flushing anything held back for it.
        if popup.is_none() && !bar.is_up() {
            attachment.stop_watching_the_terminal()?;
        }

        // Release what was held back for the overlays: the session's own frame
        // close, else nvmux's span (a terminal inside either shows nothing).
        // After the bar, which is written after the keys: a frame closed before
        // it may be presented without it.
        attachment.release_the_sessions_close()?;
        attachment.close_span()?;

        // Cards: take the notifier's posts, tell the desk whether the prefix is
        // held, and send what waits.
        if n > 0 && ready(&fds[3]) {
            let posts = board.take();
            if let Some(desk) = attachment.desk.as_mut() {
                desk.take(posts);
            }
        }
        if let Some(desk) = attachment.desk.as_mut() {
            desk.keys(prefix.pending() == Some(Pending::Command));
        }
        attachment.send_cards(Instant::now());

        // Only on an idle tick: an exit shows through the pty above, so this
        // is for a *stopped* child; a `waitid` per keystroke buys nothing.
        if n == 0 && check_child(attachment) == ChildState::Gone {
            return Ok(Outcome::ChildExited);
        }
    }
}

/// `<prefix> d` with unsaved settings: the question on the bottom row, up
/// until answered (see [`reply`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Leaving {
    /// The detach key, which answers "detach anyway".
    detach: u8,
    /// The file changed what is being saved, so `w` overwrites it.
    overwrite: bool,
}

impl Leaving {
    fn question(&self, unsaved: usize) -> String {
        format!(
            "{unsaved} unsaved setting{} — w save and detach  {} detach anyway  esc stay",
            if unsaved == 1 { "" } else { "s" },
            keys::key_glyph(self.detach)
        )
    }

    /// The question again after a failed save, saying why: the file also
    /// changed these settings (a second `w` overwrites), or some other error.
    fn not_saved(&mut self, e: &crate::error::SaveError) -> String {
        let detach = keys::key_glyph(self.detach);
        self.overwrite = e.changed_in_file().is_some();
        if self.overwrite {
            format!("{e} — w overwrites and detaches  {detach} detach anyway  esc stay")
        } else {
            format!("{e} — {detach} detach anyway  esc stay")
        }
    }
}

/// A reply to the question [`Leaving`] asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reply {
    Save,
    Detach,
    Stay,
}

/// The first answer to [`Leaving`]'s question in a read of stdin: `w`, the
/// detach key, `Esc`, or a click (stays). Anything else is swallowed. Keys
/// may be kitty-protocol reports, whose releases (such as of the `d` that
/// asked) and repeats are skipped; clicks are SGR or X10 mouse reports.
fn reply(bytes: &[u8], detach: u8) -> Option<Reply> {
    let key = |code: u32| match code {
        27 => Some(Reply::Stay),
        c if c == u32::from(b'w') => Some(Reply::Save),
        c if c == u32::from(detach) => Some(Reply::Detach),
        _ => None,
    };
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        if rest[0] != keyseq::ESC {
            if let Some(reply) = key(u32::from(rest[0])).filter(|r| *r != Reply::Stay) {
                return Some(reply);
            }
            i += 1;
            continue;
        }
        if rest.len() == 1 {
            return Some(Reply::Stay);
        }
        let Some(len) = csi_len(rest) else {
            // Not a whole known sequence: nothing here answers.
            return None;
        };
        let seq = &rest[..len];
        if seq == b"\x1b[M" {
            // X10: button and coordinates follow, each a byte plus 32.
            let button = rest.get(len).map(|b| b.wrapping_sub(32));
            if button.is_some_and(|b| b & 0b11 != 3 && b & (32 | 64) == 0) {
                return Some(Reply::Stay);
            }
            i += len + 3;
            continue;
        }
        if let Some(button) = sgr_press(seq) {
            if button & 0b11 != 3 && button & (32 | 64) == 0 {
                return Some(Reply::Stay);
            }
        } else if let Some(reply) = kitty_press(seq).and_then(key) {
            return Some(reply);
        }
        i += len;
    }
    None
}

/// The length of the `CSI` sequence at the start of `bytes`, through its
/// final byte; `None` if there is not a whole one.
fn csi_len(bytes: &[u8]) -> Option<usize> {
    let body = bytes.strip_prefix(b"\x1b[")?;
    let end = body.iter().position(|b| (0x40..=0x7e).contains(b))?;
    Some(2 + end + 1)
}

/// The button of an SGR mouse report that is a press — `CSI < button ; x ;
/// y M` — or `None` for anything else, a release (`m`) included.
fn sgr_press(seq: &[u8]) -> Option<u32> {
    let body = seq.strip_prefix(b"\x1b[<")?.strip_suffix(b"M")?;
    let button = body.split(|b| *b == b';').next()?;
    std::str::from_utf8(button).ok()?.parse().ok()
}

/// The key of a kitty-protocol key report that is a press of it alone —
/// `CSI code [: alternates] [; modifiers [: event]] u`, with no modifier but
/// the locks and an event of 1 or none — or `None` for anything else.
fn kitty_press(seq: &[u8]) -> Option<u32> {
    let body = std::str::from_utf8(seq.strip_prefix(b"\x1b[")?.strip_suffix(b"u")?).ok()?;
    let mut fields = body.split(';');
    let code = fields.next()?.split(':').next()?.parse().ok()?;
    if let Some(modifiers) = fields.next() {
        let mut parts = modifiers.split(':');
        let held: u32 = parts.next()?.parse().ok()?;
        // One plus a bitmask; caps lock (64) and num lock (128) change nothing.
        if held.checked_sub(1)? & !(64 | 128) != 0 {
            return None;
        }
        if parts.next().is_some_and(|event| event != "1") {
            return None;
        }
    }
    Some(code)
}

/// Carry out one prefix-machine step: forward bytes, or map a command to its
/// [`Outcome`]. No `_` arm, so a new [`Action`] cannot be silently dropped.
fn act(writer: &mut dyn Write, step: Step) -> Result<Option<Outcome>> {
    Ok(match step {
        Step::Forward(bytes) => {
            writer.write_all(&bytes)?;
            writer.flush()?;
            None
        }
        Step::Act(Action::Picker) => Some(Outcome::ToPicker),
        Step::Act(Action::Detach) => Some(Outcome::Detached),
        Step::Act(Action::Create) => Some(Outcome::CreateNew),
        Step::Act(Action::Help) => Some(Outcome::ShowHelp),
        Step::Act(Action::Settings) => Some(Outcome::ShowSettings),
        Step::Act(Action::Switch(num)) => Some(Outcome::Switch(Target::Number(num))),
        Step::Act(Action::Cycle(dir)) => Some(Outcome::Switch(Target::Step(dir))),
        // Handled by the relay, which knows the cards (see `take_step`).
        Step::Act(Action::Go) => None,
    })
}

/// [`act`], plus the cards: forwarded bytes tell the desk when a card can go
/// in, and `<prefix> Enter` acts on the newest card.
fn take_step(attachment: &mut Attachment, mut step: Step) -> Result<Option<Outcome>> {
    attachment.saw_input(&mut step);
    if step == Step::Act(Action::Go) {
        return Ok(attachment.go(Instant::now()));
    }
    if let (Step::Forward(bytes), Some(desk)) = (&step, attachment.desk.as_mut()) {
        desk.forwarded.saw(bytes, Instant::now());
    }
    act(&mut attachment.master, step)
}

/// Float `outcome`'s panel over the session if it floats and `[style] float`
/// allows. `None` once it is taken down; else how the relay ends.
fn float_over(
    attachment: &mut Attachment,
    master_fd: RawFd,
    winch: &winch::Winch,
    outcome: Outcome,
) -> Result<Option<Outcome>> {
    attachment.to_float = None;
    match panel::What::of(outcome, config::get().style.float) {
        Some(what) => {
            let floated = panel::float(attachment, master_fd, winch, what);
            // However it went, drop what its frames were diffed against.
            if let Some(copy) = attachment.copy() {
                copy.forget_under();
            }
            floated
        }
        None => Ok(Some(outcome)),
    }
}

/// How long a sequence cut off by a read boundary waits for its rest.
/// Short: a bare `ESC` key is delayed this long (tmux's `escape-time`
/// defaults to 10 ms too).
const SEQUENCE_TIMEOUT: Duration = Duration::from_millis(10);

/// How often to check an idle child: a *stopped* one makes no fd readable
/// and sends no signal we watch.
const IDLE_POLL_MS: libc::c_int = 1000;

#[derive(Debug, PartialEq, Eq)]
enum ChildState {
    Running,
    Gone,
}

/// Notice a child that has exited, and continue one that has stopped:
/// `Ctrl-z` is forwarded as a byte, and a client that stops itself would
/// freeze the screen. Each stop is consumed when read, so this cannot loop.
/// Not `Child::try_wait`, which lacks `WUNTRACED` and so misses a stop.
fn check_child(attachment: &mut Attachment) -> ChildState {
    use nix::sys::signal::{kill, Signal};
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};

    let pid = child_pid(&attachment.child);

    match peek_child(pid) {
        Peek::Stopped => {
            // Consume only the stop notification (no reap), or every poll
            // would see it again.
            let sig = match waitpid(pid, Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED)) {
                Ok(WaitStatus::Stopped(_, sig)) => Some(sig),
                _ => None,
            };
            tracing::info!(?sig, "the client stopped; continuing it");
            let _ = kill(pid, Signal::SIGCONT);
            ChildState::Running
        }
        Peek::Gone => ChildState::Gone,
        Peek::Running => ChildState::Running,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Peek {
    Running,
    Stopped,
    Gone,
}

fn child_pid(child: &Child) -> nix::unistd::Pid {
    nix::unistd::Pid::from_raw(child.id() as i32)
}

/// Look at a child's state without changing it. `waitid`, since Linux's
/// wait4 rejects `WNOWAIT` with EINVAL, and through `libc` because nix lacks
/// it on macOS.
fn peek_child(pid: nix::unistd::Pid) -> Peek {
    // WNOHANG with nothing to report succeeds and leaves `si_signo` zero,
    // which is only detectable if the struct starts zeroed.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WNOHANG | libc::WEXITED | libc::WSTOPPED | libc::WNOWAIT;
    let rc = unsafe { libc::waitid(libc::P_PID, pid.as_raw() as libc::id_t, &mut info, flags) };
    if rc < 0 {
        return match nix::errno::Errno::last() {
            // Already reaped by someone else, which also means it is gone.
            nix::errno::Errno::ECHILD => Peek::Gone,
            _ => Peek::Running,
        };
    }
    if info.si_signo == 0 {
        return Peek::Running;
    }
    match info.si_code {
        libc::CLD_STOPPED | libc::CLD_TRAPPED => Peek::Stopped,
        libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED => Peek::Gone,
        _ => Peek::Running,
    }
}

/// A relayed write: the session's bytes, after a synchronized-update open
/// unless one is already `open` (a second would be a stray sequence).
fn framed(bytes: &[u8], open: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(fade::SYNC_BEGIN.len() + bytes.len());
    if !open {
        out.extend_from_slice(fade::SYNC_BEGIN);
    }
    out.extend_from_slice(bytes);
    out
}

/// Read whatever is available, once `poll` says the fd is ready.
pub(crate) fn read_fd(fd: RawFd, buf: &mut [u8]) -> std::io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// A `poll` entry for `fd` being readable; `poll` skips a negative `fd`.
pub(crate) fn pollfd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

/// Whether a write of at least one byte would not block (zero-timeout check).
fn writable(fd: RawFd) -> bool {
    let mut p = [libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    }];
    let n = unsafe { libc::poll(p.as_mut_ptr(), 1, 0) };
    n > 0 && p[0].revents & libc::POLLOUT != 0
}

/// Readable, or gone: on Linux the master reports `POLLHUP`, not `POLLIN`,
/// once the child exits.
pub(crate) fn ready(p: &libc::pollfd) -> bool {
    p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
}

/// Discard buffered child output, returning whether it held a DA1 request
/// (only [`Attachment::reap`] answers it).
fn discard_pending(fd: RawFd) -> bool {
    let mut buf = [0u8; 8192];
    let mut asked = false;
    loop {
        let mut p = [pollfd(fd)];
        let n = unsafe { libc::poll(p.as_mut_ptr(), 1, 0) };
        if n <= 0 || !ready(&p[0]) {
            return asked;
        }
        match read_fd(fd, &mut buf) {
            Ok(len) if len > 0 => {
                asked = asked || asks_for_device_attributes(&buf[..len]);
                continue;
            }
            _ => return asked,
        }
    }
}

/// Whether this output contains a DA1 request. A split one is missed, which
/// only costs a second; Neovim's shutdown sequence comes in one write.
fn asks_for_device_attributes(chunk: &[u8]) -> bool {
    memmem::find(chunk, DA1_REQUEST).is_some()
}

/// The range, rows and cols of the first in-band size report
/// (`CSI 48 ; rows ; cols ; height ; width t`) in terminal input.
fn find_size_report(bytes: &[u8]) -> Option<(std::ops::Range<usize>, u16, u16)> {
    const START: &[u8] = b"\x1b[48;";
    let at = memmem::find(bytes, START)?;
    let params = &bytes[at + START.len()..];
    let len = params
        .iter()
        .take_while(|b| b.is_ascii_digit() || **b == b';')
        .count();
    if params.get(len) != Some(&b't') {
        return None;
    }
    let mut fields = std::str::from_utf8(&params[..len]).ok()?.split(';');
    let rows = fields.next()?.parse().ok()?;
    let cols = fields.next()?.parse().ok()?;
    Some((at..at + START.len() + len + 1, rows, cols))
}

/// Whether terminal input carries a DA1 answer, `CSI ? Ps ; … c`, the last
/// reply to a client's startup queries.
fn answers_device_attributes(bytes: &[u8]) -> bool {
    const START: &[u8] = b"\x1b[?";
    memmem::find_iter(bytes, START).any(|at| {
        let rest = &bytes[at + START.len()..];
        let params = rest
            .iter()
            .take_while(|b| b.is_ascii_digit() || **b == b';')
            .count();
        params > 0 && rest.get(params) == Some(&b'c')
    })
}

/// The chunk with every DA1 request removed: exactly `ESC [ c`, which
/// neither the reply nor DA2 contains.
fn without_device_attributes_request(chunk: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    without(chunk, DA1_REQUEST)
}

/// Repaint after one of nvmux's own screens cleared the terminal: `:mode`
/// via RPC (see [`repaint_through_server`]), the only way that reaches a
/// client ignoring `SIGWINCH` (DEC mode 2048), or failing that a [`nudge`].
/// The pty is resized first. On a resume the mouse mode, which the client
/// sets only at startup, is restored as the server reports it, or Neovim's
/// default from a busy one.
fn repaint(attachment: &mut Attachment, why: Repaint) {
    attachment.resize_to(term::terminal_size());
    let served = repaint_through_server(&attachment.sock, why);
    apply(attachment, why, served);
}

/// Act on a repaint's answer: the fallback nudge if it failed, and on resume
/// the reported mouse mode. Runs on the relay thread, which owns `resize`.
fn apply(attachment: &mut Attachment, why: Repaint, served: Served) {
    if why == Repaint::Resume {
        term::set_mouse_reporting(served.mouse.unwrap_or(term::MouseReporting::Buttons));
    }
    if !served.repainted {
        // The shrink and its undo are not shown to the shadow: what the
        // client draws after them is for the size it was just told.
        nudge(&attachment.master, term::terminal_size());
    }
}

/// Show terminal buffer `buf` in the session on `sock`, for `<prefix> Enter`
/// on its card: switch to a window showing it, in its tab page, or open it in
/// a new tab page. Best effort, from a thread, and skipped for an editor
/// waiting for a key.
fn show_buffer(sock: PathBuf, buf: i64) {
    const SHOW: &str = r#"local buf = ...
if not vim.api.nvim_buf_is_valid(buf) then
  return false
end
for _, tab in ipairs(vim.api.nvim_list_tabpages()) do
  for _, win in ipairs(vim.api.nvim_tabpage_list_wins(tab)) do
    if vim.api.nvim_win_get_buf(win) == buf then
      vim.api.nvim_set_current_tabpage(tab)
      vim.api.nvim_set_current_win(win)
      return true
    end
  end
end
vim.cmd('tab sbuffer ' .. buf)
return true
"#;
    let spawned = std::thread::Builder::new()
        .name("nvmux-show".into())
        .spawn(move || {
            let shown = rpc::Client::connect(&sock, RESUME_TIMEOUT).and_then(|mut client| {
                if client.get_mode()?.blocking {
                    return Ok(false);
                }
                client.call(
                    "nvim_exec_lua",
                    vec![
                        rmpv::Value::from(SHOW),
                        rmpv::Value::Array(vec![rmpv::Value::from(buf)]),
                    ],
                )?;
                Ok(true)
            });
            if let Err(e) = shown {
                tracing::debug!(error = %e, "card: could not show the terminal");
            }
        });
    if let Err(e) = spawned {
        tracing::debug!(error = %e, "card: no thread to show the terminal on");
    }
}

/// Ask a returning kept client's server for its exact screen from a thread,
/// as [`Repaint::Notice`] (no prompt answered); the copy covers meanwhile.
/// `None` if no thread, after the fallback.
fn kept_clients_repaint(attachment: &mut Attachment) -> Option<Erasing> {
    match Erasing::start_for(&attachment.sock, "kept client's return") {
        Ok(started) => Some(started),
        Err(e) => {
            tracing::debug!(error = %e, "no thread for the kept client's repaint");
            apply(attachment, Repaint::Notice, Served::NONE);
            None
        }
    }
}

/// The repaint that removes the attach notice, on its own thread: two round
/// trips (measured 123 ms over a 60 ms link, 303 ms over 150 ms) would stall
/// the relay. The repaint arrives as session output; the answer only decides
/// the fallback. Also used for the hint bar and a kept client's return.
struct Erasing {
    rx: mpsc::Receiver<Served>,
}

impl Erasing {
    /// Ask, and come back for the answer later.
    fn start(sock: &Path) -> std::io::Result<Self> {
        Self::start_for(sock, "attach notice erase")
    }

    /// [`Erasing::start`] for something other than the notice, e.g. a returning
    /// kept client (see [`relay`]); `what` names it in the timing record.
    fn start_for(sock: &Path, what: &'static str) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let sock = sock.to_path_buf();
        let asked = Instant::now();
        std::thread::Builder::new()
            .name("nvmux-notice-repaint".into())
            .spawn(move || {
                let served = repaint_through_server(&sock, Repaint::Notice);
                // Timed here: a kept client's answer is collected only after
                // the fade.
                tracing::debug!(
                    ms = asked.elapsed().as_secs_f64() * 1000.0,
                    repainted = served.repainted,
                    "timing: {} (:mode repaint)",
                    what
                );
                // A dropped receiver means the relay ended first.
                let _ = tx.send(served);
            })?;
        Ok(Self { rx })
    }

    /// The verdict, if it has arrived. A worker that ended without one gives
    /// [`Served::NONE`], so the fallback runs.
    fn answer(&self) -> Option<Served> {
        match self.rx.try_recv() {
            Ok(served) => Some(served),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Served::NONE),
        }
    }
}

/// How often the relay checks for [`Erasing`]'s answer, bounding how late a
/// fallback nudge can be: a server too busy to answer produces no output to
/// wake the loop.
const ERASE_POLL: Duration = Duration::from_millis(50);

/// Why the screen is being repainted, which decides what the repaint may do.
/// A resume may end a hit-enter prompt (drawn over, so the user cannot
/// answer it) and asks for the mouse setting. Removing the notice, or the
/// hint bar without a shadow, may do neither: the user may be reading that
/// prompt (likely a startup error). A [`nudge`] stands in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Repaint {
    Resume,
    Notice,
}

/// What [`repaint_through_server`] got out of the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Served {
    /// The server was asked to repaint, and did.
    repainted: bool,
    /// The client's mouse setting, where the server could be asked for it.
    mouse: Option<term::MouseReporting>,
}

impl Served {
    /// Nothing was served: the server could not be asked.
    const NONE: Served = Served {
        repainted: false,
        mouse: None,
    };
}

/// The client's mouse reporting from its server's answer to
/// `[&mouse, &mousemoveevent]`, as Neovim's TUI derives it; `None` if the
/// answer is not that pair. The editor's mode is ignored, so resuming on the
/// command line enables buttons slightly early, until the next mode change.
fn mouse_reporting(answer: &rmpv::Value) -> Option<term::MouseReporting> {
    let pair = answer.as_array()?;
    let mouse = pair.first()?.as_str()?;
    let moves = pair.get(1)?;
    let moves = moves.as_bool().or_else(|| moves.as_i64().map(|n| n != 0))?;
    Some(if mouse.is_empty() {
        term::MouseReporting::Off
    } else if moves {
        term::MouseReporting::Motion
    } else {
        term::MouseReporting::Buttons
    })
}

/// Shrink the pty by a row and restore it, so the client asks for a resize,
/// which the server answers with a full redraw once its loop is free. For a
/// server that cannot be asked over RPC; ignored by a client using in-band
/// resize reports.
fn nudge(master: &Pty, size: PtySize) {
    let nudged = PtySize {
        rows: size.rows.saturating_sub(1).max(1),
        ..size
    };
    let _ = master.resize(nudged.into());
    let _ = master.resize(size.into());
}

/// The size the pty was last told, read back with `TIOCGWINSZ`; `None` for
/// a pty that will not say.
fn pty_size(master: &Pty) -> Option<PtySize> {
    // SAFETY: zero is a valid `winsize`, and the kernel fills it in.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let read = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) };
    (read == 0).then_some(PtySize {
        rows: ws.ws_row,
        cols: ws.ws_col,
        pixel_width: ws.ws_xpixel,
        pixel_height: ws.ws_ypixel,
    })
}

/// How long a resume waits on the server. `nvim_get_mode` answers within a
/// ms if at all; `:mode` can take a few hundred ms with a heavy config
/// (measured: 384 ms with AstroNvim).
const RESUME_TIMEOUT: Duration = Duration::from_secs(1);

/// Ask the server over RPC to repaint the whole screen and, on a resume, for
/// the client's mouse setting. A server at a hit-enter prompt runs no
/// events, so on a resume the prompt is first ended with `<CR>`; other
/// key-waiting states are left alone (`<CR>` would scroll a more-prompt or
/// complete an operator). `:mode` clears and redraws, unlike `:redraw!`; it
/// also resets pending-message state, and ending the prompt discards its
/// text from every UI (`:messages` keeps it).
fn repaint_through_server(sock: &Path, why: Repaint) -> Served {
    let mut client = match rpc::Client::connect(sock, RESUME_TIMEOUT) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "resume: could not reach the server");
            return Served::NONE;
        }
    };
    let mode = match client.get_mode() {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(error = %e, "resume: the server is busy; nudge only");
            return Served::NONE;
        }
    };
    if mode.blocking && !(mode.at_hit_enter() && why == Repaint::Resume) {
        tracing::debug!(
            mode = %mode.mode,
            "resume: the server is waiting for a key nvmux must not type; nudge only"
        );
        return Served::NONE;
    }
    if mode.at_hit_enter() {
        tracing::debug!("resume: ending the hit-enter prompt");
        if let Err(e) = client.input("<CR>") {
            tracing::debug!(error = %e, "resume: could not end the prompt; nudge only");
            return Served::NONE;
        }
    }
    // Before the repaint, which can take a few hundred ms: the mouse answer
    // is wanted even if that times out.
    let mouse = match why {
        Repaint::Resume => match client.eval("[&mouse, &mousemoveevent]") {
            Ok(answer) => mouse_reporting(&answer),
            Err(e) => {
                tracing::debug!(error = %e, "resume: could not ask about the mouse");
                None
            }
        },
        Repaint::Notice => None,
    };
    let repainted = match client.command("mode") {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!(error = %e, "resume: gave up waiting for the repaint; nudging");
            false
        }
    };
    Served { repainted, mouse }
}

#[cfg(test)]
mod tests {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    use super::*;
    use crate::keys::Direction;

    /// Poll until the child reaches `want` (signals are asynchronous). Returns
    /// what it last saw, so callers assert on the state, not on a timeout.
    fn settle(pid: Pid, want: Peek) -> Peek {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let got = peek_child(pid);
            if got == want || Instant::now() > deadline {
                return got;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The shadow must keep what the client drew under an overlay, the only
    /// copy left; the bar's erase is composited from it too. Fed the box,
    /// `has_contents` would also release a hold before the client drew.
    #[test]
    fn nothing_written_over_the_session_reaches_the_shadows_grid() {
        let mut a = attached_to("exit 0");
        let mut shadow = shadow::Shadow::new(8, 40);
        shadow.feed(b"\x1b[4;1Hthe editor drew this");
        a.shadow = Some(shadow);
        assert!(a.drawn(), "the fixture put nothing on the screen");

        let over = announce::overlay(
            "dotfiles",
            PtySize {
                rows: 8,
                cols: 40,
                ..PtySize::default()
            },
        )
        .expect("a box");
        a.write_over_session(&announce::plain_bytes(&over, None))
            .expect("write");

        let colours = crate::test_support::palette();
        let grid = a.shadow.as_mut().expect("a shadow");
        let painted = grid.frame(0.0, &colours, shadow::Cursor::Hidden);
        let frame = String::from_utf8_lossy(&painted).to_string();
        assert!(
            frame.contains("the editor drew this"),
            "the grid lost the editor's screen: {frame:?}"
        );
        assert!(!frame.contains('╭'), "the grid took the box: {frame:?}");
    }

    /// However reads fall, nvmux leaves the terminal between the session's
    /// sequences; 7-byte chunks split sequences often.
    #[test]
    fn a_relayed_frame_leaves_the_terminal_between_the_sessions_sequences() {
        let mut a = attached_to("exit 0");
        a.boundary = Some(boundary::Boundary::new());
        let now = Instant::now();
        let frame = b"\x1b[1;1H\x1b[38;2;1;2;3mhello\x1b[2;1H\x1b[0m\xe2\x96\x80\x1b[?25h";
        for chunk in frame.chunks(7) {
            a.relay_output(chunk, now).expect("relay");
            assert!(
                a.between_sequences(),
                "a read left the terminal inside one of the session's sequences"
            );
        }

        // The bytes held back for that are released once the notice is over.
        a.stop_watching_the_terminal().expect("release");
        assert!(a.kept_due().is_none(), "bytes outlived the notice");
        assert!(
            !a.between_sequences(),
            "nvmux is still watching a terminal it has nothing to draw on"
        );
    }

    /// A terminal left inside a span shows nothing until its own timeout.
    #[test]
    fn a_synchronized_update_never_outlives_the_pass_that_opened_it() {
        let mut a = attached_to("exit 0");
        a.boundary = Some(boundary::Boundary::new());
        a.relay_output(b"\x1b[1;1Hhello", Instant::now())
            .expect("relay");
        assert!(a.span, "the session's repaint was not held back at all");

        a.close_span().expect("close");
        assert!(!a.span);
        // Closing a closed span is a no-op, so every exit path may call it.
        a.close_span().expect("close");
        assert!(!a.span);
    }

    /// The span's reset, written between a UTF-8 lead byte and its
    /// continuation, would corrupt the character, so the span stays open.
    #[test]
    fn a_span_is_not_closed_through_the_middle_of_a_character() {
        let mut a = attached_to("exit 0");
        a.boundary = Some(boundary::Boundary::new());
        let t0 = Instant::now();
        // Two thirds of `▀`, kept back for the third that has not arrived.
        a.relay_output(b"\x1b[1;1Hab\xe2\x96", t0).expect("relay");
        a.close_span().expect("close");
        assert!(!a.span, "a whole write was left holding the terminal");

        // The session says no more, so the two bytes go out unfinished.
        a.relay_output(b"", t0 + boundary::KEPT_FOR).expect("relay");
        assert!(a.span, "the unfinished tail went out unwrapped");
        a.close_span().expect("close");
        assert!(
            a.span,
            "the reset was written between a lead byte and its continuation"
        );

        // And it closes the moment the character is whole again.
        a.relay_output(b"\x80", t0 + boundary::KEPT_FOR)
            .expect("relay");
        a.close_span().expect("close");
        assert!(!a.span, "the span outlived what was holding it open");
    }

    /// A pass finding the span open adds to it: a second open would land in
    /// the same unfinished character.
    #[test]
    fn a_relayed_write_opens_a_span_once_however_many_passes_it_takes() {
        assert_eq!(
            framed(b"\x1b[1;1Hab", false),
            [fade::SYNC_BEGIN, b"\x1b[1;1Hab"].concat(),
            "a write with no span open was not put inside one"
        );
        assert_eq!(
            framed(b"\x80", true),
            b"\x80",
            "a second open was written into the character the first is holding"
        );
    }

    /// With no next pass after the notice, the span closes whatever the
    /// session is in the middle of.
    #[test]
    fn a_span_does_not_outlive_the_notice_that_opened_it() {
        let mut a = attached_to("exit 0");
        a.boundary = Some(boundary::Boundary::new());
        let t0 = Instant::now();
        a.relay_output(b"\x1b[1;1Hab\xe2\x96", t0).expect("relay");
        a.relay_output(b"", t0 + boundary::KEPT_FOR).expect("relay");
        assert!(a.span, "nothing to close");

        a.stop_watching_the_terminal().expect("stop");
        assert!(!a.span, "the terminal was left holding a frame for ever");
    }

    /// `?2026` is a mode, not a count, so nvmux's span around the client's own
    /// would end at the client's reset and present the frame early.
    #[test]
    fn a_session_that_brackets_its_own_frames_is_left_to_do_it() {
        let mut a = attached_to("exit 0");
        a.boundary = Some(boundary::Boundary::new());
        let now = Instant::now();
        a.relay_output(b"\x1b[?2026h\x1b[1;1Hhello\x1b[?2026l", now)
            .expect("relay");
        assert!(!a.span, "nvmux opened a span inside the client's own");
        a.relay_output(b"\x1b[2;1Hmore", now).expect("relay");
        assert!(
            !a.span,
            "one frame of its own was enough; nvmux went back to wrapping"
        );
    }

    /// While the bar is up, a session frame's close waits for it. A second
    /// watch (here mid-character) keeps the existing parser.
    #[test]
    fn the_bar_has_the_sessions_frames_wait_for_it_while_it_is_up() {
        let mut a = attached_to("exit 0");
        assert!(!a.watching(), "followed with nothing on the screen");
        a.watch_the_terminal();
        assert!(
            a.between_sequences(),
            "a fresh parser starts between sequences"
        );

        let now = Instant::now();
        a.relay_output(b"\x1b[?2026h\x1b[40;1Hbuilding... 40%", now)
            .expect("relay");
        assert!(
            !a.between_sequences(),
            "the bar would go into the middle of the session's frame"
        );
        a.relay_output(b"\x1b[?2026l", now).expect("relay");
        assert!(
            a.between_sequences(),
            "the frame went to the terminal without the bar in it"
        );
        a.release_the_sessions_close().expect("close");

        // Two thirds of `▀`, kept back, and then let go unfinished.
        a.relay_output(b"\x1b[40;1H\xe2\x96", now).expect("relay");
        a.relay_output(b"", now + boundary::KEPT_FOR)
            .expect("relay");
        assert!(a.mid_sequence(), "the fixture left the character whole");
        a.watch_the_terminal();
        assert!(
            a.mid_sequence(),
            "a second watch forgot the character the first was waiting on"
        );

        a.stop_watching_the_terminal().expect("stop");
        assert!(!a.watching(), "still followed with the bar down");
        assert!(a.kept_due().is_none(), "bytes outlived the bar");
    }

    /// A held first paint has not reached the terminal, so the box has nothing
    /// to sit on, whatever the boundary says.
    #[test]
    fn nothing_goes_over_a_first_paint_that_is_still_held() {
        let mut a = attached_to("exit 0");
        a.boundary = Some(boundary::Boundary::new());
        assert!(a.between_sequences());
        a.hold = Some(Hold::new(Instant::now()));
        assert!(
            !a.between_sequences(),
            "the box would go up over a screen the editor has not been let paint"
        );
    }

    /// A stand-in client on a real pty running `sh -c $script`: the teardown
    /// tests are about what a *signal* does to a process.
    fn attached_to(script: &str) -> Attachment {
        let cmd = Command::new("sh").arg("-c").arg(script);
        let mut a = spawn_client_with(
            "id000000",
            Path::new("/nvmux-test-never-read.sock"),
            "",
            cmd,
            false,
        )
        .expect("spawn sh");
        // No announce, shadow or ledger unless a test installs them, so the
        // fixture ignores the fade globals and `[client] per_session`.
        a.announce = None;
        a.shadow = None;
        a.screen = None;
        a.ledger = None;
        a
    }

    /// Wait for a stand-in to echo `r`, meaning its traps are set; a signal
    /// sent sooner meets `sh`'s default disposition and the test passes for
    /// the wrong reason.
    fn wait_until_ready(a: &Attachment) {
        assert!(
            wait_for(a, b'r'),
            "the stand-in client never reported ready"
        );
    }

    /// Read the stand-in's output until `byte` shows up, or give up.
    fn wait_for(a: &Attachment, byte: u8) -> bool {
        let fd = a.master.as_raw_fd();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 256];
        while Instant::now() < deadline {
            let mut p = [pollfd(fd)];
            // SAFETY: one initialised pollfd, and the fd outlives the call.
            unsafe { libc::poll(p.as_mut_ptr(), 1, 50) };
            if ready(&p[0]) {
                if let Ok(n) = read_fd(fd, &mut buf) {
                    if buf[..n].contains(&byte) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn pid_of(a: &Attachment) -> Pid {
        child_pid(&a.child)
    }

    /// On Linux the master reports `POLLHUP`, not `POLLIN`, once the child is
    /// gone; ignoring it would spin the loop on a dead pty.
    #[test]
    fn readiness_counts_hangup_and_error_not_just_input() {
        let mk = |revents| libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents,
        };
        assert!(ready(&mk(libc::POLLIN)));
        assert!(ready(&mk(libc::POLLHUP)), "a hangup must wake the loop");
        assert!(ready(&mk(libc::POLLERR)), "an error must wake the loop");
        assert!(ready(&mk(libc::POLLIN | libc::POLLHUP)));
        assert!(!ready(&mk(0)), "nothing pending is not readiness");
        assert!(
            !ready(&mk(libc::POLLOUT)),
            "writability alone is not what this loop waits for"
        );
    }

    #[test]
    fn forwarded_bytes_reach_the_writer_and_end_nothing() {
        let mut out = Vec::new();
        let r = act(&mut out, Step::Forward(b"hello".to_vec())).expect("write");
        assert_eq!(r, None);
        assert_eq!(out, b"hello");
    }

    #[test]
    fn the_question_before_detaching_is_answered_by_its_keys_alone() {
        let d = b'd';
        for (bytes, want) in [
            (&b"w"[..], Some(Reply::Save)),
            (b"d", Some(Reply::Detach)),
            (b"\x1b", Some(Reply::Stay)),
            (b"x", None),
            (b"xyz", None),
            // kitty: Esc, a release of the `d` that asked, a press of `w`.
            (b"\x1b[27u", Some(Reply::Stay)),
            (b"\x1b[100;1:3u", None),
            (b"\x1b[100;1:3uw", Some(Reply::Save)),
            (b"\x1b[119u", Some(Reply::Save)),
            (b"\x1b[119;5u", None),
            (b"\x1b[100;65u", Some(Reply::Detach)),
            // SGR: a press stays; a release, motion and the wheel do not.
            (b"\x1b[<0;10;5M", Some(Reply::Stay)),
            (b"\x1b[<0;10;5m", None),
            (b"\x1b[<32;10;5M", None),
            (b"\x1b[<64;10;5M", None),
            // X10: a press stays, and its coordinates are not keys.
            (b"\x1b[M !!", Some(Reply::Stay)),
            (b"\x1b[M#ww", None),
            // Cut off: nothing yet.
            (b"\x1b[", None),
        ] {
            assert_eq!(
                reply(bytes, d),
                want,
                "{:?}",
                String::from_utf8_lossy(bytes)
            );
        }
        assert_eq!(
            reply(b"x", b'x'),
            Some(Reply::Detach),
            "the detach key as moved"
        );
        assert_eq!(reply(b"d", b'x'), None);
    }

    #[test]
    fn the_question_names_the_detach_key() {
        let leave = Leaving {
            detach: b' ',
            overwrite: false,
        };
        assert_eq!(
            leave.question(2),
            "2 unsaved settings — w save and detach  ␣ detach anyway  esc stay"
        );
        let mut leave = Leaving {
            detach: b'd',
            overwrite: false,
        };
        let clash = crate::error::SaveError::Changed {
            path: "/x/config.toml".into(),
            settings: vec!["client.lazy".into()],
        };
        assert_eq!(
            leave.not_saved(&clash),
            "client.lazy changed in config.toml too — w overwrites and detaches  d detach anyway  esc stay"
        );
        assert!(leave.overwrite);
        assert!(leave
            .not_saved(&crate::error::SaveError::Nowhere)
            .ends_with("— d detach anyway  esc stay"));
        assert!(!leave.overwrite);
    }

    /// Told first among the opening commands, and only once.
    #[test]
    fn a_relay_opens_by_telling_a_stale_client_the_settings() {
        let mut a = attached_to("exec sleep 30");
        a.draws_cards = true;
        a.heard = crate::config::generation().wrapping_add(1);
        a.open_desk();
        assert_eq!(
            a.opening.first(),
            Some(&card::Command::Settings(crate::config::get().drawing()))
        );
        assert_eq!(a.heard, crate::config::generation());
        a.open_desk();
        assert!(
            !a.opening
                .iter()
                .any(|c| matches!(c, card::Command::Settings(_))),
            "told twice"
        );
    }

    #[test]
    fn every_action_maps_to_its_outcome() {
        let mut out = Vec::new();
        let cases = [
            (Action::Picker, Outcome::ToPicker),
            (Action::Detach, Outcome::Detached),
            (Action::Create, Outcome::CreateNew),
            (Action::Help, Outcome::ShowHelp),
            (Action::Settings, Outcome::ShowSettings),
            (Action::Switch(7), Outcome::Switch(Target::Number(7))),
            (
                Action::Cycle(Direction::Next),
                Outcome::Switch(Target::Step(Direction::Next)),
            ),
            (
                Action::Cycle(Direction::Prev),
                Outcome::Switch(Target::Step(Direction::Prev)),
            ),
        ];
        for (action, want) in cases {
            let got = act(&mut out, Step::Act(action)).expect("no io");
            assert_eq!(got, Some(want), "{action:?}");
        }
        assert!(out.is_empty(), "a command writes nothing to the child");
    }

    /// A lull before anything is drawn is a client between its queries and its
    /// grid, so it does not end the hold.
    #[test]
    fn a_hold_waits_for_a_drawn_screen_and_then_a_lull() {
        let t0 = Instant::now();
        let mut hold = Hold::new(t0);
        assert!(
            !hold.due(t0 + Duration::from_secs(5), true),
            "nothing written yet"
        );
        assert_eq!(hold.wake_at(true), None);

        hold.take(b"\x1b[?1049h\x1b[c", t0 + Duration::from_millis(10));
        let quiet = t0 + Duration::from_millis(10) + HOLD_SETTLE;
        assert!(!hold.due(quiet, false), "quiet, but nothing drawn");
        assert!(hold.due(quiet, true), "quiet and drawn");
        assert_eq!(hold.wake_at(true), Some(quiet));
        // Undrawn: only the cap is worth waking for.
        assert_eq!(
            hold.wake_at(false),
            Some(t0 + Duration::from_millis(10) + HOLD_CAP)
        );

        // Every chunk restarts the lull.
        hold.take(b"~", t0 + Duration::from_millis(30));
        assert!(!hold.due(quiet, true));
        assert!(hold.due(t0 + Duration::from_millis(30) + HOLD_SETTLE, true));
    }

    /// No panel frame may land on the primary screen the shell returns to.
    /// With no copy it is back at once (the panel reports the problem).
    #[test]
    fn settings_turned_to_float_wait_for_the_sessions_screen() {
        let mut a = attached_to("exit 0");
        assert!(a.screen_is_back(), "no copy: the panel's to say");

        // A fresh client's copy as the relay begins: primary screen, cleared.
        let mut copy = shadow::Shadow::new(8, 40);
        copy.feed(term::HANDOVER);
        a.shadow = Some(copy);
        assert!(!a.screen_is_back(), "on the primary screen, nothing drawn");
        a.shadow_saw(b"\x1b[?1049h");
        assert!(a.screen_is_back(), "on its alternate screen");
        a.hold = Some(Hold::new(Instant::now()));
        assert!(!a.screen_is_back(), "its first paint still held");
        a.hold = None;

        // A client that draws on the primary screen, with no shadow.
        let mut copy = shadow::Shadow::new(8, 40);
        copy.feed(term::HANDOVER);
        a.shadow = None;
        a.screen = Some(copy);
        assert!(!a.screen_is_back());
        a.shadow_saw(b"\x1b[1;1H$ ");
        assert!(a.screen_is_back(), "something drawn on the primary screen");
    }

    #[test]
    fn a_hold_is_released_at_the_cap_or_by_size() {
        let t0 = Instant::now();
        let mut hold = Hold::new(t0);
        hold.take(b"x", t0);
        for ms in (0..700).step_by(10) {
            hold.take(b"x", t0 + Duration::from_millis(ms));
        }
        assert!(!hold.due(t0 + Duration::from_millis(700), false));
        assert!(
            hold.due(t0 + HOLD_CAP, false),
            "the cap releases an undrawn screen too"
        );
        assert_eq!(
            hold.wake_at(false),
            Some(t0 + HOLD_CAP),
            "undrawn, only the cap is worth waking for"
        );
        assert_eq!(
            hold.wake_at(true),
            Some(t0 + Duration::from_millis(690) + HOLD_SETTLE),
            "drawn, the lull after the last chunk comes before the cap"
        );

        let mut big = Hold::new(t0);
        big.take(&vec![b' '; HOLD_MAX], t0);
        assert!(big.due(t0, false));
    }

    /// So the fade frames land on the screen the client is about to draw on.
    #[test]
    fn the_release_splits_just_past_the_alternate_screen_entry() {
        let bytes = b"\x1b[?1004h\x1b[?1049h\x1b[2J~";
        let at = split_at_alt_screen_entry(bytes);
        assert_eq!(&bytes[..at], b"\x1b[?1004h\x1b[?1049h");
        assert_eq!(&bytes[at..], b"\x1b[2J~");
        assert_eq!(split_at_alt_screen_entry(b"\x1b[2J~"), 0);
        assert_eq!(split_at_alt_screen_entry(b""), 0);
    }

    #[test]
    fn what_is_let_through_is_one_update() {
        let frames = b"\x1b[?2026h\x1b[1;1Ha\x1b[?2026l\x1b[?2026h\x1b[1;1Hb\x1b[?2026l~";
        assert_eq!(
            &*unsynced(frames),
            b"\x1b[?2026h\x1b[1;1Ha\x1b[?2026h\x1b[1;1Hb~"
        );
        // At either end, and back to back.
        assert_eq!(&*unsynced(b"\x1b[?2026l\x1b[?2026la\x1b[?2026l"), b"a");
        assert!(
            matches!(unsynced(b"\x1b[2J~"), std::borrow::Cow::Borrowed(_)),
            "nothing to take out"
        );
        assert_eq!(&*unsynced(b""), b"");
    }

    /// What a terminal shows of `bytes`: its first rows, a row to a line.
    fn shown(bytes: &[u8]) -> Vec<String> {
        let mut p = vt100::Parser::new(4, 40, 0);
        p.process(bytes);
        p.screen()
            .contents()
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect()
    }

    #[test]
    fn a_finished_first_paint_is_let_through_in_one_update() {
        let paint = b"\x1b[?2026h\x1b[1;1Hone\x1b[?2026l\x1b[?2026h\x1b[2;1Htwo\x1b[?2026l";
        let out = replay(paint);
        assert!(out.starts_with(fade::SYNC_BEGIN) && out.ends_with(fade::SYNC_END));
        assert_eq!(memmem::find_iter(&out, fade::SYNC_END).count(), 1);
        assert_eq!(shown(&out)[..2], ["one", "two"]);
    }

    /// Released mid-frame (here mid-colour), the update closes before that
    /// frame, which follows still open for the client to finish.
    #[test]
    fn a_first_paint_let_go_part_way_through_a_frame_is_closed_in_front_of_it() {
        let done = b"\x1b[?2026h\x1b[1;1Hdrawn\x1b[?2026l";
        let open = b"\x1b[?2026h\x1b[2;1H\x1b[38;2;207;2";
        let rest = b"07;207mnext\x1b[?2026l";
        let out = replay(&[&done[..], &open[..]].concat());
        assert_eq!(
            shown(&[&out[..], &rest[..]].concat())[..2],
            ["drawn", "next"]
        );
        let close = memmem::find(&out, fade::SYNC_END).expect("closed");
        assert_eq!(&out[close + fade::SYNC_END.len()..], &open[..]);
    }

    /// With no client frame open, it closes before the partial sequence or
    /// character.
    #[test]
    fn a_first_paint_let_go_part_way_through_a_sequence_is_closed_in_front_of_it() {
        for (paint, rest, row) in [
            (
                &b"\x1b[1;1Hdrawn \x1b[38;2;20"[..],
                &b"7;0;0mon"[..],
                "drawn on",
            ),
            (
                &b"\x1b[1;1Hdrawn \xe2\x94"[..],
                &b"\x82on"[..],
                "drawn \u{2502}on",
            ),
        ] {
            let out = replay(paint);
            let close = memmem::find(&out, fade::SYNC_END).expect("closed");
            assert!(
                paint.ends_with(&out[close + fade::SYNC_END.len()..]),
                "{row}: what follows the close is the client's unfinished tail"
            );
            assert_eq!(shown(&[&out[..], rest].concat())[0], row);
        }
    }

    /// Every variant is checked, so a new one must decide rather than silently
    /// start nothing.
    #[test]
    fn only_a_switch_names_a_session_to_begin_early() {
        for target in [Target::Number(3), Target::Step(Direction::Next)] {
            assert_eq!(Outcome::Switch(target).switch_target(), Some(target));
        }
        let id = crate::ids::Id::new("abcd2345").expect("an id");
        for other in [
            Outcome::ToPicker,
            Outcome::CreateNew,
            Outcome::CreateAfter(Some(id)),
            Outcome::CreateAfter(None),
            Outcome::Rename(id),
            Outcome::ShowHelp,
            Outcome::ShowSettings,
            Outcome::Resettle,
            Outcome::Detached,
            Outcome::ChildExited,
            Outcome::StdinClosed,
        ] {
            assert_eq!(
                other.switch_target(),
                None,
                "{other:?} has no session to begin an attachment for"
            );
        }
    }

    /// Too narrow leaves the old session on screen through the spawn; too wide
    /// clears a screen about to draw anyway.
    #[test]
    fn only_a_switch_reaches_the_next_session_without_a_screen() {
        for target in [Target::Number(3), Target::Step(Direction::Next)] {
            assert!(Outcome::Switch(target).leads_straight_into_another_relay());
        }
        let id = crate::ids::Id::new("abcd2345").expect("an id");
        for other in [
            Outcome::ToPicker,
            Outcome::CreateNew,
            Outcome::CreateAfter(Some(id)),
            Outcome::CreateAfter(None),
            Outcome::Rename(id),
            Outcome::ShowHelp,
            Outcome::ShowSettings,
            Outcome::Resettle,
            Outcome::Detached,
            Outcome::ChildExited,
            Outcome::StdinClosed,
        ] {
            assert!(
                !other.leads_straight_into_another_relay(),
                "{other:?} opens a screen, goes back by the session loop, or ends the relay"
            );
        }
    }

    /// A fake msgpack-RPC server that answers every call the probe could make
    /// and records the methods asked.
    fn recording_server(
        sock: &Path,
        mode: &'static str,
        blocking: bool,
    ) -> std::thread::JoinHandle<Vec<String>> {
        recording_server_with(sock, mode, blocking, 0, DEFAULT_MOUSE)
    }

    /// Neovim's default answer to `[&mouse, &mousemoveevent]`.
    const DEFAULT_MOUSE: (&str, i64) = ("nvi", 0);

    /// The same, reporting `uis` attached UIs.
    fn recording_server_with_uis(
        sock: &Path,
        mode: &'static str,
        blocking: bool,
        uis: usize,
    ) -> std::thread::JoinHandle<Vec<String>> {
        recording_server_with(sock, mode, blocking, uis, DEFAULT_MOUSE)
    }

    /// The same, with `mouse` as its `'mouse'` and `'mousemoveevent'`.
    fn recording_server_with(
        sock: &Path,
        mode: &'static str,
        blocking: bool,
        uis: usize,
        mouse: (&'static str, i64),
    ) -> std::thread::JoinHandle<Vec<String>> {
        serve_recording(sock, mode, blocking, uis, mouse, None, false)
    }

    /// A server whose prompt ends when typed at: `mode` until the first
    /// `nvim_input`, then idle `n`.
    fn recording_server_clearing_on_input(
        sock: &Path,
        mode: &'static str,
    ) -> std::thread::JoinHandle<Vec<String>> {
        serve_recording(sock, mode, true, 0, DEFAULT_MOUSE, None, true)
    }

    /// An idle server that records `method` and never answers it, as a busy
    /// session looks to the probe.
    fn recording_server_stalling_on(
        sock: &Path,
        method: &'static str,
    ) -> std::thread::JoinHandle<Vec<String>> {
        serve_recording(sock, "n", false, 0, DEFAULT_MOUSE, Some(method), false)
    }

    /// A server waiting for a key: answers fast calls but records and never
    /// answers the deferred `method`.
    fn recording_server_blocked_stalling_on(
        sock: &Path,
        mode: &'static str,
        method: &'static str,
    ) -> std::thread::JoinHandle<Vec<String>> {
        serve_recording(sock, mode, true, 0, DEFAULT_MOUSE, Some(method), false)
    }

    fn serve_recording(
        sock: &Path,
        mode: &'static str,
        blocking: bool,
        uis: usize,
        mouse: (&'static str, i64),
        stall_on: Option<&'static str>,
        clears_on_input: bool,
    ) -> std::thread::JoinHandle<Vec<String>> {
        use rmpv::Value;

        let listener = std::os::unix::net::UnixListener::bind(sock).expect("bind");
        // Non-blocking, so a probe that never connects fails the test rather
        // than hanging it.
        listener
            .set_nonblocking(true)
            .expect("a non-blocking listener");
        std::thread::spawn(move || {
            let mut asked = Vec::new();
            // Set by the first `nvim_input`; ends the prompt if
            // `clears_on_input`.
            let mut typed_at = false;
            let deadline = Instant::now() + Duration::from_secs(5);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() > deadline {
                            return asked;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return asked,
                }
            };
            // The accepted stream inherits O_NONBLOCK on some platforms, and
            // the reads below are blocking ones with their own bound.
            stream.set_nonblocking(false).expect("a blocking stream");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("a bounded read");
            let mut reader = std::io::BufReader::new(&stream);
            let mut out = &stream;
            // Ends on EOF, when the caller drops its client after its last
            // call, so joining waits for the whole exchange.
            while let Ok(frame) = rmpv::decode::read_value(&mut reader) {
                let Some(request) = frame.as_array() else {
                    break;
                };
                let (Some(msgid), Some(method)) = (
                    request.get(1).cloned(),
                    request.get(2).and_then(Value::as_str),
                ) else {
                    break;
                };
                let result = match method {
                    "nvim_get_api_info" => Value::Array(vec![
                        Value::from(1u64),
                        Value::Map(vec![(
                            Value::String("version".into()),
                            Value::Map(vec![
                                (Value::String("major".into()), Value::from(0u64)),
                                (Value::String("minor".into()), Value::from(12u64)),
                                (Value::String("patch".into()), Value::from(0u64)),
                            ]),
                        )]),
                    ]),
                    "nvim_get_mode" if clears_on_input && typed_at => Value::Map(vec![
                        (Value::String("mode".into()), Value::String("n".into())),
                        (Value::String("blocking".into()), Value::Boolean(false)),
                    ]),
                    "nvim_get_mode" => Value::Map(vec![
                        (Value::String("mode".into()), Value::String(mode.into())),
                        (Value::String("blocking".into()), Value::Boolean(blocking)),
                    ]),
                    "nvim_list_uis" => Value::Array(vec![Value::Nil; uis]),
                    // The byte count, which nvmux ignores.
                    "nvim_input" => Value::from(4u64),
                    // Only the expression the resume asks; else nil.
                    "nvim_eval"
                        if request
                            .get(3)
                            .and_then(|p| p.as_array())
                            .and_then(|p| p.first())
                            .and_then(Value::as_str)
                            == Some("[&mouse, &mousemoveevent]") =>
                    {
                        Value::Array(vec![Value::String(mouse.0.into()), Value::from(mouse.1)])
                    }
                    _ => Value::Nil,
                };
                // Record the keys too: which prompt gets a `<CR>` matters.
                let keys = request
                    .get(3)
                    .and_then(|p| p.as_array())
                    .and_then(|p| p.first())
                    .and_then(Value::as_str);
                asked.push(match keys {
                    Some(keys) if method == "nvim_input" => format!("{method}({keys})"),
                    _ => method.to_string(),
                });
                if method == "nvim_input" {
                    typed_at = true;
                }
                if stall_on == Some(method) {
                    // Never answered; reading on until EOF proves the cancelled
                    // probe's hangup reached the server.
                    continue;
                }
                let reply = Value::Array(vec![Value::from(1u64), msgid, Value::Nil, result]);
                if rmpv::encode::write_value(&mut out, &reply).is_err() {
                    break;
                }
            }
            asked
        })
    }

    /// The server records the mode, then `:mode`, and nothing typed.
    #[test]
    fn the_notice_repaint_is_asked_from_a_thread_and_answered_later() {
        let sock = crate::test_support::scratch_sock("pty-erasing");
        let server = recording_server(&sock, "n", false);

        let erasing = Erasing::start(&sock).expect("a thread");
        let served = wait_for_the_answer(&erasing);
        assert!(
            served.repainted,
            "the server painted; the relay must not nudge on top of it"
        );

        assert_eq!(
            server.join().expect("the server thread"),
            ["nvim_get_mode", "nvim_command"],
            "the notice repaint asked for something else"
        );
        let _ = std::fs::remove_file(&sock);
    }

    /// **The point of the thread**: a server that never answers `:mode` used
    /// to stall the relay for all of [`RESUME_TIMEOUT`].
    #[test]
    fn asking_for_the_notice_repaint_does_not_wait_for_it() {
        let sock = crate::test_support::scratch_sock("pty-erasingstall");
        let server = recording_server_stalling_on(&sock, "nvim_command");

        let asked = Instant::now();
        let erasing = Erasing::start(&sock).expect("a thread");
        let took = asked.elapsed();
        assert!(
            took < RESUME_TIMEOUT / 4,
            "asking took {took:?}, which is a wait, not an ask"
        );
        assert_eq!(
            erasing.answer(),
            None,
            "an answer cannot have arrived from a server that never sent one"
        );

        // When the budget runs out, the verdict is the fallback: nothing
        // painted, so the relay nudges.
        let served = wait_for_the_answer(&erasing);
        assert_eq!(served, Served::NONE);
        drop(erasing);
        let _ = std::fs::remove_file(&sock);
        let _ = server.join();
    }

    /// Poll for [`Erasing`]'s verdict the way the relay does, with a bound so
    /// a worker that never answers fails the test rather than hanging it.
    fn wait_for_the_answer(erasing: &Erasing) -> Served {
        let deadline = Instant::now() + RESUME_TIMEOUT * 4;
        while Instant::now() < deadline {
            if let Some(served) = erasing.answer() {
                return served;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the notice repaint never answered");
    }

    /// The notice's prompt may be one the user is reading (likely a startup
    /// error).
    #[test]
    fn only_a_resume_ends_a_hit_enter_prompt_to_repaint() {
        for (tag, why, want) in [
            (
                "promptyes",
                Repaint::Resume,
                &[
                    "nvim_get_mode",
                    "nvim_input(<CR>)",
                    "nvim_eval",
                    "nvim_command",
                ][..],
            ),
            ("promptno", Repaint::Notice, &["nvim_get_mode"][..]),
        ] {
            let sock = crate::test_support::scratch_sock(&format!("pty-{tag}"));
            let server = recording_server(&sock, "r", true);
            let served = repaint_through_server(&sock, why);
            assert_eq!(
                served.repainted,
                why == Repaint::Resume,
                "{why:?} at a hit-enter prompt"
            );
            assert_eq!(
                server.join().expect("the server thread"),
                want,
                "{why:?} asked the wrong things at a hit-enter prompt"
            );
            let _ = std::fs::remove_file(&sock);
        }
    }

    /// Removing a notice does not ask: nothing ran in between.
    #[test]
    fn a_resume_asks_the_server_for_the_clients_mouse_setting() {
        use term::MouseReporting;
        for (tag, mouse, want) in [
            ("mouseoff", ("", 0), Some(MouseReporting::Off)),
            ("mousenvi", ("nvi", 0), Some(MouseReporting::Buttons)),
            ("mousemove", ("a", 1), Some(MouseReporting::Motion)),
        ] {
            let sock = crate::test_support::scratch_sock(&format!("pty-{tag}"));
            let server = recording_server_with(&sock, "n", false, 0, mouse);
            let served = repaint_through_server(&sock, Repaint::Resume);
            assert_eq!(served.mouse, want, "{mouse:?}");
            assert!(served.repainted);
            assert_eq!(
                server.join().expect("the server thread"),
                ["nvim_get_mode", "nvim_eval", "nvim_command"],
                "{mouse:?}"
            );
            let _ = std::fs::remove_file(&sock);
        }

        let sock = crate::test_support::scratch_sock("pty-mousenotice");
        let server = recording_server_with(&sock, "n", false, 0, ("", 0));
        let served = repaint_through_server(&sock, Repaint::Notice);
        assert_eq!(served.mouse, None, "a notice must not ask");
        assert!(served.repainted);
        assert_eq!(
            server.join().expect("the server thread"),
            ["nvim_get_mode", "nvim_command"]
        );
        let _ = std::fs::remove_file(&sock);
    }

    /// The resume then falls back on the default.
    #[test]
    fn a_server_that_cannot_be_asked_says_nothing_about_the_mouse() {
        let sock = crate::test_support::scratch_sock("pty-mousebusy");
        // Blocking on something other than hit-enter, e.g. an operator
        // awaiting its motion.
        let server = recording_server_with(&sock, "no", true, 0, ("", 0));
        let served = repaint_through_server(&sock, Repaint::Resume);
        assert_eq!(served, Served::NONE);
        assert_eq!(server.join().expect("the server thread"), ["nvim_get_mode"]);
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn the_mouse_setting_is_read_the_way_neovims_tui_reads_it() {
        use rmpv::Value;
        use term::MouseReporting;
        let pair =
            |mouse: &str, moves: Value| Value::Array(vec![Value::String(mouse.into()), moves]);
        assert_eq!(
            mouse_reporting(&pair("", Value::from(0))),
            Some(MouseReporting::Off)
        );
        assert_eq!(
            mouse_reporting(&pair("", Value::from(1))),
            Some(MouseReporting::Off),
            "mousemoveevent means nothing with the mouse off"
        );
        assert_eq!(
            mouse_reporting(&pair("nvi", Value::from(0))),
            Some(MouseReporting::Buttons)
        );
        assert_eq!(
            mouse_reporting(&pair("a", Value::from(1))),
            Some(MouseReporting::Motion)
        );
        assert_eq!(
            mouse_reporting(&pair("n", Value::Boolean(true))),
            Some(MouseReporting::Motion),
            "a boolean spelling of the flag reads the same"
        );
        for malformed in [
            Value::Nil,
            Value::String("nvi".into()),
            Value::Array(vec![]),
            Value::Array(vec![Value::String("nvi".into())]),
            Value::Array(vec![Value::from(1), Value::from(0)]),
            Value::Array(vec![Value::String("nvi".into()), Value::String("0".into())]),
        ] {
            assert_eq!(mouse_reporting(&malformed), None, "{malformed:?}");
        }
    }

    /// A client idling on its pty until hung up, standing in for `nvim`; `tag`
    /// on its command line makes it findable in the process table.
    fn standin_client(tag: &str) -> Command {
        Command::new("sh")
            .arg("-c")
            .arg(format!("while :; do read x; done # {}", standin_mark(tag)))
    }

    fn standin_mark(tag: &str) -> String {
        format!("nvmux-standin-{}-{tag}", std::process::id())
    }

    /// Whether a stand-in client with this tag is in the process table.
    fn standin_running(tag: &str) -> bool {
        let out = std::process::Command::new("ps")
            .args(["-ww", "-eo", "args="])
            .output()
            .expect("ps");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.contains(&standin_mark(tag)) && !l.contains("ps -ww"))
    }

    /// Mode and UI count together, then `<CR>` only for a prompt it ends; no
    /// `nvim_get_api_info`.
    #[test]
    fn the_attach_probe_asks_the_mode_and_the_ui_count_together_and_nothing_else() {
        for (tag, mode, want) in [
            ("idle", "n", &["nvim_get_mode", "nvim_list_uis"][..]),
            // The mode is re-read after the `<CR>`; the `list_uis` already in
            // flight gives the count.
            (
                "hitenter",
                "r",
                &[
                    "nvim_get_mode",
                    "nvim_list_uis",
                    "nvim_input(<CR>)",
                    "nvim_get_mode",
                ][..],
            ),
        ] {
            let sock = crate::test_support::scratch_sock(&format!("pty-{tag}"));
            let server = if mode == "r" {
                recording_server_clearing_on_input(&sock, mode)
            } else {
                recording_server(&sock, mode, false)
            };
            // Bound to `_` so the attachment is retired at once.
            let _ = spawn_with("probe", &sock, "1  probe", standin_client(tag), false)
                .expect("a passed probe is an attachment");
            assert_eq!(
                server.join().expect("the server thread"),
                want,
                "the attach probe asked something else in mode {mode:?}"
            );
            let _ = std::fs::remove_file(&sock);
        }
    }

    /// Appended to the session's name rather than replacing it.
    #[test]
    fn a_blocked_session_says_so_on_its_notice_without_losing_its_name() {
        let mut attachment = attached_to("while :; do read x; done");
        attachment.announce = Some("dotfiles".to_string());
        attachment.note_waiting_for_a_key();
        let said = attachment.announce.clone().expect("a notice");
        assert!(said.starts_with("dotfiles"), "{said:?}");
        assert!(said.contains("press a key"), "{said:?}");

        // Saying it twice says it once.
        attachment.note_waiting_for_a_key();
        assert_eq!(attachment.announce.as_deref(), Some(said.as_str()));

        // A notice already shown stays shown.
        attachment.announce = None;
        attachment.note_waiting_for_a_key();
        assert_eq!(attachment.announce, None);
    }

    /// The name is not repeated, but "waiting for a key" is.
    #[test]
    fn going_back_to_the_session_just_left_keeps_only_a_notice_with_news_in_it() {
        let mut attachment = attached_to("while :; do read x; done");
        attachment.announce = Some("dotfiles".to_string());
        attachment.arrive_unannounced();
        assert_eq!(attachment.announce, None, "the name, already seen");

        attachment.announce = Some("dotfiles".to_string());
        attachment.note_waiting_for_a_key();
        let said = attachment.announce.clone();
        attachment.arrive_unannounced();
        assert_eq!(attachment.announce, said, "the key it is waiting for");
    }

    /// **Regression test for the attach that never came back.** These servers
    /// never answer the deferred `nvim_list_uis`; the probe must still report
    /// `Blocked` rather than hang `ui::attaching` (measured on 0.12.5: all
    /// three states did).
    #[test]
    fn a_session_waiting_for_a_key_is_never_waited_on_for_a_deferred_answer() {
        for (tag, mode, keys) in [
            // A more-prompt. `<CR>` scrolls it a line rather than ending it.
            ("more", "rm", 0),
            // A half-typed `g` (`n` with `blocking`), as in the original
            // report: nothing is typed.
            ("pending", "n", 0),
            // A hit-enter prompt outlasting every key: the budget is spent,
            // then the terminal is handed over.
            ("stacked", "r", MAX_PROMPTS),
        ] {
            let sock = crate::test_support::scratch_sock(&format!("pty-{tag}"));
            let server = recording_server_blocked_stalling_on(&sock, mode, "nvim_list_uis");
            let mut probe = Probe::start(tag, &sock).expect("connected");
            let answer = probe
                .wait(Duration::from_secs(5))
                .expect("the probe must answer rather than park")
                .expect("a blocked session is not an error");
            assert_eq!(
                answer,
                Answer::Blocked { mode: mode.into() },
                "{tag}: the probe did not report the session as blocked"
            );
            drop(probe);

            let asked = server.join().expect("the server thread");
            assert_eq!(
                asked.iter().filter(|m| *m == "nvim_list_uis").count(),
                1,
                "{tag}: the UI count is asked once, with the mode, and not \
                 again per prompt: {asked:?}"
            );
            assert_eq!(
                asked.iter().filter(|m| m.starts_with("nvim_input")).count(),
                keys,
                "{tag}: the wrong number of keys was typed at the session: {asked:?}"
            );
            let _ = std::fs::remove_file(&sock);
        }
    }

    /// The client starts before the probe passes, so either refusal must take
    /// it down again.
    #[test]
    fn a_failed_probe_leaves_no_client_behind() {
        for (tag, too_many) in [("nobody", false), ("toomany", true)] {
            let sock = crate::test_support::scratch_sock(&format!("pty-{tag}"));
            let server =
                too_many.then(|| recording_server_with_uis(&sock, "n", false, MAX_UIS + 1));
            if !too_many {
                // A file where the socket should be: as dead as a missing one.
                std::fs::write(&sock, b"not a socket").expect("a file where the socket was");
            }
            let err = spawn_with("gone", &sock, "1  gone", standin_client(tag), false)
                .expect_err("the probe must refuse");
            match (too_many, &err) {
                (true, NvmuxError::Session(crate::error::SessionError::TooManyUis { .. })) => {}
                (false, NvmuxError::Rpc(_)) => {}
                _ => panic!("{tag}: the wrong refusal: {err}"),
            }
            if let Some(server) = server {
                server.join().expect("the server thread");
            }
            // Reaped before the error returned: not even a zombie remains.
            assert!(
                !standin_running(tag),
                "{tag}: the client is still there after a failed probe"
            );
            let _ = std::fs::remove_file(&sock);
        }
    }

    /// The worker must stop, or it would answer the session's prompt once
    /// free, and the server must see the connection close.
    #[test]
    fn dropping_a_probe_stops_its_worker_and_hangs_up_on_the_server() {
        let sock = crate::test_support::scratch_sock("pty-stall");
        let server = recording_server_stalling_on(&sock, "nvim_list_uis");
        let mut probe = Probe::start("stall", &sock).expect("connected");
        assert!(
            probe.wait(Duration::from_millis(150)).is_none(),
            "a server that does not answer must not produce a verdict"
        );

        let started = Instant::now();
        let worker = probe.abandon();
        assert!(
            crate::test_support::wait_until(Duration::from_secs(2), || worker.is_finished()),
            "the worker is still parked in its read after the interrupt"
        );
        worker.join().expect("the worker thread");
        assert_eq!(
            server.join().expect("the server thread"),
            ["nvim_get_mode", "nvim_list_uis"],
            "the server saw something other than the probe's first two calls"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the hangup took {:?} to reach the server",
            started.elapsed()
        );
        let _ = std::fs::remove_file(&sock);
    }

    /// The count the probe reads may or may not include the client started
    /// alongside it, so the guard tolerates exactly one over.
    #[test]
    fn the_ui_limit_allows_for_the_client_being_counted() {
        let sock = crate::test_support::scratch_sock("pty-atlimit");
        let server = recording_server_with_uis(&sock, "n", false, MAX_UIS);
        let attached = spawn_with("limit", &sock, "1  limit", standin_client("atlimit"), false);
        assert!(
            attached.is_ok(),
            "{MAX_UIS} UIs, one of them possibly ours, must attach"
        );
        drop(attached);
        server.join().expect("the server thread");
        let _ = std::fs::remove_file(&sock);
    }

    /// The hangup must signal and return at once, for the switch path's
    /// overlap. `portable_pty`'s `Child::kill` passed the state checks; only
    /// the time bound caught it.
    #[test]
    fn hanging_up_signals_the_client_without_waiting_for_it() {
        // `read` is a shell builtin, so the signalled process is the one
        // that reports.
        let mut a = attached_to("trap 'printf H' HUP; printf r; while :; do read x; done");
        let pid = pid_of(&a);
        // The `printf r` follows the `trap`, so from here the hangup is
        // caught, not fatal.
        wait_until_ready(&a);

        let start = Instant::now();
        a.hang_up();
        let took = start.elapsed();

        // A shell may defer a trap for a signal arriving as it enters `read`
        // until `read` returns (bash 3.2, macOS's `sh`). A newline ends the
        // `read` and prints nothing, so the `H` is still the trap's alone.
        a.master.write_all(b"\n").expect("write");
        a.master.flush().expect("flush");

        assert!(
            wait_for(&a, b'H'),
            "the client never reported a hangup, so none was delivered"
        );
        assert_eq!(
            peek_child(pid),
            Peek::Running,
            "the hangup must not escalate past the signal the client caught"
        );
        assert!(
            took < Duration::from_millis(100),
            "the hangup took {took:?}; a hangup that blocks leaves the switch \
             path nothing to overlap with the next client's spawn"
        );
        assert!(!a.reaped, "the hangup must leave the reaping to `reap`");

        // The reap kills a client that will not leave.
        a.reap();
        assert_eq!(
            settle(pid, Peek::Gone),
            Peek::Gone,
            "a client that stays after its hangup must still be killed by the reap"
        );
    }

    /// The picker stops reading the outgoing client, so one mid-repaint blocks
    /// in `write` and never reaches its handler; the reap drains it while
    /// waiting.
    #[test]
    fn a_client_blocked_writing_still_takes_its_hangup() {
        // Traps the hangup: the default disposition is fatal even when
        // blocked, and would prove nothing.
        let mut a = attached_to(
            "trap 'exit 0' HUP; printf r; while :; do printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; done",
        );
        let pid = pid_of(&a);
        wait_until_ready(&a);
        // Long enough to fill the master's buffer and block in `write`.
        std::thread::sleep(Duration::from_millis(400));

        let start = Instant::now();
        a.hang_up();
        a.reap();
        let took = start.elapsed();

        assert_eq!(peek_child(pid), Peek::Gone);
        assert!(
            took < Duration::from_millis(500),
            "the client took {took:?} — it was killed by the reap's deadline \
             rather than read out and left to act on its hangup"
        );
    }

    /// Neovim's `tui_stop` waits up to 1 s for a DA1 reply; the stand-in does
    /// the same with `dd`.
    #[test]
    fn a_client_waiting_on_its_terminal_is_answered_and_goes() {
        // `stty raw`: the reply has no newline, so a canonical read would
        // block for a line and the test would fail for the wrong reason.
        let mut a = attached_to(
            r#"stty raw -echo; trap 'printf "\033[c"; dd bs=1 count=1 >/dev/null 2>&1; exit 0' HUP; printf r; while :; do sleep 0.05; done"#,
        );
        let pid = pid_of(&a);
        wait_until_ready(&a);

        let start = Instant::now();
        a.hang_up();
        a.reap();
        let took = start.elapsed();

        assert_eq!(peek_child(pid), Peek::Gone);
        assert!(
            took < Duration::from_millis(500),
            "the client took {took:?} — nothing answered the device attributes \
             request it was waiting on, so it sat there until the reap's deadline"
        );
    }

    /// A stand-in kept client (one with a ledger).
    fn kept(script: &str) -> Attachment {
        let mut a = attached_to(script);
        a.ledger = Some(Ledger::new());
        a
    }

    /// Wait for a parked client's thread to finish, or give up.
    fn settle_parked(parked: &Parked) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while parked.is_alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        !parked.is_alive()
    }

    /// The title written after 200 KB, which would block an unread client,
    /// reaches the ledger.
    #[test]
    fn a_parked_client_is_read_while_it_waits() {
        let a = kept(
            r#"head -c 200000 /dev/zero | tr '\0' x; printf '\033]0;said it all\007'; exec sleep 30"#,
        );
        let pid = pid_of(&a);
        let parked = a.park().expect("parked");
        // An unread client would still be stuck a second later.
        std::thread::sleep(Duration::from_secs(1));
        let a = parked.unpark().expect("handed back");
        assert_eq!(pid_of(&a), pid, "not the same client");
        assert_eq!(
            a.ledger.as_ref().and_then(Ledger::title),
            Some(&b"\x1b]0;said it all\x07"[..]),
            "the client was not read to the end of what it said while parked"
        );
    }

    #[test]
    fn unparking_hands_back_the_same_client() {
        let a = kept("printf r; exec sleep 30");
        wait_until_ready(&a);
        let pid = pid_of(&a);
        let parked = a.park().expect("parked");
        assert!(parked.is_alive());
        let back = parked.unpark().expect("handed back");
        assert_eq!(pid_of(&back), pid);
        assert_eq!(
            peek_child(pid),
            Peek::Running,
            "the client did not survive parking"
        );
    }

    /// Exited while parked: not handed back, and reaped.
    #[test]
    fn a_client_that_leaves_while_parked_is_not_handed_back() {
        let a = kept("sleep 0.2; exit 0");
        let pid = pid_of(&a);
        let parked = a.park().expect("parked");
        assert!(
            settle_parked(&parked),
            "the thread never noticed its client go"
        );
        assert!(
            parked.unpark().is_none(),
            "a client that left was handed back"
        );
        assert!(reaped(pid), "and it was not reaped");
    }

    /// A parked client sending DA1 is exiting or suspending; it is retired
    /// quickly, not answered and kept.
    #[test]
    fn a_parked_client_on_its_way_out_is_retired() {
        let a = kept(
            r#"stty raw -echo; trap 'exit 0' HUP; printf r; sleep 0.2; printf '\033[?1049l\033[c'; while :; do sleep 0.05; done"#,
        );
        wait_until_ready(&a);
        let pid = pid_of(&a);
        let parked = a.park().expect("parked");
        let start = Instant::now();
        assert!(settle_parked(&parked), "the finishing client was kept");
        assert!(
            parked.unpark().is_none(),
            "the finishing client was handed back"
        );
        assert!(reaped(pid), "the finishing client was not reaped");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "retiring it took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn dropping_a_parked_client_retires_it() {
        let a = kept("printf r; exec sleep 30");
        wait_until_ready(&a);
        let pid = pid_of(&a);
        drop(a.park().expect("parked"));
        assert!(reaped(pid), "the client outlived its parking");
    }

    /// Not even the ledger sees it, or it would record a title the terminal has
    /// not been told. Its startup DA1 requests are not taken as an exit.
    #[test]
    fn a_client_started_ahead_keeps_what_it_writes_whole_and_unseen() {
        let a = kept(
            r#"printf '\033[?1049hhello\033['; sleep 0.1; printf 'c\033]0;a title\007\033[c'; exec sleep 30"#,
        );
        let pid = pid_of(&a);
        let held = a.start_ahead(None).expect("held");
        std::thread::sleep(Duration::from_millis(400));
        assert!(held.is_alive(), "its startup was taken for an exit");
        let back = held.unpark().expect("handed back");
        assert_eq!(pid_of(&back), pid, "not the same client");
        assert!(back.started_ahead());
        assert_eq!(
            back.unshown.as_deref(),
            Some(&b"\x1b[?1049hhello\x1b[c\x1b]0;a title\x07\x1b[c"[..]),
            "what it wrote was not kept as it wrote it"
        );
        assert_eq!(
            back.ledger.as_ref().and_then(Ledger::title),
            None,
            "the ledger was shown what the terminal has not been"
        );
    }

    /// Card-drawing clients are sent [`card::Command::Away`] when parked or
    /// started ahead; Neovim's client is sent nothing.
    #[test]
    fn a_client_put_aside_is_told_it_is_out_of_sight() {
        for draws_cards in [true, false] {
            let said = card::encode(&card::Command::Away);
            // What the client reads of its input, said back as a title in hex.
            let script = format!(
                r#"printf r; heard=$(head -c {} | od -An -tx1 | tr -d ' \n'); printf '\033]0;%s\007' "$heard"; exec sleep 30"#,
                said.len()
            );
            let mut heard = b"\x1b]0;".to_vec();
            for b in &said {
                heard.extend_from_slice(format!("{b:02x}").as_bytes());
            }
            heard.push(0x07);
            let told = draws_cards;
            let mut a = kept(&script);
            a.draws_cards = draws_cards;
            wait_until_ready(&a);
            let parked = a.park().expect("parked");
            std::thread::sleep(Duration::from_millis(500));
            let back = parked.unpark().expect("handed back");
            assert_eq!(
                back.ledger.as_ref().and_then(Ledger::title),
                told.then_some(&heard[..]),
                "parked, drawing cards: {draws_cards}"
            );
            assert_eq!(back.away, told);
            assert_eq!(back.crawling, told);

            let mut a = kept(&script);
            a.draws_cards = draws_cards;
            let held = a.start_ahead(None).expect("held");
            std::thread::sleep(Duration::from_millis(500));
            let back = held.unpark().expect("handed back");
            let mut unshown = b"r".to_vec();
            if told {
                unshown.extend_from_slice(&heard);
            }
            assert_eq!(
                back.unshown.as_deref(),
                Some(&unshown[..]),
                "started ahead, drawing cards: {draws_cards}"
            );
            assert_eq!(back.away, told);
            assert_eq!(back.crawling, told);
        }
    }

    /// Leaving the alternate screen means exit or suspend, however the reads
    /// split the sequence.
    #[test]
    fn a_client_started_ahead_on_its_way_out_is_retired() {
        for (tag, finish) in [
            ("whole", r#"printf '\033[?1049l\033[c'"#),
            ("split", r#"printf '\033[?10'; sleep 0.1; printf '49l'"#),
        ] {
            let a = kept(&format!(
                r#"stty raw -echo; trap 'exit 0' HUP; printf '\033[c'; sleep 0.2; {finish}; while :; do sleep 0.05; done"#
            ));
            let pid = pid_of(&a);
            let held = a.start_ahead(None).expect("held");
            assert!(settle_parked(&held), "{tag}: the finishing client was kept");
            assert!(
                held.unpark().is_none(),
                "{tag}: the finishing client was handed back"
            );
            assert!(reaped(pid), "{tag}: the finishing client was not reaped");
        }
    }

    #[test]
    fn a_client_started_ahead_that_says_too_much_is_let_go() {
        let a = kept(&format!(
            "head -c {} /dev/zero | tr '\\0' x; exec sleep 30",
            AHEAD_MAX + 1
        ));
        let pid = pid_of(&a);
        let held = a.start_ahead(None).expect("held");
        assert!(settle_parked(&held), "a client past the bound was kept");
        assert!(held.unpark().is_none());
        assert!(reaped(pid), "and it was not reaped");
    }

    /// A session with room keeps it and drops the probe; too many UIs or a key
    /// wait retire it, so the first visit attaches afresh.
    #[test]
    fn a_client_started_ahead_is_kept_only_if_its_probe_says_so() {
        for (tag, mode, blocking, uis, keeps) in [
            ("room", "n", false, 0, true),
            ("full", "n", false, MAX_UIS + 1, false),
            ("blocked", "rm", true, 0, false),
        ] {
            let sock = crate::test_support::scratch_sock(&format!("pty-ahead-{tag}"));
            let server = if blocking {
                recording_server_blocked_stalling_on(&sock, mode, "nvim_list_uis")
            } else {
                recording_server_with_uis(&sock, mode, false, uis)
            };
            let probe = Probe::start(tag, &sock).expect("connected");
            let a = kept("printf r; exec sleep 30");
            let pid = pid_of(&a);
            let held = a.start_ahead(Some(probe)).expect("held");
            if keeps {
                // Long enough for one local round trip.
                std::thread::sleep(Duration::from_millis(500));
                let mut back = held
                    .unpark()
                    .unwrap_or_else(|| panic!("{tag}: not handed back"));
                assert_eq!(pid_of(&back), pid);
                assert!(back.take_probe().is_none(), "{tag}: the probe was kept");
            } else {
                assert!(settle_parked(&held), "{tag}: the client was kept");
                assert!(held.unpark().is_none(), "{tag}: and handed back");
                assert!(reaped(pid), "{tag}: and not reaped");
            }
            server.join().expect("the server thread");
            let _ = std::fs::remove_file(&sock);
        }
    }

    /// The caller then waits on the probe as for a fresh client.
    #[test]
    fn a_probe_still_out_comes_back_with_its_client() {
        let sock = crate::test_support::scratch_sock("pty-ahead-busy");
        let server = recording_server_stalling_on(&sock, "nvim_list_uis");
        let probe = Probe::start("busy", &sock).expect("connected");
        let held = kept("printf r; exec sleep 30")
            .start_ahead(Some(probe))
            .expect("held");
        std::thread::sleep(Duration::from_millis(100));
        let mut back = held.unpark().expect("handed back");
        assert!(
            back.take_probe().is_some(),
            "the probe still out was not handed back"
        );
        drop(back);
        server.join().expect("the server thread");
        let _ = std::fs::remove_file(&sock);
    }

    /// The tail catches a sequence wherever the growth split it, but not one
    /// wholly before it.
    #[test]
    fn the_tail_looked_at_takes_in_a_sequence_split_by_the_growth() {
        let grown = b"xx\x1b[?1049l";
        for before in 2..grown.len() {
            assert!(
                leaves_alt_screen(&grown[tail_from(before, ALT_SCREEN_LEAVE)..]),
                "grown from {before} bytes"
            );
        }
        assert!(!leaves_alt_screen(
            &grown[tail_from(grown.len(), ALT_SCREEN_LEAVE)..]
        ));
        assert_eq!(tail_from(3, ALT_SCREEN_LEAVE), 0);
    }

    /// Whether the process is gone and reaped within 5 s.
    fn reaped(pid: Pid) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if kill(pid, None) == Err(nix::errno::Errno::ESRCH) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Such a client ignores `SIGWINCH`, so it is sent the report a terminal
    /// would send, whole, as input; only for a changed size.
    #[test]
    fn a_client_that_reads_its_size_in_band_is_sent_a_new_one() {
        let mut a = kept(
            r#"stty raw -echo; printf r; head -c 15 | od -An -c | tr -d ' \n'; printf '|'; exec sleep 30"#,
        );
        wait_until_ready(&a);
        a.ledger_saw(b"\x1b[?2048h");
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        a.tell_its_size(size, false);
        assert!(
            !wait_briefly_for(&a, b'|'),
            "an unchanged size was reported"
        );
        a.tell_its_size(size, true);
        let heard = read_until(&a, b'|');
        assert!(
            heard.contains("033[48;24;80;0;0t|"),
            "the client read something else: {heard:?}"
        );
    }

    /// The resize is enough, and a report would reach it as keys.
    #[test]
    fn a_client_that_reads_its_size_from_the_pty_is_sent_no_report() {
        let mut a =
            kept(r#"stty raw -echo; printf r; head -c 1 >/dev/null; printf '|'; exec sleep 30"#);
        wait_until_ready(&a);
        a.tell_its_size(PtySize::default(), true);
        assert!(
            !wait_briefly_for(&a, b'|'),
            "a report reached a client that asked for none"
        );
    }

    /// A client reading the return and the card apart draws a frame before the
    /// card; the relay must read past it, or the card appears after the fade.
    #[test]
    fn a_card_is_read_for_past_a_frame_that_comes_before_it() {
        let mut a = kept(
            r#"stty raw -echo; printf r; head -c 1 >/dev/null; printf '\033[2;1Hback'; sleep 0.05; printf '\033[1;11Hcard'; exec sleep 30"#,
        );
        a.shadow = Some(shadow::Shadow::new(24, 80));
        let card = shadow::Kept::new(
            [(0, 10, 13)],
            [(0, 10, 'c'), (0, 11, 'a'), (0, 12, 'r'), (0, 13, 'd')],
        );
        wait_until_ready(&a);
        a.tell_client(&card::Command::Clear);
        assert!(
            a.drain_until_shown(&card, Instant::now() + Duration::from_secs(5)),
            "the card never reached the shadow"
        );
    }

    /// **Regression test for the hex above the shell's prompt.** Input to a
    /// client is never echoed, before it makes its terminal raw or after it
    /// restores it.
    #[test]
    fn nothing_written_into_a_clients_input_comes_back_out() {
        for (when, script) in [
            (
                "before the client made its terminal raw",
                "printf r; exec sleep 30",
            ),
            (
                "once the client had put its terminal back",
                r#"t=$(stty -g); stty raw -echo; stty "$t"; printf r; exec sleep 30"#,
            ),
        ] {
            let mut a = attached_to(script);
            wait_until_ready(&a);
            a.tell_client(&card::Command::Clear);
            assert!(!wait_briefly_for(&a, b'_'), "the card came back out {when}");
        }
    }

    /// Even before the client sets up its terminal: no line buffering, no `DEL`
    /// editing, and `CR` stays `CR`.
    #[test]
    fn what_is_written_into_a_clients_input_reaches_it_as_written() {
        let mut a = attached_to(
            r#"printf r; head -c 3 | od -An -c | tr -d ' \n'; printf '|'; exec sleep 30"#,
        );
        wait_until_ready(&a);
        a.master.write_all(b"a\r\x7f").expect("write");
        a.master.flush().expect("flush");
        let heard = read_until(&a, b'|');
        assert!(
            heard.ends_with("a\\r177|"),
            "the client read something else: {heard:?}"
        );
    }

    /// Neither an fd nvmux made without close-on-exec (like the SIGWINCH
    /// self-pipe) nor one nvmux inherited (see `before_exec`).
    #[test]
    fn a_client_inherits_no_descriptor_but_its_terminal() {
        let (read, write) = nix::unistd::pipe().expect("a pipe without close-on-exec");
        let a = attached_to(&format!(
            "[ -e /dev/fd/0 ] && printf T; \
             for fd in {} {}; do [ -e /dev/fd/$fd ] && printf L; done; \
             printf '|'; exec sleep 30",
            read.as_raw_fd(),
            write.as_raw_fd(),
        ));
        let heard = read_until(&a, b'|');
        assert!(
            heard.contains('T'),
            "the client could not see its own terminal: {heard:?}"
        );
        assert!(
            !heard.contains('L'),
            "the client inherited a descriptor of ours: {heard:?}"
        );
    }

    /// Output up to and including `byte`, as text, or what came within 5 s.
    fn read_until(a: &Attachment, byte: u8) -> String {
        let fd = a.master.as_raw_fd();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        while Instant::now() < deadline && !out.contains(&byte) {
            let mut p = [pollfd(fd)];
            unsafe { libc::poll(p.as_mut_ptr(), 1, 50) };
            if ready(&p[0]) {
                if let Ok(n) = read_fd(fd, &mut buf) {
                    out.extend_from_slice(&buf[..n]);
                }
            }
        }
        String::from_utf8_lossy(&out).to_string()
    }

    /// [`wait_for`], for something that must not happen: a short look.
    fn wait_briefly_for(a: &Attachment, byte: u8) -> bool {
        let fd = a.master.as_raw_fd();
        let deadline = Instant::now() + Duration::from_millis(300);
        let mut buf = [0u8; 256];
        while Instant::now() < deadline {
            let mut p = [pollfd(fd)];
            unsafe { libc::poll(p.as_mut_ptr(), 1, 50) };
            if ready(&p[0]) {
                if let Ok(n) = read_fd(fd, &mut buf) {
                    if buf[..n].contains(&byte) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// What the relay passes on of `bytes` from the terminal, having shown
    /// them to [`Attachment::saw_input`].
    fn passed_on(a: &mut Attachment, bytes: &[u8]) -> Vec<u8> {
        let mut step = Step::Forward(bytes.to_vec());
        a.saw_input(&mut step);
        match step {
            Step::Forward(bytes) => bytes,
            Step::Act(action) => panic!("{action:?} out of forwarded bytes"),
        }
    }

    /// Only that answer, only at the client's size, only once, and only while
    /// expected.
    #[test]
    fn the_terminals_answer_to_a_replayed_size_mode_is_kept_from_the_client() {
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let report: &[u8] = b"\x1b[48;24;80;480;800t";
        let mut a = kept("exec sleep 30");
        a.startup_answered = true;
        assert_eq!(passed_on(&mut a, report), report, "not looked for");

        a.expecting_a_size_report = Some((size, Instant::now() + SIZE_REPORT_WAIT));
        assert_eq!(passed_on(&mut a, b"j"), b"j");
        assert_eq!(
            passed_on(&mut a, b"j\x1b[48;24;80;480;800tk"),
            b"jk",
            "the keys either side of it go through"
        );
        assert!(a.expecting_a_size_report.is_none());
        assert_eq!(passed_on(&mut a, report), report, "taken out once");

        a.expecting_a_size_report = Some((size, Instant::now() + SIZE_REPORT_WAIT));
        let resized: &[u8] = b"\x1b[48;30;100;600;1000t";
        assert_eq!(passed_on(&mut a, resized), resized, "a resize since");
        assert!(a.expecting_a_size_report.is_none());

        a.expecting_a_size_report = Some((size, Instant::now()));
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(passed_on(&mut a, report), report, "too late to be it");
        assert!(a.expecting_a_size_report.is_none());
    }

    #[test]
    fn a_size_report_is_found_by_its_shape() {
        assert_eq!(
            find_size_report(b"ab\x1b[48;24;80;0;0tcd"),
            Some((2..17, 24, 80))
        );
        assert_eq!(find_size_report(b"\x1b[48;24;80;0;0"), None, "unfinished");
        assert_eq!(find_size_report(b"\x1b[48;24t"), None, "no columns");
        assert_eq!(find_size_report(b"\x1b[4;24;80;0;0t"), None);
        assert_eq!(find_size_report(b"\x1b[?2048;2$y"), None);
    }

    /// The DA1 answer is the last reply a client's startup waits for.
    #[test]
    fn a_client_is_parkable_once_its_questions_are_answered() {
        let mut a = kept("exec sleep 30");
        assert!(!a.is_parkable(), "parkable before any answer");
        passed_on(&mut a, b"j");
        assert!(!a.is_parkable(), "a key is not an answer");
        passed_on(&mut a, b"\x1b[?62;22c");
        assert!(a.is_parkable());
        let mut plain = attached_to("exec sleep 30");
        passed_on(&mut plain, b"\x1b[?62;22c");
        assert!(
            !plain.is_parkable(),
            "a client that is not kept is never parked"
        );
    }

    #[test]
    fn a_device_attributes_answer_is_recognised_by_its_shape() {
        assert!(answers_device_attributes(b"\x1b[?62;22c"));
        assert!(answers_device_attributes(b"x\x1b[?1;2c\x1b[?0u"));
        assert!(answers_device_attributes(b"\x1b[?0u\x1b[?64;1;9c"));
        assert!(!answers_device_attributes(b"\x1b[?0u"), "the kitty answer");
        assert!(!answers_device_attributes(b"\x1b[c"), "the request");
        assert!(!answers_device_attributes(b"\x1b[?c"), "no attributes");
        assert!(
            !answers_device_attributes(b"\x1b[?2048;2$y"),
            "a mode report"
        );
    }

    #[test]
    fn a_device_attributes_request_is_recognised_in_a_chunk_of_output() {
        assert!(asks_for_device_attributes(b"\x1b[c"));
        // As it really arrives: at the tail of a restore sequence.
        assert!(asks_for_device_attributes(
            b"\x1b[?2004l\x1b[?1004l\x1b[?1049l\x1b[c"
        ));
        assert!(!asks_for_device_attributes(b""));
        assert!(!asks_for_device_attributes(b"\x1b[?1049l"));
        // A reply is not a request; answering one with another is a loop.
        assert!(!asks_for_device_attributes(DA1_REPLY));
    }

    /// SIGHUP is not delivered to a stopped process, so without SIGCONT this
    /// would wait out `REAP_TIMEOUT`. Reachable: a client `check_child` has not
    /// yet revived can be retired on a switch.
    #[test]
    fn a_suspended_client_is_continued_so_the_hangup_lands() {
        let mut a = attached_to("exec sleep 30");
        let pid = pid_of(&a);
        kill(pid, Signal::SIGSTOP).expect("stop");
        assert_eq!(settle(pid, Peek::Stopped), Peek::Stopped);

        a.hang_up();

        // Gone without the reap killing it: the pending hangup lands only once
        // the process runs again.
        assert_eq!(
            settle(pid, Peek::Gone),
            Peek::Gone,
            "a stopped client stays stopped unless the hangup continues it"
        );
        a.reap();
    }

    /// `Drop` is the retirement path every `break` out of the session loop
    /// relies on.
    #[test]
    fn dropping_an_attachment_hangs_it_up_and_reaps_it() {
        let a = attached_to("exec sleep 30");
        let pid = pid_of(&a);

        let start = Instant::now();
        drop(a);
        let took = start.elapsed();

        // Reaped, so the peek gets ECHILD, which reads as gone.
        assert_eq!(peek_child(pid), Peek::Gone);
        // "Gone" alone cannot tell a hangup exit from a kill at the reap's
        // deadline; the time bound can.
        assert!(
            took < Duration::from_millis(500),
            "dropping took {took:?}, so the client was killed by the deadline \
             rather than by the hangup"
        );
    }

    /// The switch path: hang up, then drop later. The second hangup must be
    /// harmless and the client reaped exactly once.
    #[test]
    fn hanging_up_then_dropping_reaps_the_client_exactly_once() {
        use nix::sys::wait::{waitpid, WaitPidFlag};

        let mut a = attached_to("exec sleep 30");
        let pid = pid_of(&a);

        a.hang_up();
        let start = Instant::now();
        drop(a);
        let took = start.elapsed();

        assert_eq!(peek_child(pid), Peek::Gone);
        assert!(
            took < Duration::from_millis(500),
            "dropping took {took:?}: the second hangup cost the client its life \
             at the deadline rather than being the no-op it should be"
        );
        // ECHILD: no zombie left, the half of "exactly once" a peek cannot see.
        assert_eq!(
            waitpid(pid, Some(WaitPidFlag::WNOHANG)),
            Err(nix::errno::Errno::ECHILD),
            "a zombie was left behind, so the client was never waited for"
        );
    }

    /// Against a real child because `waitid` goes through `libc` (nix lacks it
    /// on macOS), and the flags and `si_code` handling must hold on both.
    #[test]
    fn peek_reports_stopped_and_gone_without_consuming_either() {
        use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};

        // Kill and reap the child however the test ends.
        struct Guard(std::process::Child);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let mut guard = Guard(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn sleep"),
        );
        let pid = Pid::from_raw(guard.0.id() as i32);
        assert_eq!(peek_child(pid), Peek::Running);

        kill(pid, Signal::SIGSTOP).expect("stop");
        assert_eq!(settle(pid, Peek::Stopped), Peek::Stopped);
        // WNOWAIT: the notification is still there for the next look.
        assert_eq!(peek_child(pid), Peek::Stopped);

        // What `check_child` does with a stop: consume it, then continue.
        assert!(matches!(
            waitpid(pid, Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED)),
            Ok(WaitStatus::Stopped(_, Signal::SIGSTOP))
        ));
        kill(pid, Signal::SIGCONT).expect("continue");
        assert_eq!(settle(pid, Peek::Running), Peek::Running);

        kill(pid, Signal::SIGKILL).expect("kill");
        assert_eq!(settle(pid, Peek::Gone), Peek::Gone);
        // Not reaped by the peek: the owner's `wait` still gets the status.
        let status = guard.0.wait().expect("reap");
        assert!(!status.success());
        // And once it is reaped, ECHILD reads as gone too.
        assert_eq!(peek_child(pid), Peek::Gone);
    }

    /// The child is gone and the loop must end.
    #[test]
    fn a_failed_forward_is_an_error() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("gone"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(act(&mut Broken, Step::Forward(b"x".to_vec())).is_err());
    }

    /// Everything around the request, and sequences that merely resemble it,
    /// pass untouched.
    #[test]
    fn the_drain_takes_the_da1_request_out_and_nothing_else() {
        let restore = b"\x1b[?1049l\x1b[?25h\x1b[c\x1b[0m";
        assert_eq!(
            &*without_device_attributes_request(restore),
            b"\x1b[?1049l\x1b[?25h\x1b[0m"
        );
        // Twice in one chunk, back to back.
        assert_eq!(
            &*without_device_attributes_request(b"a\x1b[c\x1b[cb"),
            b"ab"
        );
        // Not a request: a reply, DA2, and a lone CSI.
        for left_alone in [&b"\x1b[?1;2c"[..], b"\x1b[>c", b"\x1b[", b"plain text", b""] {
            assert_eq!(
                &*without_device_attributes_request(left_alone),
                left_alone,
                "{left_alone:?} was rewritten"
            );
        }
    }
}
