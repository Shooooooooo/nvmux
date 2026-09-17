//! Terminal mode handling for the attach path.
//!
//! # What raw mode is for here
//!
//! While a session is attached, nvmux stays in raw mode with `ISIG` disabled.
//! That is what makes `Ctrl-c`, `Ctrl-z` and `Ctrl-s` arrive at Neovim as
//! ordinary bytes instead of being turned into signals for nvmux. The proxy
//! forwards them; the editor decides what they mean.
//!
//! `cfmakeraw` already clears `ISIG` and `IXON`, disables `ICANON`, `ECHO` and
//! `OPOST`, and sets `VMIN = 1` / `VTIME = 0` — measured, not assumed. The one
//! genuinely non-redundant step is disabling `VSUSP`.
//!
//! # The failure this module exists to prevent
//!
//! A `Drop` guard and a panic hook do not cover a *signal*. Because raw mode
//! clears `ISIG`, `^C` cannot reach nvmux at all — but a plain `kill` from
//! another window still can, and returning from that without restoring the
//! terminal leaves the user's shell with no echo and no line editing. That is
//! the worst thing this program could do to somebody, and it happens after
//! nvmux is already gone, so there is nothing left to fix it.
//!
//! So the saved termios also lives in a static that a signal handler can read,
//! and the handler restores it before re-raising the signal with the default
//! disposition — which keeps the reported exit status honest.

use std::os::fd::BorrowedFd;
use std::sync::OnceLock;

use nix::sys::termios::{self, SetArg, SpecialCharacterIndices, Termios};

use crate::error::Result;

/// A `libc::termios` we can read from a signal handler: plain C data, and
/// `OnceLock::get` is an atomic load plus a read of initialised memory.
struct Saved(libc::termios);

// SAFETY: `libc::termios` is a plain-data struct with no pointers or interior
// mutability; sharing a copy across threads (and into a signal handler) is safe.
unsafe impl Send for Saved {}
unsafe impl Sync for Saved {}

static SAVED: OnceLock<Saved> = OnceLock::new();

fn stdin_fd() -> BorrowedFd<'static> {
    // SAFETY: fd 0 is valid for the lifetime of the process.
    unsafe { BorrowedFd::borrow_raw(libc::STDIN_FILENO) }
}

/// Restore the terminal, then die the way the signal intended.
///
/// Async-signal-safe: `tcsetattr`, `signal` and `raise` are all on POSIX's list.
/// Re-raising under the default disposition rather than calling `_exit` means
/// the shell still reports "terminated" or "hangup" rather than a plain exit
/// code that hides why the process died.
extern "C" fn restore_and_reraise(sig: libc::c_int) {
    if let Some(saved) = SAVED.get() {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved.0);
        }
    }
    // Undo whatever screen state was mid-flight — the picker's alternate
    // screen, a hidden cursor — so a signal does not hand the shell back a
    // blank screen with no cursor. A single fixed-string `write` is
    // async-signal-safe, like the `tcsetattr` above, and is harmless when
    // nothing was in progress.
    unsafe {
        libc::write(libc::STDOUT_FILENO, RESET.as_ptr().cast(), RESET.len());
    }
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// What a client cut off mid-frame leaves behind, and nothing else: an open
/// synchronized-update span (Neovim draws inside one) and whatever SGR
/// attributes were in force on the cell it had reached. Both are terminal-wide
/// state rather than per-screen-buffer, so they outlive the client's screen and
/// go on describing whatever nvmux draws next.
const INHERITED: &[u8] = b"\x1b[?2026l\x1b[0m";

/// The screen-state reset: [`INHERITED`], plus leaving the alternate screen (a
/// no-op when not in it), showing the cursor, and turning mouse reporting off
/// ([`MOUSE_OFF`]). One fixed string so the signal handler can write it, and so
/// the ordinary error paths put the screen back exactly the way a signal would.
///
/// The mouse is in here because every path this ends on is a shell prompt, and
/// a shell wants no mouse reports: the picker turns reporting on when nothing
/// is behind it, and a client killed with nvmux never gets to turn its own off.
const RESET: &[u8] = b"\x1b[?2026l\x1b[?1049l\x1b[0m\x1b[?25h\x1b[?1003l\x1b[?1006l\x1b[?1002l";

/// Mouse reporting as Neovim's own TUI turns it on with `'mousemoveevent'`
/// set: button-event tracking (`?1002`: presses, releases, and drags with a
/// button held) in the SGR encoding (`?1006`: a click past column 223 still
/// says where it landed), and then any-event tracking (`?1003`), which adds
/// the plain motion the picker follows with its highlight. In that order, so
/// a terminal without `?1003` is still left reporting clicks.
///
/// Fixed strings of nvmux's own rather than crossterm's `EnableMouseCapture`
/// (which also sets `?1000` and `?1015`, redundant under the two above):
/// [`RESET`] has to carry the undo as one string a signal handler can write,
/// and the handover strings are pinned not to mention any of these modes.
/// crossterm's parser reads SGR reports whichever command enabled them.
const MOUSE_ON: &[u8] = b"\x1b[?1002h\x1b[?1006h\x1b[?1003h";

/// [`MOUSE_ON`] undone, in reverse order.
const MOUSE_OFF: &[u8] = b"\x1b[?1003l\x1b[?1006l\x1b[?1002l";

/// What a resumed client's terminal mouse reporting is put back to — see
/// [`set_mouse_reporting`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseReporting {
    /// The client had the mouse off (`mouse=`).
    Off,
    /// Presses, releases and drags, in SGR: what Neovim's TUI turns on for a
    /// non-empty `'mouse'`.
    Buttons,
    /// [`MouseReporting::Buttons`] plus plain motion: `'mousemoveevent'` set.
    Motion,
}

/// [`MouseReporting::Buttons`], starting from a screen that had `?1003` on.
/// `?1003` off *first*: on xterm, resetting any one of the tracking modes
/// clears tracking altogether, so the two the client wants have to be set
/// after it and not before.
const MOUSE_BUTTONS: &[u8] = b"\x1b[?1003l\x1b[?1002h\x1b[?1006h";

/// Turn mouse reporting on, for a screen of nvmux's own.
///
/// Every screen does, whatever is behind it, and turns it off again as it
/// closes ([`disable_mouse`]). A held client's own setting is put back by the
/// relay as it resumes the client, with [`set_mouse_reporting`]: a
/// `--remote-ui` client enables the mouse once, at startup, and never again
/// (as it enters the alternate screen once, see [`enter_alt_screen_and_clear`]),
/// so nothing but nvmux can put the terminal back the way the client left it —
/// and what the client had is a question for its server, which the resume
/// already asks to repaint.
pub fn enable_mouse() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(MOUSE_ON);
    let _ = out.flush();
}

/// Put mouse reporting back to what a client being resumed had, after a screen
/// that set its own. See [`crate::pty`], which asks the client's server.
pub fn set_mouse_reporting(reporting: MouseReporting) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(match reporting {
        MouseReporting::Off => MOUSE_OFF,
        MouseReporting::Buttons => MOUSE_BUTTONS,
        MouseReporting::Motion => MOUSE_ON,
    });
    let _ = out.flush();
}

/// Turn mouse reporting off again, for the screen that turned it on.
pub fn disable_mouse() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(MOUSE_OFF);
    let _ = out.flush();
}

/// Show the cursor — `DECTCEM` on — and nothing else.
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

/// Show the cursor, for a client about to be resumed onto a screen nothing
/// dissolved in.
///
/// A fade hides the cursor and, on a fade in, puts it back on the last frame
/// (see [`crate::shadow`]). A resume with no fade in — the fade is off, the
/// shadow has nothing to show or cannot be trusted — inherits the hide from the
/// fade out that ended the last relay and has nothing to undo it: the repaint
/// asked for next is asked for, not certain. Shown here, where the erase left
/// it, which is where every screen used to leave it before there was a fade.
pub fn show_cursor() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(SHOW_CURSOR);
    let _ = out.flush();
}

/// Put the screen back to a state a shell can be used in: cursor visible,
/// colours reset, primary screen. For the paths that end at a shell prompt
/// with a message — an attach that failed, a detach — where the last thing
/// drawn may have been the client's, mid-frame, with the cursor hidden.
pub fn reset_screen() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(RESET);
    let _ = out.flush();
}

/// Home the cursor and erase the line it is on. Not part of [`RESET`], which a
/// signal handler writes and which every nvmux screen shares: this is only for
/// the two paths that hang up on a client and then end at a shell.
const ERASE_LINE: &[u8] = b"\r\x1b[2K";

/// Wipe the line a hung-up client printed its last words on.
///
/// nvmux retires a client with SIGHUP, and Neovim's shutdown runs its terminal
/// restore *before* its signal handler prints — leaving the alternate screen
/// first, so `Nvim: Caught deadly signal 'SIGHUP'` lands on the user's own
/// screen rather than on the one being torn down. The detach path relays all of
/// that on purpose (see [`crate::pty::Attachment::drain_until_eof`]): the
/// restore sequence carries things nvmux does not send itself — the kitty
/// keyboard pop, bracketed paste, the title — so refusing to relay it would
/// leave the terminal worse off than the stray line does.
///
/// So the line goes afterwards instead, once the client is gone and [`RESET`]
/// has left the alternate screen — which restores the cursor to where the
/// client printed from, so that is the line this erases. Nothing of the
/// client's output is filtered on the way through; nvmux cleans up after it in
/// nvmux's own bytes, which is what keeps "the bytes pass through untouched"
/// true.
///
/// Erasing it is safe because of where it is: the shell's cursor was at the
/// start of an empty line when nvmux was launched, and that is the line the
/// prompt is about to be drawn on anyway. Most prompts — zsh, starship,
/// powerlevel10k, fish — already erase it and so already hide this; a bare
/// `PS1='$ '` is what leaves it standing.
pub fn erase_hung_up_clients_line() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(ERASE_LINE);
    let _ = out.flush();
}

/// Drop the state inherited from a client cut off mid-frame, without saying
/// anything about whose screen it is.
///
/// For the paths that end at another nvmux screen rather than at a shell:
/// `<prefix> Space`, `<prefix> c` and `<prefix> ?` all stop the relay mid-stream
/// and leave the client running, so nothing emits the restore sequence
/// [`reset_screen`] relies on.
///
/// The picker cannot paint over the leftovers by itself, because it sets no
/// colours: every cell it draws is `Color::Reset` and every distinction is a
/// modifier, which is what makes it inherit the terminal's palette on purpose.
/// Measured against ratatui 0.30's crossterm backend: `draw` tracks fg and bg
/// from `Color::Reset` and emits SGR only on a difference, so a screen of
/// default cells writes none at all — and it skips blank cells entirely, so the
/// background is whatever the last erase painted. The editor's last attributes
/// are then the picker's, which is the bug this prevents.
///
/// Must run before the alternate screen is entered and before the clear: both
/// erase with the *current* background colour.
pub fn reset_inherited_attributes() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(INHERITED);
    let _ = out.flush();
}

/// Install the restore-on-signal safety net for the whole run.
///
/// Called once from `main`, before any screen is drawn: the picker runs in raw
/// mode and the alternate screen for as long as the user is choosing, and a
/// `kill` during that time would otherwise leave the terminal that way. The
/// termios saved here is the shell's, taken before anything changed it, which
/// is the one every later restore should return to. [`RawMode::enter`] also
/// calls this, for callers (tests, other embedders) that never went through
/// `main`.
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

/// Raw mode, restored when this is dropped. Restoring twice is harmless, so the
/// explicit [`RawMode::restore`] and the panic-path `Drop` can both run.
pub struct RawMode {
    saved: Termios,
    restored: bool,
}

impl RawMode {
    /// Put the terminal into raw mode with signals disabled.
    pub fn enter() -> Result<Self> {
        let saved = termios::tcgetattr(stdin_fd())?;

        // The signal-readable copy is taken with a raw `tcgetattr` rather than
        // by converting the nix value: `From<Termios> for libc::termios`
        // returns a cached inner struct **without syncing the public fields**
        // — only `tcsetattr` syncs — so a converted value silently loses
        // anything set through `control_chars`. A no-op when `main` already
        // did this at startup, which is the normal case.
        install_signal_safety_net();

        let mut raw = saved.clone();
        termios::cfmakeraw(&mut raw);
        // The one thing cfmakeraw does not do: a terminal whose VSUSP is not
        // Ctrl-z could otherwise suspend nvmux mid-session.
        raw.control_chars[SpecialCharacterIndices::VSUSP as usize] = termios::_POSIX_VDISABLE;

        // TCSANOW, not TCSAFLUSH: discarding type-ahead would silently eat
        // keystrokes someone typed while the session was still starting.
        termios::tcsetattr(stdin_fd(), SetArg::TCSANOW, &raw)?;

        Ok(Self {
            saved,
            restored: false,
        })
    }

    /// Put the terminal back the way it was.
    ///
    /// `TCSADRAIN` so that anything already written — in particular the child's
    /// own restore sequence, which nvmux passes through verbatim — reaches the
    /// terminal before the mode changes underneath it.
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

/// The terminal's current size, as a PTY size.
///
/// Note the argument order. `crossterm::terminal::size()` returns
/// `(columns, rows)` while `PtySize` is `{ rows, cols }`; passing them
/// positionally transposes the screen and hands Neovim an 80x24-shaped 24x80
/// grid, which looks like a rendering bug rather than a units bug.
///
/// `pixel_width` and `pixel_height` are filled in where the terminal reports
/// them, because leaving them zero breaks the sixel and kitty image protocols
/// inside the attached editor.
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

/// What hands the terminal to a session: [`INHERITED`], then `?1049l` to leave
/// the alternate screen, then clear + home so the session starts from a known
/// state.
///
/// Every part of that order is load-bearing, and the test below pins it. An
/// erase paints with the *current* background colour, so the attributes an
/// outgoing client left behind have to go first; and the erase has to land on
/// the screen the session will draw on rather than on the one being left. One
/// string, so it is also one `write`: a terminal cannot present the moment in
/// between, which is the outgoing session's frame.
pub(crate) const HANDOVER: &[u8] = b"\x1b[?2026l\x1b[0m\x1b[?1049l\x1b[2J\x1b[H";

/// Leave the alternate screen and clear, without touching the terminal modes.
///
/// **For a client about to be spawned, and belongs before the spawn, never
/// after.** Whatever is on the terminal stays there until this runs, and on a
/// switch that is the session being switched away from — so with this on the
/// far side of a spawn (two RPC round trips and a whole new `nvim` process) the
/// old session is what the user watches for the length of the switch. The
/// callers are therefore the picker and the prompt as they give the terminal
/// back, `<prefix> <number>` as it leaves one relay for the next, and
/// [`crate::pty::relay`] as the backstop for a path that did neither.
///
/// A client that is *resumed* rather than spawned needs the opposite —
/// [`enter_alt_screen_and_clear`] — and the relay is what tells the two apart.
///
/// Written straight to stdout rather than through crossterm so it cannot
/// re-enter any mode handling: raw mode is the caller's business, and the
/// callers differ on whether they have dropped it yet.
pub fn leave_alt_screen_and_clear() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(HANDOVER);
    let _ = out.flush();
}

/// What hands the terminal back to a client nvmux took it from: [`INHERITED`],
/// then `?1049h` to re-enter the alternate screen, then clear + home.
///
/// The same order as [`HANDOVER`], for the same reasons, with the one switch
/// pointing the other way. `?1049h` on a terminal already in the alternate
/// screen is a no-op on xterm and kitty (which is what the picker relies on to
/// open over a client's frame), so the erase is explicit rather than left to
/// the switch. No `\e[22;0;0t` beside it: the client's own `smcup` pushed the
/// title once, its `rmcup` will pop it once, and a second push here would leave
/// the stack one deep.
pub(crate) const RESUME: &[u8] = b"\x1b[?2026l\x1b[0m\x1b[?1049h\x1b[2J\x1b[H";

/// Put the terminal back in the alternate screen, cleared, for a client that
/// is about to be resumed rather than spawned.
///
/// A `--remote-ui` client enters the alternate screen once, at startup, and
/// leaves it once, at exit. Nothing nvmux can ask of the server makes it do
/// either again: `:mode` and a resize repaint the grid wherever the terminal
/// is. But the picker, the prompt and the help screen all end by leaving the
/// alternate screen (see `ui::Screen`), and a switch that names the session
/// already attached goes through [`leave_alt_screen_and_clear`] — so a client
/// resumed after any of them has been moved to the primary screen without
/// being told, and repaints there.
///
/// Nothing looks wrong until that client leaves. Its `rmcup` then has no
/// alternate screen to leave, and neither does the `?1049l` nvmux writes on a
/// detach, so the editor's last frame stays put and the shell prompt is printed
/// onto it. Whether a given detach showed the bug depended on whether the user
/// had been to the picker and back first, which is what made it intermittent.
///
/// A terminal with no alternate screen ignores `?1049h` as it ignored the
/// client's; one whose `TERM` lacks `smcup` while the terminal itself has the
/// mode is put in the alternate screen here on a client that never asked, and
/// taken out of it by the `?1049l` every way out of nvmux ends with.
pub fn enter_alt_screen_and_clear() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(RESUME);
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// The two resets must go on agreeing about what a cut-off client leaves
    /// behind. They are written out separately because the signal handler needs
    /// `RESET` to be one fixed string it can `write`, so nothing but this stops
    /// them drifting apart.
    #[test]
    fn the_inherited_reset_is_the_part_of_reset_that_says_nothing_about_the_screen() {
        for undo in [&b"\x1b[?2026l"[..], &b"\x1b[0m"[..]] {
            assert!(contains(INHERITED, undo), "INHERITED dropped {undo:?}");
            assert!(contains(RESET, undo), "RESET dropped {undo:?}");
        }
        // And only those two. Leaving the alternate screen would show the shell
        // for a frame before the picker enters it again, and showing the cursor
        // would undo the hide the screen about to be drawn does for itself.
        assert_eq!(INHERITED, b"\x1b[?2026l\x1b[0m");
        assert!(!contains(INHERITED, b"\x1b[?1049l"));
        assert!(!contains(INHERITED, SHOW_CURSOR));
        assert!(!contains(INHERITED, b"\x1b[?1002l"));
    }

    /// The show a resume falls back on is the one every reset carries, and
    /// says nothing else: a client is about to draw, and its screen and modes
    /// are its own.
    #[test]
    fn the_cursor_show_is_the_one_the_reset_carries_and_nothing_more() {
        assert!(contains(RESET, SHOW_CURSOR));
        assert_eq!(SHOW_CURSOR, b"\x1b[?25h");
        assert!(
            !contains(HANDOVER, SHOW_CURSOR) && !contains(RESUME, SHOW_CURSOR),
            "the hand-offs leave the cursor to whoever draws next"
        );
    }

    /// Every path `RESET` ends on is a shell prompt, and a shell wants no mouse
    /// reports — including after a `kill` during the picker, which is why the
    /// mouse-off has to be part of the one string the signal handler writes.
    #[test]
    fn the_reset_turns_mouse_reporting_off() {
        assert!(
            RESET.ends_with(MOUSE_OFF),
            "RESET does not end with MOUSE_OFF"
        );
    }

    /// The erase is the hangup paths' alone. Folding it into `RESET` would put
    /// it in the signal handler and on every error path that borrows the same
    /// string — wiping a line of someone's scrollback on a `kill` nvmux did not
    /// ask for, when no client had written anything there to clean up.
    #[test]
    fn erasing_the_hung_up_clients_line_is_not_part_of_the_shared_reset() {
        assert_eq!(ERASE_LINE, b"\r\x1b[2K");
        assert!(!contains(RESET, ERASE_LINE));
        assert!(!contains(INHERITED, ERASE_LINE));
        // Home first, so it erases the line the cursor is on wherever the
        // client left the column, and leaves the shell a cursor at column 0.
        assert!(ERASE_LINE.starts_with(b"\r"));
    }

    /// The off string undoes exactly the modes the on string sets, last first.
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

    /// Putting a client's button tracking back after a screen had motion
    /// tracking on: the reset must come first, or it would clear what was
    /// just set.
    #[test]
    fn restoring_button_tracking_resets_motion_before_it_sets_anything() {
        let at = |needle: &[u8]| {
            MOUSE_BUTTONS
                .windows(needle.len())
                .position(|w| w == needle)
                .unwrap_or_else(|| panic!("MOUSE_BUTTONS dropped {needle:?}"))
        };
        assert!(at(b"\x1b[?1003l") < at(b"\x1b[?1002h"));
        assert!(at(b"\x1b[?1003l") < at(b"\x1b[?1006h"));
        assert!(!contains(MOUSE_BUTTONS, b"\x1b[?1003h"));
        assert!(!contains(MOUSE_BUTTONS, b"\x1b[?1002l"));
        assert!(!contains(MOUSE_BUTTONS, b"\x1b[?1006l"));
    }

    /// The mouse is the screen's while a screen is up and the relay's as a
    /// client resumes, so the strings that hand a terminal to a client, back
    /// to one, or to a screen must not touch it: whichever of the two wrote
    /// last would be undone.
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

    /// [`HANDOVER`] is three steps whose order is the whole of its correctness,
    /// and nothing but this stops a later edit reshuffling them.
    #[test]
    fn the_handover_drops_inherited_attributes_before_it_erases() {
        assert!(
            HANDOVER.starts_with(INHERITED),
            "the erase paints with the current background colour, so the \
             outgoing client's attributes must go first"
        );
        let at = |needle: &[u8]| {
            HANDOVER
                .windows(needle.len())
                .position(|w| w == needle)
                .unwrap_or_else(|| panic!("HANDOVER dropped {needle:?}"))
        };
        assert!(
            at(b"\x1b[?1049l") < at(b"\x1b[2J"),
            "the erase must land on the screen the session will draw on, not \
             on the alternate screen being left"
        );
        assert!(at(b"\x1b[2J") < at(b"\x1b[H"));
    }

    /// [`RESUME`] is [`HANDOVER`] with the switch pointing the other way, and
    /// only that: same attributes-first order, same erase-then-home, and it
    /// must *enter* the alternate screen — a resumed client draws on whichever
    /// screen the terminal is on, and its exit only leaves the one it entered.
    #[test]
    fn the_resume_re_enters_the_alternate_screen_before_it_erases() {
        assert!(
            RESUME.starts_with(INHERITED),
            "the erase paints with the current background colour, so the \
             picker's or the client's leftover attributes must go first"
        );
        let at = |needle: &[u8]| {
            RESUME
                .windows(needle.len())
                .position(|w| w == needle)
                .unwrap_or_else(|| panic!("RESUME dropped {needle:?}"))
        };
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
        // And the two hand over the same amount of state, so an edit to one
        // that forgets the other shows up here.
        assert_eq!(
            RESUME.len(),
            HANDOVER.len(),
            "RESUME and HANDOVER should differ only in the direction of the switch"
        );
    }

    /// The distinction the module docs rest on: `cfmakeraw` already does most of
    /// what the design calls for, and `VSUSP` is the part it does not.
    #[test]
    fn cfmakeraw_covers_isig_and_ixon_but_not_vsusp() {
        use nix::sys::termios::{ControlFlags, InputFlags, LocalFlags, OutputFlags};

        // A default-ish termios to exercise the flag maths without a real tty.
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

        // ...and the one thing it leaves alone, which is why we set it.
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

    /// `Termios -> libc::termios` is LOSSY, and this pins that down.
    ///
    /// nix keeps a cached `libc::termios` alongside the public `control_chars`
    /// and flag fields, and only `tcsetattr` syncs one into the other.
    /// `.into()` hands back the stale cache, so a value edited through the
    /// public fields converts to something that does not contain those edits.
    ///
    /// This is why the signal-handler copy is taken with a raw `tcgetattr`
    /// instead of by converting. If nix ever makes the conversion sync, this
    /// test fails and the comment in `RawMode::enter` can be revisited — that
    /// is the point of asserting it rather than merely writing it down.
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
        // The edit is still visible on the original, which is what makes the
        // loss silent rather than obvious.
        assert_eq!(
            t.control_chars[SpecialCharacterIndices::VSUSP as usize],
            0x1a
        );
    }
}
