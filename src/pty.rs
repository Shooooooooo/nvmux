//! The PTY proxy.
//!
//! nvmux runs `nvim --server <local_sock> --remote-ui` as a child on a PTY and
//! sits in the byte stream between the user's terminal and that child:
//!
//! ```text
//! stdin  -> [ prefix state machine ] -> pty master
//! stdout <-      (untouched)        <- pty master
//! ```
//!
//! **The child-to-terminal direction is never rewritten or acted on.** That
//! is the entire reason this design works: bracketed paste, the kitty
//! keyboard protocol, truecolor, undercurl, terminal titles, OSC 52 clipboard
//! and DA1/XTGETTCAP round-trips all function because the child negotiates
//! directly with the real terminal. Any "improvement" that changes this
//! direction on the strength of what it sees breaks a subset of them. There
//! is likewise no `nvim_ui_attach`, no `grid_line` handling and no grid
//! diffing anywhere in this crate — `nvim --remote-ui` already is that client.
//!
//! Two readers of this direction. The shadow grid ([`crate::shadow`]) is
//! shown a *copy* and can neither alter a write nor make one depend on what it
//! saw; it exists for the fade, and is off with it. The boundary
//! ([`crate::boundary`]) is shown the bytes as they go and keeps back the few
//! at the end of a write that would leave the terminal's parser inside a
//! sequence — a delay of microseconds, until the client writes the rest of
//! them, and only while there is a notice on screen to protect.
//!
//! The direction is also *held* once per relay: with the fade on, a session's
//! first paint is kept back from the terminal until it has settled, so the
//! shadow can be dissolved in first, and is then written out exactly as it
//! came, in one synchronized update, so the terminal ends in the state the
//! client meant (see [`Hold`]). The queries in that paint reach the terminal
//! that much later, and their answers reach the client that much later, which
//! it takes as it takes any answer. **A held byte — by either of the two — is
//! never dropped, reordered or changed.**
//!
//! Three things ever *join* this direction. The attach notice (see
//! [`crate::announce`]): a box, drawn where the child's own bytes leave the
//! terminal between escape sequences, which [`crate::boundary`] finds with a
//! decoder rather than with a clock, and written inside a synchronized update
//! that also carries the session's own bytes, so that the terminal has no
//! moment at which it could present the one without the other (see
//! [`Attachment::write_session_frame`]). Two answers, because the first one
//! on its own was not enough: a lull — 25 ms of the child saying nothing — is
//! something a session with a window repainting at sixty frames a second
//! never gives, and writing the box after every repaint instead only wins the
//! moments the terminal happens not to present in. And the fade's frames
//! ([`crate::fade`]), written before the held first paint and after the relay
//! has stopped. And the hint bar ([`crate::hint`]): the prefix's one-row bar,
//! written at a lull and put back from the shadow, and never fed to it either.
//!
//! None of them is ever fed back to the shadow. All three are nvmux's own
//! picture of the screen rather than the session's bytes, and the notice's
//! frames are composited *out of* the shadow — so showing them to it would
//! have it holding nvmux's drawing of the editor with a box in the middle, and
//! nothing underneath to give back (see [`Attachment::write_over_session`]).
//!
//! The pty itself, and the waits on it, are the platform's: a Unix pty or a
//! Windows pseudoconsole, `poll` or a wait on handles — see [`crate::sys`],
//! which also keeps the hazards that come with each.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub use crate::sys::PtySize;

use crate::error::{NvmuxError, Result};
use crate::keys::{Action, Prefix, Step, Wait};
use crate::ledger::Ledger;
use crate::sys::{ChildState, Order, Peek};
use crate::term::write_stdout;
use crate::{announce, boundary, fade, hint, rpc, shadow, sys, term};

/// Neovim aborts on the seventeenth attached UI rather than returning an error
/// (`src/nvim/ui.c`: `if (ui_count == MAX_UI_COUNT) { abort(); }`), which would
/// SIGABRT the whole server and destroy the session. Refuse well before that.
const MAX_UIS: usize = 8;

// At compile time: getting this wrong does not fail a test, it SIGABRTs a
// user's session.
const _: () = assert!(MAX_UIS < 16);

/// What the notice adds for a session that is waiting for a key.
///
/// An em dash and four words, appended to the name the box already carries:
/// the box is one line wide and up for a second, so this is the shortest thing
/// that says both what is wrong and what to do about it.
const WAITING: &str = " — press a key";

/// How the relay ended. The attachment is handed back separately by [`relay`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// `<prefix> Space` — show the picker. The child keeps running.
    ToPicker,
    /// `<prefix> d` — detach and exit, leaving the session running.
    Detached,
    /// `<prefix> c` — prompt for a name and create a new session. The child keeps
    /// running, so a cancelled prompt puts the user straight back.
    CreateNew,
    /// `<prefix> ?` — show the key bindings. The child keeps running.
    ShowHelp,
    /// `<prefix> <number>`, `<prefix> n` or `<prefix> p` — attach to the session
    /// this names. The child keeps running, so a switch that names nothing puts
    /// the user straight back.
    Switch(Target),
    /// The child exited on its own.
    ChildExited,
    /// Our own stdin reached EOF: the terminal went away or the input was
    /// redirected. The child is still running, so this is handled like a
    /// detach — the session is left alone — rather than like a child exit,
    /// which would wait on a child that has no reason to leave.
    StdinClosed,
}

impl Outcome {
    /// Whether this leads from one relay straight into the next, with no
    /// [`crate::ui`] screen in between to clear the outgoing frame on the way.
    ///
    /// Only a switch does — by number or by step. `ToPicker`, `CreateNew` and
    /// `ShowHelp` all open a screen, and a screen clears as it opens; the rest
    /// end the relay for good.
    fn leads_straight_into_another_relay(self) -> bool {
        matches!(self, Outcome::Switch(_))
    }

    /// The session this outcome names, for the caller that starts the next
    /// attachment while the outgoing one dissolves (see [`relay`]). `None` for
    /// every other way a relay can end: the three that open a screen have
    /// nothing to start yet, and the three that end it have nowhere to go.
    ///
    /// Its own accessor rather than a `matches!` at the call site, so the one
    /// rule it carries is pinned by a test — a new outcome that leads into
    /// another relay must not be able to arrive here and silently begin
    /// nothing, which would cost a fade's worth of overlap and no error.
    fn switch_target(self) -> Option<Target> {
        match self {
            Outcome::Switch(target) => Some(target),
            Outcome::ToPicker
            | Outcome::Detached
            | Outcome::CreateNew
            | Outcome::ShowHelp
            | Outcome::ChildExited
            | Outcome::StdinClosed => None,
        }
    }
}

/// Which session a `<prefix>` switch named: one by its number, or the one next
/// to this one in the picker's order.
///
/// One outcome carrying which, rather than two outcomes, so the answer stays a
/// value from the keypress all the way to the lookup instead of being spelled
/// out again at every hand-off. Either can name nothing — a number nobody has,
/// an empty listing — and that is the session loop's to settle, not the relay's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Number(u32),
    Step(crate::keys::Direction),
}

/// A running `--remote-ui` client, and the PTY it is talking through.
pub struct Attachment {
    /// Which session this client is attached to.
    pub session_id: String,
    /// The socket the client was attached through — locally the session's
    /// own, over SSH the local end of the forward — so a resume can ask the
    /// server a question without going back through the transport.
    sock: PathBuf,
    /// What to announce when this client first takes the terminal, and `None`
    /// once it has. `spawn` runs exactly when the attached session changed —
    /// `session_loop` reuses the client otherwise — so this being `Some` *is*
    /// the "the session changed" test, with no flag threaded through anything.
    /// A kept client coming back in place of another session's is the one
    /// other change of session, and has it set again for that relay (see
    /// [`Attachment::announce_on_arrival`]).
    announce: Option<String>,
    /// The client and its pty; see [`crate::sys::Pty`].
    pty: sys::Pty,
    writer: Box<dyn Write + Send>,
    /// True once the child has been through a full relay, so a resume knows it
    /// must force a repaint — and that the client is one that already entered
    /// the alternate screen and will not enter it again, so the terminal has
    /// to be put back there before it draws.
    resumed: bool,
    /// True once the child has been waited for, so `Drop` does not do it twice.
    reaped: bool,
    /// The session's screen as its bytes have described it, kept so the
    /// session can be dissolved in and out (see [`crate::shadow`]). `None`
    /// with the fade off or `fade.session` false, and then nothing is parsed.
    shadow: Option<shadow::Shadow>,
    /// A kept client's copy of its screen, where there is no shadow to be one:
    /// fed exactly as the shadow is, by [`Attachment::shadow_saw`], and used
    /// for one thing only — to put the screen back the moment the client
    /// returns to the front (see [`Attachment::paint_kept_screen`]), where
    /// the shadow would have dissolved it in. `None` whenever there is a
    /// shadow, or the client is not kept.
    screen: Option<shadow::Shadow>,
    /// The first paint, while it is being kept back from the terminal so it
    /// can dissolve in. `None` once released, and always without a shadow.
    hold: Option<Hold>,
    /// Where the terminal's parser stands, kept only while there is a notice
    /// to write over the session (see [`crate::boundary`]). `None` the rest of
    /// the time, which is the whole of a relay after its first second and a
    /// bit: the hint bar, the one other thing nvmux writes over a session,
    /// waits for a lull instead and only asks the parser (`mid_sequence`)
    /// while one happens to be there for the notice.
    ///
    /// While it is there, **every byte written to the terminal is shown to it
    /// exactly once, and said whose it is**: [`Attachment::relay_output`]
    /// hands it the session's through `relay`, the notice's frames — nvmux's
    /// own — say `saw_own`, and the released first paint, the session's by
    /// another route, says `saw_session`. A byte shown twice moves the parser
    /// twice; one not shown leaves it describing a terminal that no longer
    /// exists, and the next thing nvmux draws lands inside a sequence.
    boundary: Option<boundary::Boundary>,
    /// A synchronized update nvmux has opened around a relayed write and
    /// still owes the close of. Closed on the pass that opened it unless the
    /// session's bytes ran out mid-sequence, where it is held open rather
    /// than sawn through: see [`Attachment::close_span`].
    span: bool,
    /// What the client has told its terminal that it would not tell it again
    /// (see [`crate::ledger`]). `Some` exactly when clients are kept one per
    /// session (`[client] per_session`), which makes it this client's mark of
    /// being one: a kept client is parked rather than left unread, and comes
    /// back to the front with what it told the terminal put back — see
    /// [`Parked`] and [`Attachment::put_back_what_it_told`].
    ledger: Option<Ledger>,
    /// The terminal has answered this client's attributes request — the last
    /// of the questions a client opens with, which a terminal answers in the
    /// order they were asked. Until it has, the client is never parked (see
    /// [`Attachment::is_parkable`]): cut off before its answers came, which a
    /// switch typed within its first paint does, it would go without them —
    /// no keyboard protocol, no synchronized updates, no in-band sizes — for
    /// as long as it was kept, where a client replaced on the next switch, as
    /// one not kept always is, simply asks again.
    startup_answered: bool,
    /// Its in-band size reports were just turned back on (see
    /// [`Attachment::put_back_what_it_told`]): the size it already has, and
    /// until when the terminal's answer — a report of that same size — is
    /// looked for, to be kept from it (see [`Attachment::saw_input`]).
    expecting_a_size_report: Option<(PtySize, Instant)>,
    /// How many client exits had been relayed to the terminal when this one
    /// was parked, against [`EXITS_RELAYED`] when it comes back: a difference
    /// is some other client's exit sequence having turned off modes this one
    /// set once and relies on (see [`crate::ledger::Ledger::put_back`]).
    exits_seen: u64,
}

/// How many clients have had their exit sequences relayed to the terminal:
/// the ones whose sessions ended while in front. Such a sequence turns off,
/// for the whole terminal, modes every Neovim client sets once and relies on,
/// which a kept client coming back afterwards must have put back.
static EXITS_RELAYED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A session's first paint, held back from the terminal.
///
/// A fade in needs the finished screen before any of it is shown, and a
/// client paints as it likes: startup queries, the alternate screen, then the
/// grid in bursts. So from the start of a fresh relay the client's output is
/// kept here — and shown to the shadow — rather than written, until the
/// screen has been drawn on and the client has been quiet for [`HOLD_SETTLE`].
/// The lull is the whole of the test here, and it is the right one: what this
/// is waiting for is a *finished picture*, which is a question about the
/// client and not about its bytes, so [`crate::boundary`] — which answers a
/// question about the bytes — has nothing to say about it. Failing a lull,
/// [`HOLD_CAP`] after the first byte, so a session that never stops drawing
/// still appears; failing both, [`HOLD_MAX`] bytes, so a hold can never be a
/// leak.
///
/// The release dissolves the shadow in and then writes everything held, in
/// the order it came and inside one synchronized update, so the terminal
/// processes the client's paint as one frame over nvmux's last one. The
/// client's alternate-screen entry is written *before* the frames, so they
/// land on the screen the session will draw on rather than on the primary
/// screen the shell comes back to (see [`Attachment::release_hold`]).
#[derive(Debug)]
struct Hold {
    bytes: Vec<u8>,
    /// When the first byte arrived, or `None` until it has.
    first: Option<Instant>,
    /// When the client last wrote.
    last: Instant,
}

/// How long the client must have been quiet, with something drawn, before
/// its first paint is called complete. The same number as `hint::SETTLE`, and
/// for the same reason given there: a pty whose buffer filled leaves the
/// master unreadable for microseconds in the middle of a frame, and this
/// steps over that gap.
const HOLD_SETTLE: Duration = Duration::from_millis(25);

/// The longest a first paint is held after its first byte. A local session
/// settles in about 100 ms; over a distant SSH forward the grid can take a
/// few round trips to start arriving. A session still drawing at the cap is
/// dissolved in as far as it has got, and finishes live.
const HOLD_CAP: Duration = Duration::from_millis(750);

/// The most a hold keeps before releasing regardless. Far past any first
/// paint — a full 200×50 grid in truecolor is a few tens of KB.
const HOLD_MAX: usize = 1 << 20;

/// The alternate-screen entry a client sends before it draws. The one
/// sequence a hold looks for, as a fixed string like the DA1 request below.
const ALT_SCREEN_ENTER: &[u8] = b"\x1b[?1049h";

/// And its exit, which a parked client sends only on its way out.
const ALT_SCREEN_LEAVE: &[u8] = b"\x1b[?1049l";

impl Hold {
    fn new(now: Instant) -> Self {
        Self {
            bytes: Vec::new(),
            first: None,
            last: now,
        }
    }

    /// Keep a chunk the client wrote.
    fn take(&mut self, chunk: &[u8], now: Instant) {
        self.bytes.extend_from_slice(chunk);
        self.first.get_or_insert(now);
        self.last = now;
    }

    /// When the relay must next wake up on the hold's account, once there is
    /// something to wait on. `drawn` is whether the shadow has a glyph yet:
    /// until it has, a lull releases nothing, so waking for one would spin
    /// the relay on a `poll` that returns at once until the cap.
    fn wake_at(&self, drawn: bool) -> Option<Instant> {
        let first = self.first?;
        let cap = first + HOLD_CAP;
        Some(if drawn {
            (self.last + HOLD_SETTLE).min(cap)
        } else {
            cap
        })
    }

    /// Whether the paint is over: drawn on and quiet, or held long enough, or
    /// large enough. `drawn` is whether the shadow has a glyph anywhere yet.
    fn due(&self, now: Instant, drawn: bool) -> bool {
        let Some(first) = self.first else {
            return false;
        };
        now >= first + HOLD_CAP
            || self.bytes.len() >= HOLD_MAX
            || (drawn && now >= self.last + HOLD_SETTLE)
    }
}

/// Where the held bytes split for the release: just past the client's
/// alternate-screen entry, or at the start if it never sent one — a client
/// whose `TERM` has no `smcup` draws on the primary screen, and so do the
/// frames then.
fn split_at_alt_screen_entry(bytes: &[u8]) -> usize {
    bytes
        .windows(ALT_SCREEN_ENTER.len())
        .position(|w| w == ALT_SCREEN_ENTER)
        .map_or(0, |i| i + ALT_SCREEN_ENTER.len())
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

/// How long the teardown paths relay a departing client's restore sequence.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// How long the session must have been quiet before a switch is called painted.
///
/// Diagnostics only, for the `timing:` record in [`pump`]. Long enough to step
/// over the gaps between a heavy config's redraw chunks — an AstroNvim session
/// on a 200x50 terminal paints in bursts over about 100 ms — and short enough
/// that the record lands while the switch it describes is still the last thing
/// that happened.
const PAINT_SETTLED: Duration = Duration::from_millis(150);

/// A DA1 request: `ESC [ c`. What a departing client sends, and what the two
/// drains watch for — see [`Attachment::answer_device_attributes`].
const DA1_REQUEST: &[u8] = b"\x1b[c";

/// What a terminal answers a DA1 request with, and what nvmux answers one with
/// in a terminal's place.
///
/// "VT100 with an advanced video option", which every terminal emulator in
/// existence can claim to be. The content is not what is being asked for: the
/// client asks at shutdown only to learn that its terminal has worked through
/// everything it sent, so the arrival is the whole of the message.
const DA1_REPLY: &[u8] = b"\x1b[?1;2c";

/// The longest a kept client being resumed is given to finish a frame it
/// was caught in the middle of (see `Attachment::drain_unrelayed`). A frame
/// is written in one go, so the rest is microseconds behind; this is only the
/// bound on a client that stopped half way through one for some other reason.
const FRAME_WAIT: Duration = Duration::from_millis(50);

/// The most a resume reads of a kept client's backlog before handing the rest
/// to the relay, once it has a whole frame: a parked client is read all the
/// while it is parked, so anything more than a few frames is a client that is
/// still writing, not one that is behind.
const DRAIN_MAX: usize = 64 * 1024;

/// How long after a resume turns a kept client's in-band size reports back
/// on the terminal's answer is looked for (see `Attachment::saw_input`). A
/// terminal answers as soon as it reads the mode; this only bounds how long
/// one that does not leaves a report of the same size to be kept back.
const SIZE_REPORT_WAIT: Duration = Duration::from_secs(1);

/// The report a terminal with in-band resize reports on (DEC mode 2048)
/// sends when its size changes: `CSI 48 ; rows ; cols ; height ; width t`,
/// the last two in pixels, which a terminal that does not know them reports as
/// zero — as `PtySize` does.
fn in_band_size_report(size: PtySize) -> Vec<u8> {
    format!(
        "\x1b[48;{};{};{};{}t",
        size.rows, size.cols, size.pixel_height, size.pixel_width
    )
    .into_bytes()
}

impl Attachment {
    /// Say on the notice that the session is waiting for a key.
    ///
    /// A blocked session draws nothing until one arrives, so what the user is
    /// handed is a screen that looks broken — which is exactly how the bug
    /// this exists for was reported. The notice is nvmux's own box, written
    /// over the session's screen rather than by the editor, so it goes up on a
    /// wedged session as readily as an idle one (see [`crate::announce`]) and
    /// is the one thing that *can* be said here.
    ///
    /// Set between the probe and the relay, which is the only point at which
    /// anyone knows: the label is chosen when the client is forked, a moment
    /// before the probe has an answer. A no-op once the notice has been shown,
    /// like everything else that reads `announce`.
    pub fn note_waiting_for_a_key(&mut self) {
        let Some(label) = self.announce.take() else {
            return;
        };
        // Idempotent, so a caller that says it twice does not stutter. One
        // attach only ever says it once; this is so that stays a property of
        // the function rather than of every caller.
        self.announce = Some(if label.ends_with(WAITING) {
            label
        } else {
            format!("{label}{WAITING}")
        });
    }

    /// Terminate the client, leaving the session's server running. Verified:
    /// killing a `--remote-ui` client — with SIGHUP, SIGTERM or SIGKILL — does
    /// not kill a `--headless --listen` server; the "channel closes, Nvim
    /// exits" rule is scoped to `--embed`.
    pub fn terminate(self) {
        // `Drop` does the work, so every path that lets go of a live client —
        // an explicit terminate, an early `?`, a `break` out of the session
        // loop — retires it the same way.
        drop(self);
    }

    /// Send the client its hangup and return *without waiting for it to go*, so
    /// the caller can get on with something — spawning the replacement — while
    /// it dies. SIGHUP is what the client would get if the terminal itself went
    /// away. `reap` does the waiting and `Drop` calls it, so an attachment must
    /// still be held until the client is genuinely wanted gone.
    ///
    /// Through `libc` rather than `portable_pty`'s `Child::kill`, which is not a
    /// signal but a whole teardown: it sends SIGHUP and then blocks polling for
    /// the exit — measured at 50 ms for a client that goes quietly, 200 ms
    /// before it gives up and escalates to SIGKILL. That is `reap`'s job, and a
    /// hangup that blocks leaves the switch path nothing to overlap. Its poll
    /// also *reaped* the child, which is why `reap` used to return on its first
    /// look and why `reap`'s own escalation had never run.
    ///
    /// SIGCONT as well, because SIGHUP is not delivered to a *stopped* process
    /// at all, and a client that stopped itself (see `sys::Pty::check`) can be
    /// retired before the pump's idle poll has revived it. `reap`'s `try_wait`
    /// does not pass `WUNTRACED` either, so without this a stopped client would
    /// read as running for the whole of `REAP_TIMEOUT`. Hung up first and
    /// continued second, it takes the pending hangup and goes.
    ///
    /// Sending it twice is harmless, which is what lets `Drop` run after an
    /// explicit `hang_up`: nothing in between waits for the child — the peek in
    /// `sys::Pty::check` passes `WNOWAIT` — so the pid is still ours and cannot have
    /// been recycled.
    ///
    /// On Windows there is no hangup a console process would act on and still
    /// go quickly, and the client is ended outright — which leaves its server
    /// running just the same (see `sys::windows::conpty::Pty::hang_up`).
    pub fn hang_up(&mut self) {
        if self.reaped {
            return;
        }
        self.pty.hang_up();
    }

    /// Write to the terminal, then let the shadow see what was written.
    ///
    /// The plain path for the session's bytes — a relay with no notice up and
    /// no first paint held — and for the tails the boundary lets go of
    /// ([`Attachment::release_the_sessions_close`],
    /// [`Attachment::release_kept`]). The two other paths write the session's
    /// bytes themselves and show the shadow the same bytes:
    /// [`Attachment::write_session_frame`] wraps a relayed piece in a
    /// synchronized update and feeds the shadow after the write (or, for a
    /// client that brackets its own frames, comes back through here), and a
    /// held first paint is fed to the shadow as it is held
    /// ([`Attachment::relay_output`]) and written later by
    /// [`Attachment::release_hold`]. Across all three the shadow sees every
    /// byte of the session's screen exactly once — the same rule the boundary
    /// lives by — so what it holds is what the client drew. Here the write
    /// comes first and the shadow after: nothing about the shadow may delay or
    /// change what the user sees.
    ///
    /// nvmux's own notice does not come through here — see
    /// [`Attachment::write_over_session`].
    fn write_terminal(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        write_stdout(bytes)?;
        self.shadow_saw(bytes);
        Ok(())
    }

    /// Write something of nvmux's own over the session's screen: the attach
    /// notice, and the hint bar the prefix puts up ([`crate::hint`]).
    ///
    /// The shadow must not take these for the session's. Its grid is what the
    /// *client* drew, not what the terminal shows, and keeping it that way is
    /// exactly what lets the notice dissolve into the screen instead of into a
    /// hole — the cells it is covering are still there to be given back (see
    /// [`crate::shadow::Shadow::under`]). Shown the notice, the shadow would
    /// hold the box and have nothing underneath it to restore.
    ///
    /// The terminal has still been written to behind the shadow's back, so the
    /// diff its next frame would trust no longer matches the screen and has to
    /// go — the same reason [`Attachment::release_hold`] invalidates after
    /// replaying what it held.
    ///
    /// The boundary *is* shown them, because its subject is the terminal
    /// rather than the session (see [`Attachment::boundary`]).
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

    /// Let the shadow see bytes something else wrote to the terminal — the
    /// hand-off strings [`crate::term`] writes on the client's behalf.
    fn shadow_saw(&mut self, bytes: &[u8]) {
        if let Some(shadow) = self.shadow.as_mut() {
            shadow.feed(bytes);
        }
        if let Some(screen) = self.screen.as_mut() {
            screen.feed(bytes);
        }
    }

    /// Let the ledger see what the client wrote — every byte of it, once,
    /// whichever way it went: relayed, held as a first paint, or read while
    /// parked.
    fn ledger_saw(&mut self, bytes: &[u8]) {
        if let Some(ledger) = self.ledger.as_mut() {
            ledger.saw(bytes);
        }
    }

    /// Note what the user's terminal sends the client, for the two things a
    /// kept client needs to know of it: that its opening questions have been
    /// answered (see [`Attachment::startup_answered`]), and the size report a
    /// resume set off — which is taken out.
    ///
    /// A terminal answers in-band size reports being turned back on with a
    /// report of the size it has, which is the size the client has too.
    /// Passed on, it has the client ask its server to resize to that same
    /// size, and a server in `input()` or `getchar()` answers that with a
    /// clear and nothing but the prompt's row — after the `:mode` the resume
    /// asked for, as often as not, which is then wiped. Kept from the client
    /// it changes nothing: a size that changed while the client was parked it
    /// has been told already (see [`Attachment::tell_its_size`]). A report of
    /// any other size is a resize made since, and goes through.
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

    /// Whether this client is kept one per session (`[client] per_session`):
    /// parked rather than left unread, and resumed with its screen painted
    /// straight back.
    pub fn is_kept(&self) -> bool {
        self.ledger.is_some()
    }

    /// Whether this client may be parked: kept, and past the handshake a
    /// client opens with (see [`Attachment::startup_answered`]).
    pub fn is_parkable(&self) -> bool {
        self.is_kept() && self.startup_answered
    }

    /// Say on the next relay which session this is, as a fresh client does:
    /// `label` is what [`announce::label`] made of the session's name, the one
    /// place a name is made safe to reach a raw terminal.
    ///
    /// For a kept client coming back to the front in place of another
    /// session's: that is as much a change of session as a spawn, and the
    /// label is the session's name as it is now, which a rename since the
    /// client started may have changed.
    pub fn announce_on_arrival(&mut self, label: String) {
        self.announce = Some(label);
    }

    /// Read whatever the client has written since anything last did, and show
    /// it to the shadow and the ledger rather than to the terminal.
    ///
    /// For a kept client being resumed, in place of discarding it: those
    /// bytes describe the screen the client believes the terminal has — which
    /// is the one about to be painted back — and any word it said in them
    /// about the mouse or the title is one it will not say again.
    ///
    /// And a client caught half way through a frame is let finish it here,
    /// for up to [`FRAME_WAIT`]: the half already read went to the shadow,
    /// not the terminal, so relaying the other half would hand the terminal
    /// the tail of a sequence whose head it never saw — printed as text until
    /// the repaint clears it. Only a client that was busy while parked — a
    /// `:terminal` running something — is ever caught like that, and Neovim
    /// writes a frame in one go, so the rest is already on its way.
    ///
    /// Bounded either way, by [`FRAME_WAIT`] and [`DRAIN_MAX`]: a client
    /// parked with a `:terminal` running something never runs out of things
    /// to say, and what it says after this is the relay's.
    fn drain_unrelayed(&mut self) {
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
            match self.pty.wait_output(wait) {
                Ok(true) => {}
                // The relay's `SIGWINCH` handler is in by now, and a resize
                // during the wait is no reason to hand over half a frame.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                _ => return,
            }
            match self.pty.read_output(&mut buf) {
                Ok(len) if len > 0 => {
                    self.saw_unrelayed(&buf[..len]);
                    taken += len;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                _ => return,
            }
        }
    }

    /// What a client wrote that did not reach the terminal, and never will:
    /// the shadow and the ledger keep up with it all the same, so the one
    /// knows the screen the client will repaint and the other what it told
    /// the terminal on the way.
    fn saw_unrelayed(&mut self, bytes: &[u8]) {
        self.shadow_saw(bytes);
        self.ledger_saw(bytes);
    }

    /// Put back what the client told the terminal and would not tell it
    /// again, before it draws on it: its mouse reporting, its title, its
    /// keyboard protocol and its cursor's shape and colour — and, if another
    /// client's exit has reached the terminal since this one was parked, the
    /// modes that exit turned off (see [`crate::ledger::Ledger::put_back`]).
    /// `size` is the size the client has now, the one the terminal answers
    /// in-band size reports turned back on with (see
    /// [`Attachment::saw_input`]).
    ///
    /// For a kept client's return, where it is exact — the ledger holds what
    /// the client itself last wrote. A client that is not kept has its
    /// server asked about the mouse instead, and the rest left alone (see
    /// [`repaint_through_server`]).
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

    /// Tell a kept client coming back to the front the size the terminal
    /// has now, if it changed while the client was parked.
    ///
    /// Usually the pty is enough: `resize_to` gives it the new size and the
    /// kernel signals the client. Not for a client that has turned in-band
    /// resize reports on (DEC mode 2048: kitty, ghostty, foot) — it takes its
    /// size from those and ignores `SIGWINCH` altogether, and the reports the
    /// terminal sent while it was parked went to whatever was in front. So it
    /// is sent the report the terminal would have sent, written to its input
    /// in one write. Measured on 0.11.0 and 0.12.5: taken as a size, never as
    /// keys, by a client that turned the mode on — the ledger says which kind
    /// of client this is, from its own output.
    ///
    /// Only for a size that changed. The repaint is asked of the server with
    /// `:mode` all the same (see [`relay`]): a client told its size asks its
    /// server to resize, and a server answers a resize to the size it already
    /// has with a clear and — in `input()` or `getchar()` — nothing but the
    /// prompt's row, where `:mode` draws every row.
    fn tell_its_size(&mut self, size: PtySize, resized: bool) {
        if resized && self.ledger.as_ref().is_some_and(Ledger::reads_size_in_band) {
            self.report_size_in_band(size);
        }
    }

    /// Write an in-band size report into the client's input — `CSI 48 ; rows
    /// ; cols ; height px ; width px t`, what a terminal with DEC mode 2048
    /// sends on a resize — and say whether it went out whole.
    ///
    /// Only to a client that turned the mode on, which is one that reads this
    /// as a report and not as keys. One `write` and never a `write_all`, for
    /// the reason [`Attachment::answer_device_attributes`] gives: a report in
    /// pieces *would* be read as keys, and a report that did not fit is
    /// better not sent.
    fn report_size_in_band(&mut self, size: PtySize) -> bool {
        let report = in_band_size_report(size);
        match self.pty.write_now(&report) {
            None => {
                tracing::debug!("the client's input queue is full; not reporting its size");
                false
            }
            Some(n) if n != report.len() as isize => {
                tracing::debug!(wrote = n, "the in-band size report went out short");
                false
            }
            Some(_) => true,
        }
    }

    /// Show the terminal what the client wrote — or, while its first paint is
    /// held, keep it and show only the shadow.
    ///
    /// While a notice is up the chunk goes through the boundary first, which
    /// keeps back whatever would leave the terminal's parser inside a
    /// sequence. A few bytes, for as long as it takes the client to write the
    /// rest of them, and [`crate::boundary`] says why it is worth it: it is
    /// what lets the notice go back up after *every* write rather than after
    /// the one read in six that happens to end where a sequence does.
    fn relay_output(&mut self, chunk: &[u8], now: Instant) -> std::io::Result<()> {
        self.ledger_saw(chunk);
        if let Some(hold) = self.hold.as_mut() {
            hold.take(chunk, now);
            if let Some(shadow) = self.shadow.as_mut() {
                shadow.feed(chunk);
            }
            return Ok(());
        }
        if let Some(boundary) = self.boundary.as_mut() {
            let ready = boundary.relay(chunk, now);
            // A read that was all tail leaves nothing to do. Not merely a
            // wasted write: the shadow throws its diff away on every feed, so
            // an empty one would cost the next session fade a full repaint.
            if ready.is_empty() {
                return Ok(());
            }
            let wrap = !boundary.session_brackets_its_own_frames();
            return self.write_session_frame(&ready, wrap);
        }
        self.write_terminal(chunk)
    }

    /// Relay a piece of the session's screen with the notice's own frame
    /// following it inside one synchronized update.
    ///
    /// **This is what keeps the notice on the screen rather than merely
    /// putting it back there.** Writing the box after the session's repaint
    /// wins nothing on its own: a terminal parses asynchronously and presents
    /// on its own clock, so what it puts on the glass is whatever prefix of
    /// the stream it has reached — and most of those prefixes fall in the
    /// middle of a repaint, with the box wiped and the rewrite not yet
    /// arrived. Measured on a flight with a starfield in a `:terminal`
    /// buffer: the whole box stood in 10% of them.
    ///
    /// A synchronized update takes the terminal's choice away. Between
    /// `SYNC_BEGIN` and `SYNC_END` it presents nothing, so the only states it
    /// can show are the ones nvmux ends a span on — and nvmux ends every one
    /// of them with the notice. The span is opened here, in front of the
    /// session's bytes, and closed by [`Attachment::close_span`] before this
    /// pass of the relay ends, whether or not a frame of the notice went in
    /// between.
    ///
    /// `wrap` is false for a client that brackets its own frames, where a
    /// span of nvmux's would end at the client's reset rather than at its
    /// own; see [`boundary::Boundary::session_brackets_its_own_frames`]. A
    /// terminal that does not know `?2026` at all ignores both sequences and
    /// gets what it got before any of this, which is the honest fallback:
    /// there is no way to hold a frame back from a terminal that will not
    /// hold one.
    ///
    /// The span's own two sequences are deliberately not shown to the
    /// boundary — they are nvmux's rather than the session's, and the reason
    /// is written down on [`boundary::Boundary`].
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

    /// Write the session's own frame close, held back so the notice could be
    /// put in front of it.
    ///
    /// Called on every pass that could have held one, right after the notice
    /// has had its chance at the screen — so the sequence is delayed by one
    /// write of nvmux's and by nothing else, and the frame the session
    /// presents is the frame with the box in it. See
    /// [`boundary::Boundary::holding_the_sessions_close`].
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

    /// Close a synchronized update nvmux opened, if it has one open and the
    /// terminal is in a state to be written to.
    ///
    /// Called at the end of every pass of the relay that could have opened
    /// one, so a span does not outlive the pass that began it — a terminal
    /// left inside one shows nothing until its own timeout runs out, which is
    /// the one way this mechanism could be worse than no mechanism.
    ///
    /// Unconditional rather than conditional on the notice having painted:
    /// the notice's own frame ends in a reset of its own, which is what
    /// actually presents the frame with the box in it, and a second reset of
    /// a mode already reset is nothing at all. Leaning on the notice's byte
    /// shape instead would make this correct only by coincidence.
    ///
    /// The one thing it does wait for is the session finishing what it was
    /// saying. These two sequences are nvmux's bytes like any other, and a
    /// tail that ran out of patience mid-character hands the relay a write
    /// that ends between a lead byte and its continuation — where a reset
    /// dropped in leaves the terminal a character it cannot read and the rest
    /// of one it never asked for. Measured over eight flights before it was
    /// noticed: up to six such characters in one, in both colour paths.
    /// Holding the span open across the pass instead costs the terminal one
    /// more pass of not presenting, which is the wait it was opened for.
    fn close_span(&mut self) -> std::io::Result<()> {
        if !self.span || self.mid_sequence() {
            return Ok(());
        }
        self.span = false;
        write_stdout(fade::SYNC_END)
    }

    /// Whether the session is half way through a character or a sequence, so
    /// nothing of nvmux's may be written at all.
    fn mid_sequence(&self) -> bool {
        self.boundary
            .as_ref()
            .is_some_and(boundary::Boundary::mid_sequence)
    }

    /// Whether nvmux may write over the session right now: the terminal is
    /// between the session's sequences, and its first paint is not still being
    /// held back from it.
    fn between_sequences(&self) -> bool {
        self.hold.is_none()
            && self
                .boundary
                .as_ref()
                .is_some_and(boundary::Boundary::between_sequences)
    }

    /// When a tail kept back for the notice's sake must go to the terminal
    /// whether or not the rest of it has arrived.
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

    /// The notice is over. The hint bar still writes over a session, but it
    /// waits for a lull rather than for the parser, so the terminal's parser
    /// need not be followed any further — but every byte still being kept on
    /// its account goes out first, or a client's frame would end a few bytes
    /// short for the rest of the relay.
    fn stop_watching_the_terminal(&mut self) -> std::io::Result<()> {
        self.close_span()?;
        self.release_the_sessions_close()?;
        self.release_kept()?;
        // And a span the session's unfinished business held open goes now
        // whatever state that business is in: there is no next pass to close
        // it on, and a terminal left inside one shows nothing until its own
        // timeout runs out. At worst that is one character cut in half, once,
        // at the end of a notice — against a picture frozen for seconds.
        if std::mem::take(&mut self.span) {
            write_stdout(fade::SYNC_END)?;
        }
        self.boundary = None;
        Ok(())
    }

    /// Whether the shadow has a glyph anywhere yet — what a lull needs before
    /// it counts as the first paint being over.
    fn drawn(&self) -> bool {
        self.shadow
            .as_ref()
            .is_some_and(shadow::Shadow::has_contents)
    }

    /// Whether the held first paint is over and should be released now.
    fn hold_is_due(&self, now: Instant) -> bool {
        let drawn = self.drawn();
        self.hold.as_ref().is_some_and(|hold| hold.due(now, drawn))
    }

    /// When the relay must next wake up for the held first paint, if it is
    /// holding one.
    fn hold_wake_at(&self) -> Option<Instant> {
        let drawn = self.drawn();
        self.hold.as_ref().and_then(|hold| hold.wake_at(drawn))
    }

    /// Let the held first paint through: dissolve the shadow in, when asked
    /// to and there is something drawn to dissolve, then write everything
    /// held exactly as it came. Harmless without a hold.
    ///
    /// The bytes up to the client's alternate-screen entry go first and the
    /// frames after, so they are drawn on the screen the client is about to
    /// draw on; the rest is written inside one synchronized update, so the
    /// client's own clear and paint replace the last frame in one step.
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
        if dissolve && drawn {
            write_stdout(&bytes[..split])?;
            if let Some(shadow) = self.shadow.as_mut() {
                // A frame that could not be written is not a reason to keep
                // the paint: the replay below is what the user is waiting for.
                dissolved = match fade::fade_in_session(shadow) {
                    Ok(faded) => faded,
                    Err(e) => {
                        tracing::debug!(error = %e, "the session's fade-in did not complete");
                        true
                    }
                };
            }
        }
        let mut out = std::io::stdout().lock();
        out.write_all(fade::SYNC_BEGIN)?;
        out.write_all(&bytes[split..])?;
        out.write_all(fade::SYNC_END)?;
        out.flush()?;
        drop(out);
        if let Some(shadow) = self.shadow.as_mut() {
            shadow.invalidate();
        }
        // The held bytes are the only ones here that can leave the terminal's
        // parser anywhere: the dissolve's frames and the synchronized update
        // around the replay are nvmux's own and each ends where it began, and
        // the split falls just past a whole `?1049h`.
        if let Some(boundary) = self.boundary.as_mut() {
            boundary.saw_session(&bytes);
        }
        tracing::debug!(
            held_ms = held_for.map(|d| d.as_secs_f64() * 1000.0),
            bytes = bytes.len(),
            dissolved,
            "timing: first paint held"
        );
        Ok(())
    }

    /// Dissolve the screen the shadow holds in from the background — for a
    /// resumed client, whose last screen the shadow still has — ending with
    /// the cursor back on the cell that screen had it on. Says whether it
    /// did; if not, the terminal is as the hand-off left it, cursor included.
    fn dissolve_in(&mut self) -> bool {
        let Some(shadow) = self.shadow.as_mut().filter(|s| s.has_contents()) else {
            return false;
        };
        match fade::fade_in_session(shadow) {
            Ok(faded) => faded,
            Err(e) => {
                tracing::debug!(error = %e, "the resumed session's fade-in did not complete");
                true
            }
        }
    }

    /// Tell the pty — and the shadow — the terminal's size.
    fn resize_to(&mut self, size: PtySize) {
        self.pty.resize(size);
        if let Some(shadow) = self.shadow.as_mut() {
            shadow.resize(size.rows, size.cols);
        }
        if let Some(screen) = self.screen.as_mut() {
            screen.resize(size.rows, size.cols);
        }
    }

    /// Leave the terminal's pen and cursor where the client's own last write
    /// left them, after a fade in that did not: see
    /// [`shadow::Shadow::hand_back`].
    fn hand_back_the_pen(&self) {
        let Some(shadow) = self.shadow.as_ref().filter(|s| s.is_usable()) else {
            return;
        };
        if let Err(e) = write_stdout(&shadow.hand_back()) {
            tracing::debug!(error = %e, "could not hand the client's pen back");
        }
    }

    /// Put a kept client's screen back on the terminal at once, from its copy
    /// of it, where there is no fade to dissolve it in. Says whether it did;
    /// if not, the terminal is as the hand-off left it.
    ///
    /// What comes back is the screen as the client last drew it — kept
    /// current while it was parked — in the colours and attributes it drew it
    /// in (see [`shadow::Shadow::paint`]). The server's own repaint, asked for
    /// next, lands on top of it a few round trips later; until then, and for
    /// as long as a busy server or a prompt waiting for a key holds that
    /// repaint up, the user is looking at the session rather than at a blank
    /// screen.
    fn paint_kept_screen(&mut self) -> bool {
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

    /// Wait for the client to be gone, escalating to SIGKILL after
    /// [`REAP_TIMEOUT`] so a client that ignores SIGHUP cannot hold nvmux —
    /// and the user's terminal — hostage.
    ///
    /// All of the waiting and all of the escalation is here. It used to be
    /// shared with `portable_pty`'s `Child::kill`, which reaped the child inside
    /// the hangup and reached SIGKILL first, so this loop returned on its first
    /// poll and the escalation below never ran.
    fn reap(&mut self) {
        if self.reaped {
            return;
        }
        self.reaped = true;
        // Read the client out while waiting for it, and throw it away. What it
        // writes on the way out describes a screen that has already been cleared
        // or restored, so none of it is wanted — but it has to be *read*, because
        // a client parked in `write` on a master nobody drains never reaches its
        // own signal handler. That is not hypothetical on the switch path: the
        // picker leaves the outgoing client unread for as long as it is up, and a
        // client that was mid-repaint when the switch was typed fills the buffer
        // and stops there. Without this it cannot act on the hangup at all, and
        // a healthy client ends up killed by the deadline below.
        let deadline = Instant::now() + REAP_TIMEOUT;
        // Remembered rather than answered once: the request is seen on exactly
        // one pass of this loop, and that pass is the likeliest moment for the
        // client's input queue to be momentarily full.
        let mut owed = false;
        while Instant::now() < deadline {
            let mut asked = false;
            self.pty
                .discard_pending(|chunk| asked |= asks_for_device_attributes(chunk));
            owed |= asked;
            if owed {
                owed = !self.answer_device_attributes();
            }
            // Exited, or already reaped by someone else.
            if self.pty.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(pid) = self.pty.pid() {
            tracing::warn!(pid, "the remote-ui client ignored SIGHUP; killing it");
            self.pty.kill();
        }
        self.pty.wait();
    }

    /// Answer the DA1 request a departing client has just sent, as its terminal
    /// would have.
    ///
    /// **This is worth a full second on every switch.** Neovim's shutdown path
    /// (`tui.c`, `tui_stop`) emits its terminal restore sequences, sends `ESC [
    /// c`, and then *waits* for the reply — `EXIT_TIMEOUT_MS`, one second —
    /// before it will exit, so that it knows the terminal has processed
    /// everything it sent. A client nvmux is retiring has no terminal left to
    /// ask: nothing relays its output once the switch is typed, and a reply
    /// could not reach it through a stopped pump in any case. So it waits out
    /// the whole timeout, every time. Measured at 1003 ms, and it is the same
    /// second whether the client is asked to leave with SIGHUP or with
    /// `:detach` — both arrive at `tui_stop`.
    ///
    /// nvmux answers because, for this client, nvmux *is* the terminal. It then
    /// goes in about 4 ms.
    ///
    /// Only ever in reply to a request that was actually read, and never
    /// speculatively at hangup: a reply written before the client's shutdown
    /// installs the callback waiting for it is consumed by the startup handler
    /// instead, and the second is paid anyway. Measured both ways — answering
    /// on sight is reliable, answering at hangup is a race that loses about as
    /// often as it wins.
    ///
    /// One `write_all`, and only ever to a client that has been hung up. A
    /// reply that reached a live client would be harmless — Neovim's input
    /// parser consumes a DA1 response rather than typing it, in insert mode
    /// too, which was checked — but one delivered in *pieces* is read as keys,
    /// and `ESC [ ? 1 ;` followed later by `2 c` leaves the editor in
    /// operator-pending. Seven bytes in one write to a pty cannot be torn.
    fn answer_device_attributes(&mut self) -> bool {
        // Asked first, and skipped rather than waited for.
        //
        // What this is guarding: both callers run from `Drop`, and neither may
        // block. `reap` cannot reach its own deadline from inside a parked
        // write, and `drain_until_eof` holds the user's terminal in raw mode
        // while it runs — so a park in either is an nvmux that never comes back.
        // A pty master does stop accepting bytes: measured, a sustained writer
        // stalls indefinitely once the slave's queue fills and nothing reads it.
        //
        // What it is *not*: a fix for an observed hang. This reply is seven
        // bytes, and seven bytes were never seen to block — a line discipline
        // with no reader discards what it has no room for, so the room comes
        // back within milliseconds whatever the client does (measured: full,
        // then writable again 50 ms later, with the client asleep). It is the
        // cost of the guard that justifies it — a `poll` with a zero timeout —
        // against a wait that has no bound at all if it ever does happen.
        //
        // The cost of skipping is the second this saves — the client waits out
        // `EXIT_TIMEOUT_MS` as it did before the answer existed — so this
        // reports whether the reply went out and both callers, which are loops,
        // carry the debt to their next pass. Retried, never waited for.
        //
        // And the room can be missing for a moment without the client being
        // wedged at all: the Linux tty layer stages a write in a flip buffer and
        // moves it on from a work queue, so a master that has just taken a burst
        // reports no room until that queue runs, a few milliseconds later. A
        // one-shot answer would lose that coin toss; a retried one cannot.
        //
        // One `write`, not `write_all`: `writable` promises room for a byte,
        // not for seven, and the loop `write_all` would do on a short write is
        // the same park by another name. A torn reply cannot hurt this client —
        // it is exiting, and the worst case is the wait it would have had
        // anyway. (Which is why no live client is ever written to here.)
        let Some(n) = self.pty.write_now(DA1_REPLY) else {
            tracing::debug!("the departing client's input queue is full; not answering yet");
            return false;
        };
        tracing::debug!("answered the departing client's DA1 request");
        if n != DA1_REPLY.len() as isize {
            tracing::debug!(wrote = n, "the DA1 answer went out short");
        }
        true
    }

    /// Relay whatever the client writes on its way out, then stop.
    ///
    /// The teardown counterpart to the drain in [`Attachment::reap`], and the
    /// opposite policy: here the client's restore sequence *is* wanted, because
    /// this runs on the paths that hand the terminal back to the shell.
    ///
    /// The DA1 request buried in that sequence still has to be answered, and
    /// nvmux answers it, as on a switch: the pump that would carry a terminal's
    /// reply back to the client has already stopped. Without this, `<prefix> d`
    /// costs the same second a switch used to.
    ///
    /// The request itself is the one thing taken *out* of what is relayed —
    /// the single exception to the module's rule, and only on this path. Left
    /// in, it reaches the real terminal, which answers it, and nobody reads
    /// that answer: the client has gone on nvmux's reply, and nvmux is out of
    /// raw mode and gone a few milliseconds later. The reply then lands in the
    /// shell as typed text, `^[[?61;4;6;…c` above the next prompt — seen, on
    /// a terminal slow enough to answer after nvmux had exited, and possible on
    /// any. A request split across two reads is missed here as it is by
    /// `asks_for_device_attributes`, with the same consequence as before this
    /// existed, and for the same reason it is survivable.
    ///
    /// Bounded, because a client that ignores its termination would otherwise
    /// hold the terminal in raw mode indefinitely.
    fn drain_until_eof(&mut self) {
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        let mut buf = [0u8; 8192];
        let mut owed = false;
        while Instant::now() < deadline {
            if !matches!(self.pty.wait_output(Duration::from_millis(50)), Ok(true)) {
                continue;
            }
            match self.pty.read_output(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(len) => {
                    // Read before the write, because the write borrows `self`.
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

/// The client must be dead before the PTY writer is dropped: `portable-pty`'s
/// writer writes a newline and `VEOF` to the master on drop, and a client that
/// is still attached would deliver both to the editor as keystrokes — an Enter
/// in insert mode is a new line in the user's buffer. Fields drop after this
/// runs, so the writer only ever goes out on a pty nobody is reading.
impl Drop for Attachment {
    fn drop(&mut self) {
        self.hang_up();
        self.reap();
    }
}

impl Attachment {
    /// Put a kept client aside while something else has the front: see
    /// [`Parked`]. `None` if it could not be parked — no stop channel or no
    /// thread to be had for it — in which case it has been retired and the
    /// next attach to its session is a fresh one.
    pub fn park(mut self) -> Option<Parked> {
        self.exits_seen = EXITS_RELAYED.load(std::sync::atomic::Ordering::Relaxed);
        let session_id = self.session_id.clone();
        let (stop, stopped) = match sys::stop_pair() {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "could not park a client; retiring it");
                return None;
            }
        };
        let thread = std::thread::Builder::new()
            .name("nvmux-parked".into())
            .spawn(move || keep_parked(self, stopped));
        match thread {
            Ok(thread) => Some(Parked {
                session_id,
                stop: Some(stop),
                thread: Some(thread),
            }),
            // The closure, and the client in it, went with the failed spawn:
            // dropped, so hung up and reaped.
            Err(e) => {
                tracing::warn!(error = %e, "could not park a client; retired it");
                None
            }
        }
    }
}

/// A kept client that is not in front, on a thread of its own until it is
/// wanted back.
///
/// With `[client] per_session` a client that leaves the front — for the
/// picker, the prompt, the help, or another session — is not retired and not
/// left unread. It stays attached to its server, which goes on sending it the
/// session's screen; and something has to read what it writes meanwhile, or
/// it stops at the pty's buffer while its server keeps every frame for it in
/// memory, to be played back, stale, when it is next read.
///
/// That something is a thread that owns the whole [`Attachment`]. Owns, and
/// not borrows, because ownership is what makes it true that one reader, and
/// only one, is on a pty at any time — the rule [`relay`] is built on: the
/// relay cannot have the master back until the thread has handed it over,
/// which it does by ending. What it reads goes to the shadow (or the kept
/// screen) and the ledger, never to the terminal — so that, wanted back, the
/// screen it last had can go straight onto the terminal.
///
/// A client that leaves while parked — its session killed from the picker,
/// quit from another UI, its ssh link gone — is retired on the thread and
/// not handed back. Nothing else would notice: no transport call reports it.
pub struct Parked {
    session_id: String,
    /// Dropped to stop the thread: the far end then reads as hung up.
    stop: Option<sys::StopTx>,
    /// `Some` until joined. What it returns is the client, or `None` for one
    /// that left.
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
    /// Which session this client is attached to.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Whether the client is still there to be had back. The thread ends only
    /// when told to or when the client has left, and only one of those can
    /// have happened to a `Parked` nobody has taken back yet.
    pub fn is_alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Have the client back, ready to be resumed — or `None` if it left while
    /// parked. The thread notices at once — it is in a `poll` on the other end
    /// of `stop` — unless it is already retiring a client that was leaving,
    /// which is its business and not worth waiting on: past [`UNPARK_WAIT`]
    /// this gives up on it, and the thread finishes the retirement on its own.
    pub fn unpark(mut self) -> Option<Attachment> {
        drop(self.stop.take());
        let attachment = self.join_within(UNPARK_WAIT)?;
        // One that left in the moment before it was asked back — its session
        // killed, its link gone — before the thread saw it go: the pty says
        // so at once, and so does its pid. Handed back, it would put a dead
        // session's last screen up only to lose it a moment later.
        let hung_up = attachment.pty.hung_up();
        let exited = attachment.pty.peek() == Peek::Gone;
        if hung_up || exited {
            tracing::debug!(id = %self.session_id, "a parked client left as it was asked back");
            return None;
        }
        Some(attachment)
    }

    /// Tell the thread to retire its client — hang it up and reap it, on the
    /// thread — and do not wait for it. Dropping the `Parked` then waits a
    /// little (see its `Drop`); telling several first is what lets them go
    /// side by side.
    pub fn tell_to_retire(&mut self) {
        if let Some(stop) = self.stop.as_mut() {
            stop.retire();
        }
    }

    /// The thread's answer, if it gives one within `wait`. A thread that does
    /// not is left to finish by itself: what it holds is its own to drop.
    fn join_within(&mut self, wait: Duration) -> Option<Attachment> {
        let thread = self.thread.take()?;
        let deadline = Instant::now() + wait;
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                tracing::debug!(id = %self.session_id, "a parked client's thread is still busy");
                return None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        // A panic on the thread dropped the client on its way out, and a
        // dropped client is a retired one.
        thread.join().ok().flatten()
    }
}

/// How long a switch waits for a parked client's thread to hand it back.
/// Instant for a thread in its `poll`; this bounds only one caught retiring a
/// client that was on its way out, which the switch then treats as gone.
const UNPARK_WAIT: Duration = Duration::from_millis(50);

/// How long letting go of a parked client waits for its thread to have
/// retired it. A client that goes quietly takes about 10 ms.
const RETIRE_WAIT: Duration = Duration::from_millis(100);

/// Dropping one retires the client — hung up and reaped on its own thread, as
/// any `Attachment` is on its way out — and waits [`RETIRE_WAIT`] at most for
/// that. Told first, then waited for, so that a pool letting go of several
/// can tell them all before waiting on any (see [`crate::pool::Pool`]).
impl Drop for Parked {
    fn drop(&mut self) {
        self.tell_to_retire();
        drop(self.stop.take());
        drop(self.join_within(RETIRE_WAIT));
    }
}

/// The parked client's thread: read what the client writes until told to
/// stop, and hand the client back — or retire it, if told to or if it is
/// leaving anyway.
///
/// What it reads goes to the shadow and the ledger (see
/// [`Attachment::saw_unrelayed`]). Never a byte to stdout, which belongs to
/// whatever is in front, and never a write to the client — with one
/// exception, on its way out.
///
/// **A parked client that asks for its terminal's attributes, or leaves the
/// alternate screen, is finishing.** Healthy, it does neither: they are the
/// ends of Neovim's exit sequence — and of its `suspend`, which any other UI
/// on the session can set off with `Ctrl-Z` and which reaches every client
/// attached. Unanswered, such a client waits for a reply forever and never
/// repaints again; answered and left running, it starts over against a
/// terminal that answers nothing else, and comes back without its keyboard
/// protocol, its synchronized updates or its in-band sizes for the rest of
/// its life. So it is retired instead: hung up first — so that a client that
/// was suspending finds its hangup already waiting — then answered, which is
/// what lets it go in milliseconds rather than a second, and reaped. The
/// session's next visit is a fresh client. Measured on 0.11.0 and 0.12.5.
///
/// A client found stopped is retired likewise, and one that has exited is
/// let go. Only ever with [`sys::Pty::peek`], which changes nothing:
/// [`sys::Pty::check`] consumes a stop, and a pid is only this thread's to wait
/// for.
fn keep_parked(mut attachment: Attachment, stop: sys::StopRx) -> Option<Attachment> {
    let id = attachment.session_id.clone();
    let mut buf = [0u8; 8192];
    loop {
        let woke = match sys::park_wait(&stop, &attachment.pty, IDLE_POLL_MS) {
            Ok(woke) => woke,
            // A `SIGWINCH` meant for the relay can land on this thread.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                tracing::warn!(%id, "a parked client could not be watched; retiring it");
                return None;
            }
        };
        // The client first: whatever it has said is read before the thread
        // lets go of it, and a client that has gone is not handed back.
        if woke.output {
            match attachment.pty.read_output(&mut buf) {
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
        if woke.stop {
            // A byte is an order to retire the client; the end of the stream
            // is the client wanted back.
            return match stop.order() {
                Order::Retire => None,
                Order::Back => Some(attachment),
            };
        }
        if woke.idle {
            match attachment.pty.peek() {
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

/// Whether this chunk of a client's output leaves the alternate screen: the
/// other half, with a DA1 request, of what a finishing client writes (see
/// [`keep_parked`]). Written in one flush with the rest of the exit sequence,
/// so, like the DA1 request, it arrives whole.
fn leaves_alt_screen(chunk: &[u8]) -> bool {
    chunk
        .windows(ALT_SCREEN_LEAVE.len())
        .any(|w| w == ALT_SCREEN_LEAVE)
}

/// A stand-in kept client for the tests of what holds clients (see
/// [`crate::pool`]): `sh -c script` on a real pty, as the tests here use,
/// attached to nothing, with a ledger and no screen of its own.
#[cfg(all(test, unix))]
pub(crate) fn kept_standin(session_id: &str, script: &str) -> Attachment {
    let mut cmd = sys::Command::new("sh");
    cmd.arg("-c");
    cmd.arg(script);
    let mut a = spawn_client_with(
        session_id,
        Path::new("/nvmux-test-never-read.sock"),
        "",
        cmd,
    )
    .expect("spawn sh");
    a.announce = None;
    a.shadow = None;
    a.screen = None;
    a.ledger = Some(Ledger::new());
    a.startup_answered = true;
    a
}

#[cfg(all(test, unix))]
impl Attachment {
    /// The stand-in's pid, for a test asking what became of the process.
    pub(crate) fn pid_for_test(&self) -> i32 {
        self.pty.pid().expect("a spawned child has a pid") as i32
    }

    /// As if its terminal had not yet answered its opening questions.
    pub(crate) fn cut_short_for_test(&mut self) {
        self.startup_answered = false;
    }

    /// Wait for a stand-in whose script prints `r` once its traps are set,
    /// so that a signal sent next meets them rather than the shell's own
    /// startup disposition. Says whether it came.
    pub(crate) fn ready_for_test(&self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 256];
        while Instant::now() < deadline {
            if matches!(self.pty.wait_output(Duration::from_millis(50)), Ok(true))
                && self
                    .pty
                    .read_output(&mut buf)
                    .is_ok_and(|n| buf[..n].contains(&b'r'))
            {
                return true;
            }
        }
        false
    }
}

/// Start a `--remote-ui` client for a session and wait for the probe to pass.
///
/// `sock` must already be reachable from *this* machine — locally the session
/// socket, over SSH the local end of a forward.
///
/// `announce` is what the first relay says the session is (see
/// [`crate::announce::label`]). Not an `Option`: every spawn is a change of
/// session, and making that unrepresentable is the point.
///
/// The blocking form, with the probe on [`rpc::PROBE_TIMEOUT`]: what the
/// integration tests drive, since a test wants an answer or a failure and
/// nobody at a keyboard. The binary does not use it. There the attach is the
/// same two halves — [`spawn_client`], then [`Probe::start`] — with the wait
/// on a screen of its own (`ui::attaching`), which has no budget because the
/// user is watching it and can give up whenever they choose.
///
/// A session waiting for a key is an attachment, not an error — [`Answer`] for
/// why. It used to be one, by running the budget out, and that refusal was the
/// bug: the way out of that state is a key pressed on the very client the
/// refusal was throwing away. Only a session that cannot be reached, or one
/// with too many UIs already, is still refused here.
pub fn spawn(session_id: &str, sock: &Path, announce: &str) -> Result<Attachment> {
    spawn_with(session_id, sock, announce, client_command(sock))
}

/// Start a `--remote-ui` client for a session, and nothing else: the half of
/// [`spawn`] that forks. The probe is the caller's, through [`Probe::start`].
///
/// The client is started *first* and the server probed while it connects.
/// The two have nothing to say to each other — the probe asks the server what
/// state it is in, the client asks it for a grid — and over an SSH forward
/// each is a run of round trips: three for the probe, two for the client
/// before its first paint. In series they were the whole of a switch on a
/// distant host; overlapped, a switch costs the longer of the two.
///
/// The probe still decides. Nothing the client prints reaches the terminal
/// before it has passed: the client writes into its pty, and only the relay
/// copies a pty to the terminal, which the caller starts once the probe has
/// said yes. A probe that fails — or a wait the user gives up on — ends the
/// client by dropping the attachment it was building, through the same hangup
/// and reap as any other retirement, and its output dies with the master.
/// That matters over a forward whose remote end has gone: the client's own
/// failure message there is empty, after ~165 bytes of escape sequences it
/// has already written, and none of them are seen.
///
/// A client that could not be started at all is reported before any probe,
/// since the message — `nvim` is not here — is the useful one.
pub fn spawn_client(session_id: &str, sock: &Path, announce: &str) -> Result<Attachment> {
    spawn_client_with(session_id, sock, announce, client_command(sock))
}

/// `nvim --server <sock> --remote-ui`, in our working directory.
fn client_command(sock: &Path) -> sys::Command {
    // The child's environment is ours, so `TERM`, `COLORTERM` and everything
    // else the client negotiates with reach it without being copied by hand.
    let mut cmd = sys::Command::new("nvim");
    cmd.arg("--server");
    cmd.arg(sock);
    cmd.arg("--remote-ui");
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }
    // The other half of the pair the user calls their editor, so it gets the
    // umask nvmux was started with for the same reason the session does — see
    // `paths::restrict_umask`. A `--remote-ui` client holds no buffers and so
    // writes next to nothing, which is a reason for the exception to go
    // unnoticed rather than a reason to make one.
    #[cfg(unix)]
    if let Some(mask) = crate::paths::launch_umask() {
        cmd.umask(Some(mask.bits()));
    }
    cmd
}

/// [`spawn`] with the client command supplied, which is how the tests run it
/// against a stand-in client where there is no `nvim`.
///
/// The same sequence `main` runs under its spinner — the client, then the
/// probe on its own thread — waited out here, with the budget a test expects.
fn spawn_with(
    session_id: &str,
    sock: &Path,
    announce: &str,
    cmd: sys::Command,
) -> Result<Attachment> {
    let t_spawn = std::time::Instant::now();
    let attachment = spawn_client_with(session_id, sock, announce, cmd)?;
    tracing::debug!(
        ms = t_spawn.elapsed().as_secs_f64() * 1000.0,
        "timing: client spawn"
    );

    // The round trips to the session — over ssh, through the forward — while
    // the client makes its own.
    let t_probe = std::time::Instant::now();
    let verdict = Probe::start(session_id, sock).and_then(|mut probe| {
        // A wait that runs out is the timeout it used to be, so the callers
        // that read the error see the shape they always did. The probe drops
        // at the end of this closure, which is what ends its worker.
        probe
            .wait(rpc::PROBE_TIMEOUT)
            .unwrap_or(Err(NvmuxError::Rpc(crate::error::RpcError::Timeout(
                rpc::PROBE_TIMEOUT,
            ))))
    });
    match verdict {
        // A blocked session is attached to, as the binary attaches to one:
        // the refusal it used to get helped nobody, since the way out of that
        // state is a key the user presses on the client this is about to hand
        // them. Only the notice differs, and this variant draws none.
        Ok(answer) => tracing::debug!(?answer, "attach probe answered"),
        Err(e) => {
            // Explicitly, so the retirement is not left to a binding: the
            // client must be gone, and its pty with it, before the error is
            // shown.
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

/// The attach probe, in flight on a thread of its own.
///
/// The probe is a run of blocking calls to the session, and the last of them
/// is deferred: a session in the middle of `:!make`, or of CPU-bound Lua,
/// answers it when it is done and not before. It used to be given three
/// seconds and then refused. Now it is given as long as it takes, and the
/// screen that waits on it (`ui::attaching`) is what the user gives up from —
/// which means that screen has to keep reading keys while the probe blocks,
/// and a blocking read cannot share a thread with a key loop.
///
/// So this is the second thread in the crate, and the reasons
/// [`crate::ui::complete`] gives for the first are the reasons here: the
/// worker reads its own socket and nothing else, so it cannot split the
/// terminal's stream, and it owns nothing the screen needs back. What it
/// adds is a way to be stopped. A thread parked in a blocking read cannot be
/// told to stop, and nothing waits for it — but the socket can be shut down
/// under it, and then the read returns at once (see [`rpc::Interrupt`]).
/// That happens on every way out, through `Drop`, and it is not tidiness: a
/// worker left parked behind a `:!make` would get its answer when the make
/// ended, and if the session were then at the make's own hit-enter prompt it
/// would type `<CR>` into a prompt somebody else was reading.
///
/// Told the answer through a channel rather than joined, so the screen can
/// poll it on the same tick it draws on.
pub struct Probe {
    rx: mpsc::Receiver<Result<Answer>>,
    interrupt: rpc::Interrupt,
    /// `None` only once a test has taken it, to prove the worker ended.
    thread: Option<std::thread::JoinHandle<()>>,
}

/// What the attach probe found, for a session it could reach at all.
///
/// The distinction is not how healthy the session is but whether anything
/// deferred can be asked of it. A session waiting for a key runs its main loop
/// with the event queue switched off, so `nvim_list_uis` — and the client's own
/// `nvim_ui_attach` behind it — are not answered until a key arrives. The probe
/// must therefore say so and get out of the way rather than join the queue: see
/// [`probe_on`] for what nvmux will and will not type to end that wait itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The session answered a deferred call, and has room for this client.
    Ready,
    /// Reachable, and waiting for a key nvmux may not press on the user's
    /// behalf. No deferred call is served until one arrives, so the UI count
    /// is unknown and the client's first paint is queued behind the same key.
    Blocked {
        /// The mode it is waiting in, as `nvim_get_mode` spells it, for the log.
        mode: String,
    },
}

impl Probe {
    /// Connect to the session and start asking.
    ///
    /// The connect happens here, on the caller's thread: a unix socket connect
    /// either finds a listener or does not, in no time at all, so a dead
    /// socket — nothing listening, a file where the socket was — is refused at
    /// once rather than from a thread a moment later. Only the calls, which
    /// are where the waiting is, go to the worker.
    pub fn start(session_id: &str, sock: &Path) -> Result<Self> {
        let mut client = rpc::Client::connect_with(sock, None)?;
        let interrupt = client.interrupt()?;
        let (tx, rx) = mpsc::channel();
        let id = session_id.to_string();
        let thread = std::thread::Builder::new()
            .name("nvmux-probe".into())
            .spawn(move || {
                let verdict = probe_on(&mut client, &id);
                // A receiver that has gone is a wait that was given up on,
                // and there is nobody left to tell.
                let _ = tx.send(verdict);
            })?;
        Ok(Self {
            rx,
            interrupt,
            thread: Some(thread),
        })
    }

    /// Wait up to `timeout` for the verdict. `None` is "not yet"; an answer
    /// is given once, after which the probe is done with. A worker that died
    /// without answering is an error, not a `None`: a screen looping on this
    /// must never be left spinning for a verdict that is not coming.
    pub fn wait(&mut self, timeout: Duration) -> Option<Result<Answer>> {
        match self.rx.recv_timeout(timeout) {
            Ok(verdict) => Some(verdict),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => Some(Err(NvmuxError::Io(
                std::io::Error::other("the attach probe ended without a verdict"),
            ))),
        }
    }

    /// Give up on the probe as `Drop` does, but hand the worker back so a
    /// test can prove the interrupt ended it, and not only the connection.
    #[cfg(all(test, unix))]
    fn abandon(mut self) -> std::thread::JoinHandle<()> {
        self.thread.take().expect("the worker is taken once")
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        // Every way out, the errors included — see the type's docs for why a
        // worker must never be left parked. On the success path the worker
        // has already closed its end and this shuts down a socket that is
        // about to be dropped anyway, which is nothing. Not joined: the
        // shutdown is what unparks the worker, and nothing here needs it gone
        // before going on.
        self.interrupt.fire();
        if let Some(thread) = &self.thread {
            tracing::debug!(finished = thread.is_finished(), "attach probe retired");
        }
    }
}

/// Ask the server whether it can take a client, and make it ready for one.
///
/// The `get_mode` here is the reachability check, and the only one. It is a
/// fast call but still a full request/response round trip, so an answer
/// proves a real Neovim is behind the socket, a forward whose remote end has
/// gone resets instead, and a peer that is not Neovim fails on its reply
/// shape — and `rpc` classifies all of that identically whichever method
/// asked, which is what the `?` relies on.
///
/// An `api_info` used to run in front of it, and proved nothing more *here*:
/// its result was discarded, nothing on this path reads a version (the gates
/// are `nvim::check_local` and the remote `--version` banner), and the `?` on
/// `get_mode` refuses what this path can actually meet. It does prove more
/// *elsewhere* — it answers during `:!cmd`, where `get_mode` is queued like
/// any other call, which is why `rpc::probe` asks both — so this is not a
/// claim that the two are interchangeable. What it cost was a round trip on
/// every attach, forwarded over SSH.
fn probe_on(client: &mut rpc::Connection, session_id: &str) -> Result<Answer> {
    // Both questions go out before either answer is read. They are independent
    // — what state the editor is in, and how many UIs it has — and over an SSH
    // forward each answer is a round trip, so asking the second only once the
    // first had come back cost two of them, on the one path the user is
    // watching. Measured against 0.12.5 through a link with a 150 ms round
    // trip: 302 ms in series, 151 ms asked together.
    //
    // A server waiting for a key cannot *answer* `list_uis`, a deferred call,
    // until one arrives. So the mode is the reply that is read first, every
    // time round, and nothing deferred is waited on until it says the queue is
    // running. Waiting anyway is what this loop exists to stop: the connection
    // has no budget (see [`Probe::start`]), so a deferred call to a session in
    // that state is not a slow answer but no answer at all, and the screen
    // waiting on it spins until the user gives up.
    //
    // Sending it early is not that hazard. `nvim_list_uis` reads a list and
    // changes nothing, so a copy of it sitting in a blocked session's queue
    // does nothing until the queue runs and nothing anyone can see when it
    // does; if the probe has given up by then, the reply lands on a socket
    // that has been shut down (see [`Probe::drop`]) and Neovim closes the
    // channel, as it does for every client that hangs up. What must never be
    // sent ahead of a fresh reading is a *key* — see the `<CR>` below.
    //
    // The one wait nvmux ends itself is the hit-enter prompt, with `<CR>`, the
    // key it consumes without running anything — see `rpc::Mode::at_hit_enter`
    // for why only that one. Usually there is nobody else to type it: the
    // client that was showing the prompt is gone, and this is its replacement.
    //
    // Re-read rather than sent and forgotten, because one `<CR>` ends one
    // prompt and Neovim stacks them: three scheduled callbacks that each raise
    // an error leave three, and the count is not knowable in advance. Measured
    // against 0.12.5, the round trip is its own settle — the mode read after a
    // `<CR>` already reflects it, so exactly three are sent for three prompts
    // and none is wasted. Each one is justified by its own fresh reading,
    // which is a stricter licence than the single blind `<CR>` this replaces.
    //
    // A deliberate trade-off where another UI is still attached: that user's
    // prompt ends too, and whatever it was showing — a `:!make` page, a Lua
    // traceback — leaves their screen unread. `g<` brings the page back and
    // `:messages` keeps the rest. Advisory, not a lock: a key from that other
    // UI in the microseconds between two calls ends the prompt first, and the
    // `<CR>` then runs in whatever mode it left.
    //
    // Which is why the loop itself is not pipelined, though it is two round
    // trips a turn. `nvim_input` and `nvim_get_mode` are both fast calls,
    // answered from the socket's read callback, so a mode asked behind a
    // `<CR>` would be answered before the main loop had consumed the key and
    // would report the prompt still up — earning a second `<CR>` for one
    // prompt, which is the bug the fresh reading exists to prevent. The pair
    // above are safe together only because neither is a key.
    let mut asking_mode = client.ask_mode()?;
    let counting = client.ask_uis()?;

    for _ in 0..MAX_PROMPTS {
        let mode = client.mode_reply(asking_mode)?;
        if !mode.blocking {
            return ui_count(client, session_id, counting);
        }
        if !mode.at_hit_enter() {
            // A more-prompt, a half-typed `g`, a script parked on a key: over
            // RPC these are one state, and nvmux cannot tell a session wedged
            // by a crashed callback from a command the user started and left.
            // So it says so and hands the terminal over — the client's own
            // keys go out as `nvim_input`, a fast call, so the first thing the
            // user presses lands, ends the wait, and releases the queued
            // `nvim_ui_attach` behind it.
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

    // Still prompting after the budget: something is raising them faster than
    // they are answered. Report it rather than park on it — the user's own
    // keys are a better answer than an unbounded wait, and they can see what
    // the prompts say.
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

/// How many hit-enter prompts one attach will answer before handing the
/// terminal over instead.
///
/// Three scheduled callbacks that each raise an error stack three prompts, so
/// the budget is one more than that: enough for the case that produced it, and
/// small enough that a session generating them faster than nvmux can answer is
/// given back to the user rather than typed at indefinitely.
const MAX_PROMPTS: usize = 4;

/// The deferred half of the probe: whether this session has room for another
/// client. The question went out with the mode; this is where its answer is
/// waited for, and only ever on a mode reading that said the queue is running.
///
/// The wait here is still unbounded, and deliberately: a session in the middle
/// of a `:!make` answers when the make is done, and that is one to wait for
/// with a spinner rather than to refuse (see `ui::attaching`). What the caller
/// has ruled out first is the wait that would never end on its own.
///
/// One question is asked however many prompts were answered in between: the
/// reply describes the moment the queue ran, which is after the last `<CR>`,
/// and re-asking would buy a fresher count of a thing that is racy by one
/// either way (see the `>` below).
///
/// Not `unwrap_or(0)`: a server too busy to answer is exactly the one whose UI
/// count is unknown, and guessing zero is how the seventeenth attach happens.
///
/// `>` rather than `>=`: the client this probe runs alongside may or may not be
/// attached yet, so the count is off by at most one, in the direction of our own
/// client. `MAX_UIS` is half of Neovim's limit, so a count one too high refuses
/// the ninth other client rather than the eighth, and a count one too low still
/// stops seven short of the abort.
fn ui_count(
    client: &mut rpc::Connection,
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
    cmd: sys::Command,
) -> Result<Attachment> {
    // Right *before* the child starts: `PtySize::default()` is 24x80. Note
    // `crossterm::size()` is (cols, rows) while `PtySize` is { rows, cols } —
    // passing them positionally transposes the screen.
    let size = term::terminal_size();
    let (pty, writer) = sys::Pty::spawn(cmd, size)?;

    let fades = fade::enabled() && fade::session();
    let shadow = fades.then(|| shadow::Shadow::new(size.rows, size.cols));
    let kept = crate::config::get().client.per_session;

    Ok(Attachment {
        session_id: session_id.to_string(),
        sock: sock.to_path_buf(),
        announce: Some(announce.to_string()),
        pty,
        writer,
        resumed: false,
        reaped: false,
        // Only when it will be used: a shadow costs a parse of everything the
        // client writes.
        shadow,
        screen: (kept && !fades).then(|| shadow::Shadow::new(size.rows, size.cols)),
        hold: None,
        // Set by `relay`, which knows whether there is a notice to write.
        boundary: None,
        span: false,
        // Like the shadow, only when it will be used: a kept client is the
        // only kind that is ever resumed from what it wrote.
        ledger: kept.then(Ledger::new),
        startup_answered: false,
        expecting_a_size_report: None,
        exits_seen: 0,
    })
}

/// Relay bytes between the terminal and the child until something interrupts it.
///
/// One thread, one `poll` over stdin, the PTY master and the SIGWINCH
/// self-pipe, deliberately rather than incidentally:
///
/// * `try_clone_reader` dups the *same* open file description, so two readers
///   race and split the stream — in a proxy that means randomly deleting chunks
///   of the user's screen. One loop, one reader.
/// * A thread parked in a blocking `read(0)` cannot be cancelled, so returning
///   to the picker would hang or swallow the first keystroke. A `poll` loop
///   just stops polling.
///
/// # Starting the next session before this one has finished leaving
///
/// `begin_next` is called with the target of a `<prefix>` switch, once, the
/// moment [`pump`] returns one — before this session dissolves and before the
/// terminal is handed back. What follows that call is a dissolve — half of
/// `fade.duration_ms`, which measures both directions — of nvmux drawing its
/// own frames off a grid it already has, and the work the next attachment
/// opens with is a fork and a round trip that touch nothing this function is
/// using: a client writes into its own pty and nothing reaches the terminal
/// until a relay copies it (see [`spawn_client`]), and the probe answers on a
/// thread of its own (see [`Probe`]). Started here, both run underneath the
/// dissolve instead of after it.
///
/// So the callback must *start* things and not wait for them: it is called
/// with the terminal still raw and the outgoing session still on it, and
/// anything it blocks on is time the user watches a frozen screen for. It must
/// not draw. Every other outcome calls it not at all — the picker, the prompt
/// and the help screen have nothing to start, and the three that end the relay
/// have nowhere to go.
pub fn relay(
    mut attachment: Attachment,
    highest_session_num: u32,
    begin_next: &mut dyn FnMut(Target),
) -> Result<(Outcome, Option<Attachment>)> {
    // The one wait the relay makes, set up — on Unix, with its `SIGWINCH`
    // handler in — before anything else here.
    let mut waiter = sys::Waiter::new(&attachment.pty)?;
    // A client that has not painted yet, whose first paint can be held.
    let fresh = !attachment.resumed;
    if attachment.resumed {
        // The client entered the alternate screen itself on its first relay
        // and has not been told that every screen since — the picker, the
        // prompt, the help, the `Switch` branch below — left it. It will not
        // enter it again: the repaint below redraws the grid wherever the
        // terminal is. Left on the primary screen it draws there, and then
        // on a detach its `rmcup` and nvmux's own `?1049l` have nothing to
        // leave, and its last frame is what the shell prompt lands on.
        // Not shown to the shadow: the erase would take the screen the
        // client had out of it, and that screen is what dissolves back in
        // below, ahead of the repaint that then puts the real one back.
        term::enter_alt_screen_and_clear();
    } else {
        // Normally already done by whoever handed the terminal over — the
        // picker, the prompt, or the `Switch` branch below — because a clear
        // on this side of the client spawn is a clear too late, and the old
        // session is what fills the wait. This is the backstop for a path
        // that did neither, and a repeat costs one write on a screen nothing
        // has drawn to since. The client enters the alternate screen for
        // itself, as part of its startup.
        term::leave_alt_screen_and_clear();
        attachment.shadow_saw(term::HANDOVER);
    }
    let mut raw = term::RawMode::enter()?;

    // Whether the session's screen was dissolved in from the shadow on a
    // resume, before the client has written a byte of this relay: what lets
    // the notice go up over a kept session that has nothing to say (see
    // `pump`).
    let mut dissolved = false;
    // A kept client's repaint, asked on its return and answered while the
    // relay runs.
    let mut erasing: Option<Erasing> = None;
    // What the child buffered while blocked mid-write describes a screen the
    // picker has since drawn over.
    if attachment.resumed {
        let t_resume = Instant::now();
        // A kept client's is read rather than thrown away: it was parked,
        // not blocked, and what it wrote is the screen about to be painted
        // back — see [`Attachment::drain_unrelayed`].
        if attachment.is_kept() {
            attachment.drain_unrelayed();
        } else {
            attachment.pty.discard_pending(|_| {});
        }
        // The size first, so the frames fit the terminal as it is now; then
        // the screen the client had dissolves in, and only then is the
        // client asked to repaint — its paint lands over the last frame.
        //
        // Either way the cursor is shown before the repaint is asked for,
        // not left to it: the fade out that ended the last relay hid the
        // cursor, and a repaint is what a server *may* do — one busy in Lua
        // or waiting for a key does nothing until that is over, and an idle
        // editor writes nothing on its own. A fade in puts the cursor back
        // on the session's own cell as its last frame; without one it is
        // shown where the erase left it, as every screen left it before
        // there was a fade.
        //
        // A kept client with no fade to dissolve it in has its screen painted
        // straight back instead, from its copy: it is on the glass at once
        // either way, whatever the server is doing.
        let size = term::terminal_size();
        // Asked before the resize, because the resize is what changes it.
        let resized = attachment.pty.size() != Some(size);
        attachment.resize_to(size);
        dissolved = attachment.dissolve_in();
        let on_screen = dissolved || attachment.paint_kept_screen();
        if !on_screen {
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
            // What the client told the terminal comes back from the ledger,
            // and the exact screen from its server — asked from a thread, as
            // the notice's repaint is, since the screen is back already and
            // the relay has a session to run meanwhile. With nothing typed
            // into a prompt: the prompt is on the screen, for the user.
            // The fade's last frame leaves a reset pen and a hidden cursor
            // wherever it was; a kept client may write before the repaint
            // lands, and writes on the belief that neither has moved.
            if dissolved {
                attachment.hand_back_the_pen();
            }
            attachment.put_back_what_it_told(size);
            attachment.tell_its_size(size, resized);
            erasing = match Erasing::start_for(&attachment.sock, "kept client's return") {
                Ok(started) => Some(started),
                // No thread to ask from: the fallback a server that could not
                // be asked would have got, as the notice's repaint has.
                Err(e) => {
                    tracing::debug!(error = %e, "no thread for the kept client's repaint");
                    apply(&mut attachment, Repaint::Notice, Served::NONE);
                    None
                }
            };
        } else {
            repaint(&mut attachment, Repaint::Resume);
        }
    }
    attachment.resumed = true;
    if fresh && attachment.shadow.is_some() {
        attachment.hold = Some(Hold::new(Instant::now()));
    }

    // Taken, not copied: `<prefix> Space` and `<prefix> ?` come back to this same
    // client, and a session you never left has nothing to announce.
    let popup = announce::Popup::arm(attachment.announce.take(), Instant::now());
    // Only for the notice, and only from here: everything written to the
    // terminal above is nvmux's own and complete, so a parser that starts
    // between sequences starts right.
    attachment.boundary = popup.is_some().then(boundary::Boundary::new);

    let outcome = pump(
        &mut attachment,
        &mut waiter,
        highest_session_num,
        popup,
        dissolved,
        erasing,
    );
    // From here on nothing the client writes reaches the terminal until it is
    // relayed again: what the ledger hears next is news to the terminal.
    if let Some(ledger) = attachment.ledger.as_mut() {
        ledger.left_the_terminal();
    }

    // As early as there is anything to say: everything below this line is
    // nvmux drawing, and what the caller starts here runs underneath it. See
    // the note on `begin_next`.
    if let Some(target) = outcome.as_ref().ok().and_then(|o| o.switch_target()) {
        begin_next(target);
    }

    // Whatever ended the relay, nothing the client wrote may stay unshown —
    // which is as true of the few bytes the notice was holding a sequence
    // together with as it is of a whole first paint.
    if let Err(e) = attachment.stop_watching_the_terminal() {
        tracing::debug!(error = %e, "could not write out what the notice was keeping");
    }
    // A relay cut short inside the hold — a prefix typed at once, a child that
    // exited — lets the paint through as it is, with no fade.
    if let Err(e) = attachment.release_hold(false) {
        tracing::debug!(error = %e, "could not release the held first paint");
    }

    match outcome {
        Ok(
            held
            @ (Outcome::ToPicker | Outcome::CreateNew | Outcome::ShowHelp | Outcome::Switch(_)),
        ) => {
            // Dissolve the session out first: before the modes go back, so a
            // key typed during it is not echoed, and before the hand-off
            // below, which erases with the colour the last frame ends on. A
            // frame that could not be written must not skip the restore, so
            // its error is logged and dropped rather than propagated.
            if let Some(shadow) = attachment.shadow.as_mut() {
                if let Err(e) = fade::fade_out_session(shadow) {
                    tracing::debug!(error = %e, "the session's fade-out did not complete");
                }
            }
            raw.restore();
            // Clear here rather than leaving it to the next `relay`, which runs
            // on the far side of the client spawn: what is on the terminal until
            // then is the session being switched away from. Harmless when the
            // number names nothing — the same client is resumed, and a resume
            // forces a repaint.
            // Not shown to the shadow, for the reason the resume's erase is
            // not: a switch that names this same session resumes it, and the
            // screen it had is what dissolves back in.
            if held.leads_straight_into_another_relay() {
                term::leave_alt_screen_and_clear();
            }
            Ok((held, Some(attachment)))
        }
        Ok(Outcome::ChildExited) => {
            // The child is gone and has emitted its own restore; relay the rest
            // of it, then hand back to the picker. A kept client parked
            // meanwhile relies on modes that restore has just turned off, and
            // is told so when it comes back (see [`EXITS_RELAYED`]).
            EXITS_RELAYED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            attachment.drain_until_eof();
            raw.restore();
            attachment.reap();
            Ok((Outcome::ChildExited, None))
        }
        Ok(other @ (Outcome::Detached | Outcome::StdinClosed)) => {
            // Leave the session running and let the child put the terminal
            // back itself — a competing reset while it is still writing would
            // corrupt its own restore sequence. Once it is gone, a final reset
            // is harmless and covers a client that died before it got that far.
            attachment.hang_up();
            attachment.drain_until_eof();
            raw.restore();
            attachment.reap();
            term::reset_screen();
            // After the reset, not before: leaving the alternate screen puts the
            // cursor back on the line the client printed its hangup message
            // from, which is the line to erase.
            term::erase_hung_up_clients_line();
            Ok((other, None))
        }
        Err(e) => {
            // The error is about to be printed to a shell: put the cursor and
            // the colours back first, in case the client died mid-frame with
            // the cursor hidden or an SGR attribute set. `attachment` is
            // dropped on the way out, which retires the client.
            attachment.hang_up();
            attachment.drain_until_eof();
            raw.restore();
            attachment.reap();
            term::reset_screen();
            // The same hangup, so the same stray line — and here the error is
            // about to be printed onto it.
            term::erase_hung_up_clients_line();
            Err(e)
        }
    }
}

fn pump(
    attachment: &mut Attachment,
    waiter: &mut sys::Waiter,
    highest_session_num: u32,
    mut popup: Option<announce::Popup>,
    dissolved: bool,
    mut erasing: Option<Erasing>,
) -> Result<Outcome> {
    let keys = crate::config::get().keys;
    let mut prefix = Prefix::with_prefix(highest_session_num, keys.prefix);
    let prefix_timeout = Duration::from_millis(keys.timeout_ms);
    // When a pending prefix, half-typed number or cut-off escape sequence must
    // be settled. An instant rather than a per-poll timeout on purpose: a child
    // that keeps producing output keeps `poll` returning early, and a timeout
    // that restarted on every wake-up would never fire while a spinner is
    // running. It is set when the machine arms and cleared when it settles.
    let mut deadline: Option<Instant> = None;
    // The notice's repaint while it is in flight; see [`Erasing`]. Shared with
    // the hint bar, which wants the same repaint for the same reason and only
    // when this slot is free: a `:mode` in flight repaints the whole screen and
    // covers the bar's row on its way past. And with a kept client's return,
    // which comes in with one already asked for.
    // The row that says what the next key does while a `<prefix>` waits.
    let mut bar = hint::Bar::new(Instant::now(), term::terminal_size());
    let mut buf = [0u8; 8192];
    // Diagnostics only, and what makes a switch that felt slow attributable:
    // everything before this is nvmux's own work and is timed where it happens,
    // while this is the session's — its redraw, and the terminal's drawing of
    // it. Measured from the start of the relay, which is the moment the
    // terminal became the new session's to paint.
    let relay_started = Instant::now();
    let mut first_byte: Option<Duration> = None;
    let mut painted_bytes = 0usize;
    let mut last_byte = Instant::now();
    let mut painted = false;

    loop {
        // The attach notice has a clock of its own — a life to end, and a
        // dissolve to pace — and so do a held first paint and a tail kept back
        // to keep a sequence whole; all of it is folded in here so it is
        // honoured by the same `poll`. Absolute on every side, for the reason
        // above.
        let now = Instant::now();
        let next = [
            deadline,
            popup.as_ref().map(announce::Popup::wake_at),
            bar.wake_at(now),
            attachment.hold_wake_at(),
            attachment.kept_due(),
            erasing.as_ref().map(|_| now + ERASE_POLL),
        ]
        .into_iter()
        .flatten()
        .min();

        // A stopped child produces no poll activity at all, so even an idle
        // wait is bounded; an armed prefix shortens it to its own deadline.
        let timeout_ms = match next {
            Some(d) => {
                // Rounded up, not down: rounded down, the last fraction of a
                // millisecond before every deadline would be spent in
                // `poll` calls that return at once.
                let left = d.saturating_duration_since(Instant::now());
                let ms = left.as_micros().div_ceil(1000);
                ms.min(u128::from(IDLE_POLL_MS)) as u64
            }
            None => IDLE_POLL_MS,
        };

        // A wait a signal cut short is gone round again.
        let Some(woke) = waiter.wait(timeout_ms).map_err(NvmuxError::Io)? else {
            continue;
        };

        // Output first, so the screen is current before a keystroke is acted on.
        let child_spoke = woke.child;
        if child_spoke {
            match waiter.read_child(&mut buf) {
                // EOF on macOS, EIO on Linux: both mean the slave closed.
                Ok(0) | Err(_) => return Ok(Outcome::ChildExited),
                Ok(len) => {
                    // Byte for byte, and held only while the first paint is;
                    // see the module docs. The shadow, if there is one, is
                    // shown a copy.
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

        // The held first paint, once it is over: dissolved in from the
        // shadow, then let through. Before the notice below, which paints
        // over the session and so must find it on screen.
        if attachment.hold_is_due(Instant::now()) {
            attachment.release_hold(true)?;
        }

        // A tail the notice is holding a sequence together with is never held
        // long: a client that stopped in the middle of one may be waiting for
        // the terminal's answer to it. See [`crate::boundary`].
        if attachment
            .kept_due()
            .is_some_and(|due| Instant::now() >= due)
        {
            attachment.release_kept()?;
        }

        // Once per relay, on the first lull after the session has said
        // anything. After the read above, so a burst that ends on this pass is
        // counted before the lull is measured.
        if !painted && first_byte.is_some() && last_byte.elapsed() >= PAINT_SETTLED {
            painted = true;
            tracing::debug!(
                first_byte_ms = first_byte.map(|d| d.as_secs_f64() * 1000.0),
                painted_ms = (last_byte - relay_started).as_secs_f64() * 1000.0,
                bytes = painted_bytes,
                "timing: session painted"
            );
        }

        if woke.resized {
            // Enough on its own: the kernel signals the pty's foreground group
            // and the client calls try_resize. (A pseudoconsole tells its
            // client the same way, as a console event.)
            attachment.resize_to(term::terminal_size());
        }

        // After the frame it sits on and before any keystroke is acted on. The
        // size is read here rather than once outside the loop, so every frame
        // of the notice — it dissolves in and out, so there are many — is
        // drawn for the screen as it is now, and a resize between the drawing
        // and the erasing is seen by both.
        if let Some(p) = popup.as_mut() {
            // A held first paint counts as the child still talking, even
            // though the terminal has not heard a word of it. The box must not
            // go up over a screen the editor has not been let paint yet: there
            // would be nothing under it to melt into, and the dissolve that
            // ends the hold repaints every cell — it would wipe the box off a
            // screen the popup still believes it is on. `between_sequences` is
            // what refuses it, and the box has `GIVE_UP` against the hold's
            // much shorter `HOLD_CAP`.
            let pass = announce::Pass {
                wrote: child_spoke || attachment.hold.is_some(),
                // Not before the client has written a byte, and not only
                // because there would be nothing under the box. Its first act
                // is to enter the alternate screen, and until it has, the
                // terminal is still showing the primary one nvmux cleared on
                // the way in — which is the screen the user's shell comes back
                // to, and no place to leave three rows of border. The hold
                // says the same thing the other way round when there is one.
                //
                // Unless the session's screen was dissolved in from the
                // shadow, which a kept client's return does: there the box has
                // a screen to sit on before the client says anything — a
                // session that is idle, or busy, may say nothing for a while
                // yet — and the shadow to take it off again whatever the
                // server does. A screen painted straight back has no shadow
                // behind it, and waits for the client like any other.
                between: (first_byte.is_some() || dissolved) && attachment.between_sequences(),
            };
            let act = p.step(
                Instant::now(),
                pass,
                term::terminal_size(),
                attachment.shadow.as_ref(),
            );
            match act {
                announce::Act::Idle => {}
                announce::Act::Paint(bytes) => {
                    attachment.write_over_session(&bytes)?;
                    // On a three-row terminal — and only there — the box's
                    // bottom row is the bar's row, so the bar can no longer
                    // trust that what it drew is still on the screen. Told one
                    // way only: telling the notice that the bar is up would
                    // freeze its dissolve for as long as a prefix is armed, on
                    // every terminal, to settle a collision that exists on one
                    // size. The bar's own block runs below this one, so on that
                    // size it takes the shared row back within this pass.
                    bar.overdrawn();
                }
                announce::Act::Erase => {
                    // The same repaint a resume asks for, and for the same
                    // reason: the cells the box covered are the server's, and
                    // only the server can say what was under them. Not the same
                    // licence, though — see [`Repaint`] — and not the same
                    // wait: this one is asked from a thread, because the relay
                    // has a session to keep running while the answer comes
                    // (see [`Erasing`]). The size goes first, as it does for a
                    // resume, and on this thread, where a `resize` belongs.
                    attachment.resize_to(term::terminal_size());
                    match Erasing::start(&attachment.sock) {
                        Ok(started) => erasing = Some(started),
                        // No thread to ask from: the fallback is what a server
                        // that could not be asked would have got anyway.
                        Err(e) => {
                            tracing::debug!(error = %e, "no thread for the notice repaint");
                            apply(attachment, Repaint::Notice, Served::NONE);
                        }
                    }
                    popup = None;
                }
                announce::Act::Done => popup = None,
            }
            // Where the terminal stood when the notice's life ended. The one
            // question a report of "the box never appeared" turns on, and
            // there is no answering it from outside the process: the box is
            // written into somebody else's byte stream and what is on the
            // screen afterwards says nothing about why.
            if popup.is_none() {
                tracing::debug!(
                    between = attachment.between_sequences(),
                    "timing: attach notice over"
                );
            }
            // The notice is the only thing that needs the terminal's parser
            // watched — the hint bar waits for a lull instead — so when it
            // ends that stops being anybody's business, and whatever was being
            // kept back to keep a sequence whole goes out with it.
            if popup.is_none() {
                attachment.stop_watching_the_terminal()?;
            }
        }

        // Whatever the notice did or did not do, whatever was held back for
        // it goes out here, on the pass that held it: the session's own frame
        // close if it brackets its frames, and otherwise the synchronized
        // update nvmux opened in front of the write. A terminal left inside
        // either shows nothing until its own timeout expires. See
        // [`Attachment::write_session_frame`].
        attachment.release_the_sessions_close()?;
        attachment.close_span()?;

        // The notice's repaint, if it has answered. Only the fallback waits on
        // this — the repaint itself arrives as the session's own output — so a
        // pass that finds nothing costs a `try_recv`.
        if let Some(served) = erasing.as_ref().and_then(Erasing::answer) {
            let asked = erasing.take().expect("it answered a moment ago");
            // Timed because it is a second full repaint of the session, a
            // whole second after the switch — the one nvmux asks for rather
            // than the one the attach produced. What the number now measures
            // is how long it took somewhere else, which is the point of it.
            tracing::debug!(
                ms = asked.started.elapsed().as_secs_f64() * 1000.0,
                repainted = served.repainted,
                "timing: {} (:mode repaint)",
                asked.what
            );
            apply(attachment, Repaint::Notice, served);
        }

        // Settle a pending prefix or number before reading anything new, so a
        // key that arrives just after the deadline is not swallowed as a
        // command. A cut-off escape sequence is the other way round: input
        // already waiting is the rest of it, or the key that decides it, and
        // must be read first — the screen write above can outlast the short
        // wait, and settling then would break a sequence whose remainder had
        // already arrived.
        let stdin_ready = woke.stdin;
        let overdue = deadline.is_some_and(|d| Instant::now() >= d);
        if overdue && !(stdin_ready && prefix.wait() == Some(Wait::Sequence)) {
            deadline = None;
            // `timeout` resolves a lone prefix into a literal one, but also a
            // half-typed session number into a switch — so the actions it
            // produces must be acted on, not just the bytes.
            for mut step in prefix.timeout() {
                attachment.saw_input(&mut step);
                if let Some(outcome) = act(&mut attachment.writer, step)? {
                    return Ok(outcome);
                }
            }
        }

        if stdin_ready {
            match waiter.read_stdin(&mut buf) {
                Ok(0) => return Ok(Outcome::StdinClosed),
                Err(e) => return Err(NvmuxError::Io(e)),
                Ok(len) => {
                    // Deliberately NOT logged: every keystroke the user types,
                    // into a /tmp file that outlives the session.
                    for mut step in prefix.feed(&buf[..len]) {
                        attachment.saw_input(&mut step);
                        if let Some(outcome) = act(&mut attachment.writer, step)? {
                            return Ok(outcome);
                        }
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

        // The hint bar, after the keys and not before them: the prefix that
        // raises it arrives in the read above, and the timeout that lowers it is
        // settled above too, so anywhere earlier in the loop the bar would be a
        // whole wake-up late — and the only wake-up scheduled is the prefix's own
        // deadline, so it would appear exactly as the prefix expired. After the
        // resize, so the row is the terminal's as it is now, and after the notice,
        // for the reason written down there.
        //
        // `busy` is the bar's lull rule: the child having spoken this pass, and
        // a held first paint — a screen the terminal has not been shown a word
        // of yet — both mean this is no moment to write. The notice's own pass
        // above shares the first two terms but asks the boundary decoder
        // instead of waiting for a lull.
        //
        // A lull is not enough while the notice is up, though: a tail that ran
        // out of patience can leave the terminal half way through one of the
        // session's characters on a pass the session said nothing on.
        //
        // A command that ends the relay has already returned from the block
        // above and never reaches this, which is what stops `<prefix> Space`
        // flashing a bar on its way to the picker.
        let busy = child_spoke || attachment.hold.is_some() || attachment.mid_sequence();
        match bar.step(
            Instant::now(),
            busy,
            term::terminal_size(),
            prefix.pending(),
            attachment.shadow.as_ref(),
        ) {
            hint::Act::Idle => {}
            hint::Act::Write(bytes) => attachment.write_over_session(&bytes)?,
            hint::Act::Blanked(bytes) => {
                // The bar has blanked its row because nothing here knows what
                // was under it — no shadow, so no copy of those cells anywhere
                // but the server. The same repaint the notice's erase asks for,
                // asked the same way; skipped when one is already in flight,
                // since a `:mode` repaints the whole screen and this row with it.
                attachment.write_over_session(&bytes)?;
                if erasing.is_none() {
                    attachment.resize_to(term::terminal_size());
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

        // Only on an idle tick: a child that exits is noticed through its pty
        // above, so this exists for the one that *stopped*, and a `waitid` per
        // keystroke would buy nothing.
        if woke.idle && attachment.pty.check() == ChildState::Gone {
            return Ok(Outcome::ChildExited);
        }
    }
}

/// Carry out one instruction from the prefix machine: forward bytes to the
/// child, or turn a command into the [`Outcome`] that ends the relay.
///
/// Exhaustive on purpose — no `_` arm — so a new [`Action`] cannot be dropped
/// silently. The timeout and the feed path both come through here, which is
/// what stops them drifting apart.
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
        Step::Act(Action::Switch(num)) => Some(Outcome::Switch(Target::Number(num))),
        Step::Act(Action::Cycle(dir)) => Some(Outcome::Switch(Target::Step(dir))),
    })
}

/// How long an escape sequence cut short by the end of a read waits for the
/// rest of itself, before being passed on as it stands.
///
/// The bytes of one key are split only by a buffer boundary, so this is
/// machine time, not human time, and it should be short: in a terminal that
/// still sends the Escape key as a bare `ESC`, every `Esc` is held for exactly
/// this long before Neovim sees it. tmux's `escape-time`, which is the same
/// wait in the same place, defaults to 10 ms.
const SEQUENCE_TIMEOUT: Duration = Duration::from_millis(10);

/// How often to look at an otherwise-idle child.
///
/// A child that has *stopped* generates no readable fd and no signal we
/// subscribe to, so without a bounded wait the relay would sit forever against a
/// frozen screen.
const IDLE_POLL_MS: u64 = 1000;

/// A relayed write as it goes to the terminal: the session's own bytes behind
/// the sequence that opens a synchronized update, and behind nothing at all
/// when `open` says one already is.
///
/// Terminals hold `?2026` as a mode rather than as a count, so a second open
/// inside a span changes nothing — and a sequence that changes nothing is
/// still a sequence, landing wherever the session's last write happened to
/// stop. See [`Attachment::close_span`] for what stops one there.
fn framed(bytes: &[u8], open: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(fade::SYNC_BEGIN.len() + bytes.len());
    if !open {
        out.extend_from_slice(fade::SYNC_BEGIN);
    }
    out.extend_from_slice(bytes);
    out
}

/// Whether this chunk of client output contains a DA1 request.
///
/// A request split across two reads is missed, and that is survivable rather
/// than handled: the reply is an optimisation, and missing one costs the second
/// it used to cost every time. Neovim writes its whole shutdown sequence in one
/// `flush_buf`, so in practice the request arrives whole.
fn asks_for_device_attributes(chunk: &[u8]) -> bool {
    chunk.windows(DA1_REQUEST.len()).any(|w| w == DA1_REQUEST)
}

/// Where in these bytes, on their way from the terminal to a client, an
/// in-band size report is — `CSI 48 ; rows ; cols ; height ; width t` — and
/// the rows and columns it reports. The first one only, which is all a
/// terminal answering a mode being set sends; and only whole, which is how
/// the key parser passes on a sequence (see [`crate::keys`]).
fn find_size_report(bytes: &[u8]) -> Option<(std::ops::Range<usize>, u16, u16)> {
    const START: &[u8] = b"\x1b[48;";
    let at = bytes.windows(START.len()).position(|w| w == START)?;
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

/// Whether these bytes, on their way from the terminal to a client, carry a
/// terminal's answer to DA1: `CSI ? Ps ; … c`. What says a client's opening
/// questions have all been answered (see `Attachment::startup_answered`).
fn answers_device_attributes(bytes: &[u8]) -> bool {
    let mut from = 0;
    while let Some(at) = bytes[from..]
        .windows(3)
        .position(|w| w == b"\x1b[?")
        .map(|i| from + i + 3)
    {
        let rest = &bytes[at..];
        let params = rest
            .iter()
            .take_while(|b| b.is_ascii_digit() || **b == b';')
            .count();
        if params > 0 && rest.get(params) == Some(&b'c') {
            return true;
        }
        from = at;
    }
    false
}

/// The chunk with every DA1 request taken out, and nothing else touched. See
/// [`Attachment::drain_until_eof`] for why a request nvmux answers itself must
/// not reach the real terminal as well.
///
/// Exact bytes only: `ESC [ c` is the request, and neither the reply shape
/// (`ESC [ ? … c`) nor DA2 (`ESC [ > c`) contains it.
fn without_device_attributes_request(chunk: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if !asks_for_device_attributes(chunk) {
        return std::borrow::Cow::Borrowed(chunk);
    }
    let mut out = Vec::with_capacity(chunk.len());
    let mut i = 0;
    while i < chunk.len() {
        if chunk[i..].starts_with(DA1_REQUEST) {
            i += DA1_REQUEST.len();
        } else {
            out.push(chunk[i]);
            i += 1;
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Paint the screen again after one of nvmux's own screens cleared it.
///
/// A `--remote-ui` client paints only what the server sends, so this is a
/// request to the server, and there are two ways to make one. Over RPC,
/// `:mode` clears the grid and redraws it in full ([`repaint_through_server`]);
/// that is the one that works, and the only one that works on a terminal
/// with in-band resize reports (DEC mode 2048 — kitty, ghostty, foot), where
/// the client ignores `SIGWINCH`. Failing that, a [`nudge`] of the pty size,
/// which needs no answer from the server and so is what a server too busy to
/// give one gets: its resize is queued and repaints once the loop is free.
///
/// Whichever way, the pty is first told the terminal's current size, since
/// the terminal may have changed shape while the picker was up and the pty is
/// where the client learns its size from. A no-op when nothing changed, and
/// a resize the client acts on when it did — one more repaint in that case,
/// which is rare enough not to matter.
///
/// A resume also puts the terminal's mouse reporting back to what the client
/// had. The screen that ran in between set its own and turned it off again,
/// and the client — which enabled the mouse once, at startup — will not do so
/// again. What it had is its server's to say (see [`repaint_through_server`]);
/// a server too busy to say gets Neovim's default, `mouse=nvi` without
/// `'mousemoveevent'`, which is [`term::MouseReporting::Buttons`]. Wrong for a
/// `mouse=` session behind a busy server, which comes back with button
/// tracking on until its Neovim next flips the mouse itself — and right for
/// everything else, where the alternative is a client that no longer answers
/// the mouse at all.
///
/// The asking and the waiting are one step here, which is a resume's to be:
/// the terminal is blank, no relay has started, and the screen coming back is
/// the thing the user is waiting for. The other caller wants the same repaint
/// without the wait, and takes the two apart — see [`Erasing`].
fn repaint(attachment: &mut Attachment, why: Repaint) {
    attachment.resize_to(term::terminal_size());
    let served = repaint_through_server(&attachment.sock, why);
    apply(attachment, why, served);
}

/// What the relay does once a repaint has answered — or has failed to: the
/// fallback for a server that could not be asked, and, for a resume, the mouse
/// mode it reported.
///
/// Its own step because the two callers reach it from different places. A
/// resume asks and waits, one statement above; the attach notice asks from a
/// thread ([`Erasing`]) and gets here whenever the answer turns up. Both need
/// the same two things done, and both can only do them here — a `resize` is
/// the relay thread's to make.
fn apply(attachment: &mut Attachment, why: Repaint, served: Served) {
    if why == Repaint::Resume {
        term::set_mouse_reporting(served.mouse.unwrap_or(term::MouseReporting::Buttons));
    }
    if !served.repainted {
        // The shrink and its undo are not shown to the shadow: what the
        // client draws after them is for the size it was just told.
        nudge(&attachment.pty, term::terminal_size());
    }
}

/// The repaint that takes the attach notice off the screen, in flight on a
/// thread of its own.
///
/// The cells the box covered are the server's, and asking it for them costs
/// two round trips — `nvim_get_mode`, and then the `:mode` that does the
/// painting. Measured against sessions behind a delay proxy: 123 ms over a
/// 60 ms link, 303 ms over a 150 ms one. Waited for on the relay's own thread,
/// that is exactly how long the editor forwards no keystroke and copies no
/// byte — a stall a whole second after the switch that put the box up, which
/// is to say in the middle of whatever the user started typing when they
/// arrived.
///
/// Nothing about the relay needs the answer, so it does not wait for one. The
/// repaint arrives as ordinary output from the session, like every other byte
/// the server sends; the only thing the answer decides is the fallback, and
/// [`apply`] performs that on the relay's next pass.
///
/// The cheapest of the crate's threads: it owns one connection, reads nothing
/// else, and every call it makes is bounded ([`RESUME_TIMEOUT`]), so it ends on
/// its own whether or not anyone is still listening. A relay that ends first
/// drops the receiver, and the send fails into nothing.
///
/// Not a resume's repaint, which stays where it is: there the terminal is
/// blank, the relay has not started, and the screen coming back *is* what the
/// user is waiting for. Except a kept client's (`[client] per_session`), whose
/// screen is already painted back from its copy by then: its repaint is asked
/// here too, with the relay running (see [`relay`]).
struct Erasing {
    rx: mpsc::Receiver<Served>,
    started: Instant,
    /// What the repaint is for, for the timing record.
    what: &'static str,
}

impl Erasing {
    /// Ask, and come back for the answer later.
    fn start(sock: &Path) -> std::io::Result<Self> {
        Self::start_for(sock, "attach notice erase")
    }

    /// [`Erasing::start`], for something other than the notice: a kept
    /// client coming back to the front, whose screen is already painted back
    /// and wants the server's exact one on top (see [`relay`]).
    fn start_for(sock: &Path, what: &'static str) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let sock = sock.to_path_buf();
        std::thread::Builder::new()
            .name("nvmux-notice-repaint".into())
            .spawn(move || {
                // A receiver that has gone is a relay that ended first, and
                // there is nobody left to tell.
                let _ = tx.send(repaint_through_server(&sock, Repaint::Notice));
            })?;
        Ok(Self {
            rx,
            started: Instant::now(),
            what,
        })
    }

    /// The verdict, if it has arrived. A worker that ended without one is
    /// [`Served::NONE`], which is what any other failure to be answered is:
    /// the fallback runs, exactly as it would have.
    fn answer(&self) -> Option<Served> {
        match self.rx.try_recv() {
            Ok(served) => Some(served),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Served::NONE),
        }
    }
}

/// How often the relay looks for [`Erasing`]'s answer while one is in flight.
///
/// Only the fallback waits on it, so this is not a deadline, it is a bound on
/// how late a nudge can be: without it an idle relay would not look again for
/// [`IDLE_POLL_MS`], and the session that needs the nudge — one too busy to
/// answer — is exactly the one producing nothing to wake the loop.
const ERASE_POLL: Duration = Duration::from_millis(50);

/// Why the screen is being painted again, which decides what the repaint may
/// do on the way.
///
/// A resume may end a hit-enter prompt to get itself served: the screen it is
/// coming back to is blank, the prompt was painted over by whatever nvmux
/// drew, and the user has no way to answer one they cannot see. It also asks
/// what the client's mouse setting is, so the terminal can be put back to it.
///
/// Taking an attach notice off the screen may do neither. The prompt would be
/// one the user is looking at right now — a startup error, most likely — and a
/// notice that says which session you are in is never worth answering
/// somebody's editor for. A [`nudge`] stands in, and where that does not reach
/// (a terminal with in-band resize reports) the box simply waits for the prompt
/// to end. And the mouse is the client's own already: nothing ran in between.
///
/// Its second caller is the hint bar ([`crate::hint`]) coming down where there
/// is no shadow to put its row back from, and every word above applies to it
/// unchanged — more so, if anything: the bar is asked for by a keystroke rather
/// than once per attach, so the one thing it must never do is answer a prompt
/// the user is in the middle of reading.
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

/// The terminal mouse reporting a client has on, from its server's answer to
/// `[&mouse, &mousemoveevent]` — the two options Neovim's TUI enables the
/// modes from: nothing for an empty `'mouse'`, button tracking otherwise, and
/// motion on top of it with `'mousemoveevent'` set. `None` for an answer that
/// is not that pair.
///
/// The mode the editor is in is not consulted, though Neovim does: `mouse=nvi`
/// turns the mouse off on the command line and at a hit-enter prompt. A
/// resume from either puts button tracking on a little early; Neovim turns it
/// off and on again itself as the mode next changes, which is the moment it
/// would have anyway.
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

/// Shrink the pty by a row and put it back, so the client asks the server for
/// a resize, which the server answers — when its main loop is free to — with
/// a cleared, fully redrawn grid.
///
/// What a server that cannot be asked over RPC gets: one busy in a `:!make`
/// or in Lua, where the repaint then arrives with the prompt the command ends
/// in; or one waiting for a key with its event queue off, where it repaints
/// at the more-prompt and not otherwise (a pending operator stays blank until
/// its next key, which completes it). A client on a terminal with in-band
/// resize reports ignores this altogether.
fn nudge(pty: &sys::Pty, size: PtySize) {
    let nudged = PtySize {
        rows: size.rows.saturating_sub(1).max(1),
        ..size
    };
    pty.resize(nudged);
    pty.resize(size);
}

/// How long a resume waits on the server before relaying regardless.
///
/// `nvim_get_mode` answers within a millisecond whenever it answers at all —
/// idle, or waiting for a key — so no answer means the loop is busy in a
/// `:!make` or in Lua, and nothing would repaint yet whatever nvmux did. The
/// `:mode` that follows is a deferred call whose reply can take a few hundred
/// milliseconds under a plugin-heavy config (measured: 384 ms with AstroNvim),
/// so this is longer than `rpc::CONNECT_TIMEOUT` and much shorter than
/// `rpc::PROBE_TIMEOUT`.
const RESUME_TIMEOUT: Duration = Duration::from_secs(1);

/// Ask the server, over RPC, to paint the whole screen again — and, for a
/// resume, what the client's mouse setting is. Says whether it repainted.
///
/// A server parked at a hit-enter prompt does not run its event queue, so a
/// resize would sit there and the terminal would stay blank until a key ended
/// the prompt — the key the user would have to press blind, and which then
/// also runs as a command unless it is one of the few the prompt consumes. So
/// that prompt is ended first — where `prompt` allows it, which a resume does
/// and taking an attach notice off the screen does not — with the `<CR>` it
/// consumes (a fast call, delivered at once). Any other state that is waiting
/// for a key is left alone: `<CR>` would scroll the more-prompt or complete a
/// pending operator.
/// Then `:mode`, which clears the grid and redraws it — `:redraw!` does not
/// clear, and a redraw of an unchanged grid sends the client nothing.
///
/// Two things `:mode` does that are accepted rather than wanted. It resets
/// the editor's pending-message state, so a script that printed something and
/// is waiting in `getchar()` for a key will not get the hit-enter prompt it
/// would have had once the key arrived (the text itself was already gone from
/// the screen). And ending the prompt discards what it showed, from every
/// attached UI; `:messages` keeps it. The reply is waited for so the relay
/// does not start over a repaint in flight; a reply that is late rather than
/// missing still repaints, a moment after the nudge that stands in for it.
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
    // Before the repaint, which is the call that can take a few hundred
    // milliseconds: the answer is wanted whether or not that one comes back
    // in time.
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

#[cfg(all(test, unix))]
mod tests {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    use crate::sys::unix::{peek_child, pollfd, read_fd, ready};

    use super::*;
    use crate::keys::Direction;

    /// Poll until the child reaches `want`: signals are delivered
    /// asynchronously, so the state is not visible on the first look. Returns
    /// what it last saw, so a caller asserts on the state rather than on a
    /// timeout — a wait that ends in the wrong state is the failure.
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

    /// The notice is nvmux's own drawing, not the session's, and the shadow
    /// must not take it for one.
    ///
    /// The whole cross-fade rests on this: the grid holds what the client drew
    /// where the box is, which is the only copy of it anywhere once the
    /// terminal has been written over. Shown the notice, the shadow would hold
    /// the box and have nothing underneath it to give back — and
    /// `has_contents` cannot tell nvmux's border from Neovim's text, so the
    /// box would also answer for the client's first paint and release a hold
    /// over a screen the client had not drawn.
    ///
    /// The hint bar goes through the same `write_over_session`, and for the
    /// bar the stake is sharper: its erase is a composite out of this grid —
    /// it is what puts the editor's last row back — so a grid that had
    /// swallowed the bar would erase the bar with the bar.
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

    /// The rule [`Attachment::boundary`] states, end to end: however the reads
    /// happened to fall, what nvmux leaves on the terminal is between the
    /// session's own sequences. That is what lets the notice go straight back
    /// up after every write instead of waiting for a lull that a busy session
    /// never gives it.
    ///
    /// Cut at seven bytes, which lands inside a sequence far more often than
    /// the relay's own 8 KiB does and is therefore the harder case.
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

        // And the few bytes held back to make that true are not the notice's
        // to keep once the notice is over.
        a.stop_watching_the_terminal().expect("release");
        assert!(a.kept_due().is_none(), "bytes outlived the notice");
        assert!(
            !a.between_sequences(),
            "nvmux is still watching a terminal it has nothing to draw on"
        );
    }

    /// A relayed write is held back from the terminal's eye inside a
    /// synchronized update, so the notice that follows it lands in the same
    /// presented frame — and the span never outlives the pass that opened it,
    /// because a terminal left inside one shows nothing at all until its own
    /// timeout runs out.
    #[test]
    fn a_synchronized_update_never_outlives_the_pass_that_opened_it() {
        let mut a = attached_to("exit 0");
        a.boundary = Some(boundary::Boundary::new());
        a.relay_output(b"\x1b[1;1Hhello", Instant::now())
            .expect("relay");
        assert!(a.span, "the session's repaint was not held back at all");

        a.close_span().expect("close");
        assert!(!a.span);
        // And closing one that is already closed is nothing at all, which is
        // what lets every way out of the relay call it without asking.
        a.close_span().expect("close");
        assert!(!a.span);
    }

    /// Except that it waits for the session to finish its sentence. A tail
    /// that ran out of patience is handed over mid-character, and the two
    /// sequences a span is made of are nvmux's bytes like any other: dropped
    /// between a lead byte and its continuation they leave the terminal a
    /// character it cannot read. So the span is held open for a pass instead,
    /// which is the very wait it exists to impose.
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

    /// And the other half of holding one open: the pass that finds it open
    /// adds its bytes to it rather than opening a second, which would be the
    /// same sequence dropped into the same half-finished character.
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

    /// And when the notice is over there is no next pass to close it on, so
    /// whatever the session is half way through, the span goes. A terminal
    /// left inside one shows nothing until its own timeout runs out.
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

    /// A client that brackets its own frames is left to it. Terminals hold
    /// `?2026` as a mode rather than as a count, so a span of nvmux's around
    /// one of the client's would end at the client's reset and present the
    /// frame early — the tear the span exists to prevent.
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

    /// A held first paint has not reached the terminal, so there is nothing
    /// for a box to sit on and nothing under it to melt into — whatever the
    /// boundary makes of bytes the terminal has not been shown.
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

    /// A stand-in client on a real pty, driven by `sh -c $script`.
    ///
    /// Real, because what the teardown tests are about is what a *signal* does
    /// to a process: a fake `Child` would supply the `kill` under test and
    /// answer for itself. `sock` is never read on these paths. The pty is
    /// opened by [`spawn_client_with`], the same path the real client takes,
    /// sized from `term::terminal_size()` (or 24x80), which no caller depends
    /// on.
    fn attached_to(script: &str) -> Attachment {
        let mut cmd = sys::Command::new("sh");
        cmd.arg("-c");
        cmd.arg(script);
        let mut a = spawn_client_with(
            "id000000",
            Path::new("/nvmux-test-never-read.sock"),
            "",
            cmd,
        )
        .expect("spawn sh");
        // Nothing to announce and no shadow unless a test installs one: keep
        // the fixture independent of the fade/palette process globals.
        a.announce = None;
        a.shadow = None;
        a
    }

    /// Wait for a stand-in client to say it is ready, by reading the `r` its
    /// script echoes.
    ///
    /// Without this a test that signals the child immediately races `sh`'s own
    /// startup: the signal lands before the script has run, under whatever
    /// disposition the shell was exec'd with rather than the one the script sets.
    /// Measured — a `trap '' HUP` client signalled straight after
    /// [`attached_to`] dies of the hangup it is supposed to ignore, and the test
    /// then passes for the wrong reason.
    fn wait_until_ready(a: &Attachment) {
        assert!(
            wait_for(a, b'r'),
            "the stand-in client never reported ready"
        );
    }

    /// Read the stand-in client's output until `byte` shows up, or give up.
    ///
    /// The only channel a shell has to report what it did, which is how a test
    /// tells "the signal was delivered" from "the process happens to still be
    /// in the state I expected".
    fn wait_for(a: &Attachment, byte: u8) -> bool {
        let fd = a.pty.fd().expect("the master has an fd");
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

    /// The pid of a stand-in client. Every teardown assertion is about a pid
    /// rather than about the `Attachment`, because the point is what is left
    /// behind after the attachment is gone.
    fn pid_of(a: &Attachment) -> Pid {
        Pid::from_raw(a.pty.pid().expect("a spawned child has a pid") as i32)
    }

    /// A hangup must count as "something happened", not be ignored.
    ///
    /// On Linux the pty master reports `POLLHUP` rather than `POLLIN` once the
    /// child is gone. Waiting only for `POLLIN` would leave the loop spinning
    /// against a dead pty at full speed, because `poll` returns immediately
    /// with a revents the loop then does nothing about.
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
    fn every_action_maps_to_its_outcome() {
        let mut out = Vec::new();
        let cases = [
            (Action::Picker, Outcome::ToPicker),
            (Action::Detach, Outcome::Detached),
            (Action::Create, Outcome::CreateNew),
            (Action::Help, Outcome::ShowHelp),
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

    /// A hold ends at the first lull after something has been drawn — not
    /// at a lull before, which is a client between its queries and its grid.
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
        // Nothing drawn: the lull is not worth waking for, only the cap is —
        // or the relay would spin on a wake-up that is already past.
        assert_eq!(
            hold.wake_at(false),
            Some(t0 + Duration::from_millis(10) + HOLD_CAP)
        );

        // Every chunk restarts the lull.
        hold.take(b"~", t0 + Duration::from_millis(30));
        assert!(!hold.due(quiet, true));
        assert!(hold.due(t0 + Duration::from_millis(30) + HOLD_SETTLE, true));
    }

    /// A client that never stops drawing is released at the cap, drawn or
    /// not; one that writes an absurd amount is released by size.
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

    /// The frames go on the screen the client is about to draw on: the split
    /// is just past its alternate-screen entry, or at the start without one.
    #[test]
    fn the_release_splits_just_past_the_alternate_screen_entry() {
        let bytes = b"\x1b[?1004h\x1b[?1049h\x1b[2J~";
        let at = split_at_alt_screen_entry(bytes);
        assert_eq!(&bytes[..at], b"\x1b[?1004h\x1b[?1049h");
        assert_eq!(&bytes[at..], b"\x1b[2J~");
        assert_eq!(split_at_alt_screen_entry(b"\x1b[2J~"), 0);
        assert_eq!(split_at_alt_screen_entry(b""), 0);
    }

    /// The one outcome with a session to begin before the screen goes. Every
    /// variant is asked, so a new one has to decide rather than default to
    /// starting nothing — the failure that would cost an overlap silently.
    #[test]
    fn only_a_switch_names_a_session_to_begin_early() {
        for target in [Target::Number(3), Target::Step(Direction::Next)] {
            assert_eq!(Outcome::Switch(target).switch_target(), Some(target));
        }
        for other in [
            Outcome::ToPicker,
            Outcome::CreateNew,
            Outcome::ShowHelp,
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

    /// Exactly one outcome reaches the next session without a screen on the
    /// way, and it is the one that has to clear for itself. Getting this wrong
    /// in either direction is invisible in a test that only checks the
    /// outcomes: too narrow leaves the old session on screen through the
    /// spawn, too wide clears a screen that is about to draw anyway.
    #[test]
    fn only_a_switch_reaches_the_next_session_without_a_screen() {
        for target in [Target::Number(3), Target::Step(Direction::Next)] {
            assert!(Outcome::Switch(target).leads_straight_into_another_relay());
        }
        for other in [
            Outcome::ToPicker,
            Outcome::CreateNew,
            Outcome::ShowHelp,
            Outcome::Detached,
            Outcome::ChildExited,
            Outcome::StdinClosed,
        ] {
            assert!(
                !other.leads_straight_into_another_relay(),
                "{other:?} either opens a screen or ends the relay"
            );
        }
    }

    /// A msgpack-RPC server that answers what the attach probe asks and records
    /// the methods it was asked, so the probe's call sequence is observable
    /// without a real Neovim — which the machines running these tests may not
    /// have, and which would answer whatever it liked anyway.
    ///
    /// It answers *every* call the probe could plausibly make, `nvim_get_api_info`
    /// included, so a regression fails on the sequence rather than on a fake that
    /// could not keep up.
    fn recording_server(
        sock: &Path,
        mode: &'static str,
        blocking: bool,
    ) -> std::thread::JoinHandle<Vec<String>> {
        recording_server_with(sock, mode, blocking, 0, DEFAULT_MOUSE)
    }

    /// What a server with Neovim's defaults answers `[&mouse, &mousemoveevent]`.
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

    /// A server whose prompt *ends* when it is typed at: it answers `mode`
    /// until the first `nvim_input`, and an idle `n` from then on.
    ///
    /// The fixed-mode servers above model a prompt that outlasts every key,
    /// which is the state the probe must refuse to wait on. This models the
    /// ordinary one, and is the only way to see the probe go on to the UI
    /// count after answering a prompt — the thing that made the `<CR>` worth
    /// sending in the first place.
    fn recording_server_clearing_on_input(
        sock: &Path,
        mode: &'static str,
    ) -> std::thread::JoinHandle<Vec<String>> {
        serve_recording(sock, mode, true, 0, DEFAULT_MOUSE, None, true)
    }

    /// An idle server that records `method` and then never answers it — a
    /// session that is busy, as the attach probe sees one.
    fn recording_server_stalling_on(
        sock: &Path,
        method: &'static str,
    ) -> std::thread::JoinHandle<Vec<String>> {
        serve_recording(sock, "n", false, 0, DEFAULT_MOUSE, Some(method), false)
    }

    /// A server waiting for a key: it reports `mode` with `blocking` set and
    /// answers every fast call, while `method` — a deferred one — is recorded
    /// and never answered. Which is what a session in that state does: the
    /// event queue is off, so the call is taken and not run.
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
        // Bounded rather than blocking: if the probe never connects at all — a
        // socket path this platform refuses, say — an accept that waited forever
        // would hang the test instead of failing it on an empty recording.
        listener
            .set_nonblocking(true)
            .expect("a non-blocking listener");
        std::thread::spawn(move || {
            let mut asked = Vec::new();
            // Set by the first `nvim_input` where the prompt is one that ends.
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
            // The accepted stream inherits O_NONBLOCK on some platforms, and the
            // reads below are blocking ones with their own bound.
            stream.set_nonblocking(false).expect("a blocking stream");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("a bounded read");
            let mut reader = std::io::BufReader::new(&stream);
            let mut out = &stream;
            // Ends on EOF, which is `spawn` dropping the client after the last
            // call — so joining this handle waits for the whole probe.
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
                    // Only the one expression the resume asks; anything else
                    // is answered with nothing, as a fake should.
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
                // The keys go on the record too: which prompt gets a `<CR>` is
                // half of what this pins.
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
                    // Recorded and never answered. The loop goes on reading,
                    // so it ends on the EOF a cancelled probe's shutdown
                    // produces — which is how joining this proves the hangup
                    // reached the server.
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

    /// The notice's repaint is asked from a thread and collected later: the
    /// two round trips it takes happen while the relay goes on relaying.
    ///
    /// What the server records is the whole of what is asked — the mode, and
    /// then the `:mode` that paints — so a change that started typing at the
    /// session to take a box off its screen fails here.
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

    /// **The point of the thread.** Asking must not wait, whatever the session
    /// is doing: a server that never answers the `:mode` used to hold the relay
    /// for a whole [`RESUME_TIMEOUT`], with the editor forwarding no keystroke
    /// and copying no byte for as long.
    ///
    /// Bounded well under that budget rather than at it, so the assertion is
    /// about not waiting rather than about how fast a thread starts.
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

        // And when the budget does run out, the verdict is the fallback's:
        // nothing was painted, so the relay nudges.
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

    /// A resume may end a hit-enter prompt to get its screen back; taking an
    /// attach notice off the screen may not.
    ///
    /// The prompt in that case is one the user is looking at right now — a
    /// startup error, most likely, since the notice is up for a second either
    /// side of an attach — and answering somebody's editor is far too much to
    /// pay for tidying a notice away. Without the gate the two paths share one
    /// `<CR>`, and nothing about the call sequence would look wrong.
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

    /// A resume asks the server what the client's mouse setting is and reads
    /// the answer as the modes Neovim's TUI would have enabled from it; taking
    /// a notice off the screen does not ask, since nothing ran in between.
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

    /// A server that cannot be asked — busy, or waiting for a key — says
    /// nothing about the mouse, and the resume falls back on the default.
    #[test]
    fn a_server_that_cannot_be_asked_says_nothing_about_the_mouse() {
        let sock = crate::test_support::scratch_sock("pty-mousebusy");
        // Blocking on something that is not a hit-enter prompt: an operator
        // waiting for its motion, say.
        let server = recording_server_with(&sock, "no", true, 0, ("", 0));
        let served = repaint_through_server(&sock, Repaint::Resume);
        assert_eq!(served, Served::NONE);
        assert_eq!(server.join().expect("the server thread"), ["nvim_get_mode"]);
        let _ = std::fs::remove_file(&sock);
    }

    /// What Neovim's TUI enables from the two options, and nothing from an
    /// answer that is not the pair asked for.
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

    /// A client that sits on its pty until it is hung up, in place of `nvim`,
    /// which the machines running these tests may not have. `tag` goes on its
    /// command line, so the process table says whether it is still there once
    /// `spawn` has let go of it — a pid it wrote to a file would not do, since
    /// a probe can fail before the shell has run a single line.
    fn standin_client(tag: &str) -> sys::Command {
        let mut cmd = sys::Command::new("sh");
        cmd.arg("-c");
        cmd.arg(format!("while :; do read x; done # {}", standin_mark(tag)));
        cmd
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

    /// The attach probe's call sequence, which is the whole of what a change to it
    /// can break: the mode and the UI count together — one round trip, not two —
    /// and then a `<CR>` for the one prompt that key ends.
    ///
    /// The `nvim_get_api_info` that used to lead this cost a round trip, and a
    /// forwarded one over SSH, while proving nothing the `?` on the mode does not.
    /// The server here would still answer it, so its absence is what is asserted.
    #[test]
    fn the_attach_probe_asks_the_mode_and_the_ui_count_together_and_nothing_else() {
        for (tag, mode, want) in [
            ("idle", "n", &["nvim_get_mode", "nvim_list_uis"][..]),
            // The prompt ends on the key, and the mode is read again before the
            // UI count is: the `<CR>` is not sent and forgotten, because the
            // reading it was sent on is the only thing that says *waiting* for
            // `list_uis` is safe. One `nvim_list_uis` however many prompts were
            // answered — the one already in flight is what the count comes from.
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
            let _ = spawn_with("probe", &sock, "1  probe", standin_client(tag))
                .expect("a passed probe is an attachment");
            assert_eq!(
                server.join().expect("the server thread"),
                want,
                "the attach probe asked something else in mode {mode:?}"
            );
            let _ = std::fs::remove_file(&sock);
        }
    }

    /// The notice is the only thing a blocked session can be told with, so it
    /// has to carry the one instruction that gets the user out: press a key.
    ///
    /// Amended rather than replaced, because the name is still the answer to
    /// the question the box normally answers — which session is this — and a
    /// blocked attach should not stop saying it. A notice already spent says
    /// nothing new: the box is up for a second once, and there is no second
    /// one to correct.
    #[test]
    fn a_blocked_session_says_so_on_its_notice_without_losing_its_name() {
        let mut attachment = attached_to("while :; do read x; done");
        attachment.announce = Some("dotfiles".to_string());
        attachment.note_waiting_for_a_key();
        let said = attachment.announce.clone().expect("a notice");
        assert!(said.starts_with("dotfiles"), "{said:?}");
        assert!(said.contains("press a key"), "{said:?}");

        // Saying it twice says it once: no stutter on the one line the box has.
        attachment.note_waiting_for_a_key();
        assert_eq!(attachment.announce.as_deref(), Some(said.as_str()));

        // A notice already spent stays spent: the box is up once, and there is
        // no second one to correct.
        attachment.announce = None;
        attachment.note_waiting_for_a_key();
        assert_eq!(attachment.announce, None);
    }

    /// **The regression test for the attach that never came back.** A session
    /// waiting for a key must never be *waited on* for a deferred answer.
    ///
    /// `nvim_list_uis` is deferred, and the probe's connection has no budget,
    /// so an answer from a session in one of these states is not slow but
    /// absent: a probe that waited for it would park, `ui::attaching` would
    /// spin, and the only way out would be the user giving up. Measured
    /// against 0.12.5, all three of these did exactly that.
    ///
    /// So these servers never answer it at all, and the assertion is that the
    /// probe says `Blocked` anyway — from which the attach goes ahead, because
    /// the client nvmux is about to hand over is how the user presses the key
    /// that ends the wait. Sending the question is not the hazard and is no
    /// longer forbidden (see [`probe_on`]); waiting for it is, and a probe that
    /// read the count before deciding would hang here rather than fail.
    #[test]
    fn a_session_waiting_for_a_key_is_never_waited_on_for_a_deferred_answer() {
        for (tag, mode, keys) in [
            // A more-prompt. `<CR>` scrolls it a line rather than ending it.
            ("more", "rm", 0),
            // A half-typed `g`: `n` with `blocking` set, which is the state the
            // reported session was found in. Indistinguishable over RPC from a
            // command the user started, so nvmux types nothing.
            ("pending", "n", 0),
            // A hit-enter prompt that outlasts every key — something is raising
            // them faster than they are answered. The budget is spent and then
            // it is handed over, rather than typed at for ever.
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

    /// The client is started before the probe has said yes, so a probe that
    /// says no must take it down again: nothing left running, nothing left to
    /// print on the terminal. Both ways a probe can say no — nothing serving
    /// the socket, and a server with too many UIs — and the error is the
    /// probe's, not the client's.
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
            let err = spawn_with("gone", &sock, "1  gone", standin_client(tag))
                .expect_err("the probe must refuse");
            match (too_many, &err) {
                (true, NvmuxError::Session(crate::error::SessionError::TooManyUis { .. })) => {}
                (false, NvmuxError::Rpc(_)) => {}
                _ => panic!("{tag}: the wrong refusal: {err}"),
            }
            if let Some(server) = server {
                server.join().expect("the server thread");
            }
            // Retired and reaped before the error came back, so it is not in
            // the process table at all — not even as a zombie.
            assert!(
                !standin_running(tag),
                "{tag}: the client is still there after a failed probe"
            );
            let _ = std::fs::remove_file(&sock);
        }
    }

    /// The attach probe waits on the session with no budget, so the user's
    /// cancel is the only thing that ends a wait on a busy one — and it has
    /// to end all of it. The worker thread must stop, or it would answer the
    /// prompt the session shows once it is free; and the server must see the
    /// connection go, so the `<CR>` that answer would have carried can never
    /// be sent on it.
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
        let attached = spawn_with("limit", &sock, "1  limit", standin_client("atlimit"));
        assert!(
            attached.is_ok(),
            "{MAX_UIS} UIs, one of them possibly ours, must attach"
        );
        drop(attached);
        server.join().expect("the server thread");
        let _ = std::fs::remove_file(&sock);
    }

    /// The invariant the switch path's overlap rests on, and the one that was
    /// silently false before: the hangup signals and comes straight back.
    ///
    /// The client catches the hangup and reports it rather than dying of it, so
    /// all three things are separable — that a signal was sent at all, that
    /// nothing escalated past it, and that nothing waited for it. On the state
    /// alone the old path looked the same, because `portable_pty`'s `Child::kill`
    /// SIGKILLed without waiting; the elapsed bound is what fails on it, measured
    /// at 200 ms there against two `kill` syscalls here.
    #[test]
    fn hanging_up_signals_the_client_without_waiting_for_it() {
        // `read` blocks in the shell itself rather than in a child, so the
        // process being signalled is the one that reports.
        let mut a = attached_to("trap 'printf H' HUP; printf r; while :; do read x; done");
        let pid = pid_of(&a);
        // The `printf r` comes after the `trap`, so this is the point from which
        // the hangup is genuinely caught rather than fatal.
        wait_until_ready(&a);

        let start = Instant::now();
        a.hang_up();
        let took = start.elapsed();

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

        // And the reap is what gets rid of a client that will not leave. This is
        // the only test that reaches that escalation: `portable_pty`'s own
        // SIGKILL used to get there first, so the SIGKILL below it had never run.
        a.reap();
        assert_eq!(
            settle(pid, Peek::Gone),
            Peek::Gone,
            "a client that stays after its hangup must still be killed by the reap"
        );
    }

    /// A client blocked writing into a master nobody reads still takes its
    /// hangup, because the reap reads it out while it waits for it.
    ///
    /// That is the state the switch path leaves behind: the picker stops reading
    /// the outgoing client the moment it opens, so one that was mid-repaint fills
    /// the pty buffer and stops there, in a `write` it cannot leave and with its
    /// handler never reached. Measured: 10 ms to exit on its own with the drain,
    /// against the full `REAP_TIMEOUT` and a SIGKILL without it.
    #[test]
    fn a_client_blocked_writing_still_takes_its_hangup() {
        // Catches the hangup rather than dying of it: a default disposition is
        // fatal whether the process is blocked or not, and would prove nothing.
        let mut a = attached_to(
            "trap 'exit 0' HUP; printf r; while :; do printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; done",
        );
        let pid = pid_of(&a);
        wait_until_ready(&a);
        // Long enough to fill the master's buffer and stop in `write`; a machine
        // fast enough to still be running only makes the assertion easier.
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

    /// A client that will not leave until its terminal answers the question it
    /// asks on the way out is answered, and leaves.
    ///
    /// Neovim's shape, not an invented one: `tui_stop` writes its restore
    /// sequence, sends `ESC [ c`, and blocks for `EXIT_TIMEOUT_MS` — a whole
    /// second — waiting for the reply that says the terminal has caught up.
    /// The stand-in does the same thing with `dd`. Without the answer this is
    /// the reap's deadline and a SIGKILL two seconds later; with it, the client
    /// goes at once.
    #[test]
    fn a_client_waiting_on_its_terminal_is_answered_and_goes() {
        // `stty raw` because a real client's terminal is in raw mode and the
        // reply carries no newline: left canonical, the stand-in's read would
        // block for a line that never comes and the test would pass for the
        // wrong reason.
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

    /// A stand-in client that is kept: it has a ledger, which is what makes
    /// an attachment one to park.
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

    /// Parked is not left unread: a client with far more to say than a pty
    /// holds says all of it while parked, and what it said last — a title,
    /// after two hundred kilobytes that would have stopped it cold — is in
    /// the ledger when it is taken back.
    #[test]
    fn a_parked_client_is_read_while_it_waits() {
        let a = kept(
            r#"head -c 200000 /dev/zero | tr '\0' x; printf '\033]0;said it all\007'; exec sleep 30"#,
        );
        let pid = pid_of(&a);
        let parked = a.park().expect("parked");
        // Two hundred kilobytes read in pieces take a few milliseconds; a
        // client nobody reads would still be stuck a second later.
        std::thread::sleep(Duration::from_secs(1));
        let a = parked.unpark().expect("handed back");
        assert_eq!(pid_of(&a), pid, "not the same client");
        assert_eq!(
            a.ledger.as_ref().and_then(Ledger::title),
            Some(&b"\x1b]0;said it all\x07"[..]),
            "the client was not read to the end of what it said while parked"
        );
    }

    /// Taking a parked client back hands over the very client — alive, and
    /// its thread gone.
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

    /// A client that leaves while parked — its session killed, its link gone
    /// — is not handed back, and is not left behind either.
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

    /// A parked client that asks its terminal for its attributes is on its
    /// way out — or suspending, which from here is no better — and is retired,
    /// quickly, rather than answered and kept.
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

    /// Letting go of a parked client retires it, on its thread.
    #[test]
    fn dropping_a_parked_client_retires_it() {
        let a = kept("printf r; exec sleep 30");
        wait_until_ready(&a);
        let pid = pid_of(&a);
        drop(a.park().expect("parked"));
        assert!(reaped(pid), "the client outlived its parking");
    }

    /// Whether the process is gone and waited for — not merely exited, which
    /// a zombie nobody reaped is too — within a few seconds.
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

    /// A client that takes its size in band ignores the signal, so a new size
    /// is sent to it as the report a terminal would send, whole, as its input
    /// — and only a new size: the repaint is asked of the server regardless.
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

    /// And a client that takes its size from the pty is told nothing: the
    /// resize is enough, and a report would reach it as keys.
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

    /// Output up to and including `byte`, as text, or whatever came before a
    /// few seconds ran out.
    fn read_until(a: &Attachment, byte: u8) -> String {
        let fd = a.pty.fd().expect("the master has an fd");
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
        let fd = a.pty.fd().expect("the master has an fd");
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

    /// The terminal's answer to in-band size reports turned back on is taken
    /// out of what it sends the client: only that answer, only at the size
    /// the client has, only once, and only while it is looked for.
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

    /// A report by its shape, wherever it falls in what the terminal sent,
    /// and nothing short of one.
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

    /// A kept client is parked only once the terminal has answered what it
    /// asked on the way up — the answer to its DA1, the last of its questions,
    /// forwarded to it through the relay.
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

    /// The answer by its shape, wherever it falls in what the terminal sent,
    /// and nothing that merely looks like it.
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

    /// The request as Neovim spells it, and nothing else.
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

    /// A client that stopped itself takes the hangup only because it is
    /// continued as well.
    ///
    /// SIGHUP is not delivered to a stopped process at all, and `reap`'s
    /// `try_wait` does not pass `WUNTRACED`, so it reads a stopped child as
    /// still running: without the SIGCONT this sits for the whole of
    /// `REAP_TIMEOUT` and then kills it outright. Reachable because `<prefix>`
    /// chords are read by nvmux's own stdin loop, not by the client, so a client
    /// that `Pty::check` has not revived yet can be retired on the switch.
    #[test]
    fn a_suspended_client_is_continued_so_the_hangup_lands() {
        let mut a = attached_to("exec sleep 30");
        let pid = pid_of(&a);
        kill(pid, Signal::SIGSTOP).expect("stop");
        assert_eq!(settle(pid, Peek::Stopped), Peek::Stopped);

        a.hang_up();

        // Gone, and without the reap having killed it: the pending hangup was
        // delivered, which only happens once the process is running again.
        assert_eq!(
            settle(pid, Peek::Gone),
            Peek::Gone,
            "a stopped client stays stopped unless the hangup continues it"
        );
        a.reap();
    }

    /// `Drop` is the one retirement path — every `break` out of the session loop
    /// leans on it — and it was untested. No explicit hangup, and the client is
    /// still gone by the time the attachment is.
    #[test]
    fn dropping_an_attachment_hangs_it_up_and_reaps_it() {
        let a = attached_to("exec sleep 30");
        let pid = pid_of(&a);

        let start = Instant::now();
        drop(a);
        let took = start.elapsed();

        // Reaped, so the peek gets ECHILD, which reads as gone.
        assert_eq!(peek_child(pid), Peek::Gone);
        // Gone alone cannot tell "left on its hangup" from "killed by the reap's
        // deadline two seconds later", and only the first is what `Drop` claims.
        assert!(
            took < Duration::from_millis(500),
            "dropping took {took:?}, so the client was killed by the deadline \
             rather than by the hangup"
        );
    }

    /// The switch path's shape: hang up, do something slow, then drop. The
    /// second hangup `Drop` sends has to be harmless — nothing in between waits
    /// for the child, so the pid is still ours and cannot have been recycled —
    /// and the client has to end up reaped exactly once.
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
        // Reaped, not merely dead: ECHILD means no zombie was left behind, which
        // is the half of "exactly once" a peek cannot see.
        assert_eq!(
            waitpid(pid, Some(WaitPidFlag::WNOHANG)),
            Err(nix::errno::Errno::ECHILD),
            "a zombie was left behind, so the client was never waited for"
        );
    }

    /// The peek sees a stop, leaves it in place, and sees an exit without
    /// reaping it. Runs against a real child because `waitid` is what differs
    /// between platforms: nix does not bind it on macOS, so it goes through
    /// `libc`, and the flag and `si_code` handling has to hold on both.
    #[test]
    fn peek_reports_stopped_and_gone_without_consuming_either() {
        use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};

        /// Kill and reap the child however the test ends.
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

        // What `Pty::check` does with a stop: consume it, then continue.
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

    /// A write failure is the relay's error, not something to swallow: the
    /// child is gone and the loop must end.
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

    /// What the teardown drain relays is the client's restore sequence minus
    /// the one question nvmux answers in the terminal's place. Everything
    /// around it, and every sequence that merely resembles it, goes through
    /// untouched.
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
