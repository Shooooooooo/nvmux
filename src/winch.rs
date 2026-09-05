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
//! A `sigwait`-based version is ordering-sensitive — every signal in the waited
//! set must be blocked in every thread before any thread waits, and getting it
//! wrong produces zero deliveries and a silent hang. The self-pipe cannot.

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

        // Both ends non-blocking. The write end because a signal handler must
        // never block, and if the pipe has filled there is already an unread
        // resize notification. The read end because `drain` reads until the
        // pipe is empty, and a blocking read on an empty pipe would park the
        // whole relay until the *next* resize — see `drain`.
        set_nonblocking(&write)?;
        set_nonblocking(&read)?;

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
    ///
    /// Reads until the pipe reports empty (`EAGAIN`), which is why the read end
    /// is non-blocking: with a blocking read, a burst of exactly one buffer's
    /// worth of notifications would leave the loop parked in `read` with the
    /// pipe empty, and the relay would stop forwarding until another resize.
    pub fn drain(&self) {
        let mut buf = [0u8; 64];
        loop {
            match nix::unistd::read(&self.read, &mut buf) {
                Ok(n) if n > 0 => continue,
                _ => return,
            }
        }
    }
}

fn set_nonblocking(fd: &OwnedFd) -> Result<()> {
    let flags = nix::fcntl::OFlag::from_bits_truncate(
        nix::fcntl::fcntl(fd, nix::fcntl::F_GETFL).map_err(errno)?,
    );
    nix::fcntl::fcntl(
        fd,
        nix::fcntl::F_SETFL(flags | nix::fcntl::OFlag::O_NONBLOCK),
    )
    .map_err(errno)?;
    Ok(())
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

    /// A burst that fills `drain`'s buffer exactly must not park the relay.
    ///
    /// With a blocking read end, 64 pending bytes are read in one call, the
    /// loop continues, and the next read blocks on an empty pipe until the
    /// next resize. 65 covers the "one more than a buffer" case too.
    #[test]
    fn a_burst_that_fills_the_drain_buffer_does_not_block() {
        let _guard = serial();
        for count in [64, 65, 128] {
            let winch = Winch::install().expect("install");
            for _ in 0..count {
                unsafe {
                    libc::raise(libc::SIGWINCH);
                }
            }
            // Would hang here before the read end was made non-blocking.
            winch.drain();
            let mut fds = [libc::pollfd {
                fd: winch.fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, 50) };
            assert_eq!(n, 0, "drain left the pipe readable after {count} signals");
        }
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
