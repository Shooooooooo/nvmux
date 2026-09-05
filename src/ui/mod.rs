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

/// Run the picker until the user attaches or quits. `message` replaces the hint
/// row — how a failed attach reports itself without exiting the program.
pub fn run(transport: &dyn Transport, message: Option<String>) -> Result<Outcome> {
    // Stops crossterm second-guessing us; see the module docs on colour.
    ratatui::crossterm::style::force_color_output(true);

    let mut terminal = ratatui::try_init()?;
    // A second "enter alternate screen" is a no-op on xterm and kitty, and
    // ratatui's first draw only paints what differs from an empty buffer — so
    // without this the list lands in the middle of the editor's last frame.
    terminal.clear()?;
    // Start black, before the session listing that precedes the first real draw,
    // so entering the alternate screen does not flash its blank buffer.
    crate::fade::prime_black(&mut terminal)?;
    let outcome = run_loop(&mut terminal, transport, message);
    // Restore before propagating anything: an error that leaves the terminal in
    // raw mode with no echo is far worse than the error itself. On the attach
    // path, leave straight into a black primary screen so the client spawn that
    // follows never flashes the old primary; otherwise restore as before.
    match &outcome {
        Ok(Outcome::Attach { .. }) => crate::fade::leave_ratatui_to_black()?,
        _ => {
            ratatui::try_restore()?;
            crate::fade::show_cursor_if_enabled();
        }
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
                    match find(transport, &id)? {
                        Some(session) => break 'ui Outcome::Attach { session, highest },
                        None => {
                            app.set_message("that session is gone");
                            refresh(&mut app, &mut highest, transport)?;
                        }
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

        match request {
            Request::None => {}
            Request::Quit => break 'ui Outcome::Quit,

            Request::Attach(id) => {
                if let Some(session) = find(transport, &id)? {
                    break 'ui Outcome::Attach { session, highest };
                }
                app.set_message("that session is gone");
                refresh(&mut app, &mut highest, transport)?;
            }

            Request::NewSession => {
                if let prompt::Outcome::Created(session) =
                    prompt::run_on(terminal, transport, prompt::Task::Create, false)?
                {
                    // The new session is the highest by construction when it
                    // appends, but it may have refilled a gap — so take the
                    // larger of the two rather than assuming.
                    break 'ui Outcome::Attach {
                        highest: highest.max(session.state.num),
                        session,
                    };
                }
                refresh(&mut app, &mut highest, transport)?;
            }

            Request::RenameSession(id) => {
                if let Some(session) = find(transport, &id)? {
                    prompt::run_on(terminal, transport, prompt::Task::Rename(&session), false)?;
                }
                refresh(&mut app, &mut highest, transport)?;
            }

            Request::Kill(id) => {
                if let Some(session) = find(transport, &id)? {
                    if let Err(e) = transport.kill_session(&session) {
                        app.set_message(one_line(&e));
                    }
                }
                refresh(&mut app, &mut highest, transport)?;
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

fn find(transport: &dyn Transport, id: &str) -> Result<Option<Session>> {
    Ok(transport.list_sessions()?.into_iter().find(|s| s.id == id))
}

/// Collapse an error to something that fits on one line.
///
/// The bottom row is exactly one row; a multi-line error would be truncated at
/// the first newline and lose the part that explains itself.
fn one_line(e: &crate::error::NvmuxError) -> String {
    e.to_string().lines().collect::<Vec<_>>().join(" — ")
}

fn translate(k: KeyEvent) -> Key {
    match k.code {
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => Key::CtrlC,
        KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => Key::CtrlN,
        KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => Key::CtrlP,
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

    /// Ctrl-t gets no key of its own; only Ctrl-c, Ctrl-n and Ctrl-p do.
    /// `help::on_key` relies on this: `Ctrl-t` typed on the help screen has
    /// to look like a plain `t`, which that screen ignores, or a chord typed
    /// there would be half-forwarded.
    #[test]
    fn ctrl_t_arrives_as_a_plain_t() {
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            Key::Char('t')
        );
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
        let e = crate::error::NvmuxError::Unimplemented("a\nmultiline\nthing");
        assert!(!one_line(&e).contains('\n'));
    }
}
