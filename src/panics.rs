//! What a panic does to the terminal.
//!
//! A panic nvmux does not catch ends it, and the terminal has to be handed
//! back first — raw mode off, the alternate screen left — or the message goes
//! into a screen nobody can read. [`install`] puts in the one hook that does
//! that.
//!
//! A panic nvmux does catch is another matter. The parsers that follow a
//! session's bytes for the shadow and the ledger are somebody else's code, fed
//! whatever an editor writes, and a panic in one only retires what it was
//! keeping (see [`crate::shadow`] and [`crate::ledger`]) while the relay goes
//! on. The hook runs for that panic too, before the catch, and there it must do
//! nothing: handing the terminal back under a live session takes the session
//! off the alternate screen and writes the message over it. [`caught`] marks
//! the panics it catches, and the hook lets them be.
//!
//! ratatui's own `try_init` puts in a hook that hands the terminal back on any
//! panic, one more each time a screen opens, and never takes it out: the
//! screens open the terminal without it (see `ui::Screen`).

use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Once;

thread_local! {
    /// Whether a panic on this thread is one [`caught`] is catching.
    static CATCHING: Cell<bool> = const { Cell::new(false) };
}

/// Put in the hook every panic in nvmux goes through, once: it hands the
/// terminal back and then says what happened, as the hook before it does —
/// but for a panic [`caught`] is catching, which it only logs. Called from
/// `main` before any screen is drawn, and by every screen as it opens, for
/// callers that never went through `main`.
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
/// module docs). Unwind safety is taken on trust, as the parsers' callers take
/// it: what `f` was part way through is thrown away with it.
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

    /// The child half of the test below: set on the test binary it runs again.
    const CHILD: &str = "NVMUX_TEST_PANICS_CHILD";

    /// What the child writes once the session's screen is up, and once it is
    /// done.
    const UP: &str = "<<up>>";
    const DONE: &str = "<<done>>";

    fn catching() -> bool {
        CATCHING.with(Cell::get)
    }

    /// A panic is marked as caught only while [`caught`] runs what panics,
    /// and the mark is taken off again however that ends — nested, too.
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

    /// **The regression test for the screen a parser bug took away.** A
    /// session up — raw mode, the alternate screen — after the picker had
    /// come and gone three times, and a panic caught under it: the terminal
    /// is left as it was, and nothing is written over it. Each screen used to
    /// put in a hook of ratatui's that ran for the caught panic too, leaving
    /// the alternate screen once for every time a screen had opened, and the
    /// message was printed over the session.
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
                // A terminal's answer to where its cursor is, should a screen
                // ask.
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

    /// The child: see the test above. Nothing without [`CHILD`].
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
