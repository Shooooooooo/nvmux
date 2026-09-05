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
    /// Set by the explicit closes so `Drop` does not restore a second time.
    closed: bool,
}

impl Screen {
    /// Take the terminal. `prime` paints a first black frame before anything
    /// else happens, for a screen that is entered from black.
    pub(crate) fn open(prime: bool) -> Result<Self> {
        // Stops crossterm second-guessing us; see the module docs on colour.
        ratatui::crossterm::style::force_color_output(true);

        let mut screen = Self {
            terminal: ratatui::try_init()?,
            closed: false,
        };
        // A second "enter alternate screen" is a no-op on xterm and kitty, and
        // ratatui's first draw only paints what differs from an empty buffer —
        // so without this the content lands in the middle of the editor's last
        // frame. From here on an error restores through `Drop`.
        screen.terminal.clear()?;
        if prime {
            // Before the slow work (a session listing) that precedes the first
            // real draw, so entering the alternate screen does not flash its
            // blank buffer.
            crate::fade::prime_black(&mut screen.terminal)?;
        }
        Ok(screen)
    }

    pub(crate) fn terminal(&mut self) -> &mut ratatui::DefaultTerminal {
        &mut self.terminal
    }

    /// Give the terminal back, reporting a failure to do so.
    pub(crate) fn close(mut self) -> Result<()> {
        self.closed = true;
        ratatui::try_restore()?;
        crate::fade::show_cursor_if_enabled();
        Ok(())
    }

    /// Give the terminal back as a black primary screen, for the attach path:
    /// the client spawn that follows then never flashes the old primary.
    pub(crate) fn close_to_black(mut self) -> Result<()> {
        self.closed = true;
        crate::fade::leave_ratatui_to_black()?;
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        if !self.closed {
            // The error path: `restore` reports a failure on stderr and goes
            // on, which is all that can be done while an error is already in
            // flight. Show the cursor too, in case a fade had hidden it.
            ratatui::restore();
            crate::fade::show_cursor_if_enabled();
        }
    }
}

/// Run the picker until the user attaches or quits. `message` replaces the hint
/// row — how a failed attach reports itself without exiting the program.
pub fn run(transport: &dyn Transport, message: Option<String>) -> Result<Outcome> {
    let mut screen = Screen::open(true)?;
    let outcome = run_loop(screen.terminal(), transport, message);
    // Restore before propagating anything: an error that leaves the terminal in
    // raw mode with no echo is far worse than the error itself. On the attach
    // path, leave straight into a black primary screen so the client spawn that
    // follows never flashes the old primary; otherwise restore as before.
    match &outcome {
        Ok(Outcome::Attach { .. }) => screen.close_to_black()?,
        _ => screen.close()?,
    }
    outcome
}

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    transport: &dyn Transport,
    message: Option<String>,
) -> Result<Outcome> {
    let mut sessions = transport.list_sessions()?;
    let mut highest = highest_num(&sessions);
    let mut app = App::new(std::mem::take(&mut sessions));
    if let Some(msg) = message {
        app.set_message(msg);
    }

    // Dissolve the picker up from the primed black before taking any input.
    crate::fade::fade_in_ratatui(terminal, |f| draw::draw(f, &app))?;

    // When a half-typed session number must be settled. `App` decides every
    // unambiguous digit on its own, so this is only ever set when one number is
    // a prefix of another — sessions 1 and 12 both present.
    let mut deadline: Option<Instant> = None;

    let outcome = 'ui: loop {
        terminal.draw(|f| draw::draw(f, &app))?;

        if !event::poll(TICK)? {
            // The clock lives here rather than in `App`, which stays pure —
            // the same split `pty::pump` uses for `keys::Prefix`.
            if deadline.is_some_and(|d| Instant::now() >= d) {
                deadline = None;
                if let Request::Attach(id) = app.resolve_pending() {
                    if let Some(session) = app.session(&id).cloned() {
                        break 'ui Outcome::Attach { session, highest };
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
        deadline = app.pending().is_some().then(|| {
            Instant::now() + Duration::from_millis(crate::settings::get().keys.timeout_ms)
        });

        // Every request names a row the picker is showing, so it is resolved
        // against the list in hand rather than a fresh listing: over SSH each
        // listing is a script run (~230 ms), and the action itself finds out
        // soon enough if the session has gone in the meantime — an attach
        // pings before spawning, a kill reports "absent", a rename reports
        // "not found". The list is re-read only after something changed it.
        match request {
            Request::None => {}
            Request::Quit => break 'ui Outcome::Quit,

            Request::Attach(id) => {
                if let Some(session) = app.session(&id).cloned() {
                    break 'ui Outcome::Attach { session, highest };
                }
            }

            Request::NewSession => {
                match prompt::run_on(terminal, transport, prompt::Task::Create, false)? {
                    prompt::Outcome::Created(session) => {
                        // The new session is the highest by construction when
                        // it appends, but it may have refilled a gap — so take
                        // the larger of the two rather than assuming.
                        break 'ui Outcome::Attach {
                            highest: highest.max(session.state.num),
                            session,
                        };
                    }
                    prompt::Outcome::Cancelled => {}
                    prompt::Outcome::Renamed => refresh(&mut app, &mut highest, transport)?,
                }
            }

            Request::RenameSession(id) => {
                if let Some(session) = app.session(&id).cloned() {
                    let outcome =
                        prompt::run_on(terminal, transport, prompt::Task::Rename(&session), false)?;
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
        }
    };

    // Dissolve the picker out to black before handing off to the session; on
    // quit there is no next screen to bridge to, so leave it be.
    if matches!(outcome, Outcome::Attach { .. }) {
        crate::fade::fade_out_ratatui(terminal, |f| draw::draw(f, &app))?;
    }
    Ok(outcome)
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
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        _ => Key::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_is_distinguished_from_a_plain_c() {
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Key::CtrlC
        );
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Key::Char('c')
        );
    }

    /// Only Ctrl-c, Ctrl-n and Ctrl-p are keys of their own; every other
    /// chord is nothing. The prefix is one of these (Ctrl-t by default, but
    /// configurable), and `Ctrl-x`, `Ctrl-q`, `Ctrl-r`, `Ctrl-y` all name
    /// picker commands as bare letters.
    #[test]
    fn other_control_chords_are_ignored_not_folded_to_letters() {
        for c in ['t', 'x', 'q', 'r', 'y', 'g', 'a', 'd'] {
            assert_eq!(
                translate(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)),
                Key::Other,
                "Ctrl-{c} must not act as a plain {c}"
            );
        }
    }

    #[test]
    fn ctrl_n_and_ctrl_p_are_distinguished_from_plain_letters() {
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            Key::CtrlN
        );
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            Key::CtrlP
        );
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            Key::Char('n')
        );
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
