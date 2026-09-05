//! The new-session prompt — a second screen, shown by `Ctrl-t c`.
//!
//! The picker's own create prompt replaces the hint line in place, per the
//! contract in the parent module. This one owns the whole screen instead,
//! because what it covers is a live session rather than a list: there is no
//! hint line to borrow and nothing on screen that should stay visible. It
//! keeps the picker's visual language all the same — content centred, one dim
//! hint row on the last line, no borders, and no colour ever set.
//!
//! # The default name is a placeholder, not a pre-filled value
//!
//! Enter on an empty field creates `session 1`, `session 2`, … — the naming
//! `Ctrl-t c` used to apply silently. That default is shown dimmed *inside* the
//! field rather than named in the hint row, so it reads as what enter will do
//! right now and costs nothing to discard. A pre-filled value would have to be
//! backspaced away before a real name could be typed, which is the whole reason
//! placeholders exist.

use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::app::Key;
use super::draw;
use crate::error::Result;
use crate::session::Session;
use crate::transport::Transport;

/// What the prompt returned.
#[derive(Debug)]
pub enum Outcome {
    /// The session that was created. The caller attaches to it.
    Created(Session),
    /// The user backed out; whatever they came from is still running.
    Cancelled,
}

/// What one keypress meant.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    None,
    Submit(String),
    Cancel,
}

/// The label above the field.
const LABEL: &str = "new session";

/// Three spaces between groups, matching the picker's hint line.
const HINTS: &str = "⏎ create   esc cancel";

/// What the user has typed, and what enter would do if they typed nothing.
///
/// Split from the terminal so the whole state machine is testable without one,
/// the same way `App` is split from `draw`.
struct Prompt {
    input: String,
    default_name: String,
    message: Option<String>,
}

impl Prompt {
    fn new(default_name: String) -> Self {
        Self {
            input: String::new(),
            default_name,
            message: None,
        }
    }

    /// Report a rejected name and refresh the default.
    ///
    /// What was typed is deliberately kept: a duplicate name is usually one
    /// character away from a good one, and retyping it is not a punishment the
    /// user earned.
    fn fail(&mut self, message: String, default_name: String) {
        self.message = Some(message);
        self.default_name = default_name;
    }

    fn on_key(&mut self, key: Key) -> Step {
        // Any keypress clears a stale message, so it never lingers over an
        // unrelated action.
        self.message = None;

        match key {
            Key::Char(c) => {
                self.input.push(c);
                Step::None
            }
            Key::Backspace => {
                self.input.pop();
                Step::None
            }
            Key::Enter => {
                let text = self.input.trim();
                Step::Submit(if text.is_empty() {
                    self.default_name.clone()
                } else {
                    text.to_string()
                })
            }
            // Ctrl-C quits from the picker, but here it would tear the user out
            // of a live session they only meant to leave a prompt in.
            Key::Esc | Key::CtrlC => Step::Cancel,
            _ => Step::None,
        }
    }
}

/// Ask for a name, then create the session.
///
/// Runs its own terminal, entering and leaving the alternate screen exactly as
/// the picker does — which is what hides the session underneath and puts it
/// back afterwards.
pub fn run(transport: &dyn Transport) -> Result<Outcome> {
    // Same reasoning as the picker: `draw` never sets a colour, and crossterm's
    // own NO_COLOR handling would rewrite one into a full SGR reset that wipes
    // the dim this screen relies on.
    ratatui::crossterm::style::force_color_output(true);

    let mut terminal = ratatui::try_init()?;
    let outcome = run_loop(&mut terminal, transport);
    // Restore before propagating anything: an error that leaves the terminal in
    // raw mode with no echo is far worse than the error itself.
    ratatui::try_restore()?;
    outcome
}

fn run_loop(terminal: &mut ratatui::DefaultTerminal, transport: &dyn Transport) -> Result<Outcome> {
    let mut prompt = Prompt::new(next_free_name(transport)?);

    loop {
        terminal.draw(|f| draw(f, &prompt))?;

        if !event::poll(super::TICK)? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => super::translate(k),
            // A resize just redraws on the next pass.
            _ => continue,
        };

        match prompt.on_key(key) {
            Step::None => {}
            Step::Cancel => return Ok(Outcome::Cancelled),
            // Names are not validated here. `session::validate_name` and the
            // transport's duplicate check are the rule; duplicating either one
            // in the UI is how the two drift apart.
            Step::Submit(name) => match transport.create_session(&name) {
                Ok(session) => return Ok(Outcome::Created(session)),
                Err(e) => {
                    let refreshed = next_free_name(transport)?;
                    prompt.fail(super::one_line(&e), refreshed);
                }
            },
        }
    }
}

/// The name a session gets when the user just presses enter.
///
/// Pure, so the numbering rule is testable without a transport. Compared
/// case-insensitively because that is how the transports reject duplicates —
/// offering a default that create would then refuse is worse than no default.
fn next_name(taken: &[String]) -> String {
    for n in 1..1000 {
        let candidate = format!("session {n}");
        if !taken.iter().any(|t| t.eq_ignore_ascii_case(&candidate)) {
            return candidate;
        }
    }
    "session".to_string()
}

fn next_free_name(transport: &dyn Transport) -> Result<String> {
    let taken: Vec<String> = transport
        .list_sessions()?
        .into_iter()
        .map(|s| s.name)
        .collect();
    Ok(next_name(&taken))
}

fn draw(frame: &mut Frame, prompt: &Prompt) {
    let area = frame.area();
    if area.height == 0 || area.width == 0 {
        return;
    }

    // The last row is the hint line; everything above it is the prompt. Same
    // split as the picker, so the two screens line up.
    let body = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    let bottom = Rect {
        y: area.y + area.height - 1,
        height: 1,
        ..area
    };

    draw_body(frame, prompt, body);
    draw_hints(frame, bottom);
}

/// The label is the anchor and the field grows rightward from its left edge.
///
/// Centring the field on its own contents would be truer to "the middle of the
/// screen", but it would shuffle the line half a cell left on every keystroke.
/// The label is the one piece of content whose width never changes, so centring
/// *that* and hanging the field off it puts the prompt in the middle and still
/// leaves the cursor sitting still while you type.
fn draw_body(frame: &mut Frame, prompt: &Prompt, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let height = if prompt.message.is_some() { 3 } else { 2 };
    let anchor = draw::centre(area, LABEL.width() as u16, height);
    // Everything from the anchor to the right edge is the field's, so a long
    // name has somewhere to go.
    let width = (area.x + area.width).saturating_sub(anchor.x) as usize;

    if anchor.height >= 1 {
        let dim = Style::default().add_modifier(Modifier::DIM);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                draw::truncate(LABEL, anchor.width as usize),
                dim,
            ))),
            Rect {
                height: 1,
                ..anchor
            },
        );
    }

    if anchor.height >= 2 {
        frame.render_widget(
            Paragraph::new(input_line(prompt, width)),
            Rect {
                x: anchor.x,
                y: anchor.y + 1,
                width: width as u16,
                height: 1,
            },
        );
    }

    // The message is routinely wider than the field, so it gets the full width
    // and its own centring rather than hanging off the anchor.
    if anchor.height >= 3 {
        if let Some(msg) = prompt.message.as_deref() {
            frame.render_widget(
                Paragraph::new(Line::from(draw::truncate(msg, area.width as usize)))
                    .alignment(Alignment::Center),
                Rect {
                    y: anchor.y + 2,
                    height: 1,
                    ..area
                },
            );
        }
    }
}

/// The field: either what was typed, or the dim default behind the cursor.
///
/// Which of the two is showing is derived from the input being empty rather
/// than tracked, so backspacing back to nothing brings the placeholder back
/// with no state to keep in sync.
fn input_line(prompt: &Prompt, width: usize) -> Line<'static> {
    let room = width.saturating_sub(draw::CURSOR.width());

    if prompt.input.is_empty() {
        Line::from(vec![
            Span::raw(draw::CURSOR),
            Span::styled(
                draw::truncate(&prompt.default_name, room),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])
    } else {
        Line::from(Span::raw(format!(
            "{}{}",
            tail(&prompt.input, room),
            draw::CURSOR
        )))
    }
}

fn draw_hints(frame: &mut Frame, area: Rect) {
    let text = draw::truncate(HINTS, area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().add_modifier(Modifier::DIM),
        )))
        .alignment(Alignment::Center),
        area,
    );
}

/// Keep the *end* of `s` within `max` display columns.
///
/// The opposite choice from `draw::truncate`, and for a reason: a field has to
/// keep the cursor visible, so when a name outgrows it the start scrolls away
/// rather than the part being typed.
fn tail(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut w = 0;
    for c in s.chars().rev() {
        let cw = c.to_string().width();
        if w + cw > max {
            break;
        }
        kept.push(c);
        w += cw;
    }
    kept.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn prompt() -> Prompt {
        Prompt::new("session 3".to_string())
    }

    fn type_in(p: &mut Prompt, text: &str) {
        for c in text.chars() {
            p.on_key(Key::Char(c));
        }
    }

    /// Reconstruct the rendered lines, skipping the filler cell that follows a
    /// wide glyph so a CJK name is not reported as three columns per character.
    fn render(p: &Prompt, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        terminal.draw(|f| draw(f, p)).expect("draw");
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                let mut line = String::new();
                let mut skip = 0u16;
                for x in 0..buf.area.width {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    let sym = buf[(x, y)].symbol();
                    skip = sym.width().saturating_sub(1) as u16;
                    line.push_str(sym);
                }
                line.trim_end().to_string()
            })
            .collect()
    }

    /// The field row as (symbol, is_dim) per cell, trimmed to the part that has
    /// content. Found by the cursor, which only the field draws.
    fn field_cells(p: &Prompt, w: u16, h: u16) -> Vec<(String, bool)> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        terminal.draw(|f| draw(f, p)).expect("draw");
        let buf = terminal.backend().buffer().clone();

        let y = (0..buf.area.height)
            .find(|&y| (0..buf.area.width).any(|x| buf[(x, y)].symbol() == draw::CURSOR))
            .expect("the field row should be the one holding the cursor");

        let row: Vec<(String, bool)> = (0..buf.area.width)
            .map(|x| {
                let cell = &buf[(x, y)];
                (
                    cell.symbol().to_string(),
                    cell.modifier.contains(Modifier::DIM),
                )
            })
            .collect();

        let first = row.iter().position(|(s, _)| s != " ").expect("content");
        let last = row.iter().rposition(|(s, _)| s != " ").expect("content");
        row[first..=last].to_vec()
    }

    fn text_of(cells: &[(String, bool)]) -> String {
        cells.iter().map(|(s, _)| s.as_str()).collect()
    }

    #[test]
    fn typing_appends_and_backspace_removes() {
        let mut p = prompt();
        type_in(&mut p, "notes");
        assert_eq!(p.input, "notes");
        p.on_key(Key::Backspace);
        assert_eq!(p.input, "note");
    }

    #[test]
    fn backspace_on_an_empty_field_is_harmless() {
        let mut p = prompt();
        for _ in 0..5 {
            assert_eq!(p.on_key(Key::Backspace), Step::None);
        }
        assert_eq!(p.input, "");
    }

    #[test]
    fn enter_submits_the_trimmed_text() {
        let mut p = prompt();
        type_in(&mut p, "  my-project  ");
        assert_eq!(
            p.on_key(Key::Enter),
            Step::Submit("my-project".to_string()),
            "surrounding whitespace is not part of the name"
        );
    }

    /// The zero-keystroke path `Ctrl-t c` used to be has to survive: enter on an
    /// empty field means "whatever the placeholder is showing".
    #[test]
    fn enter_on_an_empty_field_submits_the_default() {
        let mut p = prompt();
        assert_eq!(p.on_key(Key::Enter), Step::Submit("session 3".to_string()));
    }

    #[test]
    fn a_whitespace_only_field_counts_as_empty() {
        let mut p = prompt();
        type_in(&mut p, "   ");
        assert_eq!(p.on_key(Key::Enter), Step::Submit("session 3".to_string()));
    }

    #[test]
    fn esc_cancels() {
        let mut p = prompt();
        type_in(&mut p, "notes");
        assert_eq!(p.on_key(Key::Esc), Step::Cancel);
    }

    /// Ctrl-C quits the picker. Doing that here would exit nvmux out from under
    /// a session the user is still in, so it cancels like esc instead.
    #[test]
    fn ctrl_c_cancels_rather_than_quitting() {
        let mut p = prompt();
        assert_eq!(p.on_key(Key::CtrlC), Step::Cancel);
    }

    #[test]
    fn a_rejected_name_is_kept_so_it_can_be_edited() {
        let mut p = prompt();
        type_in(&mut p, "notes");
        p.fail(
            "a session named \"notes\" already exists".to_string(),
            "session 4".to_string(),
        );
        assert_eq!(p.input, "notes", "the typed name must survive a rejection");
        assert_eq!(p.default_name, "session 4");
        assert!(p.message.is_some());
    }

    #[test]
    fn a_message_is_cleared_by_the_next_keypress() {
        let mut p = prompt();
        p.fail("nope".to_string(), "session 3".to_string());
        p.on_key(Key::Char('x'));
        assert!(p.message.is_none());
    }

    #[test]
    fn the_default_is_the_first_free_slot() {
        assert_eq!(next_name(&[]), "session 1");
        assert_eq!(
            next_name(&["session 1".to_string(), "session 2".to_string()]),
            "session 3"
        );
        assert_eq!(
            next_name(&["session 1".to_string(), "session 3".to_string()]),
            "session 2",
            "gaps are filled rather than skipped past"
        );
    }

    /// The transports reject duplicates case-insensitively, so a default that
    /// only differs in case would be offered and then refused.
    #[test]
    fn the_default_avoids_names_that_differ_only_in_case() {
        assert_eq!(next_name(&["Session 1".to_string()]), "session 2");
    }

    #[test]
    fn the_default_gives_up_gracefully_when_every_slot_is_taken() {
        let taken: Vec<String> = (1..1000).map(|n| format!("session {n}")).collect();
        assert_eq!(next_name(&taken), "session");
    }

    #[test]
    fn an_empty_field_shows_the_default_behind_the_cursor() {
        let lines = render(&prompt(), 50, 9);
        assert!(
            lines.iter().any(|l| l.contains("▋session 3")),
            "expected the placeholder in the field, got {lines:?}"
        );
    }

    #[test]
    fn typing_replaces_the_placeholder_entirely() {
        let mut p = prompt();
        type_in(&mut p, "my-project");
        let lines = render(&p, 50, 9);
        assert!(
            lines.iter().any(|l| l.contains("my-project▋")),
            "expected the typed name with a trailing cursor, got {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("session 3")),
            "the placeholder must not linger once typing has started: {lines:?}"
        );
    }

    #[test]
    fn backspacing_to_empty_brings_the_placeholder_back() {
        let mut p = prompt();
        type_in(&mut p, "ab");
        p.on_key(Key::Backspace);
        p.on_key(Key::Backspace);
        let lines = render(&p, 50, 9);
        assert!(
            lines.iter().any(|l| l.contains("▋session 3")),
            "expected the placeholder back, got {lines:?}"
        );
    }

    /// The placeholder has to be visibly *not* input, and dim is the only tool
    /// available given nothing here sets a colour.
    #[test]
    fn the_placeholder_is_dim_and_typed_text_is_not() {
        let placeholder = field_cells(&prompt(), 50, 9);
        assert_eq!(text_of(&placeholder), "\u{258b}session 3");
        assert!(
            !placeholder[0].1,
            "the cursor is the user's, not part of the placeholder"
        );
        assert!(
            placeholder[1..].iter().all(|(_, dim)| *dim),
            "every placeholder cell must be dim: {placeholder:?}"
        );

        let mut p = prompt();
        type_in(&mut p, "notes");
        let typed = field_cells(&p, 50, 9);
        assert_eq!(text_of(&typed), "notes\u{258b}");
        assert!(
            typed.iter().all(|(_, dim)| !*dim),
            "typed text must not be dim: {typed:?}"
        );
    }

    #[test]
    fn the_keybindings_are_on_the_last_row_and_do_not_change() {
        let empty = render(&prompt(), 50, 9);
        let mut p = prompt();
        type_in(&mut p, "notes");
        let typed = render(&p, 50, 9);

        assert!(
            empty[8].contains("⏎ create") && empty[8].contains("esc cancel"),
            "expected the hints on the last row, got {:?}",
            empty[8]
        );
        assert_eq!(
            empty[8], typed[8],
            "the hint row says the same thing either way — the placeholder \
             carries the default, not the hints"
        );
    }

    #[test]
    fn a_rejected_name_is_reported_under_the_field() {
        let mut p = prompt();
        type_in(&mut p, "notes");
        p.fail(
            "a session named \"notes\" already exists".to_string(),
            "session 4".to_string(),
        );
        let lines = render(&p, 50, 9);
        let field = lines
            .iter()
            .position(|l| l.contains("notes▋"))
            .expect("the field should still show what was typed");
        assert!(
            lines[field + 1].contains("already exists"),
            "expected the error on the row below the field, got {lines:?}"
        );
    }

    #[test]
    fn a_name_longer_than_the_field_keeps_its_end_visible() {
        assert_eq!(tail("abcdef", 10), "abcdef");
        assert_eq!(
            tail("abcdef", 3),
            "def",
            "the cursor is at the end, so the end is what has to stay on screen"
        );
        assert_eq!(tail("abc", 0), "");
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let mut typed = prompt();
        type_in(&mut typed, "a-fairly-long-session-name");
        let mut failed = prompt();
        failed.fail("something went wrong".to_string(), "session 3".to_string());

        for (w, h) in [(1, 1), (2, 1), (1, 2), (0, 0), (80, 1), (3, 3), (10, 2)] {
            for p in [&prompt(), &typed, &failed] {
                let _ = render(p, w.max(1), h.max(1));
            }
        }
    }

    /// Same rule as the picker: no SGR colour, so the screen inherits the
    /// terminal's palette and is correct under NO_COLOR without testing for it.
    #[test]
    fn nothing_sets_a_colour() {
        use ratatui::style::Color;
        let mut p = prompt();
        type_in(&mut p, "notes");
        p.fail("nope".to_string(), "session 3".to_string());

        let mut terminal = Terminal::new(TestBackend::new(50, 9)).expect("terminal");
        terminal.draw(|f| draw(f, &p)).expect("draw");
        let buf = terminal.backend().buffer().clone();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                assert_eq!(
                    cell.fg,
                    Color::Reset,
                    "cell ({x},{y}) set a foreground colour"
                );
                assert_eq!(
                    cell.bg,
                    Color::Reset,
                    "cell ({x},{y}) set a background colour"
                );
            }
        }
    }
}
