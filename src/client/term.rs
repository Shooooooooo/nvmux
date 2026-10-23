//! The client's terminal setup on the pty nvmux runs it on.
//!
//! Sequences match `nvim --remote-ui` exactly, because the relay in
//! [`crate::pty`] watches for them:
//!
//! - the opening enters the alternate screen first; a held first paint is
//!   split there (see `pty::Hold`);
//! - it ends with a device attributes request, whose answer means startup is
//!   done and the client may be parked (see
//!   `pty::Attachment::startup_answered`). It is sent at no other time: a
//!   parked client sending it is exiting;
//! - the modes it sets are those [`crate::ledger`] records and restores for a
//!   kept client;
//! - the closing leaves the alternate screen, which a parked client does only
//!   when exiting.
//!
//! Like Neovim's TUI, the client queries kitty keyboard support (`CSI ? u`,
//! followed by DA so a terminal that ignores the query still answers) and
//! enables its first flag, falling back to xterm's `modifyOtherKeys`.
//! [`super::input`] and [`crate::keyseq`] read both.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

use nix::sys::termios::{self, SetArg, Termios};

/// Alternate screen, save title, hide cursor until the first frame, bracketed
/// paste and focus reports on, kitty keyboard query, then DA.
pub const OPENING: &[u8] = b"\x1b[?1049h\x1b[22;0t\x1b[?25l\x1b[?2004h\x1b[?1004h\x1b[?u\x1b[c";

/// Push kitty keyboard flag 1 (disambiguate escape codes).
pub const KITTY_ON: &[u8] = b"\x1b[>1u";

/// xterm `modifyOtherKeys` level 2, for terminals without the kitty protocol.
pub const OTHER_KEYS_ON: &[u8] = b"\x1b[>4;2m";

/// Mouse reporting as Neovim's TUI sets it: SGR button events, plus all
/// motion with `'mousemoveevent'`. See [`crate::term`]'s `MOUSE_ON` for the
/// order.
pub fn mouse(on: bool, moves: bool) -> &'static [u8] {
    match (on, moves) {
        (true, true) => b"\x1b[?1002h\x1b[?1006h\x1b[?1003h",
        (true, false) => b"\x1b[?1003l\x1b[?1002h\x1b[?1006h",
        (false, _) => b"\x1b[?1003l\x1b[?1006l\x1b[?1002l",
    }
}

/// Resets everything the client may have set, leaving the alternate screen
/// last. Popping an unpushed keyboard mode or resetting an unset mode is a
/// no-op, so one fixed string works on every terminal.
pub const CLOSING: &[u8] = b"\x1b[?2026l\x1b[0m\x1b]112\x07\x1b[0 q\x1b[?25h\
\x1b[?1003l\x1b[?1006l\x1b[?1002l\x1b[?1004l\x1b[?2004l\x1b[<u\x1b[>4;0m\x1b[23;0t\x1b[?1049l";

/// OSC 2 title with control characters removed, so the text cannot end the
/// sequence and start another.
pub fn title(text: &str) -> Vec<u8> {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]2;{clean}\x07").into_bytes()
}

/// Raw mode on the pty while alive; the saved termios is restored on drop
/// (nvmux hands the pty over with input already raw, see
/// `pty::take_input_as_written`).
pub struct Raw {
    saved: Option<Termios>,
}

impl Raw {
    pub fn enter() -> std::io::Result<Self> {
        let stdin = std::io::stdin();
        let saved = match termios::tcgetattr(&stdin) {
            Ok(t) => t,
            // Not a terminal, e.g. a test feeding the client from a pipe.
            Err(_) => return Ok(Self { saved: None }),
        };
        let mut raw = saved.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(&stdin, SetArg::TCSANOW, &raw).map_err(std::io::Error::from)?;
        Ok(Self { saved: Some(saved) })
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        if let Some(saved) = &self.saved {
            let _ = termios::tcsetattr(std::io::stdin(), SetArg::TCSANOW, saved);
        }
    }
}

/// `(cols, rows)`, defaulting to 80x24.
pub fn size() -> (usize, usize) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 && ws.ws_row > 0 {
        (usize::from(ws.ws_col), usize::from(ws.ws_row))
    } else {
        (80, 24)
    }
}

/// Reported to Neovim on attach as `stdin_tty`/`stdout_tty`, as its TUI does.
/// `stdout_tty` matters: `nvim_ui_send` only reaches a UI that claims a
/// terminal, and the OSC 52 clipboard (`plugin/osc52.lua`) depends on it, so
/// without it yanks to `+` go nowhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tty {
    pub stdin: bool,
    pub stdout: bool,
}

impl Tty {
    pub fn detect() -> Self {
        use std::io::IsTerminal;
        Self {
            stdin: std::io::stdin().is_terminal(),
            stdout: std::io::stdout().is_terminal(),
        }
    }
}

/// Write end of the hangup pipe, for the signal handler.
static HANGUP_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_hangup(_sig: libc::c_int) {
    let fd = HANGUP_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        unsafe {
            libc::write(fd, c"h".as_ptr().cast(), 1);
        }
    }
}

/// Self-pipe for `SIGHUP` (how nvmux retires a client, see
/// `pty::Attachment::hang_up`) and `SIGTERM`, so the main `poll` sees it and
/// the client exits normally with [`CLOSING`], not mid-frame from a handler.
pub struct Hangup {
    read: OwnedFd,
    _write: OwnedFd,
}

impl Hangup {
    pub fn install() -> std::io::Result<Self> {
        let (read, write) = nix::unistd::pipe().map_err(std::io::Error::from)?;
        for fd in [&read, &write] {
            let flags = nix::fcntl::OFlag::from_bits_truncate(
                nix::fcntl::fcntl(fd, nix::fcntl::F_GETFL).map_err(std::io::Error::from)?,
            );
            nix::fcntl::fcntl(
                fd,
                nix::fcntl::F_SETFL(flags | nix::fcntl::OFlag::O_NONBLOCK),
            )
            .map_err(std::io::Error::from)?;
        }
        HANGUP_FD.store(write.as_raw_fd(), Ordering::Relaxed);
        for sig in [libc::SIGHUP, libc::SIGTERM] {
            // SAFETY: the handler performs a single `write(2)` and nothing else.
            unsafe {
                libc::signal(sig, on_hangup as *const () as libc::sighandler_t);
            }
        }
        Ok(Self {
            read,
            _write: write,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.read.as_raw_fd()
    }
}

impl Drop for Hangup {
    fn drop(&mut self) {
        HANGUP_FD.store(-1, Ordering::Relaxed);
        for sig in [libc::SIGHUP, libc::SIGTERM] {
            unsafe {
                libc::signal(sig, libc::SIG_DFL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The relay splits a held first paint just past the alternate-screen
    /// entry, and treats DA as the end of startup.
    #[test]
    fn the_opening_enters_the_alternate_screen_first_and_asks_last() {
        assert!(OPENING.starts_with(b"\x1b[?1049h"));
        assert!(OPENING.ends_with(b"\x1b[c"));
        assert_eq!(
            OPENING.windows(3).filter(|w| w == b"\x1b[c").count(),
            1,
            "asked once: a parked client asking it is one leaving"
        );
    }

    /// A retired client's output is discarded, so a query in the closing
    /// would be answered by the real terminal into the shell.
    #[test]
    fn the_closing_leaves_the_alternate_screen_last_and_asks_nothing() {
        assert!(CLOSING.ends_with(b"\x1b[?1049l"));
        assert!(!CLOSING.windows(3).any(|w| w == b"\x1b[c"));
        for mode in [&b"?1004l"[..], b"?2004l", b"?1002l", b"?25h"] {
            assert!(
                CLOSING.windows(mode.len()).any(|w| w == mode),
                "{}",
                String::from_utf8_lossy(mode)
            );
        }
    }

    #[test]
    fn the_mouse_is_set_as_neovims_tui_sets_it() {
        let mut ledger = crate::ledger::Ledger::new();
        ledger.saw(mouse(true, false));
        assert_eq!(ledger.mouse(), crate::term::MouseReporting::Buttons);
        ledger.saw(mouse(true, true));
        assert_eq!(ledger.mouse(), crate::term::MouseReporting::Motion);
        ledger.saw(mouse(false, false));
        assert_eq!(ledger.mouse(), crate::term::MouseReporting::Off);
    }

    #[test]
    fn a_title_cannot_carry_a_sequence() {
        assert_eq!(title("a\x1b]0;b\x07c"), b"\x1b]2;a]0;bc\x07");
    }
}
