//! The PTY proxy. Milestone 4.
//!
//! # The contract
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
//! the kitty keyboard protocol, truecolor, undercurl, terminal title sequences,
//! OSC 52 clipboard and DA1/XTGETTCAP query/response round-trips all function
//! because the child negotiates directly with the real terminal. Any
//! "improvement" that inspects this direction breaks a subset of them.
//!
//! nvmux does **not** implement a Neovim UI. There is no `nvim_ui_attach`, no
//! `grid_line` handling, no highlight table and no grid diffing anywhere in this
//! crate — that is thousands of lines of the most bug-prone code in this space,
//! and `nvim --remote-ui` already is that client.
//!
//! # Things milestone 4 must get right
//!
//! * **Read the real terminal size before spawning.** `PtySize::default()` is
//!   24x80. Note `crossterm::terminal::size()` returns `(columns, rows)` while
//!   `PtySize` is `{ rows, cols }` — passing them positionally transposes the
//!   screen. `window_size()` also gives pixel dimensions, without which sixel
//!   and kitty image protocols break inside the session.
//! * **Drop the slave after `spawn_command`**, or the master reader never sees
//!   EOF and the relay thread hangs forever after the child exits.
//! * **Take the writer exactly once.** `MasterPty::take_writer()` errors on a
//!   second call.
//! * **Never run two readers on the master.** `try_clone_reader()` dups the same
//!   open file description, so two readers race and split the stream — which in
//!   a proxy means randomly deleting chunks of the user's screen. Read once and
//!   tee in userspace if logging is ever wanted.
//! * **RPC-ping the session before spawning the child.** Against a dead socket
//!   the client prints `Remote ui failed to start: connection refused` — but
//!   through an SSH forward that message is empty, after ~165 bytes of escape
//!   sequences have already hit the terminal.
//! * **The child's exit code carries no information.** Server killed while
//!   attached gives 0; nvmux terminating the child to detach gives 1; a failed
//!   attach gives 1. Drive teardown off the master read result and nvmux's own
//!   detach state, then probe the socket afterwards to tell "detached" from
//!   "session ended".
//! * **Teardown differs by platform.** When the slave closes, the master read
//!   fails with `EIO` on Linux and returns `Ok(0)` on macOS. Both mean detached.
//! * **Let the child restore the terminal.** It emits its own
//!   `...\x1b[?1049l\x1b[23;0;0t\x1b[?25h` on exit. Pass those bytes through and
//!   only *then* leave raw mode; emitting a competing reset corrupts the display.
//! * `portable-pty` vendors its own `nix`, so never pass a `nix` type across
//!   that boundary — `MasterPty::get_termios()` returns *its* `Termios`, not ours.
//!
//! # `:q` ends the session, and that is intended
//!
//! In a `--remote-ui` session `:q` in the last window terminates the *server*,
//! not just the local view — the editor is the session. That is the documented
//! way to finish with a session and keep your work: save as usual, then quit as
//! usual. `Ctrl-t d` is the other exit, and leaves the session running.
//!
//! So this is a thing to explain rather than to guard. A `cnoreabbrev` guard
//! would also be a poor one: it covers bare `:q`, turns `:q!` into a silent
//! no-op (`bang (!) not supported yet`), and misses `:qa`, `ZZ`, `ZQ`, `:x`,
//! `:wq` and `<C-w>q` entirely.

use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::time::Duration;

pub use portable_pty::PtySize;
use portable_pty::{Child, CommandBuilder, MasterPty, PtyPair};

use crate::error::{NvmuxError, Result};
use crate::keys::{self, Action, Prefix, Step};
use crate::{rpc, term, winch};

/// Neovim aborts on the seventeenth attached UI rather than returning an error
/// (`src/nvim/ui.c`: `if (ui_count == MAX_UI_COUNT) { abort(); }`), which would
/// SIGABRT the whole server and destroy the session. Refuse well before that.
const MAX_UIS: usize = 8;

// Enforced at compile time, because getting this wrong does not fail a test —
// it SIGABRTs a user's session. Neovim's own ceiling is `MAX_UI_COUNT == 16`
// and it is enforced with `abort()`, not an error return.
const _: () = assert!(MAX_UIS < 16);

/// How the relay ended.
///
/// The attachment itself is handed back separately by [`relay`], so this stays
/// a plain value the caller can match on without moving anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// `Ctrl-t t` — show the picker. The child keeps running and the
    /// attachment comes back alongside this.
    ToPicker,
    /// `Ctrl-t d` — detach and exit, leaving the session running.
    Detached,
    /// `Ctrl-t c` — create a new session and attach to it.
    CreateNew,
    /// The child exited on its own.
    ChildExited,
}

/// A running `--remote-ui` client, and the PTY it is talking through.
pub struct Attachment {
    /// Which session this client is attached to, so a re-select can tell
    /// "the same session" from "a different one".
    pub session_id: String,
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty>,
    writer: Box<dyn Write + Send>,
    /// True once the child has been sent through a full relay at least once,
    /// so a resume knows it must force a repaint.
    resumed: bool,
}

impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attachment")
            .field("session_id", &self.session_id)
            .field("resumed", &self.resumed)
            .finish_non_exhaustive()
    }
}

impl Attachment {
    /// Terminate the client, leaving the session's server running.
    ///
    /// Verified: killing an attached `--remote-ui` client — even with SIGKILL —
    /// does not kill a `--headless --listen` server. The "channel closes, Nvim
    /// exits" rule is scoped to `--embed`.
    pub fn terminate(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start a `--remote-ui` client for a session.
///
/// `sock` must already be reachable from *this* machine — locally the session
/// socket, over SSH the local end of a forward.
pub fn spawn(session_id: &str, sock: &Path) -> Result<Attachment> {
    // Ping before spawning anything. Against a dead socket the client prints
    // "Remote ui failed to start: connection refused" and exits 1 — but through
    // an SSH forward that message is empty, after ~165 bytes of escape
    // sequences have already been sprayed at the terminal. Diagnosing it here
    // means the user gets a sentence instead of a garbled screen.
    let mut client = rpc::Client::connect(sock, rpc::PROBE_TIMEOUT)?;
    client.api_info()?;
    let uis = client.list_uis().unwrap_or(0);
    if uis >= MAX_UIS {
        return Err(NvmuxError::Session(crate::error::SessionError::NotKilled {
            name: session_id.to_string(),
            reason: "it already has too many attached UIs",
        }));
    }
    drop(client);

    // The size must be right *before* the child starts: PtySize::default() is
    // 24x80, and a client that starts at the wrong size draws a wrong-shaped
    // screen until something resizes it.
    let size = term::terminal_size();
    let pair: PtyPair = portable_pty::native_pty_system()
        .openpty(size)
        .map_err(|e| NvmuxError::Io(std::io::Error::other(e.to_string())))?;

    let mut cmd = CommandBuilder::new("nvim");
    cmd.arg("--server");
    cmd.arg(sock);
    cmd.arg("--remote-ui");
    // The child negotiates directly with the real terminal, so it needs to know
    // what that terminal is. Everything else about its environment is inherited.
    if let Ok(t) = std::env::var("TERM") {
        cmd.env("TERM", t);
    }
    if let Ok(c) = std::env::var("COLORTERM") {
        cmd.env("COLORTERM", c);
    }
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| NvmuxError::Io(std::io::Error::other(e.to_string())))?;

    // MANDATORY. While this process holds the slave open, the master never sees
    // EOF, so the relay would hang forever after the child exits instead of
    // noticing it had gone.
    drop(pair.slave);

    let writer = pair
        .master
        .take_writer()
        .map_err(|e| NvmuxError::Io(std::io::Error::other(e.to_string())))?;

    Ok(Attachment {
        session_id: session_id.to_string(),
        child,
        master: pair.master,
        writer,
        resumed: false,
    })
}

/// Relay bytes between the terminal and the child until something interrupts it.
///
/// # Shape
///
/// One thread, one `poll` over stdin, the PTY master and the SIGWINCH
/// self-pipe. That is deliberate rather than incidental:
///
/// * `try_clone_reader` dups the *same* open file description, so two readers
///   race and split the stream — in a proxy that means randomly deleting chunks
///   of the user's screen, scheduling-dependently. One loop, one reader.
/// * A thread parked in a blocking `read(0)` cannot be cancelled, so returning
///   to the picker would either hang or swallow the first keystroke typed at
///   it. A `poll` loop just stops polling.
pub fn relay(mut attachment: Attachment) -> Result<(Outcome, Option<Attachment>)> {
    let master_fd = attachment
        .master
        .as_raw_fd()
        .ok_or_else(|| NvmuxError::Io(std::io::Error::other("pty master has no fd")))?;

    let winch = winch::Winch::install()?;
    term::leave_alt_screen_and_clear();
    let mut raw = term::RawMode::enter()?;

    // On a resume the child has been blocked mid-write with nobody reading it,
    // and the picker has since drawn over the screen. Throw away whatever it
    // buffered — it describes a screen that no longer exists — and make it
    // repaint from scratch.
    if attachment.resumed {
        discard_pending(master_fd);
        force_repaint(attachment.master.as_ref());
    }
    attachment.resumed = true;

    let outcome = pump(&mut attachment, master_fd, &winch);

    match outcome {
        // Going back to the picker keeps the child, so there is no restore
        // sequence to wait for; ratatui is about to own the screen anyway.
        Ok(Outcome::ToPicker) => {
            raw.restore();
            Ok((Outcome::ToPicker, Some(attachment)))
        }
        Ok(other) => {
            // Let the child put the terminal back itself. It emits its own full
            // restore sequence on exit, and passing those bytes through is what
            // keeps the screen correct — emitting a competing reset of our own
            // corrupts it.
            if other != Outcome::ChildExited {
                let _ = attachment.child.kill();
            }
            drain_until_eof(master_fd);
            raw.restore();
            let _ = attachment.child.wait();
            Ok((other, None))
        }
        Err(e) => {
            let _ = attachment.child.kill();
            drain_until_eof(master_fd);
            raw.restore();
            Err(e)
        }
    }
}

fn pump(attachment: &mut Attachment, master_fd: RawFd, winch: &winch::Winch) -> Result<Outcome> {
    let stdin_fd = std::io::stdin().as_raw_fd();
    let mut prefix = Prefix::new();
    let mut buf = [0u8; 8192];

    loop {
        // A pending prefix needs its own deadline. Otherwise the wait is still
        // bounded, because a *stopped* child produces no poll activity at all —
        // see `check_child` — and an indefinite wait would never notice it.
        let timeout_ms = if prefix.is_armed() {
            keys::TIMEOUT.as_millis() as libc::c_int
        } else {
            IDLE_POLL_MS
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
        if n == 0 {
            // Nothing arrived in time, so a lone Ctrl-t was meant literally.
            for step in prefix.timeout() {
                if let Step::Forward(bytes) = step {
                    attachment.writer.write_all(&bytes)?;
                    attachment.writer.flush()?;
                }
            }
            if check_child(attachment) == ChildState::Gone {
                return Ok(Outcome::ChildExited);
            }
            continue;
        }

        // The child's output goes first, so the screen is up to date before any
        // keystroke is acted on.
        if ready(&fds[1]) {
            match read_fd(master_fd, &mut buf) {
                // EOF on macOS, EIO on Linux: both mean the slave closed.
                Ok(0) | Err(_) => return Ok(Outcome::ChildExited),
                Ok(len) => {
                    // Byte for byte, unparsed and unbuffered. This is the whole
                    // reason bracketed paste, the kitty keyboard protocol,
                    // truecolor, OSC 52 and DA1 round-trips work: the child is
                    // talking to the real terminal, and nvmux is not in the way.
                    let mut out = std::io::stdout().lock();
                    out.write_all(&buf[..len])?;
                    out.flush()?;
                }
            }
        }

        if ready(&fds[2]) {
            winch.drain();
            // Resizing the master is enough: the kernel signals the pty's
            // foreground group, the client calls try_resize, and the server
            // follows.
            let _ = attachment.master.resize(term::terminal_size());
        }

        if ready(&fds[0]) {
            match read_fd(stdin_fd, &mut buf) {
                Ok(0) => return Ok(Outcome::ChildExited),
                Err(e) => return Err(NvmuxError::Io(e)),
                Ok(len) => {
                    // Deliberately NOT logged. This is every keystroke the user
                    // types into their editor — passwords, tokens, private
                    // notes. Even at debug level it would write them to a file
                    // in /tmp that outlives the session.
                    for step in prefix.feed(&buf[..len]) {
                        match step {
                            Step::Forward(bytes) => {
                                attachment.writer.write_all(&bytes)?;
                                attachment.writer.flush()?;
                            }
                            Step::Act(Action::Picker) => return Ok(Outcome::ToPicker),
                            Step::Act(Action::Detach) => {
                                tracing::debug!("detach action");
                                return Ok(Outcome::Detached);
                            }
                            Step::Act(Action::Create) => return Ok(Outcome::CreateNew),
                        }
                    }
                }
            }
        }

        // Checked last so any final output above has already been relayed.
        if check_child(attachment) == ChildState::Gone {
            return Ok(Outcome::ChildExited);
        }
    }
}

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
/// So a stopped child is continued with `SIGCONT`, rather than the alternative
/// of tearing the attach down and falling back to the picker. Continuing puts
/// the user back exactly where they were, which is what pressing a key inside an
/// editor should do; falling back would silently discard the attach and lose the
/// screen they were looking at. It also cannot loop: the stop notification is
/// consumed when it is read, so a client that keeps stopping itself is continued
/// once per stop rather than spun on.
///
/// `portable_pty::Child::try_wait` cannot be used for this — it does not pass
/// `WUNTRACED`, so a stopped child simply reads as "still running". The peek
/// below uses `WNOWAIT` so that an *exit* status is left in place for
/// portable-pty's own reaper; only the stop notification is consumed, which does
/// not reap anything.
fn check_child(attachment: &mut Attachment) -> ChildState {
    use nix::sys::signal::{kill, Signal};
    use nix::sys::wait::{waitid, waitpid, Id, WaitPidFlag, WaitStatus};
    use nix::unistd::Pid;

    let Some(raw) = attachment.child.process_id() else {
        return ChildState::Running;
    };
    let pid = Pid::from_raw(raw as i32);

    // `waitid`, not `waitpid`. WNOWAIT is only valid for waitid(2): Linux's
    // wait4 rejects the flag outright with EINVAL, so the waitpid spelling of
    // this peek silently never reports anything and a stopped client would sit
    // frozen forever. (Measured — it returned EINVAL on every poll.)
    let peek =
        WaitPidFlag::WNOHANG | WaitPidFlag::WEXITED | WaitPidFlag::WSTOPPED | WaitPidFlag::WNOWAIT;

    match waitid(Id::Pid(pid), peek) {
        Ok(WaitStatus::Stopped(_, sig)) => {
            // Consume just the stop notification, which does not reap the
            // process, then wake it up. Without consuming it, the peek would
            // report the same stop on every poll and SIGCONT would be sent
            // repeatedly.
            let _ = waitpid(pid, Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED));
            tracing::info!(?sig, "the remote-ui client stopped; continuing it");
            let _ = kill(pid, Signal::SIGCONT);
            ChildState::Running
        }
        Ok(WaitStatus::Exited(..)) | Ok(WaitStatus::Signaled(..)) => ChildState::Gone,
        // Already reaped by someone else, which also means it is gone.
        Err(nix::errno::Errno::ECHILD) => ChildState::Gone,
        _ => ChildState::Running,
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

fn pollfd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

/// Readable, or gone. `POLLHUP` matters as much as `POLLIN`: on Linux the
/// master reports hangup rather than readability once the child exits, and
/// ignoring it would leave the loop spinning against a dead pty.
fn ready(p: &libc::pollfd) -> bool {
    p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
}

/// Throw away buffered child output without writing it anywhere.
fn discard_pending(fd: RawFd) {
    let mut buf = [0u8; 8192];
    loop {
        let mut p = [pollfd(fd)];
        let n = unsafe { libc::poll(p.as_mut_ptr(), 1, 0) };
        if n <= 0 || !ready(&p[0]) {
            return;
        }
        match read_fd(fd, &mut buf) {
            Ok(len) if len > 0 => continue,
            _ => return,
        }
    }
}

/// Relay whatever the child writes on its way out, then stop.
///
/// Bounded, because a child that ignores its termination would otherwise hold
/// the terminal in raw mode indefinitely.
fn drain_until_eof(fd: RawFd) {
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    let mut buf = [0u8; 8192];
    while std::time::Instant::now() < deadline {
        let mut p = [pollfd(fd)];
        let n = unsafe { libc::poll(p.as_mut_ptr(), 1, 50) };
        if n <= 0 {
            continue;
        }
        match read_fd(fd, &mut buf) {
            Ok(0) | Err(_) => return,
            Ok(len) => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&buf[..len]);
                let _ = out.flush();
            }
        }
    }
}

/// Make the client repaint the whole screen.
///
/// Nudging the size and putting it back is the reliable way to do this without
/// an RPC round trip or a synthetic keystroke: the client redraws from the
/// server's grid on every resize.
fn force_repaint(master: &dyn MasterPty) {
    let size = term::terminal_size();
    let nudged = PtySize {
        rows: size.rows.saturating_sub(1).max(1),
        ..size
    };
    let _ = master.resize(nudged);
    let _ = master.resize(size);
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
