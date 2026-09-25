//! The Unix half of [`crate::sys`]: a pty through `portable-pty`, and `poll`.
//!
//! Everything here was [`crate::pty`]'s own before there was a second
//! platform, and does what it did: the same calls, in the same order, for the
//! same reasons, which the comments that came with it still give.

use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub(crate) use portable_pty::CommandBuilder as Command;
pub use portable_pty::PtySize;
use portable_pty::{Child, MasterPty};

use super::{ChildState, Order, ParkWake, Peek, Wake};
use crate::error::{NvmuxError, Result};
use crate::winch::Winch;

/// Read whatever is available. Only called after `poll` says the fd is ready.
/// Shared with [`crate::palette`], which reads the terminal's replies the same
/// way.
pub(crate) fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// A `poll` entry asking whether `fd` is readable. Shared with
/// [`crate::proc::Shell`], which polls a child's pipes the way the relay
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
/// A zero timeout: this is a question, not a wait. The callers have nothing
/// they may block for — see `Attachment::answer_device_attributes`.
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

/// A timeout as `poll`'s milliseconds, rounded up — rounded down, the last
/// fraction of a millisecond before a deadline would be spent in `poll` calls
/// that return at once.
fn poll_ms(timeout: Duration) -> libc::c_int {
    timeout
        .as_micros()
        .div_ceil(1000)
        .min(libc::c_int::MAX as u128) as libc::c_int
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
pub(crate) fn peek_child(pid: nix::unistd::Pid) -> Peek {
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

/// The client, on a pty.
///
/// **The child's exit code carries no information** here, and nothing asks
/// for it: a server killed while attached gives 0, terminating the child to
/// detach gives 1, a failed attach gives 1. Teardown is driven off the master
/// read result instead. And `portable-pty` vendors its own `nix`, so no `nix`
/// type crosses into it — `MasterPty::get_termios()` returns *its* `Termios`.
pub(crate) struct Pty {
    master: Box<dyn MasterPty>,
    child: Box<dyn Child + Send + Sync>,
}

impl Pty {
    /// Open a pty of `size` and start `cmd` on it, handing back the writer
    /// its input goes through — taken exactly once, since
    /// `MasterPty::take_writer()` errors on a second call.
    pub(crate) fn spawn(cmd: Command, size: PtySize) -> io::Result<(Pty, Box<dyn Write + Send>)> {
        let other = |e: anyhow::Error| io::Error::other(e.to_string());
        let pair = portable_pty::native_pty_system()
            .openpty(size)
            .map_err(other)?;
        let child = pair.slave.spawn_command(cmd).map_err(other)?;
        // MANDATORY: while this process holds the slave open the master never
        // sees EOF, so the relay would hang forever after the child exits.
        drop(pair.slave);
        let writer = pair.master.take_writer().map_err(other)?;
        Ok((
            Pty {
                master: pair.master,
                child,
            },
            writer,
        ))
    }

    /// The master's fd, which every wait below is on.
    pub(crate) fn fd(&self) -> Option<RawFd> {
        self.master.as_raw_fd()
    }

    pub(crate) fn resize(&self, size: PtySize) {
        let _ = self.master.resize(size);
    }

    pub(crate) fn size(&self) -> Option<PtySize> {
        self.master.get_size().ok()
    }

    pub(crate) fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// SIGHUP, and SIGCONT after it: see `Attachment::hang_up` for why both,
    /// and why through `libc` rather than `Child::kill`.
    pub(crate) fn hang_up(&self) {
        // Nothing to signal without one, and `reap` falls back on `wait`.
        let Some(pid) = self.pid() else {
            return;
        };
        // SAFETY: a pid this process spawned and has not waited for, so it names
        // that child or nothing.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGHUP);
            libc::kill(pid as libc::pid_t, libc::SIGCONT);
        }
    }

    pub(crate) fn kill(&self) {
        if let Some(pid) = self.pid() {
            // SAFETY: as above.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }

    /// Whether the child has gone: exited, or already reaped by someone else.
    pub(crate) fn try_wait(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }

    pub(crate) fn wait(&mut self) {
        let _ = self.child.wait();
    }

    /// See [`peek_child`].
    pub(crate) fn peek(&self) -> Peek {
        match self.pid() {
            Some(pid) => peek_child(nix::unistd::Pid::from_raw(pid as i32)),
            None => Peek::Running,
        }
    }

    /// Notice a child that has exited, and revive one that has stopped.
    ///
    /// # The stop case, and why it is continued rather than escalated
    ///
    /// `Ctrl-z` is forwarded to Neovim as an ordinary byte — nvmux does not
    /// special-case it. If the `--remote-ui` client responds by stopping
    /// *itself*, the user is left looking at a frozen screen: nvmux still owns
    /// the terminal, so there is no shell underneath to have been returned to,
    /// and the suspended client is not a state anyone can do anything with.
    ///
    /// So a stopped child is continued with `SIGCONT` rather than torn down.
    /// It cannot loop: the stop notification is consumed when read, so a
    /// client that keeps stopping itself is continued once per stop.
    ///
    /// `portable_pty::Child::try_wait` cannot be used here — it does not pass
    /// `WUNTRACED`, so a stopped child reads as "still running". The peek uses
    /// `WNOWAIT` so an *exit* status is left in place for portable-pty's own
    /// reaper.
    pub(crate) fn check(&mut self) -> ChildState {
        use nix::sys::signal::{kill, Signal};
        use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
        use nix::unistd::Pid;

        let Some(raw) = self.pid() else {
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

    /// Wait up to `timeout` for the client to have written something, or to
    /// have gone. An interrupted wait is `ErrorKind::Interrupted`.
    pub(crate) fn wait_output(&self, timeout: Duration) -> io::Result<bool> {
        let Some(fd) = self.fd() else {
            return Ok(false);
        };
        let mut p = [pollfd(fd)];
        let n = unsafe { libc::poll(p.as_mut_ptr(), 1, poll_ms(timeout)) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n > 0 && ready(&p[0]))
    }

    /// Read what the client has written. EOF on macOS, EIO on Linux: both
    /// mean the slave closed.
    pub(crate) fn read_output(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self.fd() {
            Some(fd) => read_fd(fd, buf),
            None => Ok(0),
        }
    }

    /// Whether the client's side has hung up, asked without waiting.
    pub(crate) fn hung_up(&self) -> bool {
        self.fd().is_some_and(|fd| {
            let mut p = [pollfd(fd)];
            let n = unsafe { libc::poll(p.as_mut_ptr(), 1, 0) };
            n > 0 && p[0].revents & (libc::POLLHUP | libc::POLLERR) != 0
        })
    }

    /// Write `bytes` to the client's input in one `write`, if there is room
    /// for a byte: `None` when there is not, and how much went otherwise. One
    /// `write`, never a `write_all` — see `Attachment::answer_device_attributes`.
    pub(crate) fn write_now(&self, bytes: &[u8]) -> Option<isize> {
        let fd = self.fd()?;
        if !writable(fd) {
            return None;
        }
        Some(unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) })
    }

    /// Throw away buffered client output without writing it anywhere, showing
    /// `saw` each piece on the way — which is how `Attachment::reap` finds a
    /// departing client's DA1 request in it.
    pub(crate) fn discard_pending(&self, mut saw: impl FnMut(&[u8])) {
        let Some(fd) = self.fd() else {
            return;
        };
        let mut buf = [0u8; 8192];
        loop {
            let mut p = [pollfd(fd)];
            let n = unsafe { libc::poll(p.as_mut_ptr(), 1, 0) };
            if n <= 0 || !ready(&p[0]) {
                return;
            }
            match read_fd(fd, &mut buf) {
                Ok(len) if len > 0 => saw(&buf[..len]),
                _ => return,
            }
        }
    }
}

/// The relay's one `poll`, over stdin, the pty master and the `SIGWINCH`
/// self-pipe — one thread and one `poll`, deliberately; see `pty::relay`.
pub(crate) struct Waiter {
    stdin: RawFd,
    master: RawFd,
    winch: Winch,
}

impl Waiter {
    pub(crate) fn new(pty: &Pty) -> Result<Self> {
        let master = pty
            .fd()
            .ok_or_else(|| NvmuxError::Io(io::Error::other("pty master has no fd")))?;
        let winch = Winch::install()?;
        Ok(Self {
            stdin: io::stdin().as_raw_fd(),
            master,
            winch,
        })
    }

    /// Wait up to `timeout_ms`. `None` is a `poll` a signal interrupted: go
    /// round again. A resize found here is drained here; the caller acts on it.
    pub(crate) fn wait(&mut self, timeout_ms: u64) -> io::Result<Option<Wake>> {
        let mut fds = [
            pollfd(self.stdin),
            pollfd(self.master),
            pollfd(self.winch.fd()),
        ];
        let timeout = timeout_ms.min(libc::c_int::MAX as u64) as libc::c_int;
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(err);
        }
        let resized = n > 0 && ready(&fds[2]);
        if resized {
            self.winch.drain();
        }
        Ok(Some(Wake {
            stdin: n > 0 && ready(&fds[0]),
            child: n > 0 && ready(&fds[1]),
            resized,
            idle: n == 0,
        }))
    }

    pub(crate) fn read_child(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_fd(self.master, buf)
    }

    pub(crate) fn read_stdin(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_fd(self.stdin, buf)
    }
}

/// The owner's end of a parked client's stop channel: a byte is an order to
/// retire the client, and the end of the stream is the client wanted back.
pub(crate) struct StopTx(UnixStream);

/// The parked thread's end.
pub(crate) struct StopRx(UnixStream);

pub(crate) fn stop_pair() -> io::Result<(StopTx, StopRx)> {
    let (tx, rx) = UnixStream::pair()?;
    Ok((StopTx(tx), StopRx(rx)))
}

impl StopTx {
    pub(crate) fn retire(&mut self) {
        let _ = (&self.0).write_all(b"R");
    }
}

impl StopRx {
    /// What the owner said, once [`park_wait`] has said it said something.
    pub(crate) fn order(&self) -> Order {
        let mut order = [0u8; 1];
        match read_fd(self.0.as_raw_fd(), &mut order) {
            Ok(1) => Order::Retire,
            _ => Order::Back,
        }
    }
}

/// A parked client's one wait: its owner, and its output. A signal meant for
/// the relay can land on this thread, which is `ErrorKind::Interrupted`.
pub(crate) fn park_wait(stop: &StopRx, pty: &Pty, timeout_ms: u64) -> io::Result<ParkWake> {
    let mut fds = [pollfd(stop.0.as_raw_fd()), pollfd(pty.fd().unwrap_or(-1))];
    let timeout = timeout_ms.min(libc::c_int::MAX as u64) as libc::c_int;
    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ParkWake {
        stop: n > 0 && ready(&fds[0]),
        output: n > 0 && ready(&fds[1]),
        idle: n == 0,
    })
}

/// The terminal's input, for a question asked of it with a deadline on the
/// answer (see [`crate::palette`]). Only ever read from raw mode.
pub(crate) struct TermInput {
    fd: RawFd,
}

impl TermInput {
    pub(crate) fn new() -> Self {
        Self {
            fd: io::stdin().as_raw_fd(),
        }
    }

    /// Whether a read would return at once — a key already waiting.
    pub(crate) fn readable_now(&mut self) -> bool {
        let mut p = [pollfd(self.fd)];
        let n = unsafe { libc::poll(p.as_mut_ptr(), 1, 0) };
        n > 0 && ready(&p[0])
    }

    /// Read what arrives within `timeout`: `Ok(None)` for nothing in time,
    /// `ErrorKind::Interrupted` for a wait a signal cut short.
    pub(crate) fn read_within(
        &mut self,
        timeout: Duration,
        buf: &mut [u8],
    ) -> io::Result<Option<usize>> {
        let mut p = [pollfd(self.fd)];
        let wait = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        let n = unsafe { libc::poll(p.as_mut_ptr(), 1, wait) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Ok(None);
        }
        read_fd(self.fd, buf).map(Some)
    }
}
