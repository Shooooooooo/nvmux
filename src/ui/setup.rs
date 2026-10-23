//! First-run screen: pick a prefix key when there is no config yet.
//!
//! Shown only when no config file exists, `$NVMUX_CONFIG` is unset and the
//! terminal is interactive. Reads keys raw, not through `translate`, which
//! drops every chord it does not bind. Chords are validated by
//! [`crate::keys::parse_prefix`], as in the config file.
//!
//! `Enter` keeps what is shown; `Esc` and `Ctrl-c` skip. `C-c`/`C-z` can
//! still be set as prefixes in the config.

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::draw;
use crate::error::Result;

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The pressed key, or the default on a bare Enter.
    Chosen(u8),
    /// Run on defaults this time and ask again next time.
    Skipped,
}

/// What one keypress meant. Also used by [`super::settings`] to read a prefix
/// chord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Step {
    /// A valid prefix chord, as its control byte.
    Choose(u8),
    /// A chord that cannot be a prefix, with the reason to show.
    Reject(String),
    Confirm,
    /// Esc / Ctrl-c: skip without writing.
    Skip,
    Ignore,
}

/// `ctrl` is whether CONTROL was held on the raw event. The chord is spelled
/// as in the config file for [`crate::keys::parse_prefix`], so space becomes
/// `Space`: `"Ctrl- "` would not parse.
pub(super) fn interpret(code: KeyCode, ctrl: bool) -> Step {
    match code {
        KeyCode::Enter => Step::Confirm,
        KeyCode::Esc => Step::Skip,
        // Reserved for skipping, as on every other nvmux screen.
        KeyCode::Char('c') if ctrl => Step::Skip,
        KeyCode::Char(c) if ctrl => {
            let key = match c {
                ' ' => "Space".to_string(),
                c => c.to_ascii_lowercase().to_string(),
            };
            match crate::keys::parse_prefix(&format!("Ctrl-{key}")) {
                Ok(byte) => Step::Choose(byte),
                Err(message) => Step::Reject(message),
            }
        }
        _ => Step::Ignore,
    }
}

struct State {
    /// The chosen prefix, or `None` while the default is on offer.
    selected: Option<u8>,
    /// A rejection reason or hint, shown under the prefix.
    message: Option<String>,
}

/// Show the screen until the user confirms or skips.
pub fn run() -> Result<Outcome> {
    super::owning(run_loop)
}

fn run_loop(terminal: &mut ratatui::DefaultTerminal) -> Result<Outcome> {
    let mut state = State {
        selected: None,
        message: None,
    };
    loop {
        // No theme pass: reading the theme would read the config, fixing the
        // defaults in place of the file about to be loaded.
        terminal.draw(|f| draw(f, &state))?;

        if !event::poll(super::TICK)? {
            continue;
        }
        // Raw event: `translate` drops every chord but the three it binds.
        let (code, ctrl) = match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => {
                (k.code, k.modifiers.contains(KeyModifiers::CONTROL))
            }
            _ => continue,
        };

        match interpret(code, ctrl) {
            Step::Choose(byte) => {
                state.selected = Some(byte);
                state.message = None;
            }
            Step::Reject(message) => state.message = Some(message),
            Step::Confirm => {
                return Ok(Outcome::Chosen(
                    state.selected.unwrap_or(crate::keys::PREFIX),
                ))
            }
            Step::Skip => return Ok(Outcome::Skipped),
            Step::Ignore => {
                state.message = Some("press a Ctrl key, e.g. Ctrl-a or Ctrl-Space".to_string())
            }
        }
    }
}

fn draw(frame: &mut Frame, state: &State) {
    let hint = if state.selected.is_some() {
        "⏎ confirm   esc skip"
    } else {
        "⏎ keep   esc skip"
    };
    draw::screen(frame, hint, true, |frame, body| {
        draw_body(frame, state, body)
    });
}

fn draw_body(frame: &mut Frame, state: &State, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);

    let label = crate::keys::prefix_label(state.selected.unwrap_or(crate::keys::PREFIX));
    // Dim while the default is shown.
    let prefix_style = if state.selected.is_some() { bold } else { dim };

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled("Welcome to nvmux", bold)),
        Line::from(""),
        Line::from("Press the key you'd like as your prefix,"),
        Line::from("or Enter to keep the default."),
        Line::from(""),
        Line::from(vec![
            Span::raw("prefix: "),
            Span::styled(label, prefix_style),
        ]),
    ];
    if let Some(message) = &state.message {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(message.clone(), dim)));
    }

    let width = lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
    let block = draw::centre(area, width, lines.len() as u16);
    frame.render_widget(Paragraph::new(lines), block);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ctrl_letter_is_chosen_as_its_control_byte() {
        assert_eq!(interpret(KeyCode::Char('a'), true), Step::Choose(0x01));
        assert_eq!(interpret(KeyCode::Char('t'), true), Step::Choose(0x14));
        // Case-insensitive, like the config parser.
        assert_eq!(interpret(KeyCode::Char('A'), true), Step::Choose(0x01));
    }

    /// crossterm reports Ctrl-Space (from `NUL` or either keyboard protocol)
    /// as a space with CONTROL; it must not reach the parser as `"Ctrl- "`.
    #[test]
    fn ctrl_space_is_chosen_as_the_default_prefix() {
        assert_eq!(
            interpret(KeyCode::Char(' '), true),
            Step::Choose(crate::keys::PREFIX)
        );
        assert_eq!(interpret(KeyCode::Char(' '), false), Step::Ignore);
    }

    #[test]
    fn a_terminal_breaking_chord_is_rejected_with_a_reason() {
        // C-m is Enter on the wire; parse_prefix refuses it, and so do we.
        assert!(matches!(
            interpret(KeyCode::Char('m'), true),
            Step::Reject(_)
        ));
    }

    #[test]
    fn enter_confirms_and_esc_skips() {
        assert_eq!(interpret(KeyCode::Enter, false), Step::Confirm);
        assert_eq!(interpret(KeyCode::Esc, false), Step::Skip);
    }

    #[test]
    fn ctrl_c_skips_rather_than_selecting_itself() {
        assert_eq!(interpret(KeyCode::Char('c'), true), Step::Skip);
    }

    #[test]
    fn a_plain_key_is_ignored() {
        assert_eq!(interpret(KeyCode::Char('a'), false), Step::Ignore);
        assert_eq!(interpret(KeyCode::Up, false), Step::Ignore);
    }
}
