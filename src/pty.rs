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
//! **The child-to-terminal direction is never parsed, buffered by line, or
//! rewritten.** That is the entire reason this design works: bracketed paste,
//! the kitty keyboard protocol, truecolor, undercurl, terminal titles, OSC 52
//! clipboard and DA1/XTGETTCAP round-trips all function because the child
//! negotiates directly with the real terminal. Any "improvement" that inspects
//! this direction breaks a subset of them. There is likewise no
//! `nvim_ui_attach`, no `grid_line` handling and no grid diffing anywhere in
//! this crate — `nvim --remote-ui` already is that client.
//!
//! The one thing that ever *joins* this direction is the attach notice (see
//! [`crate::announce`]): a box, drawn once the child has been quiet long enough
//! that it cannot be mid-sequence. Even then nothing is parsed — the safe
//! moment is found with a clock, not a decoder.
//!
//! Three hazards that have no other home in the code:
//!
//! * **Take the writer exactly once.** `MasterPty::take_writer()` errors on a
//!   second call.
//! * **The child's exit code carries no information.** A server killed while
//!   attached gives 0; terminating the child to detach gives 1; a failed attach
//!   gives 1. Teardown is driven off the master read result instead.
//! * `portable-pty` vendors its own `nix`, so never pass a `nix` type across
//!   that boundary — `MasterPty::get_termios()` returns *its* `Termios`, not ours.

use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use portable_pty::PtySize;
use portable_pty::{Child, CommandBuilder, MasterPty, PtyPair};

use crate::error::{NvmuxError, Result};
use crate::keys::{Action, Prefix, Step, Wait};
use crate::{announce, rpc, term, winch};

/// Neovim aborts on the seventeenth attached UI rather than returning an error
/// (`src/nvim/ui.c`: `if (ui_count == MAX_UI_COUNT) { abort(); }`), which would
/// SIGABRT the whole server and destroy the session. Refuse well before that.
const MAX_UIS: usize = 8;

// At compile time: getting this wrong does not fail a test, it SIGABRTs a
// user's session.
const _: () = assert!(MAX_UIS < 16);

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
    announce: Option<String>,
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty>,
    writer: Box<dyn Write + Send>,
    /// True once the child has been through a full relay, so a resume knows it
    /// must force a repaint — and that the client is one that already entered
    /// the alternate screen and will not enter it again, so the terminal has
    /// to be put back there before it draws.
    resumed: bool,
    /// True once the child has been waited for, so `Drop` does not do it twice.
    reaped: bool,
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

impl Attachment {
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
    /// at all, and a client that stopped itself (see `check_child`) can be
    /// retired before the pump's idle poll has revived it. `reap`'s `try_wait`
    /// does not pass `WUNTRACED` either, so without this a stopped client would
    /// read as running for the whole of `REAP_TIMEOUT`. Hung up first and
    /// continued second, it takes the pending hangup and goes.
    ///
    /// Sending it twice is harmless, which is what lets `Drop` run after an
    /// explicit `hang_up`: nothing in between waits for the child — the peek in
    /// `check_child` passes `WNOWAIT` — so the pid is still ours and cannot have
    /// been recycled.
    pub fn hang_up(&mut self) {
        if self.reaped {
            return;
        }
        // Nothing to signal without one, and `reap` falls back on `wait`.
        let Some(pid) = self.child.process_id() else {
            return;
        };
        // SAFETY: a pid this process spawned and has not waited for, so it names
        // that child or nothing.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGHUP);
            libc::kill(pid as libc::pid_t, libc::SIGCONT);
        }
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
        let master = self.master.as_raw_fd();
        let deadline = Instant::now() + REAP_TIMEOUT;
        // Remembered rather than answered once: the request is seen on exactly
        // one pass of this loop, and that pass is the likeliest moment for the
        // client's input queue to be momentarily full.
        let mut owed = false;
        while Instant::now() < deadline {
            if let Some(fd) = master {
                owed |= discard_pending(fd);
                if owed {
                    owed = !self.answer_device_attributes();
                }
            }
            match self.child.try_wait() {
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                // Exited, or already reaped by someone else.
                _ => return,
            }
        }
        if let Some(pid) = self.child.process_id() {
            tracing::warn!(pid, "the remote-ui client ignored SIGHUP; killing it");
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
        let _ = self.child.wait();
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
        let Some(fd) = self.master.as_raw_fd() else {
            return true;
        };
        if !writable(fd) {
            tracing::debug!("the departing client's input queue is full; not answering yet");
            return false;
        }
        tracing::debug!("answering the departing client's DA1 request");
        // One `write`, not `write_all`: `writable` promises room for a byte,
        // not for seven, and the loop `write_all` would do on a short write is
        // the same park by another name. A torn reply cannot hurt this client —
        // it is exiting, and the worst case is the wait it would have had
        // anyway. (Which is why no live client is ever written to here.)
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
        let Some(fd) = self.master.as_raw_fd() else {
            return;
        };
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
                    // Read before the write, because the write borrows `self`.
                    owed |= asks_for_device_attributes(&buf[..len]);
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(&without_device_attributes_request(&buf[..len]));
                    let _ = out.flush();
                    drop(out);
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

/// Start a `--remote-ui` client for a session.
///
/// `sock` must already be reachable from *this* machine — locally the session
/// socket, over SSH the local end of a forward.
///
/// `announce` is what the first relay says the session is (see
/// [`crate::announce::label`]). Not an `Option`: every spawn is a change of
/// session, and making that unrepresentable is the point.
pub fn spawn(session_id: &str, sock: &Path, announce: &str) -> Result<Attachment> {
    // `CommandBuilder::new` seeds the child's environment from ours, so `TERM`,
    // `COLORTERM` and everything else the client negotiates with reach it
    // without being copied by hand.
    let mut cmd = CommandBuilder::new("nvim");
    cmd.arg("--server");
    cmd.arg(sock);
    cmd.arg("--remote-ui");
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }
    spawn_with(session_id, sock, announce, cmd)
}

/// [`spawn`] with the client command supplied, which is how the tests run it
/// against a stand-in client where there is no `nvim`.
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
/// copies a pty to the terminal, which starts after this returns. A probe that
/// fails ends the client with the attachment it was building — through the
/// same hangup and reap as any other retirement — and its output dies with the
/// master. That matters over a forward whose remote end has gone: the client's
/// own failure message there is empty, after ~165 bytes of escape sequences it
/// has already written, and none of them are seen.
///
/// A client that could not be started at all is reported before any probe,
/// since the message — `nvim` is not here — is the useful one.
fn spawn_with(
    session_id: &str,
    sock: &Path,
    announce: &str,
    cmd: CommandBuilder,
) -> Result<Attachment> {
    let t_spawn = std::time::Instant::now();
    let attachment = spawn_client(session_id, sock, announce, cmd)?;
    tracing::debug!(
        ms = t_spawn.elapsed().as_secs_f64() * 1000.0,
        "timing: client spawn"
    );

    // The round trips to the session — over ssh, through the forward — while
    // the client makes its own.
    let t_probe = std::time::Instant::now();
    if let Err(e) = probe(session_id, sock) {
        // Explicitly, so the retirement is not left to a binding: the client
        // must be gone, and its pty with it, before the error is shown.
        drop(attachment);
        return Err(e);
    }
    tracing::debug!(
        ms = t_probe.elapsed().as_secs_f64() * 1000.0,
        "timing: attach probe"
    );
    Ok(attachment)
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
fn probe(session_id: &str, sock: &Path) -> Result<()> {
    let mut client = rpc::Client::connect(sock, rpc::PROBE_TIMEOUT)?;
    // A server at a hit-enter prompt cannot answer `list_uis`, a deferred
    // call, until a key ends the prompt. Usually there is nobody to type one:
    // the client that was showing the prompt is gone, and this is its
    // replacement — which may already be attached by now, and then sees the
    // prompt end and repaints, as any other UI would. So the prompt is ended
    // here with `<CR>`, the key it consumes without running anything (see
    // `rpc::Mode::at_hit_enter` for why only this prompt), and the count that
    // follows is real, not guessed.
    //
    // A deliberate trade-off where another UI is still attached: that user's
    // prompt ends too, and whatever it was showing — a `:!make` page, a Lua
    // traceback — leaves their screen unread. `g<` brings the page back and
    // `:messages` keeps the rest; the old behaviour was a 3 s failure to
    // attach at all. Advisory, not a lock: a key from that other UI in the
    // microseconds between the two calls ends the prompt first, and this
    // `<CR>` then runs in whatever mode it left.
    if client.get_mode()?.at_hit_enter() {
        tracing::info!(
            id = session_id,
            "attach: ending the session's hit-enter prompt"
        );
        client.input("<CR>")?;
    }
    // Not `unwrap_or(0)`: a server too busy to answer is exactly the one whose
    // UI count is unknown, and guessing zero is how the seventeenth attach
    // happens.
    //
    // `>` rather than `>=`: the client this probe runs alongside may or may
    // not be attached yet, so the count is off by at most one, in the
    // direction of our own client. `MAX_UIS` is half of Neovim's limit, so a
    // count one too high refuses the ninth other client rather than the
    // eighth, and a count one too low still stops seven short of the abort.
    let uis = client.list_uis()?;
    if uis > MAX_UIS {
        return Err(NvmuxError::Session(
            crate::error::SessionError::TooManyUis {
                id: session_id.to_string(),
                attached: uis,
            },
        ));
    }
    Ok(())
}

/// Open a pty and start the client on it.
fn spawn_client(
    session_id: &str,
    sock: &Path,
    announce: &str,
    cmd: CommandBuilder,
) -> Result<Attachment> {
    // Right *before* the child starts: `PtySize::default()` is 24x80. Note
    // `crossterm::size()` is (cols, rows) while `PtySize` is { rows, cols } —
    // passing them positionally transposes the screen.
    let size = term::terminal_size();
    let pair: PtyPair = portable_pty::native_pty_system()
        .openpty(size)
        .map_err(|e| NvmuxError::Io(std::io::Error::other(e.to_string())))?;

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| NvmuxError::Io(std::io::Error::other(e.to_string())))?;

    // MANDATORY: while this process holds the slave open the master never sees
    // EOF, so the relay would hang forever after the child exits.
    drop(pair.slave);

    let writer = pair
        .master
        .take_writer()
        .map_err(|e| NvmuxError::Io(std::io::Error::other(e.to_string())))?;

    Ok(Attachment {
        session_id: session_id.to_string(),
        sock: sock.to_path_buf(),
        announce: Some(announce.to_string()),
        child,
        master: pair.master,
        writer,
        resumed: false,
        reaped: false,
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
pub fn relay(
    mut attachment: Attachment,
    highest_session_num: u32,
) -> Result<(Outcome, Option<Attachment>)> {
    let master_fd = attachment
        .master
        .as_raw_fd()
        .ok_or_else(|| NvmuxError::Io(std::io::Error::other("pty master has no fd")))?;

    let winch = winch::Winch::install()?;
    if attachment.resumed {
        // The client entered the alternate screen itself on its first relay
        // and has not been told that every screen since — the picker, the
        // prompt, the help, the `Switch` branch below — left it. It will not
        // enter it again: the repaint below redraws the grid wherever the
        // terminal is. Left on the primary screen it draws there, and then
        // on a detach its `rmcup` and nvmux's own `?1049l` have nothing to
        // leave, and its last frame is what the shell prompt lands on.
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
    }
    let mut raw = term::RawMode::enter()?;

    // What the child buffered while blocked mid-write describes a screen the
    // picker has since drawn over.
    if attachment.resumed {
        discard_pending(master_fd);
        repaint(attachment.master.as_ref(), &attachment.sock, EndPrompt::Yes);
    }
    attachment.resumed = true;

    // Taken, not copied: `<prefix> Space` and `<prefix> ?` come back to this same
    // client, and a session you never left has nothing to announce.
    let popup = announce::Popup::arm(attachment.announce.take(), Instant::now());

    let outcome = pump(
        &mut attachment,
        master_fd,
        &winch,
        highest_session_num,
        popup,
    );

    match outcome {
        Ok(
            held
            @ (Outcome::ToPicker | Outcome::CreateNew | Outcome::ShowHelp | Outcome::Switch(_)),
        ) => {
            raw.restore();
            // Clear here rather than leaving it to the next `relay`, which runs
            // on the far side of the client spawn: what is on the terminal until
            // then is the session being switched away from. Harmless when the
            // number names nothing — the same client is resumed, and a resume
            // forces a repaint.
            if held.leads_straight_into_another_relay() {
                term::leave_alt_screen_and_clear();
            }
            Ok((held, Some(attachment)))
        }
        Ok(Outcome::ChildExited) => {
            // The child is gone and has emitted its own restore; relay the rest
            // of it, then hand back to the picker.
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
) -> Result<Outcome> {
    let stdin_fd = std::io::stdin().as_raw_fd();
    let keys = crate::config::get().keys;
    let mut prefix = Prefix::with_prefix(highest_session_num, keys.prefix);
    let prefix_timeout = Duration::from_millis(keys.timeout_ms);
    // When a pending prefix, half-typed number or cut-off escape sequence must
    // be settled. An instant rather than a per-poll timeout on purpose: a child
    // that keeps producing output keeps `poll` returning early, and a timeout
    // that restarted on every wake-up would never fire while a spinner is
    // running. It is set when the machine arms and cleared when it settles.
    let mut deadline: Option<Instant> = None;
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
        // The attach notice has a clock of its own — a lull to wait out, and a
        // life to end — and it is folded in here so both are honoured by the
        // same `poll`. Absolute on both sides, for the reason above.
        let next = match (deadline, popup.as_ref()) {
            (Some(d), Some(p)) => Some(d.min(p.wake_at(Instant::now()))),
            (Some(d), None) => Some(d),
            (None, Some(p)) => Some(p.wake_at(Instant::now())),
            (None, None) => None,
        };

        // A stopped child produces no poll activity at all, so even an idle
        // wait is bounded; an armed prefix shortens it to its own deadline.
        let timeout_ms = match next {
            Some(d) => {
                // Rounded up, not down: rounded down, the last fraction of a
                // millisecond before every deadline would be spent in
                // `poll` calls that return at once.
                let left = d.saturating_duration_since(Instant::now());
                let ms = left.as_micros().div_ceil(1000);
                ms.min(IDLE_POLL_MS as u128) as libc::c_int
            }
            None => IDLE_POLL_MS,
        };

        let mut fds = [pollfd(stdin_fd), pollfd(master_fd), pollfd(winch.fd())];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(NvmuxError::Io(err));
        }

        // Output first, so the screen is current before a keystroke is acted on.
        let child_spoke = n > 0 && ready(&fds[1]);
        if child_spoke {
            match read_fd(master_fd, &mut buf) {
                // EOF on macOS, EIO on Linux: both mean the slave closed.
                Ok(0) | Err(_) => return Ok(Outcome::ChildExited),
                Ok(len) => {
                    // Byte for byte, unparsed and unbuffered; see the module
                    // docs.
                    let mut out = std::io::stdout().lock();
                    out.write_all(&buf[..len])?;
                    out.flush()?;
                    if first_byte.is_none() {
                        first_byte = Some(relay_started.elapsed());
                    }
                    painted_bytes += len;
                    last_byte = Instant::now();
                }
            }
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

        if n > 0 && ready(&fds[2]) {
            winch.drain();
            // Enough on its own: the kernel signals the pty's foreground group
            // and the client calls try_resize.
            let _ = attachment.master.resize(term::terminal_size());
        }

        // After the frame it sits on and before any keystroke is acted on. The
        // size is read here rather than once outside the loop, so a resize
        // between the drawing and the erasing is seen by both.
        if let Some(p) = popup.as_mut() {
            let act = p.step(Instant::now(), child_spoke, term::terminal_size());
            match act {
                announce::Act::Idle => {}
                announce::Act::Paint(bytes) => {
                    let mut out = std::io::stdout().lock();
                    out.write_all(&bytes)?;
                    out.flush()?;
                }
                announce::Act::Erase => {
                    // Timed because it is a second full repaint of the session,
                    // a whole second after the switch — the one nvmux asks for
                    // rather than the one the attach produced.
                    let t_erase = std::time::Instant::now();
                    // The same repaint a resume asks for, and for the same
                    // reason: the cells the box covered are the server's, and
                    // only the server can say what was under them. Not the same
                    // licence, though — see [`EndPrompt`].
                    repaint(attachment.master.as_ref(), &attachment.sock, EndPrompt::No);
                    tracing::debug!(
                        ms = t_erase.elapsed().as_secs_f64() * 1000.0,
                        "timing: attach notice erase (:mode repaint)"
                    );
                    popup = None;
                }
                announce::Act::Done => popup = None,
            }
        }

        // Settle a pending prefix or number before reading anything new, so a
        // key that arrives just after the deadline is not swallowed as a
        // command. A cut-off escape sequence is the other way round: input
        // already waiting is the rest of it, or the key that decides it, and
        // must be read first — the screen write above can outlast the short
        // wait, and settling then would break a sequence whose remainder had
        // already arrived.
        let stdin_ready = n > 0 && ready(&fds[0]);
        let overdue = deadline.is_some_and(|d| Instant::now() >= d);
        if overdue && !(stdin_ready && prefix.wait() == Some(Wait::Sequence)) {
            deadline = None;
            // `timeout` resolves a lone prefix into a literal one, but also a
            // half-typed session number into a switch — so the actions it
            // produces must be acted on, not just the bytes.
            for step in prefix.timeout() {
                if let Some(outcome) = act(&mut attachment.writer, step)? {
                    return Ok(outcome);
                }
            }
        }

        if stdin_ready {
            match read_fd(stdin_fd, &mut buf) {
                Ok(0) => return Ok(Outcome::StdinClosed),
                Err(e) => return Err(NvmuxError::Io(e)),
                Ok(len) => {
                    // Deliberately NOT logged: every keystroke the user types,
                    // into a /tmp file that outlives the session.
                    for step in prefix.feed(&buf[..len]) {
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

        // Only on an idle tick: a child that exits is noticed through its pty
        // above, so this exists for the one that *stopped*, and a `waitid` per
        // keystroke would buy nothing.
        if n == 0 && check_child(attachment) == ChildState::Gone {
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
const IDLE_POLL_MS: libc::c_int = 1000;

#[derive(Debug, PartialEq, Eq)]
enum ChildState {
    Running,
    Gone,
}

/// Notice a child that has exited, and revive one that has stopped.
///
/// # The stop case, and why it is continued rather than escalated
///
/// `Ctrl-z` is forwarded to Neovim as an ordinary byte — nvmux does not
/// special-case it. If the `--remote-ui` client responds by stopping *itself*,
/// the user is left looking at a frozen screen: nvmux still owns the terminal,
/// so there is no shell underneath to have been returned to, and the suspended
/// client is not a state anyone can do anything with.
///
/// So a stopped child is continued with `SIGCONT` rather than torn down. It
/// cannot loop: the stop notification is consumed when read, so a client that
/// keeps stopping itself is continued once per stop.
///
/// `portable_pty::Child::try_wait` cannot be used here — it does not pass
/// `WUNTRACED`, so a stopped child reads as "still running". The peek below uses
/// `WNOWAIT` so an *exit* status is left in place for portable-pty's own reaper.
fn check_child(attachment: &mut Attachment) -> ChildState {
    use nix::sys::signal::{kill, Signal};
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use nix::unistd::Pid;

    let Some(raw) = attachment.child.process_id() else {
        return ChildState::Running;
    };
    let pid = Pid::from_raw(raw as i32);

    match peek_child(pid) {
        Peek::Stopped => {
            // Consume just the stop notification, which does not reap the
            // process, then wake it up. Without consuming it, the peek would
            // report the same stop on every poll and SIGCONT would be sent
            // repeatedly.
            let sig = match waitpid(pid, Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED)) {
                Ok(WaitStatus::Stopped(_, sig)) => Some(sig),
                _ => None,
            };
            tracing::info!(?sig, "the remote-ui client stopped; continuing it");
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

/// Look at a child's state without changing it.
///
/// `waitid`, not `waitpid`. WNOWAIT is only valid for waitid(2): Linux's
/// wait4 rejects the flag outright with EINVAL, so the waitpid spelling of
/// this peek silently never reports anything and a stopped client would sit
/// frozen forever. (Measured — it returned EINVAL on every poll.)
///
/// Called through `libc` rather than `nix`: nix binds `waitid` only on Linux
/// and FreeBSD, but the call itself is POSIX and macOS has it too.
fn peek_child(pid: nix::unistd::Pid) -> Peek {
    // WNOHANG with nothing to report succeeds and leaves `si_signo` (and
    // `si_pid`) zero, which is only distinguishable if the struct starts out
    // zeroed.
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

/// Read whatever is available. Only called after `poll` says the fd is ready.
fn read_fd(fd: RawFd, buf: &mut [u8]) -> std::io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// A `poll` entry asking whether `fd` is readable. Shared with
/// [`crate::proc::Shell`], which polls a child's pipes the way this module
/// polls a pty; a negative `fd` is skipped by `poll`, which is how a pipe
/// that has reached EOF is left out.
pub(crate) fn pollfd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

/// Whether a write of at least one byte would go through without blocking.
///
/// A zero timeout: this is a question, not a wait. The one caller has nothing
/// it may block for — see [`Attachment::answer_device_attributes`].
fn writable(fd: RawFd) -> bool {
    let mut p = [libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    }];
    let n = unsafe { libc::poll(p.as_mut_ptr(), 1, 0) };
    n > 0 && p[0].revents & libc::POLLOUT != 0
}

/// Readable, or gone. `POLLHUP` matters as much as `POLLIN`: on Linux the
/// master reports hangup rather than readability once the child exits, and
/// ignoring it would leave the loop spinning against a dead pty.
pub(crate) fn ready(p: &libc::pollfd) -> bool {
    p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
}

/// Throw away buffered child output without writing it anywhere, reporting
/// whether the child asked for its terminal's device attributes on the way.
///
/// That answer is acted on by [`Attachment::reap`] alone, which is the one
/// caller whose client has no terminal left to ask. The resume path ignores it:
/// a request there belongs to a client that is *staying*, and the real terminal
/// answers that one itself as soon as the pump is running again.
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

/// Whether this chunk of client output contains a DA1 request.
///
/// A request split across two reads is missed, and that is survivable rather
/// than handled: the reply is an optimisation, and missing one costs the second
/// it used to cost every time. Neovim writes its whole shutdown sequence in one
/// `flush_buf`, so in practice the request arrives whole.
fn asks_for_device_attributes(chunk: &[u8]) -> bool {
    chunk.windows(DA1_REQUEST.len()).any(|w| w == DA1_REQUEST)
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
fn repaint(master: &dyn MasterPty, sock: &Path, prompt: EndPrompt) {
    let size = term::terminal_size();
    let _ = master.resize(size);
    if !repaint_through_server(sock, prompt) {
        nudge(master, size);
    }
}

/// Whether a repaint may end a hit-enter prompt to get itself served.
///
/// A resume may: the screen it is coming back to is blank, the prompt was
/// painted over by whatever nvmux drew, and the user has no way to answer one
/// they cannot see.
///
/// Taking an attach notice off the screen may not. The prompt would be one the
/// user is looking at right now — a startup error, most likely — and a notice
/// that says which session you are in is never worth answering somebody's
/// editor for. A [`nudge`] stands in, and where that does not reach (a terminal
/// with in-band resize reports) the box simply waits for the prompt to end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndPrompt {
    Yes,
    No,
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
fn nudge(master: &dyn MasterPty, size: PtySize) {
    let nudged = PtySize {
        rows: size.rows.saturating_sub(1).max(1),
        ..size
    };
    let _ = master.resize(nudged);
    let _ = master.resize(size);
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

/// Ask the server, over RPC, to paint the whole screen again. True if it did.
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
fn repaint_through_server(sock: &Path, prompt: EndPrompt) -> bool {
    let mut client = match rpc::Client::connect(sock, RESUME_TIMEOUT) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "resume: could not reach the server");
            return false;
        }
    };
    let mode = match client.get_mode() {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(error = %e, "resume: the server is busy; nudge only");
            return false;
        }
    };
    if mode.blocking && !(mode.at_hit_enter() && prompt == EndPrompt::Yes) {
        tracing::debug!(
            mode = %mode.mode,
            "resume: the server is waiting for a key nvmux must not type; nudge only"
        );
        return false;
    }
    if mode.at_hit_enter() {
        tracing::debug!("resume: ending the hit-enter prompt");
        if let Err(e) = client.input("<CR>") {
            tracing::debug!(error = %e, "resume: could not end the prompt; nudge only");
            return false;
        }
    }
    match client.command("mode") {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!(error = %e, "resume: gave up waiting for the repaint; nudging");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

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

    /// A stand-in client on a real pty, driven by `sh -c $script`.
    ///
    /// Real, because what the teardown tests are about is what a *signal* does
    /// to a process: a fake `Child` would supply the `kill` under test and
    /// answer for itself. `sock` is never read on these paths.
    fn attached_to(script: &str) -> Attachment {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize::default())
            .expect("openpty");
        let mut cmd = CommandBuilder::new("sh");
        cmd.arg("-c");
        cmd.arg(script);
        let child = pair.slave.spawn_command(cmd).expect("spawn sh");
        // As in `spawn`: while this process holds the slave open the master
        // never sees EOF.
        drop(pair.slave);
        let writer = pair.master.take_writer().expect("writer");
        Attachment {
            session_id: "id000000".to_string(),
            sock: PathBuf::from("/nvmux-test-never-read.sock"),
            announce: None,
            child,
            master: pair.master,
            writer,
            resumed: false,
            reaped: false,
        }
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
        let fd = a.master.as_raw_fd().expect("the master has an fd");
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
        Pid::from_raw(a.child.process_id().expect("a spawned child has a pid") as i32)
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

    /// A scratch socket path, kept short: `check_sock_path` refuses anything over
    /// `MAX_SOCK_PATH`, and macOS's temp dir is not short.
    fn temp_sock(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("nvmux-t{}-{tag}.sock", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
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
        recording_server_with_uis(sock, mode, blocking, 0)
    }

    /// The same, reporting `uis` attached UIs.
    fn recording_server_with_uis(
        sock: &Path,
        mode: &'static str,
        blocking: bool,
        uis: usize,
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
                    "nvim_get_mode" => Value::Map(vec![
                        (Value::String("mode".into()), Value::String(mode.into())),
                        (Value::String("blocking".into()), Value::Boolean(blocking)),
                    ]),
                    "nvim_list_uis" => Value::Array(vec![Value::Nil; uis]),
                    // The byte count, which nvmux ignores.
                    "nvim_input" => Value::from(4u64),
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
                let reply = Value::Array(vec![Value::from(1u64), msgid, Value::Nil, result]);
                if rmpv::encode::write_value(&mut out, &reply).is_err() {
                    break;
                }
            }
            asked
        })
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
        for (tag, prompt, want) in [
            (
                "promptyes",
                EndPrompt::Yes,
                &["nvim_get_mode", "nvim_input(<CR>)", "nvim_command"][..],
            ),
            ("promptno", EndPrompt::No, &["nvim_get_mode"][..]),
        ] {
            let sock = temp_sock(tag);
            let server = recording_server(&sock, "r", true);
            let repainted = repaint_through_server(&sock, prompt);
            assert_eq!(
                repainted,
                prompt == EndPrompt::Yes,
                "{prompt:?} at a hit-enter prompt"
            );
            assert_eq!(
                server.join().expect("the server thread"),
                want,
                "{prompt:?} asked the wrong things at a hit-enter prompt"
            );
            let _ = std::fs::remove_file(&sock);
        }
    }

    /// A client that sits on its pty until it is hung up, in place of `nvim`,
    /// which the machines running these tests may not have. `tag` goes on its
    /// command line, so the process table says whether it is still there once
    /// `spawn` has let go of it — a pid it wrote to a file would not do, since
    /// a probe can fail before the shell has run a single line.
    fn standin_client(tag: &str) -> CommandBuilder {
        let mut cmd = CommandBuilder::new("sh");
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
    /// can break: the mode first — a fast call, answered even at a prompt — then
    /// a `<CR>` for the one prompt that key ends, then the deferred UI count.
    ///
    /// The `nvim_get_api_info` that used to lead this cost a round trip, and a
    /// forwarded one over SSH, while proving nothing the `?` on the mode does not.
    /// The server here would still answer it, so its absence is what is asserted.
    #[test]
    fn the_attach_probe_asks_the_mode_then_the_ui_count_and_nothing_else() {
        for (tag, mode, blocking, want) in [
            ("idle", "n", false, &["nvim_get_mode", "nvim_list_uis"][..]),
            (
                "hitenter",
                "r",
                true,
                &["nvim_get_mode", "nvim_input(<CR>)", "nvim_list_uis"][..],
            ),
            // Blocking, but not the prompt a `<CR>` ends: a more-prompt, or a
            // half-typed `g`. Typing for the user there would run something.
            ("more", "rm", true, &["nvim_get_mode", "nvim_list_uis"][..]),
        ] {
            let sock = temp_sock(tag);
            let server = recording_server(&sock, mode, blocking);
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

    /// The client is started before the probe has said yes, so a probe that
    /// says no must take it down again: nothing left running, nothing left to
    /// print on the terminal. Both ways a probe can say no — nothing serving
    /// the socket, and a server with too many UIs — and the error is the
    /// probe's, not the client's.
    #[test]
    fn a_failed_probe_leaves_no_client_behind() {
        for (tag, too_many) in [("nobody", false), ("toomany", true)] {
            let sock = temp_sock(tag);
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

    /// The count the probe reads may or may not include the client started
    /// alongside it, so the guard tolerates exactly one over.
    #[test]
    fn the_ui_limit_allows_for_the_client_being_counted() {
        let sock = temp_sock("atlimit");
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
    /// that `check_child` has not revived yet can be retired on the switch.
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
