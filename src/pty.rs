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
use std::path::Path;
use std::time::Duration;

pub use portable_pty::PtySize;
use portable_pty::{Child, CommandBuilder, MasterPty, PtyPair};

use crate::error::{NvmuxError, Result};
use crate::keys::{Action, Prefix, Step};
use crate::{rpc, term, winch};

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
    /// `Ctrl-t t` — show the picker. The child keeps running.
    ToPicker,
    /// `Ctrl-t d` — detach and exit, leaving the session running.
    Detached,
    /// `Ctrl-t c` — prompt for a name and create a new session. The child keeps
    /// running, so a cancelled prompt puts the user straight back.
    CreateNew,
    /// `Ctrl-t ?` — show the key bindings. The child keeps running.
    ShowHelp,
    /// `Ctrl-t <number>` — attach to the session with that number. The child
    /// keeps running, so a number that names nothing puts the user back.
    Switch(u32),
    /// The child exited on its own.
    ChildExited,
}

/// A running `--remote-ui` client, and the PTY it is talking through.
pub struct Attachment {
    /// Which session this client is attached to.
    pub session_id: String,
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty>,
    writer: Box<dyn Write + Send>,
    /// True once the child has been through a full relay, so a resume knows it
    /// must force a repaint.
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
    /// Terminate the client, leaving the session's server running. Verified:
    /// killing a `--remote-ui` client, even with SIGKILL, does not kill a
    /// `--headless --listen` server — the "channel closes, Nvim exits" rule is
    /// scoped to `--embed`.
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
    // Ping before spawning anything. Through an SSH forward the client's own
    // failure message is empty, after ~165 bytes of escape sequences have
    // already been sprayed at the terminal.
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

    // Right *before* the child starts: `PtySize::default()` is 24x80. Note
    // `crossterm::size()` is (cols, rows) while `PtySize` is { rows, cols } —
    // passing them positionally transposes the screen.
    let size = term::terminal_size();
    let pair: PtyPair = portable_pty::native_pty_system()
        .openpty(size)
        .map_err(|e| NvmuxError::Io(std::io::Error::other(e.to_string())))?;

    let mut cmd = CommandBuilder::new("nvim");
    cmd.arg("--server");
    cmd.arg(sock);
    cmd.arg("--remote-ui");
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

    // MANDATORY: while this process holds the slave open the master never sees
    // EOF, so the relay would hang forever after the child exits.
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
    // Leave the alternate screen into a black primary, so the session starts
    // from the dark screen the outgoing screen dissolved to (or, from the
    // picker, so the client spawn never flashes the old primary).
    crate::fade::enter_session_black();
    let mut raw = term::RawMode::enter()?;

    // What the child buffered while blocked mid-write describes a screen the
    // picker has since drawn over.
    if attachment.resumed {
        discard_pending(master_fd);
        force_repaint(attachment.master.as_ref());
    }
    attachment.resumed = true;

    let outcome = pump(&mut attachment, master_fd, &winch, highest_session_num);

    match outcome {
        Ok(
            held
            @ (Outcome::ToPicker | Outcome::CreateNew | Outcome::ShowHelp | Outcome::Switch(_)),
        ) => {
            // Dissolve the session to black before restoring, so the picker or
            // the next session takes over from a dark screen rather than a cut.
            // A failed fade must not skip the restore below, so its error is
            // dropped rather than propagated.
            let _ = crate::fade::fade_out_raw();
            raw.restore();
            Ok((held, Some(attachment)))
        }
        Ok(Outcome::ChildExited) => {
            // The child is gone and has emitted its own restore, so there is no
            // live frame to dissolve — black the screen at once (any dissolve
            // would race the child's teardown), then let the picker fade up.
            drain_until_eof(master_fd);
            crate::fade::black_now();
            raw.restore();
            let _ = attachment.child.wait();
            Ok((Outcome::ChildExited, None))
        }
        Ok(other) => {
            // `Detached`: leave the session running and let the child put the
            // terminal back itself — a competing reset (a fade included) would
            // corrupt its own restore sequence.
            let _ = attachment.child.kill();
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

fn pump(
    attachment: &mut Attachment,
    master_fd: RawFd,
    winch: &winch::Winch,
    highest_session_num: u32,
) -> Result<Outcome> {
    let stdin_fd = std::io::stdin().as_raw_fd();
    let mut prefix = Prefix::with_prefix(highest_session_num, crate::settings::get().keys.prefix);
    let mut buf = [0u8; 8192];

    loop {
        // A pending prefix needs its own deadline: a *stopped* child produces
        // no poll activity at all, so an indefinite wait would never notice it.
        let timeout_ms = if prefix.is_armed() {
            crate::settings::get().keys.timeout_ms as libc::c_int
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
            // `timeout` resolves a lone prefix into a literal byte, but also a
            // half-typed session number into a switch — so the actions it
            // produces must be acted on, not just the bytes.
            for step in prefix.timeout() {
                match step {
                    Step::Forward(bytes) => {
                        attachment.writer.write_all(&bytes)?;
                        attachment.writer.flush()?;
                    }
                    Step::Act(Action::Switch(num)) => return Ok(Outcome::Switch(num)),
                    // `timeout` never produces the others; a `_` here would let
                    // a future one be dropped as silently as this one was.
                    Step::Act(Action::Picker) => return Ok(Outcome::ToPicker),
                    Step::Act(Action::Detach) => return Ok(Outcome::Detached),
                    Step::Act(Action::Create) => return Ok(Outcome::CreateNew),
                    Step::Act(Action::Help) => return Ok(Outcome::ShowHelp),
                }
            }
            if check_child(attachment) == ChildState::Gone {
                return Ok(Outcome::ChildExited);
            }
            continue;
        }

        // Output first, so the screen is current before a keystroke is acted on.
        if ready(&fds[1]) {
            match read_fd(master_fd, &mut buf) {
                // EOF on macOS, EIO on Linux: both mean the slave closed.
                Ok(0) | Err(_) => return Ok(Outcome::ChildExited),
                Ok(len) => {
                    // Byte for byte, unparsed and unbuffered; see the module
                    // docs.
                    let mut out = std::io::stdout().lock();
                    out.write_all(&buf[..len])?;
                    out.flush()?;
                }
            }
        }

        if ready(&fds[2]) {
            winch.drain();
            // Enough on its own: the kernel signals the pty's foreground group
            // and the client calls try_resize.
            let _ = attachment.master.resize(term::terminal_size());
        }

        if ready(&fds[0]) {
            match read_fd(stdin_fd, &mut buf) {
                Ok(0) => return Ok(Outcome::ChildExited),
                Err(e) => return Err(NvmuxError::Io(e)),
                Ok(len) => {
                    // Deliberately NOT logged: every keystroke the user types,
                    // into a /tmp file that outlives the session.
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
                            Step::Act(Action::Help) => return Ok(Outcome::ShowHelp),
                            Step::Act(Action::Switch(n)) => return Ok(Outcome::Switch(n)),
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
/// So a stopped child is continued with `SIGCONT` rather than torn down. It
/// cannot loop: the stop notification is consumed when read, so a client that
/// keeps stopping itself is continued once per stop.
///
/// `portable_pty::Child::try_wait` cannot be used here — it does not pass
/// `WUNTRACED`, so a stopped child reads as "still running". The peek below uses
/// `WNOWAIT` so an *exit* status is left in place for portable-pty's own reaper.
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
