//! The picker: a centred list of session names and one dim hint line on the
//! last row. No borders, no popups — the hint line *is* the picker's help, and
//! the filter and kill confirm replace it in place.
//!
//! [`prompt`] and [`help`] are the other two screens. Both take the whole
//! terminal rather than drawing over anything, share the same vocabulary
//! (centred content, one dim hint row, no borders, no colour), and hand the
//! same client back afterwards. `prompt::run` owns a terminal for `<prefix> c`,
//! which arrives with none; `prompt::run_on` borrows the picker's — nesting the
//! two would enter the alternate screen twice and leave it once.
//!
//! # There is no preview pane, and there must never be one
//!
//! Neovim sizes the global grid to the per-dimension **minimum** across every
//! attached UI (`ui_refresh()`, run unconditionally at the end of
//! `ui_attach_impl()`), so a small preview UI would shrink the grid of the
//! session being edited in and fire `VimResized`. Confirmed: attaching a 40x10
//! UI alongside a 120x40 one collapses both to 40x10. No `ui_option` opts out.
//! So the picker reads session state over plain RPC and never attaches a second
//! UI to a live session.
//!
//! # Colour
//!
//! Set no background and hardcode no palette. Do **not** rely on crossterm's
//! own `NO_COLOR` handling: it turns `SetForegroundColor(c)` into a bare
//! `ESC[m`, a full SGR reset that wipes bold, reverse and dim mid-line. Call
//! `force_color_output(true)`, test the variable directly, and set no colours
//! when it is present.
//!
//! Inheriting the palette has a cost the screens have to pay themselves.
//! Setting no colours means ratatui writes no SGR before its content, so a
//! screen is painted in whatever attributes the terminal was already in — and
//! coming back from a session, that is whatever the editor happened to be
//! drawing when the relay stopped. [`Screen::open`] therefore drops the
//! inherited attributes first, which is the one place that can: every screen is
//! taken through it.

pub mod app;
pub mod draw;
pub mod help;
pub mod prompt;
pub mod setup;
#[cfg(test)]
mod test_support;

/// What the picker returned.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Attach to this session.
    Attach {
        session: Session,
        /// The highest session number in the listing the picker was showing.
        /// Carried out rather than re-listed because the prefix machine needs
        /// it to know when a digit can be acted on without waiting, and over
        /// SSH a listing is a script execution.
        highest: u32,
    },
    /// The user quit.
    Quit,
}

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::error::Result;
use crate::session::Session;
use crate::transport::Transport;
use app::{App, Key, Request};

/// A bounded poll rather than an indefinite read, so a resize is not stuck
/// behind an idle keyboard.
const TICK: Duration = Duration::from_millis(250);

/// A ratatui screen that is guaranteed to be given back.
///
/// `ratatui::try_init` enables raw mode and enters the alternate screen, and
/// nothing undoes that on `Drop` — so every `?` between init and restore was a
/// way to leave the user's shell with no echo. This guard owns the terminal
/// for one screen; dropping it restores, whatever path led there. The picker,
/// the prompt, the help and the first-run screen all open through here.
pub(crate) struct Screen {
    terminal: ratatui::DefaultTerminal,
    /// Set by the explicit close so `Drop` does not restore a second time.
    closed: bool,
}

impl Screen {
    /// Take the terminal.
    pub(crate) fn open() -> Result<Self> {
        // Stops crossterm second-guessing us; see the module docs on colour.
        ratatui::crossterm::style::force_color_output(true);

        // Before the alternate screen and before the clear below, because both
        // erase with the *current* background colour, and the screen this takes
        // over from may be a Neovim client stopped mid-frame. See the module
        // docs on colour, and `term::reset_inherited_attributes`.
        crate::term::reset_inherited_attributes();

        let mut screen = Self {
            terminal: ratatui::try_init()?,
            closed: false,
        };
        // A second "enter alternate screen" is a no-op on xterm and kitty, and
        // ratatui's first draw only paints what differs from an empty buffer —
        // so without this the content lands in the middle of the editor's last
        // frame. From here on an error restores through `Drop`.
        screen.terminal.clear()?;
        Ok(screen)
    }

    pub(crate) fn terminal(&mut self) -> &mut ratatui::DefaultTerminal {
        &mut self.terminal
    }

    /// Give the terminal back, reporting a failure to do so.
    pub(crate) fn close(mut self) -> Result<()> {
        self.closed = true;
        ratatui::try_restore()?;
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        if !self.closed {
            // The error path: `restore` reports a failure on stderr and goes
            // on, which is all that can be done while an error is already in
            // flight.
            ratatui::restore();
        }
    }
}

/// Take the terminal for one screen, run `f` on it, and give it back.
///
/// The restore happens before the outcome propagates: an error that leaves the
/// terminal in raw mode with no echo is far worse than the error itself.
pub(crate) fn owning<T>(f: impl FnOnce(&mut ratatui::DefaultTerminal) -> Result<T>) -> Result<T> {
    let mut screen = Screen::open()?;
    let outcome = f(screen.terminal());
    screen.close()?;
    outcome
}

/// Wait up to [`TICK`] for a keypress the screens understand.
///
/// `Ok(None)` means nothing happened and the caller should loop — either the
/// poll timed out or the event was not a key press. Press only: with the kitty
/// protocol pushed by Neovim, a release would otherwise count as a second
/// keypress.
///
/// [`setup`] deliberately does not use this: it needs the raw chord, since
/// [`translate`] discards every control chord but the three the picker binds.
pub(crate) fn poll_key() -> Result<Option<Key>> {
    if !event::poll(TICK)? {
        return Ok(None);
    }
    Ok(match event::read()? {
        Event::Key(k) if k.kind == KeyEventKind::Press => Some(translate(k)),
        _ => None,
    })
}

/// Run the picker until the user attaches or quits. `message` replaces the hint
/// row — how a failed attach reports itself without exiting the program.
/// `focused` is the session the caller came from, if any: the cursor starts on
/// it, so `<prefix> Space` opens the picker where the user already was.
pub fn run(
    transport: &dyn Transport,
    message: Option<String>,
    focused: Option<&str>,
) -> Result<Outcome> {
    owning(|terminal| run_loop(terminal, transport, message, focused))
}

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    transport: &dyn Transport,
    message: Option<String>,
    focused: Option<&str>,
) -> Result<Outcome> {
    let mut sessions = transport.list_sessions()?;
    let mut highest = highest_num(&sessions);
    let mut app = App::new(std::mem::take(&mut sessions));
    if let Some(id) = focused {
        app.select_session(id);
    }
    if let Some(msg) = message {
        app.set_message(msg);
    }

    // When a half-typed session number must be settled. `App` decides every
    // unambiguous digit on its own, so this is only ever set when one number is
    // a prefix of another — sessions 1 and 12 both present.
    let mut deadline: Option<Instant> = None;

    loop {
        terminal.draw(|f| draw::draw(f, &app))?;

        if !event::poll(TICK)? {
            // The clock lives here rather than in `App`, which stays pure —
            // the same split `pty::pump` uses for `keys::Prefix`.
            if deadline.is_some_and(|d| Instant::now() >= d) {
                deadline = None;
                if let Request::Attach(id) = app.resolve_pending() {
                    if let Some(session) = app.session(&id).cloned() {
                        return Ok(Outcome::Attach { session, highest });
                    }
                }
            }
            continue;
        }
        let key = match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => translate(k),
            _ => continue,
        };

        let request = app.on_key(key);
        deadline = app
            .pending()
            .is_some()
            .then(|| Instant::now() + Duration::from_millis(crate::config::get().keys.timeout_ms));

        // Every request names a row the picker is showing, so it is resolved
        // against the list in hand rather than a fresh listing: over SSH each
        // listing is a script run (~230 ms), and the action itself finds out
        // soon enough if the session has gone in the meantime — an attach
        // pings before spawning, a kill reports "absent", a rename reports
        // "not found". The list is re-read only after something changed it.
        match request {
            Request::None => {}
            Request::Quit => return Ok(Outcome::Quit),

            Request::Attach(id) => {
                if let Some(session) = app.session(&id).cloned() {
                    return Ok(Outcome::Attach { session, highest });
                }
            }

            Request::NewSession => {
                match prompt::run_on(terminal, transport, prompt::Task::Create)? {
                    prompt::Outcome::Created(session) => {
                        // The new session is the highest by construction when
                        // it appends, but it may have refilled a gap — so take
                        // the larger of the two rather than assuming.
                        return Ok(Outcome::Attach {
                            highest: highest.max(session.state.num),
                            session,
                        });
                    }
                    prompt::Outcome::Cancelled => {}
                    prompt::Outcome::Renamed => refresh(&mut app, &mut highest, transport)?,
                }
            }

            Request::RenameSession(id) => {
                if let Some(session) = app.session(&id).cloned() {
                    let outcome =
                        prompt::run_on(terminal, transport, prompt::Task::Rename(&session))?;
                    if !matches!(outcome, prompt::Outcome::Cancelled) {
                        refresh(&mut app, &mut highest, transport)?;
                    }
                }
            }

            Request::Kill(id) => {
                if let Some(session) = app.session(&id).cloned() {
                    if let Err(e) = transport.kill_session(&session) {
                        app.set_message(one_line(&e));
                    }
                    refresh(&mut app, &mut highest, transport)?;
                }
            }

            Request::Help => help::run_on(terminal)?,
        }
    }
}

/// Re-list, keeping the highest number in step with what is on screen.
fn refresh(app: &mut App, highest: &mut u32, transport: &dyn Transport) -> Result<()> {
    let sessions = transport.list_sessions()?;
    *highest = highest_num(&sessions);
    app.set_sessions(sessions);
    Ok(())
}

/// The largest resolved session number in a listing, or 0 for none.
pub fn highest_num(sessions: &[Session]) -> u32 {
    sessions.iter().map(|s| s.state.num).max().unwrap_or(0)
}

/// Collapse an error to something that fits on one line.
///
/// The bottom row is exactly one row; a multi-line error would be truncated at
/// the first newline and lose the part that explains itself.
fn one_line(e: &crate::error::NvmuxError) -> String {
    e.to_string().lines().collect::<Vec<_>>().join(" — ")
}

/// Reduce a crossterm event to the keys the screens understand.
///
/// A control chord is either one of the three that are bound (`Ctrl-c`,
/// `Ctrl-n`, `Ctrl-p`) or nothing at all — never the bare letter. The prefix
/// is configurable, and someone who reflexively types `<prefix>` on the picker
/// must not find that `Ctrl-x` opened the kill confirm or `Ctrl-q` quit; nor
/// should a chord typed on the help screen close it and forward its second key.
fn translate(k: KeyEvent) -> Key {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Char('c') if ctrl => Key::CtrlC,
        KeyCode::Char('n') if ctrl => Key::CtrlN,
        KeyCode::Char('p') if ctrl => Key::CtrlP,
        KeyCode::Char(_) if ctrl => Key::Other,
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        _ => Key::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three bound chords are the only ones that survive as chords; a
    /// chord must never arrive as its bare letter (see `translate`).
    #[test]
    fn the_bound_chords_are_distinguished_from_their_plain_letters() {
        for (c, ctrl, want) in [
            ('c', true, Key::CtrlC),
            ('c', false, Key::Char('c')),
            ('n', true, Key::CtrlN),
            ('n', false, Key::Char('n')),
            ('p', true, Key::CtrlP),
            ('p', false, Key::Char('p')),
        ] {
            let mods = if ctrl {
                KeyModifiers::CONTROL
            } else {
                KeyModifiers::NONE
            };
            assert_eq!(
                translate(KeyEvent::new(KeyCode::Char(c), mods)),
                want,
                "{c:?} with ctrl={ctrl}"
            );
        }
    }

    /// Only Ctrl-c, Ctrl-n and Ctrl-p are keys of their own; every other
    /// chord is nothing. The prefix is one of these (Ctrl-Space by default,
    /// but configurable), and `Ctrl-x`, `Ctrl-q`, `Ctrl-r`, `Ctrl-y` all name
    /// picker commands as bare letters.
    ///
    /// The space bar is in the list for the default prefix's sake: typed with
    /// Ctrl it must not reach the filter as a space.
    #[test]
    fn other_control_chords_are_ignored_not_folded_to_letters() {
        for c in ['t', 'x', 'q', 'r', 'y', 'g', 'a', 'd', ' '] {
            assert_eq!(
                translate(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)),
                Key::Other,
                "Ctrl-{c:?} must not act as a plain {c:?}"
            );
        }
    }

    #[test]
    fn shifted_letters_arrive_as_uppercase() {
        // `G` must reach the app as 'G', not as 'g' plus a modifier, or
        // jump-to-last would be indistinguishable from jump-to-first.
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            Key::Char('G')
        );
    }

    /// Why [`Screen::open`] drops the inherited attributes before it draws.
    ///
    /// The screens set no colours on purpose, so ratatui's crossterm backend —
    /// which tracks fg and bg from `Color::Reset` and writes SGR only on a
    /// difference — emits nothing that would clear an attribute until the whole
    /// frame has been painted, and never writes a blank cell at all. Whatever
    /// the terminal was already in is what the screen is drawn in: coming back
    /// from a session, the editor's own colours, mid-frame.
    ///
    /// If ratatui ever leads a frame with a reset of its own this fails and the
    /// call in `Screen::open` can be revisited — the point of asserting it
    /// rather than merely writing it down.
    #[test]
    fn a_screen_paints_before_it_clears_anything() {
        let emitted = test_support::emitted(48, 6, |f| draw::draw(f, &App::new(Vec::new())));
        let content = emitted
            .find("no sessions")
            .expect("the empty-list line is the screen's first content");

        for clear in [
            "\x1b[0m", "\x1b[m", "\x1b[39m", "\x1b[49m", "\x1b[22m", "\x1b[27m",
        ] {
            assert!(
                !emitted[..content].contains(clear),
                "{clear:?} before the first glyph: the screen no longer inherits \
                 the terminal's attributes, so term::reset_inherited_attributes \
                 may be redundant"
            );
        }
        assert!(
            emitted.rfind("\x1b[0m").is_some_and(|at| at > content),
            "the only SGR reset in a frame comes after its content, which is \
             too late to undo anything inherited"
        );
    }

    #[test]
    fn errors_are_flattened_to_one_line() {
        let e = crate::error::NvmuxError::Session(crate::error::SessionError::NotReady {
            name: "x".into(),
            timeout: Duration::from_secs(1),
            log: "x.log".into(),
            log_tail: "a\nmultiline\nthing".into(),
        });
        assert!(
            e.to_string().contains('\n'),
            "the fixture must be multi-line"
        );
        assert!(!one_line(&e).contains('\n'));
    }
}
