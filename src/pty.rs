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
use std::time::{Duration, Instant};

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
    /// `<prefix> t` — show the picker. The child keeps running.
    ToPicker,
    /// `<prefix> d` — detach and exit, leaving the session running.
    Detached,
    /// `<prefix> c` — prompt for a name and create a new session. The child keeps
    /// running, so a cancelled prompt puts the user straight back.
    CreateNew,
    /// `<prefix> ?` — show the key bindings. The child keeps running.
    ShowHelp,
    /// `<prefix> <number>` — attach to the session with that number. The child
    /// keeps running, so a number that names nothing puts the user back.
    Switch(u32),
    /// The child exited on its own.
    ChildExited,
    /// Our own stdin reached EOF: the terminal went away or the input was
    /// redirected. The child is still running, so this is handled like a
    /// detach — the session is left alone — rather than like a child exit,
    /// which would wait on a child that has no reason to leave.
    StdinClosed,
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

/// How long a signalled client gets to exit before it is killed outright.
const REAP_TIMEOUT: Duration = Duration::from_secs(2);

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

    /// Ask the client to exit. `portable-pty` sends SIGHUP, which is what the
    /// client would get if the terminal itself went away.
    fn signal(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
        }
    }

    /// Wait for the client to be gone, escalating to SIGKILL after
    /// [`REAP_TIMEOUT`] so a client that ignores SIGHUP cannot hold nvmux —
    /// and the user's terminal — hostage.
    fn reap(&mut self) {
        if self.reaped {
            return;
        }
        self.reaped = true;
        let deadline = Instant::now() + REAP_TIMEOUT;
        while Instant::now() < deadline {
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
}

/// The client must be dead before the PTY writer is dropped: `portable-pty`'s
/// writer writes a newline and `VEOF` to the master on drop, and a client that
/// is still attached would deliver both to the editor as keystrokes — an Enter
/// in insert mode is a new line in the user's buffer. Fields drop after this
/// runs, so the writer only ever goes out on a pty nobody is reading.
impl Drop for Attachment {
    fn drop(&mut self) {
        self.signal();
        self.reap();
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
    // Not `unwrap_or(0)`: a server too busy to answer is exactly the one whose
    // UI count is unknown, and guessing zero is how the seventeenth attach
    // happens.
    let uis = client.list_uis()?;
    if uis >= MAX_UIS {
        return Err(NvmuxError::Session(
            crate::error::SessionError::TooManyUis {
                id: session_id.to_string(),
                attached: uis,
            },
        ));
    }
    drop(client);

    // Right *before* the child starts: `PtySize::default()` is 24x80. Note
    // `crossterm::size()` is (cols, rows) while `PtySize` is { rows, cols } —
    // passing them positionally transposes the screen.
    let size = term::terminal_size();
    let pair: PtyPair = portable_pty::native_pty_system()
        .openpty(size)
        .map_err(|e| NvmuxError::Io(std::io::Error::other(e.to_string())))?;

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
            attachment.reap();
            Ok((Outcome::ChildExited, None))
        }
        Ok(other @ (Outcome::Detached | Outcome::StdinClosed)) => {
            // Leave the session running and let the child put the terminal
            // back itself — a competing reset (a fade included) while it is
            // still writing would corrupt its own restore sequence. Once it is
            // gone, a final reset is harmless and covers a client that died
            // before it got that far.
            attachment.signal();
            drain_until_eof(master_fd);
            raw.restore();
            attachment.reap();
            term::reset_screen();
            Ok((other, None))
        }
        Err(e) => {
            // The error is about to be printed to a shell: put the cursor and
            // the colours back first, or `enter_session_black` above leaves it
            // on a black screen with no cursor. `attachment` is dropped on the
            // way out, which retires the client.
            attachment.signal();
            drain_until_eof(master_fd);
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
) -> Result<Outcome> {
    let stdin_fd = std::io::stdin().as_raw_fd();
    let keys = crate::settings::get().keys;
    let mut prefix = Prefix::with_prefix(highest_session_num, keys.prefix);
    let prefix_timeout = Duration::from_millis(keys.timeout_ms);
    // When a pending prefix or half-typed number must be settled. An instant
    // rather than a per-poll timeout on purpose: a child that keeps producing
    // output keeps `poll` returning early, and a timeout that restarted on
    // every wake-up would never fire while a spinner is running. It is set
    // when the machine arms and cleared when it settles.
    let mut deadline: Option<Instant> = None;
    let mut buf = [0u8; 8192];

    loop {
        // A stopped child produces no poll activity at all, so even an idle
        // wait is bounded; an armed prefix shortens it to its own deadline.
        let timeout_ms = match deadline {
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now()).as_millis();
                left.min(IDLE_POLL_MS as u128) as libc::c_int
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
        if n > 0 && ready(&fds[1]) {
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

        if n > 0 && ready(&fds[2]) {
            winch.drain();
            // Enough on its own: the kernel signals the pty's foreground group
            // and the client calls try_resize.
            let _ = attachment.master.resize(term::terminal_size());
        }

        // Settle a pending sequence before reading anything new, so a key that
        // arrives just after the deadline is not swallowed as a command.
        if deadline.is_some_and(|d| Instant::now() >= d) {
            deadline = None;
            // `timeout` resolves a lone prefix into a literal byte, but also a
            // half-typed session number into a switch — so the actions it
            // produces must be acted on, not just the bytes.
            for step in prefix.timeout() {
                if let Some(outcome) = act(&mut attachment.writer, step)? {
                    return Ok(outcome);
                }
            }
        }

        if n > 0 && ready(&fds[0]) {
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
                    deadline = prefix.is_armed().then(|| Instant::now() + prefix_timeout);
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
        Step::Act(Action::Switch(num)) => Some(Outcome::Switch(num)),
    })
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
            (Action::Switch(7), Outcome::Switch(7)),
        ];
        for (action, want) in cases {
            let got = act(&mut out, Step::Act(action)).expect("no io");
            assert_eq!(got, Some(want), "{action:?}");
        }
        assert!(out.is_empty(), "a command writes nothing to the child");
    }

    /// The peek sees a stop, leaves it in place, and sees an exit without
    /// reaping it. Runs against a real child because `waitid` is what differs
    /// between platforms: nix does not bind it on macOS, so it goes through
    /// `libc`, and the flag and `si_code` handling has to hold on both.
    #[test]
    fn peek_reports_stopped_and_gone_without_consuming_either() {
        use nix::sys::signal::{kill, Signal};
        use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
        use nix::unistd::Pid;

        /// Kill and reap the child however the test ends.
        struct Guard(std::process::Child);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        // Poll until the child reaches `want`: signals are delivered
        // asynchronously, so the state is not visible on the first look.
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
}
