//! Detect resizes of our own terminal via a self-pipe.
//!
//! Only detection is needed: resizing the PTY master makes the kernel send
//! `SIGWINCH` to the client, which calls `try_resize`; nvmux never forwards the
//! signal. The handler writes one byte; the read end joins the relay's `poll`
//! set. Chosen over `sigwait`, which hangs silently unless the signal is
//! blocked in every thread before any waits.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

use crate::error::Result;

/// The write end for the handler. Atomic, not `OnceLock`: the handler must not
/// block and may fire before the pipe exists or after it closes.
static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_winch(_sig: libc::c_int) {
    let fd = WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        // Result ignored: a full pipe already has a resize pending. `write` is
        // async-signal-safe.
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
        let (read, write) = nix::unistd::pipe()?;

        // Non-blocking: the handler must never block, and `drain` reads until
        // `EAGAIN`, which a blocking read end would turn into a hang.
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

    /// Consume every pending notification. Resizes coalesce; the caller reads
    /// the current size afterwards.
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
    let flags = nix::fcntl::OFlag::from_bits_truncate(nix::fcntl::fcntl(fd, nix::fcntl::F_GETFL)?);
    nix::fcntl::fcntl(
        fd,
        nix::fcntl::F_SETFL(flags | nix::fcntl::OFlag::O_NONBLOCK),
    )?;
    Ok(())
}

impl Drop for Winch {
    fn drop(&mut self) {
        // Stop the handler using the fd before it closes. SIGWINCH's default
        // disposition is to ignore it.
        WRITE_FD.store(-1, Ordering::Relaxed);
        unsafe {
            libc::signal(libc::SIGWINCH, libc::SIG_DFL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests share the SIGWINCH disposition and `WRITE_FD`, so they run
    /// serially; otherwise one test's `Drop` can tear down another's handler.
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

        let mut fds = [libc::pollfd {
            fd: winch.fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, 500) };
        assert_eq!(n, 1, "SIGWINCH did not wake the poll set");
        assert!(fds[0].revents & libc::POLLIN != 0);

        winch.drain();

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

    /// With a blocking read end, exactly 64 pending bytes (one `drain` buffer)
    /// would leave the next read blocked on an empty pipe.
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
        unsafe {
            libc::raise(libc::SIGWINCH);
        }
        assert_eq!(WRITE_FD.load(Ordering::Relaxed), -1);
    }
}
