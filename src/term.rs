//! Terminal mode handling for the attach path.
//!
//! While a session is attached, nvmux is in raw mode with `ISIG` off, so
//! `Ctrl-c`, `Ctrl-z` and `Ctrl-s` reach Neovim as bytes, not signals.
//! `cfmakeraw` already clears `ISIG` and `IXON`, disables `ICANON`, `ECHO`
//! and `OPOST`, and sets `VMIN = 1` / `VTIME = 0` (measured); only `VSUSP`
//! needs disabling on top.
//!
//! A `Drop` guard and panic hook do not cover a signal such as a `kill` from
//! another window, which would leave the shell with no echo or line editing.
//! So the saved termios also lives in a static; a signal handler restores it
//! and re-raises the signal with the default disposition, keeping the exit
//! status honest.

use std::os::fd::BorrowedFd;
use std::sync::OnceLock;

use nix::sys::termios::{self, SetArg, SpecialCharacterIndices, Termios};

use crate::error::Result;

/// Write to stdout and flush, under one lock. Every sequence nvmux writes on
/// its own account goes through here, except in [`restore_and_reraise`],
/// which as a signal handler may only `write(2)`.
pub(crate) fn write_stdout(bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    out.write_all(bytes)?;
    out.flush()
}

/// Readable from a signal handler: plain C data, and `OnceLock::get` is an
/// atomic load plus a read of initialised memory.
struct Saved(libc::termios);

// SAFETY: `libc::termios` is a plain-data struct with no pointers or interior
// mutability; sharing a copy across threads (and into a signal handler) is
// safe.
unsafe impl Send for Saved {}
unsafe impl Sync for Saved {}

static SAVED: OnceLock<Saved> = OnceLock::new();

fn stdin_fd() -> BorrowedFd<'static> {
    // SAFETY: fd 0 is valid for the lifetime of the process.
    unsafe { BorrowedFd::borrow_raw(libc::STDIN_FILENO) }
}

/// Restore the terminal, then re-raise under the default disposition (not
/// `_exit`) so the shell reports the signal. `tcsetattr`, `write`, `signal`
/// and `raise` are all async-signal-safe.
extern "C" fn restore_and_reraise(sig: libc::c_int) {
    if let Some(saved) = SAVED.get() {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved.0);
        }
    }
    // Undo screen state (alternate screen, hidden cursor); harmless if none.
    unsafe {
        libc::write(libc::STDOUT_FILENO, RESET.as_ptr().cast(), RESET.len());
    }
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// What a client cut off mid-frame leaves behind: an open synchronized update
/// (Neovim draws inside one) and its current SGR attributes. Both are
/// terminal-wide, not per screen buffer, so they apply to what nvmux draws
/// next.
const INHERITED: &[u8] = b"\x1b[?2026l\x1b[0m";

/// [`INHERITED`], plus leave the alternate screen (no-op if not in it), show
/// the cursor, and turn off focus reports, bracketed paste, modifyOtherKeys and
/// mouse reporting ([`MOUSE_OFF`]). One fixed string so the signal handler can
/// write it, and error paths match a signal's reset. The modes are included
/// because every path here ends at a shell prompt, and a client killed with
/// nvmux never turns its own off. The kitty keyboard stack is per screen, so
/// leaving the alternate screen is its reset.
const RESET: &[u8] = b"\x1b[?2026l\x1b[?1049l\x1b[0m\x1b[?25h\x1b[?1004l\x1b[?2004l\x1b[>4;0m\
\x1b[?1003l\x1b[?1006l\x1b[?1002l";

/// Mouse reporting as Neovim's TUI enables it with `'mousemoveevent'`:
/// button-event tracking (`?1002`) in SGR encoding (`?1006`, needed past
/// column 223), then any-event tracking (`?1003`) for plain motion. In that
/// order, so a terminal without `?1003` still reports clicks.
///
/// Not crossterm's `EnableMouseCapture`: [`RESET`] must carry the undo as one
/// fixed string. crossterm parses SGR reports however they were enabled.
const MOUSE_ON: &[u8] = b"\x1b[?1002h\x1b[?1006h\x1b[?1003h";

/// [`MOUSE_ON`] undone, in reverse.
const MOUSE_OFF: &[u8] = b"\x1b[?1003l\x1b[?1006l\x1b[?1002l";

/// What a resumed client's mouse reporting is restored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseReporting {
    /// `mouse=`.
    Off,
    /// Presses, releases and drags, in SGR: Neovim's TUI with a non-empty
    /// `'mouse'`.
    Buttons,
    /// `Buttons` plus plain motion: `'mousemoveevent'` set.
    Motion,
}

/// [`MouseReporting::Buttons`] from a screen with `?1003` on. `?1003` goes
/// off first: on xterm, resetting any tracking mode clears all tracking.
const MOUSE_BUTTONS: &[u8] = b"\x1b[?1003l\x1b[?1002h\x1b[?1006h";

/// Turn mouse reporting on for an nvmux screen; [`disable_mouse`] undoes it.
/// A client enables the mouse only at startup, so on resume the relay restores
/// its setting with [`set_mouse_reporting`]: from its ledger for a kept
/// client, or asked of a held client's server.
pub fn enable_mouse() {
    let _ = write_stdout(MOUSE_ON);
}

pub fn set_mouse_reporting(reporting: MouseReporting) {
    let _ = write_stdout(mouse_reporting_bytes(reporting));
}

/// What [`set_mouse_reporting`] writes, for callers batching it with other
/// output (the settings screen over a session).
pub fn mouse_reporting_bytes(reporting: MouseReporting) -> &'static [u8] {
    match reporting {
        MouseReporting::Off => MOUSE_OFF,
        MouseReporting::Buttons => MOUSE_BUTTONS,
        MouseReporting::Motion => MOUSE_ON,
    }
}

pub fn disable_mouse() {
    let _ = write_stdout(MOUSE_OFF);
}

/// `DECTCEM` on, and nothing else.
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

/// Show the cursor for a client resumed without a fade in. A fade hides the
/// cursor and only a fade in shows it again (see [`crate::shadow`]); without
/// one the hide from the last fade out would stick, since the requested
/// repaint is not guaranteed.
pub fn show_cursor() {
    let _ = write_stdout(SHOW_CURSOR);
}

/// Write [`RESET`], for paths that end at a shell prompt (failed attach,
/// detach) after a client may have been cut off mid-frame.
pub fn reset_screen() {
    let _ = write_stdout(RESET);
}

/// Home the cursor and erase its line. Only for the two paths that hang up
/// on a client and end at a shell, so not part of [`RESET`].
const ERASE_LINE: &[u8] = b"\r\x1b[2K";

/// Erase the `Nvim: Caught deadly signal 'SIGHUP'` line a retired client
/// prints. Neovim restores the terminal (leaving the alternate screen) before
/// printing it, so it lands on the user's screen. The detach path relays that
/// restore on purpose (see [`crate::pty::Attachment::drain_until_eof`]): it
/// carries the kitty keyboard pop and the bracketed paste and title restores,
/// which nvmux does not send.
///
/// Called after the client is gone and [`RESET`] has left the alternate
/// screen, which puts the cursor back on that line. The client's output is not
/// filtered. Safe because the line is where the shell prompt goes next; most
/// prompts erase it anyway, a bare `PS1='$ '` does not.
pub fn erase_hung_up_clients_line() {
    let _ = write_stdout(ERASE_LINE);
}

/// Write [`INHERITED`], for paths that stop the relay mid-stream and end at
/// another nvmux screen (`<prefix> Space`, `c`, `?`), where no client restore
/// sequence arrives.
///
/// The picker cannot paint over the leftovers: it sets no colours, and
/// ratatui 0.30's crossterm backend emits SGR only when a cell differs from
/// `Color::Reset` and skips blank cells, so the editor's last attributes would
/// become the picker's.
///
/// Must run before entering the alternate screen and before the clear: both
/// erase with the current background colour.
pub fn reset_inherited_attributes() {
    let _ = write_stdout(INHERITED);
}

/// Install the restore-on-signal handlers. Called from `main` before any
/// screen is drawn, so the saved termios is the shell's. [`RawMode::enter`]
/// also calls it, for callers that bypass `main`.
pub fn install_signal_safety_net() {
    let mut raw_saved = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: fd 0 is valid and tcgetattr fully initialises the struct on
    // success. On a non-terminal stdin it fails, and there is nothing to save.
    let got = unsafe { libc::tcgetattr(libc::STDIN_FILENO, raw_saved.as_mut_ptr()) };
    if got == 0 {
        // First writer wins: the earliest snapshot is the shell's own mode.
        let _ = SAVED.set(Saved(unsafe { raw_saved.assume_init() }));
    }
    install_signal_handlers();
}

/// Raw mode, restored on drop. [`RawMode::restore`] is idempotent, so it and
/// the panic-path `Drop` can both run.
pub struct RawMode {
    saved: Termios,
    restored: bool,
}

impl RawMode {
    pub fn enter() -> Result<Self> {
        let saved = termios::tcgetattr(stdin_fd())?;

        // Takes the signal-readable copy with a raw `tcgetattr`: converting
        // the nix value returns a stale cache that lacks edits made through
        // the public fields (only `tcsetattr` syncs it). No-op if `main` ran
        // it already.
        install_signal_safety_net();

        let mut raw = saved.clone();
        termios::cfmakeraw(&mut raw);
        // cfmakeraw leaves VSUSP set: on a terminal whose VSUSP is not Ctrl-z,
        // that could suspend nvmux mid-session.
        raw.control_chars[SpecialCharacterIndices::VSUSP as usize] = termios::_POSIX_VDISABLE;

        // TCSANOW, not TCSAFLUSH: keep keys typed while the session starts.
        termios::tcsetattr(stdin_fd(), SetArg::TCSANOW, &raw)?;

        Ok(Self {
            saved,
            restored: false,
        })
    }

    /// `TCSADRAIN`, so output already written (notably the child's restore
    /// sequence) reaches the terminal before the mode changes.
    pub fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        if let Err(e) = termios::tcsetattr(stdin_fd(), SetArg::TCSADRAIN, &self.saved) {
            tracing::error!(error = %e, "could not restore terminal modes");
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}

fn install_signal_handlers() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
            // SAFETY: `restore_and_reraise` only calls async-signal-safe
            // functions.
            unsafe {
                libc::signal(sig, restore_and_reraise as *const () as libc::sighandler_t);
            }
        }
    });
}

/// The terminal's size as a PTY size. crossterm gives `(cols, rows)` but
/// `PtySize` is `{ rows, cols }`. Pixel sizes are filled in where reported:
/// zero breaks sixel and kitty images in the editor.
pub fn terminal_size() -> crate::pty::PtySize {
    use ratatui::crossterm::terminal;
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let (pixel_width, pixel_height) = terminal::window_size()
        .map(|w| (w.width, w.height))
        .unwrap_or((0, 0));
    crate::pty::PtySize {
        rows,
        cols,
        pixel_width,
        pixel_height,
    }
}

/// Hands the terminal to a session: [`INHERITED`], `?1049l`, then clear and
/// home. The order matters (a test pins it): the erase uses the current
/// background, so attributes go first, and it must land on the screen the
/// session draws on. One `write`, so no frame in between is presented.
pub(crate) const HANDOVER: &[u8] = b"\x1b[?2026l\x1b[0m\x1b[?1049l\x1b[2J\x1b[H";

/// Leave the alternate screen and clear, for a client about to be spawned.
/// Call it before the spawn, not after: until it runs, the old session stays
/// on screen for the whole spawn. A resumed client needs
/// [`enter_alt_screen_and_clear`] instead. Bypasses crossterm so raw mode
/// stays the caller's business.
pub fn leave_alt_screen_and_clear() {
    let _ = write_stdout(HANDOVER);
}

/// [`HANDOVER`] with `?1049h` instead, to hand the terminal back to a resumed
/// client. `?1049h` while already in the alternate screen is a no-op on xterm
/// and kitty, so the erase is explicit. No title push (`\e[22;0;0t`): the
/// client's `smcup` pushed it once and its `rmcup` pops it once.
pub(crate) const RESUME: &[u8] = b"\x1b[?2026l\x1b[0m\x1b[?1049h\x1b[2J\x1b[H";

/// Re-enter the alternate screen and clear, for a client about to be resumed.
///
/// A client enters the alternate screen only at startup, but nvmux's screens
/// end on the primary one (see `ui::Screen`). Without this a resumed client
/// would draw on the primary screen, and on detach its last frame would stay
/// under the shell prompt.
pub fn enter_alt_screen_and_clear() {
    let _ = write_stdout(RESUME);
}

/// Synchronized update bounds; [`HANDOVER`] and [`RESUME`] begin with
/// `SPAN_END`.
const SPAN_BEGIN: &[u8] = b"\x1b[?2026h";
const SPAN_END: &[u8] = b"\x1b[?2026l";

/// [`leave_alt_screen_and_clear`] with `keep` drawn in the same synchronized
/// update, so the clear is never shown without it. For the picker's hand-off
/// name (see [`crate::handoff`]).
pub fn leave_alt_screen_and_clear_keeping(keep: &[u8]) {
    let _ = write_stdout(&leaving_keeping(keep));
}

fn leaving_keeping(keep: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HANDOVER.len() + keep.len() + 2 * SPAN_BEGIN.len());
    out.extend_from_slice(SPAN_END);
    out.extend_from_slice(SPAN_BEGIN);
    out.extend_from_slice(&HANDOVER[SPAN_END.len()..]);
    out.extend_from_slice(keep);
    out.extend_from_slice(SPAN_END);
    out
}

/// [`enter_alt_screen_and_clear`] with `first` written on the screen being
/// left, leaving the synchronized update open: the caller's next frame (a
/// dissolve's first) closes it, so the switch and that frame are presented
/// together. Otherwise close it with [`end_span`].
pub(crate) fn enter_alt_screen_and_clear_after(first: &[u8]) {
    let _ = write_stdout(&entering_after(first));
}

fn entering_after(first: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(RESUME.len() + first.len() + SPAN_BEGIN.len());
    out.extend_from_slice(SPAN_BEGIN);
    out.extend_from_slice(first);
    out.extend_from_slice(&RESUME[SPAN_END.len()..]);
    out
}

pub(crate) fn end_span() {
    let _ = write_stdout(SPAN_END);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The span variants slice this prefix off.
    #[test]
    fn the_switches_begin_by_closing_a_span() {
        assert!(HANDOVER.starts_with(SPAN_END));
        assert!(RESUME.starts_with(SPAN_END));
    }

    #[test]
    fn leaving_keeps_the_name_inside_the_one_span() {
        let out = leaving_keeping(b"NAME");
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "\x1b[?2026l\x1b[?2026h\x1b[0m\x1b[?1049l\x1b[2J\x1b[HNAME\x1b[?2026l"
        );
    }

    #[test]
    fn entering_erases_first_and_leaves_the_span_open() {
        let text = String::from_utf8(entering_after(b"ERASE")).unwrap();
        assert_eq!(text, "\x1b[?2026hERASE\x1b[0m\x1b[?1049h\x1b[2J\x1b[H");
        assert!(!text.ends_with("\x1b[?2026l"));
    }
    use crate::test_support::{contains, position};

    /// `RESET` is a separate fixed string for the signal handler; this keeps
    /// the two in agreement.
    #[test]
    fn the_inherited_reset_is_the_part_of_reset_that_says_nothing_about_the_screen() {
        for undo in [&b"\x1b[?2026l"[..], &b"\x1b[0m"[..]] {
            assert!(contains(INHERITED, undo), "INHERITED dropped {undo:?}");
            assert!(contains(RESET, undo), "RESET dropped {undo:?}");
        }
        // Only those two: leaving the alternate screen would flash the shell
        // before the picker re-enters it, and showing the cursor would undo
        // the next screen's hide.
        assert_eq!(INHERITED, b"\x1b[?2026l\x1b[0m");
        assert!(!contains(INHERITED, b"\x1b[?1049l"));
        assert!(!contains(INHERITED, SHOW_CURSOR));
        assert!(!contains(INHERITED, b"\x1b[?1002l"));
    }

    #[test]
    fn the_cursor_show_is_the_one_the_reset_carries_and_nothing_more() {
        assert!(contains(RESET, SHOW_CURSOR));
        assert_eq!(SHOW_CURSOR, b"\x1b[?25h");
        assert!(
            !contains(HANDOVER, SHOW_CURSOR) && !contains(RESUME, SHOW_CURSOR),
            "the hand-offs leave the cursor to whoever draws next"
        );
    }

    /// A shell wants no mouse reports, even after a `kill` during the picker,
    /// so mouse-off must be in the signal handler's string.
    #[test]
    fn the_reset_turns_mouse_reporting_off() {
        assert!(
            RESET.ends_with(MOUSE_OFF),
            "RESET does not end with MOUSE_OFF"
        );
    }

    /// Both clients turn these on, and they outlive the alternate screen, so a
    /// client whose own `CLOSING` never arrives leaves them to the shell.
    #[test]
    fn the_reset_turns_off_the_terminal_wide_modes_a_client_turns_on() {
        for undo in [&b"\x1b[?1004l"[..], b"\x1b[?2004l", b"\x1b[>4;0m"] {
            assert!(contains(RESET, undo), "RESET leaves on {undo:?}");
            assert!(
                contains(crate::client::term::CLOSING, undo),
                "the client's own CLOSING no longer undoes {undo:?}"
            );
        }
    }

    /// In `RESET` it would wipe a scrollback line on every signal and error
    /// path, where no client wrote anything.
    #[test]
    fn erasing_the_hung_up_clients_line_is_not_part_of_the_shared_reset() {
        assert_eq!(ERASE_LINE, b"\r\x1b[2K");
        assert!(!contains(RESET, ERASE_LINE));
        assert!(!contains(INHERITED, ERASE_LINE));
        // Home first, leaving the shell at column 0.
        assert!(ERASE_LINE.starts_with(b"\r"));
    }

    #[test]
    fn mouse_off_undoes_mouse_on_in_reverse() {
        let modes = |s: &[u8], fin: u8| -> Vec<Vec<u8>> {
            s.split(|&b| b == 0x1b)
                .filter(|part| !part.is_empty())
                .map(|part| {
                    assert_eq!(part.last(), Some(&fin), "{part:?}");
                    part[..part.len() - 1].to_vec()
                })
                .collect()
        };
        let on = modes(MOUSE_ON, b'h');
        let mut off = modes(MOUSE_OFF, b'l');
        off.reverse();
        assert_eq!(on, off);
        assert_eq!(
            on,
            [b"[?1002".to_vec(), b"[?1006".to_vec(), b"[?1003".to_vec()]
        );
    }

    /// The reset must come first, or it would clear what was just set.
    #[test]
    fn restoring_button_tracking_resets_motion_before_it_sets_anything() {
        let at = |needle: &[u8]| position(MOUSE_BUTTONS, needle, "MOUSE_BUTTONS");
        assert!(at(b"\x1b[?1003l") < at(b"\x1b[?1002h"));
        assert!(at(b"\x1b[?1003l") < at(b"\x1b[?1006h"));
        assert!(!contains(MOUSE_BUTTONS, b"\x1b[?1003h"));
        assert!(!contains(MOUSE_BUTTONS, b"\x1b[?1002l"));
        assert!(!contains(MOUSE_BUTTONS, b"\x1b[?1006l"));
    }

    /// Screens and the relay own mouse reporting; the hand-off strings must
    /// not undo either.
    #[test]
    fn the_handover_strings_leave_mouse_reporting_alone() {
        for (name, s) in [
            ("INHERITED", INHERITED),
            ("HANDOVER", HANDOVER),
            ("RESUME", RESUME),
        ] {
            for mode in [&b"?1002"[..], b"?1006", b"?1000", b"?1003"] {
                assert!(
                    !contains(s, mode),
                    "{name} mentions {:?}",
                    std::str::from_utf8(mode)
                );
            }
        }
    }

    #[test]
    fn the_handover_drops_inherited_attributes_before_it_erases() {
        assert!(
            HANDOVER.starts_with(INHERITED),
            "the erase paints with the current background colour, so the \
             outgoing client's attributes must go first"
        );
        let at = |needle: &[u8]| position(HANDOVER, needle, "HANDOVER");
        assert!(
            at(b"\x1b[?1049l") < at(b"\x1b[2J"),
            "the erase must land on the screen the session will draw on, not \
             on the alternate screen being left"
        );
        assert!(at(b"\x1b[2J") < at(b"\x1b[H"));
    }

    /// A resumed client draws on whichever screen is current, and its exit
    /// only leaves the one it entered.
    #[test]
    fn the_resume_re_enters_the_alternate_screen_before_it_erases() {
        assert!(
            RESUME.starts_with(INHERITED),
            "the erase paints with the current background colour, so the \
             picker's or the client's leftover attributes must go first"
        );
        let at = |needle: &[u8]| position(RESUME, needle, "RESUME");
        assert!(
            at(b"\x1b[?1049h") < at(b"\x1b[2J"),
            "the erase must land on the alternate screen the client will draw \
             on, not on the primary screen being left"
        );
        assert!(at(b"\x1b[2J") < at(b"\x1b[H"));
        assert!(
            !contains(RESUME, b"\x1b[?1049l"),
            "a resume must not leave the alternate screen: the client is not \
             going to enter it again"
        );
        assert!(
            !contains(RESUME, b"\x1b[22;"),
            "the client's own smcup pushed the title; pushing it again would \
             leave the title stack one deep after its rmcup"
        );
        // Catches an edit to one that forgets the other.
        assert_eq!(
            RESUME.len(),
            HANDOVER.len(),
            "RESUME and HANDOVER should differ only in the direction of the switch"
        );
    }

    #[test]
    fn cfmakeraw_covers_isig_and_ixon_but_not_vsusp() {
        use nix::sys::termios::{ControlFlags, InputFlags, LocalFlags, OutputFlags};

        // No real tty needed.
        let mut t: Termios = unsafe { std::mem::zeroed::<libc::termios>() }.into();
        t.local_flags = LocalFlags::ISIG | LocalFlags::ICANON | LocalFlags::ECHO;
        t.input_flags = InputFlags::IXON | InputFlags::ICRNL;
        t.output_flags = OutputFlags::OPOST;
        t.control_flags = ControlFlags::CS8;
        t.control_chars[SpecialCharacterIndices::VSUSP as usize] = 0x1a;

        termios::cfmakeraw(&mut t);

        assert!(
            !t.local_flags.contains(LocalFlags::ISIG),
            "cfmakeraw should clear ISIG"
        );
        assert!(!t.local_flags.contains(LocalFlags::ICANON));
        assert!(!t.local_flags.contains(LocalFlags::ECHO));
        assert!(
            !t.input_flags.contains(InputFlags::IXON),
            "cfmakeraw should clear IXON"
        );
        assert!(!t.output_flags.contains(OutputFlags::OPOST));

        assert_eq!(
            t.control_chars[SpecialCharacterIndices::VSUSP as usize],
            0x1a,
            "cfmakeraw does not disable VSUSP, so nvmux must"
        );

        t.control_chars[SpecialCharacterIndices::VSUSP as usize] = termios::_POSIX_VDISABLE;
        assert_eq!(
            t.control_chars[SpecialCharacterIndices::VSUSP as usize],
            termios::_POSIX_VDISABLE
        );
    }

    /// nix caches a `libc::termios` beside the public fields and only
    /// `tcsetattr` syncs them, so `.into()` drops field edits. Hence the raw
    /// `tcgetattr` in `RawMode::enter`; if nix starts syncing, this fails.
    #[test]
    fn converting_a_termios_to_libc_silently_drops_field_edits() {
        let mut t: Termios = unsafe { std::mem::zeroed::<libc::termios>() }.into();
        t.control_chars[SpecialCharacterIndices::VSUSP as usize] = 0x1a;

        let raw: libc::termios = t.clone().into();
        let back: Termios = raw.into();

        assert_eq!(
            back.control_chars[SpecialCharacterIndices::VSUSP as usize],
            0,
            "nix now syncs on conversion; the raw tcgetattr in RawMode::enter \
             can be simplified"
        );
        // The original still shows the edit, so the loss is silent.
        assert_eq!(
            t.control_chars[SpecialCharacterIndices::VSUSP as usize],
            0x1a
        );
    }
}
