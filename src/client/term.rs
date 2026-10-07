//! The client's side of its terminal: the pty nvmux runs it on.
//!
//! Everything here is said the way `nvim --remote-ui` says it, because the
//! relay in [`crate::pty`] watches a client's output for exactly those
//! sequences and acts on them:
//!
//! - the opening enters the alternate screen first, which is where a held
//!   first paint is split (see `pty::Hold`);
//! - it ends with a device attributes request, whose answer is what tells the
//!   relay the client's questions have all been answered and it may be parked
//!   (see `pty::Attachment::startup_answered`) — and it is asked at no other
//!   time, since a parked client asking it is one on its way out;
//! - the modes it sets are the ones [`crate::ledger`] records and puts back
//!   for a kept client: focus reports, bracketed paste, the mouse, the kitty
//!   keyboard protocol or `modifyOtherKeys`, the title, the cursor's shape and
//!   colour;
//! - and the closing leaves the alternate screen, which a parked client does
//!   only when it is finishing.
//!
//! # The keyboard
//!
//! Like Neovim's TUI the client asks for the kitty keyboard protocol — the
//! `CSI ? u` query, with the device attributes request behind it so that a
//! terminal that does not know the query is found out by answering the other
//! — and turns on its first flag, which spells every chord unambiguously,
//! where the terminal has it; and xterm's `modifyOtherKeys` where it does not.
//! [`super::input`] reads both, and so does nvmux's own prefix
//! ([`crate::keyseq`]).

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

use nix::sys::termios::{self, SetArg, Termios};

/// What the client says first: the alternate screen, the title saved, the
/// cursor hidden until the first frame puts it somewhere, bracketed paste and
/// focus reports on, and the keyboard question, closed by the device
/// attributes request.
pub const OPENING: &[u8] = b"\x1b[?1049h\x1b[22;0t\x1b[?25l\x1b[?2004h\x1b[?1004h\x1b[?u\x1b[c";

/// The kitty keyboard protocol's first flag, pushed: every chord spelt so
/// that it cannot be taken for another.
pub const KITTY_ON: &[u8] = b"\x1b[>1u";

/// xterm's `modifyOtherKeys`, level 2, for a terminal without kitty's.
pub const OTHER_KEYS_ON: &[u8] = b"\x1b[>4;2m";

/// Mouse reporting as Neovim's TUI turns it on: button events, in SGR, and
/// every motion on top where `'mousemoveevent'` asks for it — see
/// [`crate::term`]'s `MOUSE_ON` for why in that order.
pub fn mouse(on: bool, moves: bool) -> &'static [u8] {
    match (on, moves) {
        (true, true) => b"\x1b[?1002h\x1b[?1006h\x1b[?1003h",
        (true, false) => b"\x1b[?1003l\x1b[?1002h\x1b[?1006h",
        (false, _) => b"\x1b[?1003l\x1b[?1006l\x1b[?1002l",
    }
}

/// Everything [`OPENING`] and what followed it turned on, turned off, and the
/// terminal handed back as it was: the pen, the cursor's colour, shape and
/// visibility, the mouse, focus reports, bracketed paste, the keyboard modes,
/// the saved title, and the alternate screen last. Popping a keyboard mode
/// that was never pushed, and leaving a mode never entered, change nothing,
/// so this is one fixed string whatever the terminal turned out to have.
pub const CLOSING: &[u8] = b"\x1b[?2026l\x1b[0m\x1b]112\x07\x1b[0 q\x1b[?25h\
\x1b[?1003l\x1b[?1006l\x1b[?1002l\x1b[?1004l\x1b[?2004l\x1b[<u\x1b[>4;0m\x1b[23;0t\x1b[?1049l";

/// A title as the terminal is to show it: control characters taken out, so
/// that one cannot end the sequence it is sent in and start another.
pub fn title(text: &str) -> Vec<u8> {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]2;{clean}\x07").into_bytes()
}

/// The pty in raw mode for as long as this lives: keys arrive as bytes, a
/// `Ctrl-C` is a key rather than a signal, and nothing is echoed or
/// translated on the way in or out. Put back as it was when dropped.
pub struct Raw {
    saved: Option<Termios>,
}

impl Raw {
    pub fn enter() -> std::io::Result<Self> {
        let stdin = std::io::stdin();
        let saved = match termios::tcgetattr(&stdin) {
            Ok(t) => t,
            // Not a terminal — a test feeding the client from a pipe. There
            // is nothing to make raw.
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

/// The terminal's size, as the pty says: columns and rows.
pub fn size() -> (usize, usize) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 && ws.ws_row > 0 {
        (usize::from(ws.ws_col), usize::from(ws.ws_row))
    } else {
        (80, 24)
    }
}

/// Whether the client's input and output are a terminal, which it tells
/// Neovim as it attaches (`stdin_tty`, `stdout_tty`) the way Neovim's own TUI
/// does. Under nvmux both are the pty the relay reads, and Neovim keys what it
/// writes to a terminal on the second: `nvim_ui_send` reaches only a UI that
/// says it has one, and the OSC 52 clipboard is turned on only for a terminal
/// asked through such a UI (`plugin/osc52.lua`): without it, a yank to `+`
/// finds no clipboard to go to.
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

/// The write end of the hangup pipe, for the handler.
static HANGUP_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_hangup(_sig: libc::c_int) {
    let fd = HANGUP_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        unsafe {
            libc::write(fd, c"h".as_ptr().cast(), 1);
        }
    }
}

/// Being told to go: `SIGHUP`, which is how nvmux retires a client (see
/// `pty::Attachment::hang_up`), and `SIGTERM`. A self-pipe like
/// [`crate::winch::Winch`]'s, so the going is noticed by the loop's own `poll`
/// and the client leaves the way it always does — saying [`CLOSING`] — rather
/// than from inside a signal handler, part way through a frame.
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

    /// The relay splits a held first paint just past the alternate screen's
    /// entry, so it is the very first thing said; and the device attributes
    /// request is the last, closing the questions.
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

    /// The closing leaves the alternate screen, last, and asks nothing: a
    /// client retired with its output thrown away must not leave a question
    /// for the real terminal to answer into the shell.
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

    /// The ledger reads the mouse the way Neovim's TUI sets it, so this sets
    /// it that way.
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
