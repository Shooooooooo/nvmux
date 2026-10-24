//! What a panic does to the terminal.
//!
//! An uncaught panic ends nvmux, so the terminal must be restored first (raw
//! mode off, alternate screen left) or the message is unreadable. [`install`]
//! adds the one hook that does this.
//!
//! The third-party parsers following a session's bytes for the shadow and the
//! ledger may panic on editor output. That panic is caught and only retires
//! what they kept (see [`crate::shadow`] and [`crate::ledger`]) while the
//! relay continues. The hook still runs for it, before the catch, and must do
//! nothing: restoring the terminal under a live session would drop it off the
//! alternate screen and print over it. [`caught`] marks the panics it catches
//! for the hook to skip.
//!
//! Screens open the terminal without ratatui's `try_init` (see `ui::Screen`),
//! which would add a restoring hook per screen and never remove it.

use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Once;

thread_local! {
    /// Whether a panic on this thread is one [`caught`] is catching.
    static CATCHING: Cell<bool> = const { Cell::new(false) };
}

/// Install the panic hook, once: it restores the terminal, then defers to the
/// previous hook, except for panics [`caught`] is catching, which it only
/// logs. Called from `main` and by every screen as it opens, for callers that
/// bypass `main`.
pub fn install() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let next = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if CATCHING.with(Cell::get) {
                tracing::warn!(%info, "a panic, caught");
                return;
            }
            ratatui::restore();
            next(info);
        }));
    });
}

/// Run `f`, catching a panic in it, which the hook leaves alone (see the
/// module docs). Unwind safety is assumed: whatever `f` was doing is
/// discarded.
pub fn caught<T>(f: impl FnOnce() -> T) -> std::thread::Result<T> {
    let was = CATCHING.with(|c| c.replace(true));
    let outcome = panic::catch_unwind(AssertUnwindSafe(f));
    CATCHING.with(|c| c.set(was));
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Set on the test binary re-run as the child of the test below.
    const CHILD: &str = "NVMUX_TEST_PANICS_CHILD";

    /// Written by the child once the session's screen is up, and when done.
    const UP: &str = "<<up>>";
    const DONE: &str = "<<done>>";

    fn catching() -> bool {
        CATCHING.with(Cell::get)
    }

    /// The mark is cleared however `caught` ends, nested calls included.
    #[test]
    fn only_what_is_caught_is_marked_caught() {
        assert!(!catching());
        assert_eq!(caught(catching).ok(), Some(true));
        assert!(!catching());
        let (outer, thrown) = crate::test_support::quietly(|| {
            let outer = caught(|| {
                assert!(caught(|| panic!("a parser bug")).is_err());
                catching()
            });
            (outer, caught(|| panic!("a parser bug")))
        });
        assert_eq!(outer.ok(), Some(true), "still marked after an inner catch");
        assert!(thrown.is_err());
        assert!(!catching());
    }

    /// Regression: with a session up (raw mode, alternate screen) after three
    /// screens opened and closed, a caught panic must leave the terminal
    /// alone. Each screen used to add a ratatui hook that ran for caught
    /// panics too, leaving the alternate screen and printing over the session.
    #[test]
    fn a_caught_panic_leaves_the_session_it_happens_under_alone() {
        let (pty, pts) = pty_process::blocking::open().expect("a pty");
        pty.resize(pty_process::Size::new(24, 80)).expect("size it");
        let cmd = pty_process::blocking::Command::new(std::env::current_exe().expect("the tests"))
            .args([
                "--exact",
                "panics::tests::a_session_with_a_panic_caught_under_it",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1");
        let mut child = cmd.spawn(pts).expect("run the child");
        let mut reader = std::fs::File::from(
            pty.as_fd()
                .try_clone_to_owned()
                .expect("clone the pty master"),
        );
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
        });
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut writer = &pty;
        while !String::from_utf8_lossy(&out).contains(DONE) && Instant::now() < deadline {
            if let Ok(bytes) = rx.recv_timeout(Duration::from_millis(50)) {
                // Answer a cursor position query, should a screen ask.
                if bytes.windows(4).any(|w| w == b"\x1b[6n") {
                    let _ = writer.write_all(b"\x1b[1;1R");
                }
                out.extend_from_slice(&bytes);
            }
        }
        let _ = child.wait();
        let out = String::from_utf8_lossy(&out);
        let after = out
            .split(UP)
            .nth(1)
            .unwrap_or_else(|| panic!("the session never came up: {out:?}"));
        assert!(after.contains(DONE), "the child never finished: {out:?}");
        assert!(after.contains("raw: on"), "raw mode went: {after:?}");
        assert!(
            !after.contains("\x1b[?1049l"),
            "the session's screen was left: {after:?}"
        );
        assert!(
            !after.contains("panicked"),
            "the panic was written over the session: {after:?}"
        );
    }

    /// The child of the test above; does nothing without [`CHILD`].
    #[test]
    #[ignore = "the child half of a test; run by it, not by hand"]
    fn a_session_with_a_panic_caught_under_it() {
        if std::env::var_os(CHILD).is_none() {
            return;
        }
        install();
        for _ in 0..3 {
            let screen = crate::ui::Screen::open().expect("a screen");
            screen.close().expect("the terminal back");
        }
        let raw = crate::term::RawMode::enter().expect("raw mode");
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b[?1049h{UP}\r\n");
        let _ = stdout.flush();
        assert!(caught(|| panic!("a parser bug")).is_err());
        let termios = nix::sys::termios::tcgetattr(std::io::stdin().as_fd()).expect("termios");
        let on = !termios
            .local_flags
            .contains(nix::sys::termios::LocalFlags::ICANON);
        let _ = write!(
            stdout,
            "raw: {}\r\n{DONE}\r\n",
            if on { "on" } else { "off" }
        );
        let _ = stdout.flush();
        drop(raw);
    }
}
