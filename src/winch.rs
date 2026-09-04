//! Noticing that our own terminal was resized.
//!
//! # Why nvmux only has to notice
//!
//! Resizing the PTY master is sufficient end to end: the kernel then delivers
//! `SIGWINCH` to the pty's foreground process group, the `--remote-ui` client
//! calls `try_resize`, and the server follows. nvmux never forwards the signal
//! to the child itself.
//!
//! # The self-pipe
//!
//! The handler does one async-signal-safe `write(2)` of a single byte. The read
//! end joins the proxy's `poll` set, so a resize is just another readable fd
//! alongside stdin and the pty master — no extra thread, no shared state, and
//! nothing for the relay loop to poll for.
//!
//! A `sigwait`-based version also works but is ordering-sensitive: every signal
//! in the waited set must be blocked in *every* thread before any thread waits,
//! and getting that wrong produces zero deliveries and a silent hang. The
//! self-pipe has no such hazard.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

use crate::error::Result;

/// The write end, as a raw fd the signal handler can reach.
///
/// An atomic rather than a `OnceLock` because the handler must never block and
/// must tolerate firing before or after the pipe exists.
static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_winch(_sig: libc::c_int) {
    let fd = WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        // One byte, ignoring the result. If the pipe is full a resize is
        // already pending, which is all this byte was going to say. `write` is
        // async-signal-safe; nothing else here allocates or locks.
        unsafe {
            libc::write(fd, c"w".as_ptr().cast(), 1);
        }
    }
}

/// A `SIGWINCH` subscription. Removed when dropped.
pub struct Winch {
    read: OwnedFd,
    _write: OwnedFd,
}

impl Winch {
    pub fn install() -> Result<Self> {
        let (read, write) = nix::unistd::pipe().map_err(errno)?;

        // Non-blocking write end: a signal handler must never block, and if the
        // pipe has filled there is already an unread resize notification.
        let flags = nix::fcntl::OFlag::from_bits_truncate(
            nix::fcntl::fcntl(&write, nix::fcntl::F_GETFL).map_err(errno)?,
        );
        nix::fcntl::fcntl(
            &write,
            nix::fcntl::F_SETFL(flags | nix::fcntl::OFlag::O_NONBLOCK),
        )
        .map_err(errno)?;

        WRITE_FD.store(write.as_raw_fd(), Ordering::Relaxed);
        // SAFETY: the handler performs a single `write(2)` and nothing else.
        unsafe {
            libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t);
        }

        Ok(Self {
            read,
            _write: write,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.read.as_raw_fd()
    }

    /// Consume every pending notification.
    ///
    /// Several resizes can coalesce into one wake-up, which is fine: the caller
    /// reads the terminal's *current* size afterwards, so intermediate sizes
    /// were never interesting.
    pub fn drain(&self) {
        let mut buf = [0u8; 64];
        loop {
            match nix::unistd::read(&self.read, &mut buf) {
                Ok(n) if n == buf.len() => continue,
                _ => return,
            }
        }
    }
}

impl Drop for Winch {
    fn drop(&mut self) {
        // Stop the handler touching a fd that is about to close. Restoring the
        // default disposition is right: SIGWINCH's default is to be ignored.
        WRITE_FD.store(-1, Ordering::Relaxed);
        unsafe {
            libc::signal(libc::SIGWINCH, libc::SIG_DFL);
        }
    }
}

fn errno(e: nix::errno::Errno) -> crate::error::NvmuxError {
    crate::error::NvmuxError::Io(std::io::Error::from(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests share process-global state — one SIGWINCH disposition and
    /// one `WRITE_FD` — so running them concurrently lets one test's `Drop`
    /// tear down another's subscription mid-assertion. Serialising them is the
    /// fix; the alternative is a test that fails once every few hundred runs
    /// and gets dismissed as noise.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn a_resize_signal_becomes_a_readable_fd() {
        let _guard = serial();
        let winch = Winch::install().expect("install");

        unsafe {
            libc::raise(libc::SIGWINCH);
        }

        // The fd should now be readable without blocking.
        let mut fds = [libc::pollfd {
            fd: winch.fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, 500) };
        assert_eq!(n, 1, "SIGWINCH did not wake the poll set");
        assert!(fds[0].revents & libc::POLLIN != 0);

        winch.drain();

        // ...and after draining it is quiet again.
        let mut fds = [libc::pollfd {
            fd: winch.fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, 50) };
        assert_eq!(n, 0, "drain left the pipe readable");
    }

    #[test]
    fn several_resizes_coalesce_into_one_drain() {
        let _guard = serial();
        let winch = Winch::install().expect("install");
        for _ in 0..20 {
            unsafe {
                libc::raise(libc::SIGWINCH);
            }
        }
        winch.drain();
        let mut fds = [libc::pollfd {
            fd: winch.fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, 50) };
        assert_eq!(n, 0, "drain should consume every pending notification");
    }

    #[test]
    fn dropping_the_subscription_stops_the_handler() {
        let _guard = serial();
        let winch = Winch::install().expect("install");
        drop(winch);
        // The handler must not write to the now-closed fd.
        unsafe {
            libc::raise(libc::SIGWINCH);
        }
        assert_eq!(WRITE_FD.load(Ordering::Relaxed), -1);
    }
}
