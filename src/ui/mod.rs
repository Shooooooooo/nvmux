//! The picker. Milestone 3.
//!
//! # The contract
//!
//! The whole screen is a centered list of session names and one dimmed line of
//! keybind hints on the last row. No borders, no title bar, no status header, no
//! logo, no metadata columns, no help popup. Prompts (create, rename, kill
//! confirm, filter) replace the hint line *in place* rather than opening a modal
//! or a bordered popup.
//!
//! # There is no preview pane, and there must never be one
//!
//! Beyond wanting a clean screen, there is a hard technical reason. Neovim sizes
//! the global grid to the per-dimension **minimum** across every attached UI, so
//! attaching a small second UI to render a preview would shrink the grid of the
//! session the user is actually editing in, and fire `VimResized`:
//!
//! ```text
//!   src/nvim/ui.c, ui_refresh(), v0.11.4:
//!       int width = INT_MAX;
//!       int height = INT_MAX;
//!       for (size_t i = 0; i < ui_count; i++) {
//!         RemoteUI *ui = uis[i];
//!         width = MIN(ui->width, width);
//!         height = MIN(ui->height, height);
//!       }
//!       screen_resize(width, height);
//! ```
//!
//! `ui_refresh()` runs unconditionally at the end of `ui_attach_impl()`.
//! Confirmed empirically: attaching a 40x10 UI alongside a 120x40 one collapses
//! the grid to 40x10 for both.
//!
//! There is no read-only or observer attach mode that opts out of the size
//! calculation. None of the `ui_options` (`rgb`, `ext_cmdline`, `ext_popupmenu`,
//! `ext_tabline`, `ext_wildmenu`, `ext_messages`, `ext_linegrid`,
//! `ext_multigrid`, `ext_hlstate`, `ext_termcolors`) makes an attachment
//! non-sizing, and the `override` flag affects only ext_widgets, never width or
//! height.
//!
//! So: a second UI is never attached to a live session. Anything the picker
//! needs to know about a session comes from plain RPC instead.
//!
//! # Colour
//!
//! Use the terminal's default background; set no background colour and hardcode
//! no palette. Respect `NO_COLOR`.
//!
//! Do **not** rely on crossterm's own `NO_COLOR` handling: with `NO_COLOR` set it
//! turns `SetForegroundColor(c)` into a bare `ESC[m`, which is a full SGR reset
//! that wipes bold, reverse and dim mid-line. Call
//! `crossterm::style::force_color_output(true)` once, test the variable
//! directly, and simply do not set colours when it is present.
//! `Modifier::REVERSED` alone makes a good `NO_COLOR`-safe selection highlight.

pub mod app;
pub mod draw;

/// What the picker returned.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Attach to this session.
    Attach(Session),
    /// The user quit.
    Quit,
}

use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::error::Result;
use crate::session::Session;
use crate::transport::Transport;
use app::{App, Key, Request};

/// How long to block waiting for input before looping.
///
/// A bounded poll rather than an indefinite read so that a resize or a future
/// background refresh is not stuck behind an idle keyboard.
const TICK: Duration = Duration::from_millis(250);

/// Run the picker until the user attaches or quits.
pub fn run(transport: &dyn Transport) -> Result<Outcome> {
    // Colour decisions are made per-cell by `draw`, which never sets one. This
    // stops crossterm second-guessing us: its own NO_COLOR handling rewrites
    // SetForegroundColor into a bare `ESC[m`, a full SGR reset that would wipe
    // the bold/reverse/dim the picker relies on.
    ratatui::crossterm::style::force_color_output(true);

    let mut terminal = ratatui::try_init()?;
    let outcome = run_loop(&mut terminal, transport);
    // Restore before propagating anything: an error that leaves the terminal in
    // raw mode with no echo is far worse than the error itself.
    ratatui::try_restore()?;
    outcome
}

fn run_loop(terminal: &mut ratatui::DefaultTerminal, transport: &dyn Transport) -> Result<Outcome> {
    let mut app = App::new(transport.list_sessions()?);

    loop {
        terminal.draw(|f| draw::draw(f, &app))?;

        if !event::poll(TICK)? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => translate(k),
            // A resize just redraws on the next pass.
            _ => continue,
        };

        match app.on_key(key) {
            Request::None => {}
            Request::Quit => return Ok(Outcome::Quit),

            Request::Attach(id) => {
                if let Some(session) = find(transport, &id)? {
                    return Ok(Outcome::Attach(session));
                }
                app.set_message("that session is gone");
                app.set_sessions(transport.list_sessions()?);
            }

            Request::Create(name) => match transport.create_session(&name) {
                // Creating attaches straight away, as specified.
                Ok(session) => return Ok(Outcome::Attach(session)),
                Err(e) => {
                    app.set_message(one_line(&e));
                    app.set_sessions(transport.list_sessions()?);
                }
            },

            Request::Rename { id, name } => {
                if let Some(session) = find(transport, &id)? {
                    if let Err(e) = transport.rename_session(&session, &name) {
                        app.set_message(one_line(&e));
                    }
                }
                app.set_sessions(transport.list_sessions()?);
            }

            Request::Kill(id) => {
                if let Some(session) = find(transport, &id)? {
                    if let Err(e) = transport.kill_session(&session) {
                        app.set_message(one_line(&e));
                    }
                }
                app.set_sessions(transport.list_sessions()?);
            }
        }
    }
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
