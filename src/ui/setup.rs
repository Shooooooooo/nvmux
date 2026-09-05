//! The first-run screen — pick a prefix key once, when there is no config yet.
//!
//! Shown by the binary only when no config file exists, `$NVMUX_CONFIG` is
//! unset, and the terminal is interactive (see the guards there). It takes the
//! whole screen the way [`crate::ui::help`] does, but reads keys *raw*: the user
//! presses the actual `Ctrl-<letter>` chord they want, so this is the one place
//! that must **not** go through `translate`, which discards every chord it does
//! not bind (see the note in [`crate::ui`]). The pressed chord is validated
//! by the same [`crate::keys::parse_prefix`] the config file uses, so the rules
//! and the messages are shared.
//!
//! `Enter` keeps whatever is shown (the default until a key is pressed); `Esc`
//! and `Ctrl-c` skip. `Ctrl-c` is reserved for skipping, as everywhere else in
//! nvmux — `C-c`/`C-z` are still selectable as prefixes by editing the config.

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::draw;
use crate::error::Result;

/// What the first-run screen returned.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A prefix was confirmed (the pressed key, or the default on a bare Enter).
    Chosen(u8),
    /// The user skipped: run on defaults this time, ask again next time.
    Skipped,
}

/// What one keypress meant. Split out so the decision is testable without a
/// terminal.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// A valid prefix chord: this control byte.
    Choose(u8),
    /// A chord that cannot be a prefix; carries the reason to show.
    Reject(String),
    /// Enter: confirm what is shown.
    Confirm,
    /// Esc / Ctrl-c: skip without writing.
    Skip,
    /// Anything else: nudge the user toward a Ctrl-chord.
    Ignore,
}

/// Decide what a keypress means. `ctrl` is whether the CONTROL modifier was held
/// on the raw event.
fn interpret(code: KeyCode, ctrl: bool) -> Step {
    match code {
        KeyCode::Enter => Step::Confirm,
        KeyCode::Esc => Step::Skip,
        // Reserved for skipping, like Ctrl-c on every other nvmux screen.
        KeyCode::Char('c') if ctrl => Step::Skip,
        // The one validator, so the screen and the config file agree.
        KeyCode::Char(c) if ctrl => {
            match crate::keys::parse_prefix(&format!("Ctrl-{}", c.to_ascii_lowercase())) {
                Ok(byte) => Step::Choose(byte),
                Err(message) => Step::Reject(message),
            }
        }
        _ => Step::Ignore,
    }
}

/// What the screen is showing between keystrokes.
struct State {
    /// The chosen prefix, or `None` while the default is on offer.
    selected: Option<u8>,
    /// A rejection reason or a nudge, shown under the prefix.
    message: Option<String>,
}

/// Show the screen until the user confirms or skips. Owns the terminal, like
/// [`crate::ui::help::run`]; no fade, because nothing has faded to black yet.
pub fn run() -> Result<Outcome> {
    let mut screen = super::Screen::open(false)?;
    let outcome = run_loop(screen.terminal());
    // Restore before propagating: see `ui::Screen`.
    screen.close()?;
    outcome
}

fn run_loop(terminal: &mut ratatui::DefaultTerminal) -> Result<Outcome> {
    let mut state = State {
        selected: None,
        message: None,
    };
    loop {
        terminal.draw(|f| draw(f, &state))?;

        if !event::poll(super::TICK)? {
            continue;
        }
        // Read the RAW event — `translate` discards every chord but the three
        // it binds, and the whole point here is to see which chord was pressed.
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
                state.message = Some("press a Ctrl-<letter> key, e.g. Ctrl-a".to_string())
            }
        }
    }
}

fn draw(frame: &mut Frame, state: &State) {
    let area = frame.area();
    if area.height == 0 || area.width == 0 {
        return;
    }
    // Same split as the picker/help: body above, one dim hint row at the bottom.
    let body = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    let bottom = Rect {
        y: area.y + area.height - 1,
        height: 1,
        ..area
    };

    draw_body(frame, state, body);
    draw_hints(frame, state, bottom);
}

fn draw_body(frame: &mut Frame, state: &State, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);

    let label = crate::keys::prefix_label(state.selected.unwrap_or(crate::keys::PREFIX));
    // Bold once the user has actually pressed a key; dim while it is the default.
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

fn draw_hints(frame: &mut Frame, state: &State, area: Rect) {
    let hint = if state.selected.is_some() {
        "⏎ confirm   esc skip"
    } else {
        "⏎ keep   esc skip"
    };
    let text = draw::truncate(hint, area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().add_modifier(Modifier::DIM),
        )))
        .alignment(Alignment::Center),
        area,
    );
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

    /// Confirming without pressing anything keeps the standard prefix.
    #[test]
    fn a_bare_confirm_maps_to_the_default_at_the_call_site() {
        // `run_loop` maps Confirm with no selection to `crate::keys::PREFIX`.
        let state = State {
            selected: None,
            message: None,
        };
        assert_eq!(
            state.selected.unwrap_or(crate::keys::PREFIX),
            crate::keys::PREFIX
        );
    }
}
