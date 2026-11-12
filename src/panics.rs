//! What a panic does to the terminal.
//!
//! A panic on the main thread ends nvmux, so [`install`]'s hook first gives
//! the terminal back as a signal does (see [`crate::term`]): the shell's
//! termios, without which the message staircases, then a reset of the modes
//! the session turned on, as its client is retired without relaying its exit
//! sequence. Nothing else would in time: crossterm knows nothing of a relay's
//! raw mode, and the relay's guards restore only as the panic unwinds, after
//! the message.
//!
//! A panic on another thread (a parked client's, a probe's) ends only that
//! thread, under a screen or relay that goes on, so the hook leaves the
//! terminal alone and only logs it: printed, its message would be written
//! over that screen. One the main thread takes on from a thread it joined is
//! re-raised there with [`resume`].
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

use std::any::Any;
use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Once;

thread_local! {
    /// Whether a panic on this thread is one [`caught`] is catching.
    static CATCHING: Cell<bool> = const { Cell::new(false) };
    /// Whether this thread installed the hook (see [`install`]).
    static MAIN: Cell<bool> = const { Cell::new(false) };
}

/// Install the panic hook, once, taking the calling thread for the main one.
/// `main` calls it first; every screen calls it as it opens, for callers that
/// bypass `main`, such as tests, whose thread then stands in for it.
///
/// The hook gives the terminal back for a panic on that thread, then defers
/// to the previous hook, which prints the message. A panic on another thread,
/// or one [`caught`] is catching, is only logged: in a test binary, once a
/// test opens a screen in-process, other tests fail without a message.
pub fn install() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        MAIN.with(|main| main.set(true));
        let next = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if fatal() {
                give_back();
                next(info);
            } else if CATCHING.with(Cell::get) {
                tracing::warn!(%info, "a panic, caught");
            } else {
                tracing::error!(%info, "a panic on another thread");
            }
        }));
    });
}

/// Whether a panic on this thread ends nvmux: on the main thread, uncaught.
fn fatal() -> bool {
    MAIN.with(Cell::get) && !CATCHING.with(Cell::get)
}

/// Give the terminal back for a panic that ends nvmux.
fn give_back() {
    crate::term::restore_and_reset();
    // For a screen's raw mode when no termios was saved: with stdin not a
    // terminal, crossterm makes /dev/tty raw.
    ratatui::restore();
}

/// Re-raise a panic carried over from a joined thread, as
/// [`panic::resume_unwind`] does but through the hook, which that skips. On
/// the main thread the panic ends nvmux: the terminal must be given back, and
/// the message said, since the hook only logged it, with its location, on
/// the thread that raised it. A payload with no message is resumed as it is.
#[track_caller]
pub(crate) fn resume(payload: Box<dyn Any + Send>) -> ! {
    let message = match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(message) => (*message).to_owned(),
            Err(payload) => {
                if fatal() {
                    give_back();
                }
                panic::resume_unwind(payload)
            }
        },
    };
    panic!("{message} (re-raised from a joined thread)")
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

    /// Set on the test binary re-run as the child of a test below.
    const CHILD: &str = "NVMUX_TEST_PANICS_CHILD";

    /// Written by a child once the session's screen is up, and when done.
    const UP: &str = "<<up>>";
    const DONE: &str = "<<done>>";

    /// What a client turns on as it starts (see `crate::client::term`), an
    /// update it was cut off in, and what turns each off.
    const SESSION_MODES: [(&str, &str); 9] = [
        ("\x1b[?1049h", "\x1b[?1049l"),
        ("\x1b[?25l", "\x1b[?25h"),
        ("\x1b[?2004h", "\x1b[?2004l"),
        ("\x1b[?1004h", "\x1b[?1004l"),
        ("\x1b[>4;2m", "\x1b[>4;0m"),
        ("\x1b[?1002h", "\x1b[?1002l"),
        ("\x1b[?1006h", "\x1b[?1006l"),
        ("\x1b[?1003h", "\x1b[?1003l"),
        ("\x1b[?2026h", "\x1b[?2026l"),
    ];

    const BUG: &str = "a bug in nvmux";

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

    /// Not boxed again, which would hide its type from whoever catches it.
    #[test]
    fn a_payload_with_no_message_is_resumed_as_it_is() {
        let thrown = panic::catch_unwind(|| resume(Box::new(7_u8)));
        assert_eq!(thrown.expect_err("resumed").downcast_ref::<u8>(), Some(&7));
    }

    /// Regression: with a session up (raw mode, alternate screen) after three
    /// screens opened and closed, a caught panic must leave the terminal
    /// alone. Each screen used to add a ratatui hook that ran for caught
    /// panics too, leaving the alternate screen and printing over the session.
    #[test]
    fn a_caught_panic_leaves_the_session_it_happens_under_alone() {
        let after = after_up_in("a_session_with_a_panic_caught_under_it");
        assert!(after.contains(DONE), "the child never finished: {after:?}");
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
        report_raw_mode();
        drop(raw);
    }

    /// Otherwise the message staircases in raw mode (no `\r` before a `\n`),
    /// and the client's modes outlive nvmux: the relay retires a client
    /// without relaying its exit sequence.
    #[test]
    fn a_panic_in_a_relay_gives_the_terminal_back_before_its_message() {
        given_back_before_the_message(&after_up_in("a_relay_that_panics"));
    }

    /// The child of the test above; does nothing without [`CHILD`].
    #[test]
    #[ignore = "the child half of a test; run by it, not by hand"]
    fn a_relay_that_panics() {
        if std::env::var_os(CHILD).is_none() {
            return;
        }
        install();
        let _raw = in_a_relay();
        panic!("{BUG}");
    }

    /// As when stdin is not a terminal: no termios was saved, and the
    /// screen's raw mode is crossterm's alone.
    #[test]
    fn a_panic_under_a_screen_gives_the_terminal_back_before_its_message() {
        given_back_before_the_message(&after_up_in("a_screen_that_panics"));
    }

    /// The child of the test above; does nothing without [`CHILD`].
    #[test]
    #[ignore = "the child half of a test; run by it, not by hand"]
    fn a_screen_that_panics() {
        if std::env::var_os(CHILD).is_none() {
            return;
        }
        install();
        let _screen = crate::ui::Screen::open().expect("a screen");
        crate::term::write_stdout(format!("{UP}\r\n").as_bytes()).expect("the mark");
        panic!("{BUG}");
    }

    /// As a session's creation takes on its metadata thread's panic, which
    /// `resume_unwind` would carry past the hook: the shell would be left on
    /// the alternate screen, with no message.
    #[test]
    fn a_panic_taken_on_from_a_joined_thread_gives_the_terminal_back_too() {
        given_back_before_the_message(&after_up_in("a_relay_that_takes_on_a_panic"));
    }

    /// The child of the test above; does nothing without [`CHILD`].
    #[test]
    #[ignore = "the child half of a test; run by it, not by hand"]
    fn a_relay_that_takes_on_a_panic() {
        if std::env::var_os(CHILD).is_none() {
            return;
        }
        install();
        let _raw = in_a_relay();
        let joined = std::thread::spawn(|| panic!("{BUG}")).join();
        resume(joined.expect_err("a panic"));
    }

    /// Such a panic is resumed past the hook, with nothing to print.
    #[test]
    fn a_panic_with_no_message_taken_on_gives_the_terminal_back_too() {
        let after = after_up_in("a_relay_that_takes_on_a_panic_with_no_message");
        for (on, off) in SESSION_MODES {
            assert!(after.contains(off), "{on:?} was left on: {after:?}");
        }
    }

    /// The child of the test above; does nothing without [`CHILD`].
    #[test]
    #[ignore = "the child half of a test; run by it, not by hand"]
    fn a_relay_that_takes_on_a_panic_with_no_message() {
        if std::env::var_os(CHILD).is_none() {
            return;
        }
        install();
        let _raw = in_a_relay();
        let joined = std::thread::spawn(|| panic::panic_any(7_u8)).join();
        resume(joined.expect_err("a panic"));
    }

    /// Such a panic (a parked client's thread, a probe's) ends only its
    /// thread; the relay goes on.
    #[test]
    fn a_panic_on_another_thread_leaves_the_relay_alone() {
        let after = after_up_in("a_relay_with_a_thread_that_panics");
        assert!(after.contains(DONE), "the child never finished: {after:?}");
        assert!(after.contains("raw: on"), "raw mode went: {after:?}");
        for (on, off) in SESSION_MODES {
            assert!(
                !after.contains(off),
                "{on:?} was undone under the relay: {after:?}"
            );
        }
        assert!(
            !after.contains("panicked"),
            "the panic was written over the relay: {after:?}"
        );
    }

    /// The child of the test above; does nothing without [`CHILD`].
    #[test]
    #[ignore = "the child half of a test; run by it, not by hand"]
    fn a_relay_with_a_thread_that_panics() {
        if std::env::var_os(CHILD).is_none() {
            return;
        }
        install();
        let raw = in_a_relay();
        assert!(std::thread::spawn(|| panic!("{BUG}")).join().is_err());
        report_raw_mode();
        drop(raw);
    }

    /// Run `panics::tests::<name>` as [`CHILD`] on a fresh pty, and return
    /// what it wrote after [`UP`] by the time it exits.
    fn after_up_in(name: &str) -> String {
        let (pty, pts) = pty_process::blocking::open().expect("a pty");
        pty.resize(pty_process::Size::new(24, 80)).expect("size it");
        let test = format!("panics::tests::{name}");
        let cmd = pty_process::blocking::Command::new(std::env::current_exe().expect("the tests"))
            .args([
                "--exact",
                test.as_str(),
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
        // Ends once the child has exited and all it wrote is read.
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
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(bytes) => {
                    // Answer a cursor position query, should a screen ask.
                    if bytes.windows(4).any(|w| w == b"\x1b[6n") {
                        let _ = writer.write_all(b"\x1b[1;1R");
                    }
                    out.extend_from_slice(&bytes);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        let out = String::from_utf8_lossy(&out);
        match out.split_once(UP) {
            Some((_, after)) => after.to_string(),
            None => panic!("the session never came up: {out:?}"),
        }
    }

    /// Take the terminal as a relay holds it: handed over by a screen, as the
    /// session loop does, then raw, with a client's modes on. Writes [`UP`].
    fn in_a_relay() -> crate::term::RawMode {
        crate::ui::Screen::open()
            .expect("a screen")
            .close_for_attach(b"")
            .expect("the terminal handed over");
        let raw = crate::term::RawMode::enter().expect("raw mode");
        let on: String = SESSION_MODES.iter().map(|(on, _)| *on).collect();
        crate::term::write_stdout(format!("{on}{UP}\r\n").as_bytes()).expect("the modes");
        raw
    }

    /// Every one of [`SESSION_MODES`] is off before the last panic message,
    /// which came out cooked: no bare `\n` from there on.
    fn given_back_before_the_message(after: &str) {
        let message = after
            .rfind("panicked")
            .unwrap_or_else(|| panic!("no panic message: {after:?}"));
        for (on, off) in SESSION_MODES {
            assert!(
                after[..message].contains(off),
                "{on:?} was left on: {after:?}"
            );
        }
        let message = &after[message..];
        assert!(message.contains(BUG), "not the panic's message: {after:?}");
        assert!(
            !message.replace("\r\n", "").contains('\n'),
            "the message was written in raw mode: {after:?}"
        );
    }

    /// Write whether the terminal is still raw, then [`DONE`].
    fn report_raw_mode() {
        let termios = nix::sys::termios::tcgetattr(std::io::stdin().as_fd()).expect("termios");
        let raw = !termios
            .local_flags
            .contains(nix::sys::termios::LocalFlags::ICANON);
        let report = format!("raw: {}\r\n{DONE}\r\n", if raw { "on" } else { "off" });
        crate::term::write_stdout(report.as_bytes()).expect("the report");
    }
}
