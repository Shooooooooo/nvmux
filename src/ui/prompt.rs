//! The naming prompt — the second screen, and the only place a session is named.
//!
//! Three ways in: `Ctrl-t c` from an attached session, and `c` or `r` from the
//! picker. `run` owns a terminal for the first, `run_on` borrows one for the
//! others; a [`Task`] says whether a name is being invented or edited. The
//! picker used to have its own inline create and rename prompts on the hint
//! row, which is exactly the duplication that let the two drift apart.
//!
//! It owns the whole screen rather than replacing a hint line, per the contract
//! in the parent module, because what it covers — a live session, or a list — is
//! not something that should stay half-visible underneath a name being typed. It
//! keeps the picker's visual language all the same: content centred, one dim
//! hint row on the last line, no borders, and no colour ever set.
//!
//! # Three weights, because there are three kinds of text
//!
//! One line, `label: value`, and every part of it styled for its job:
//!
//! ```text
//!     new session name: session 3
//!     └─ bold            │ └─ dim
//!                        └─ the cursor, an inverted cell
//! ```
//!
//! **Bold** leads, because on a screen with nothing else on it the label is the
//! question being asked. **Plain** is what you type. **Dim** is what you can
//! ignore — the default and the hint row. An earlier version dimmed the label
//! too, which flattened all three into one and left nothing on screen to read
//! first. Modifiers rather than colour, for the reason the parent module gives:
//! crossterm rewrites a colour into a bare `ESC[m` under `NO_COLOR`, a full SGR
//! reset that would wipe the bold and dim mid-line.
//!
//! # The default name is a placeholder, not a pre-filled value
//!
//! Enter on an empty field creates `session 1`, `session 2`, … — the naming
//! `Ctrl-t c` used to apply silently. That default is shown dimmed *inside* the
//! field rather than named in the hint row, so it reads as what enter will do
//! right now and costs nothing to discard. A pre-filled value would have to be
//! backspaced away before a real name could be typed, which is the whole reason
//! placeholders exist.
//!
//! Renaming inverts that, and the same field serves both: it *starts* pre-filled,
//! because a rename is an edit of something that already exists rather than a
//! blank to fill. Clear it and the current name reappears as the default, which
//! is the truth either way — the default is always what enter would submit if
//! you typed nothing.
//!
//! Being a placeholder is also why the cursor *inverts* its first letter rather
//! than taking a column in front of it. The cursor marks the insertion point,
//! and while a default is showing that point is exactly where its first letter
//! sits — so the default occupies the same columns your name will, and nothing
//! shifts when you start typing.

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
    /// An existing session was renamed. The caller just relists.
    Renamed,
    /// The user backed out; whatever they came from is still there.
    Cancelled,
}

/// Which name is being asked for.
///
/// The two differ in three places and nowhere else: what the label says, what
/// the field starts as, and what pressing enter finally calls. Everything about
/// how the screen looks and behaves is shared.
#[derive(Clone, Copy)]
pub(super) enum Task<'a> {
    Create,
    Rename(&'a Session),
}

/// What one keypress meant.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    None,
    Submit(String),
    Cancel,
}

/// What the user has typed, and what enter would do if they typed nothing.
///
/// Split from the terminal so the whole state machine is testable without one,
/// the same way `App` is split from `draw`.
struct Prompt {
    /// What the field is for, in front of it.
    ///
    /// A `label: value` line rather than a bare heading, because this screen
    /// covers whatever you were looking at: it has to say both what is
    /// happening and what to type. Renaming names the session for the same
    /// reason — the list it was selected in is no longer on screen.
    prefix: String,
    hints: &'static str,
    input: String,
    /// What enter submits when the field is empty. Creating, that is the next
    /// free `session N`; renaming, it is the name the session already has, so
    /// clearing the field and pressing enter keeps it.
    default_name: String,
    message: Option<String>,
}

impl Prompt {
    fn create(default_name: String) -> Self {
        Self {
            prefix: "new session name: ".to_string(),
            hints: "⏎ create   esc cancel",
            input: String::new(),
            default_name,
            message: None,
        }
    }

    /// Pre-filled with the current name, as specified — a rename is an edit of
    /// something that already exists, not a blank field.
    fn rename(session: &Session) -> Self {
        Self {
            prefix: format!("rename {:?} to: ", session.name),
            hints: "⏎ rename   esc cancel",
            input: session.name.clone(),
            default_name: session.name.clone(),
            message: None,
        }
    }

    /// Report a rejected name, optionally moving the default with it.
    ///
    /// What was typed is deliberately kept: a duplicate name is usually one
    /// character away from a good one, and retyping it is not a punishment the
    /// user earned. `default_name` is refreshed only when creating — a rename's
    /// default is the name the session already has, and that has not moved.
    fn fail(&mut self, message: String, default_name: Option<String>) {
        self.message = Some(message);
        if let Some(name) = default_name {
            self.default_name = name;
        }
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

/// Ask for a name for a new session, owning the terminal while it does.
///
/// For `Ctrl-t c`, which arrives from an attached session and so has no
/// terminal to borrow. Entering and leaving the alternate screen is what hides
/// the session underneath and puts it back afterwards.
pub fn run(transport: &dyn Transport) -> Result<Outcome> {
    // Same reasoning as the picker: `draw` never sets a colour, and crossterm's
    // own NO_COLOR handling would rewrite one into a full SGR reset that wipes
    // the dim this screen relies on.
    ratatui::crossterm::style::force_color_output(true);

    let mut terminal = ratatui::try_init()?;
    // The session's own alternate screen is still showing, and a second "enter
    // alternate screen" does not clear it on every terminal: xterm and kitty
    // treat it as a no-op. ratatui's first draw only paints the cells that
    // differ from an empty buffer, so without this the prompt would land in
    // the middle of the editor's last frame.
    terminal.clear()?;
    let outcome = run_on(&mut terminal, transport, Task::Create);
    // Restore before propagating anything: an error that leaves the terminal in
    // raw mode with no echo is far worse than the error itself.
    ratatui::try_restore()?;
    outcome
}

/// Ask for a name on a terminal the caller already owns — how the picker drives
/// this screen for `c` and `r`.
///
/// Not `run`: initialising a second terminal inside the picker's would enter the
/// alternate screen twice and leave it once, and the picker would spend the rest
/// of its life drawing to the main screen with raw mode off. Sharing the
/// terminal is also what makes the handover invisible — `Terminal::draw` resets
/// the frame each pass, so this paints over the list and the picker's next draw
/// puts it back.
pub(super) fn run_on(
    terminal: &mut ratatui::DefaultTerminal,
    transport: &dyn Transport,
    task: Task,
) -> Result<Outcome> {
    let mut prompt = match task {
        Task::Create => Prompt::create(next_free_name(transport)?),
        Task::Rename(session) => Prompt::rename(session),
    };

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

        let name = match prompt.on_key(key) {
            Step::None => continue,
            Step::Cancel => return Ok(Outcome::Cancelled),
            Step::Submit(name) => name,
        };

        // Names are not validated here. `session::validate_name` and the
        // transport's duplicate check are the rule; duplicating either one in
        // the UI is how the two drift apart.
        let committed = match task {
            Task::Create => transport.create_session(&name).map(Outcome::Created),
            Task::Rename(session) => transport
                .rename_session(session, &name)
                .map(|_| Outcome::Renamed),
        };

        match committed {
            Ok(outcome) => return Ok(outcome),
            Err(e) => {
                // A name that was free when the prompt opened may not be now.
                let refreshed = match task {
                    Task::Create => Some(next_free_name(transport)?),
                    Task::Rename(_) => None,
                };
                prompt.fail(super::one_line(&e), refreshed);
            }
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
    draw_hints(frame, prompt, bottom);
}

/// Centre the line as it reads the moment the prompt opens, then hold it.
///
/// The anchor deliberately ignores `input`, and that is the whole trick. Sizing
/// it to the current contents would centre the line perfectly at every instant
/// and shuffle it half a cell left on every keystroke; anchoring on the prefix
/// alone would hold still but leave the real line sitting several columns right
/// of centre. Measuring the *opening* line gets both: centred when you first see
/// it, and a field whose first column never moves afterwards — which is what
/// lets the placeholder sit exactly where your first keystroke will land.
fn draw_body(frame: &mut Frame, prompt: &Prompt, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let height = if prompt.message.is_some() { 2 } else { 1 };
    let opening = (prompt.prefix.width() + prompt.default_name.width()) as u16;
    let anchor = draw::centre(area, opening, height);
    // Everything from the anchor to the right edge is the line's, so a long name
    // has somewhere to go.
    let width = (area.x + area.width).saturating_sub(anchor.x) as usize;

    if anchor.height >= 1 {
        frame.render_widget(
            Paragraph::new(prompt_line(prompt, width)),
            Rect {
                x: anchor.x,
                width: width as u16,
                height: 1,
                ..anchor
            },
        );
    }

    // The message is routinely wider than the line, so it gets the full width
    // and its own centring rather than hanging off the anchor.
    if anchor.height >= 2 {
        if let Some(msg) = prompt.message.as_deref() {
            frame.render_widget(
                Paragraph::new(Line::from(draw::truncate(msg, area.width as usize)))
                    .alignment(Alignment::Center),
                Rect {
                    y: anchor.y + 1,
                    height: 1,
                    ..area
                },
            );
        }
    }
}

/// The whole line: bold label, then either what was typed or the dim default.
///
/// Three roles, three weights, and all of it derived from state rather than
/// tracked — the prefix is constant and `input.is_empty()` decides the rest, so
/// backspacing to nothing brings the placeholder back with nothing to keep in
/// sync.
///
/// The cursor is one inverted cell and it always marks the insertion point. With
/// a placeholder showing, that point *is* its first letter, so the cursor
/// inverts the letter rather than taking a column of its own in front of it —
/// otherwise the default would sit one column right of where typing actually
/// lands, and the field would visibly jump on the first keystroke.
fn prompt_line(prompt: &Prompt, width: usize) -> Line<'static> {
    // The cursor is the only feedback that typing is doing anything, so it gets
    // a column before the label does — a long rename label on a narrow terminal
    // would otherwise fill the line and push it off the right edge.
    let label = draw::truncate(&prompt.prefix, width.saturating_sub(1));
    let room = width.saturating_sub(label.width() + 1);
    let mut spans = vec![Span::styled(
        label,
        Style::default().add_modifier(Modifier::BOLD),
    )];

    // Plain REVERSED, not the picker's REVERSED|BOLD: that pairing means "the
    // selected row", and a one-cell cursor wants the crisper form.
    let cursor = Style::default().add_modifier(Modifier::REVERSED);

    // Nothing typed yet, and a default to show it with: the cursor lands on a
    // character, so it inverts that character.
    let placeholder = if prompt.input.is_empty() {
        split_first(&prompt.default_name)
    } else {
        None
    };

    match placeholder {
        Some((first, rest)) => {
            spans.push(Span::styled(first, cursor));
            spans.push(Span::styled(
                draw::truncate(rest, room),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        // Something typed, or no default to sit on: the insertion point is past
        // the end of the text, so the cursor inverts a blank instead.
        None => {
            spans.push(Span::raw(tail(&prompt.input, room)));
            spans.push(Span::styled(" ", cursor));
        }
    }

    Line::from(spans)
}

/// Split off the first character, or `None` if there is not one.
fn split_first(s: &str) -> Option<(String, &str)> {
    let first = s.chars().next()?;
    Some((first.to_string(), &s[first.len_utf8()..]))
}

fn draw_hints(frame: &mut Frame, prompt: &Prompt, area: Rect) {
    let text = draw::truncate(prompt.hints, area.width as usize);
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
        Prompt::create("session 3".to_string())
    }

    fn renaming(name: &str) -> Prompt {
        Prompt::rename(&Session::new("id000000".to_string(), name.to_string(), 100))
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

    /// One rendered cell: what it shows and how it is styled.
    ///
    /// The modifier is the point of most of these tests, and `render` throws it
    /// away, so the styling assertions go through here instead.
    type Cell = (String, Modifier);

    /// The whole prompt line, as cells, trimmed to its content.
    ///
    /// The row is found by the cursor's inverted cell, which nothing else on the
    /// screen draws. The right edge is the last cell that either shows something
    /// or is the cursor — the cursor can be an inverted *blank* trailing what was
    /// typed, and trimming on symbol alone would throw it away.
    fn line_cells(p: &Prompt, w: u16, h: u16) -> Vec<Cell> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        terminal.draw(|f| draw(f, p)).expect("draw");
        let buf = terminal.backend().buffer().clone();

        let reversed = |x, y| buf[(x, y)].modifier.contains(Modifier::REVERSED);
        let y = (0..buf.area.height)
            .find(|&y| (0..buf.area.width).any(|x| reversed(x, y)))
            .expect("the prompt line should be the row holding the cursor");

        let row: Vec<Cell> = (0..buf.area.width)
            .map(|x| {
                let cell = &buf[(x, y)];
                (cell.symbol().to_string(), cell.modifier)
            })
            .collect();

        let shown = |x: usize| row[x].0 != " " || reversed(x as u16, y);
        let first = (0..row.len()).find(|&x| shown(x)).expect("content");
        let last = (0..row.len()).rposition(shown).expect("content");
        row[first..=last].to_vec()
    }

    /// The buffer column the field starts in — where the first character you
    /// type will land, and where the default has to be sitting before you do.
    fn field_column(p: &Prompt, w: u16, h: u16) -> usize {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        terminal.draw(|f| draw(f, p)).expect("draw");
        let buf = terminal.backend().buffer().clone();

        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)].modifier.contains(Modifier::REVERSED) {
                    // The cursor is on the field's first column only while the
                    // field is empty; once typing starts it trails the text.
                    return x as usize - p.input.width();
                }
            }
        }
        panic!("no cursor rendered");
    }

    fn text_of(cells: &[Cell]) -> String {
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
            Some("session 4".to_string()),
        );
        assert_eq!(p.input, "notes", "the typed name must survive a rejection");
        assert_eq!(p.default_name, "session 4");
        assert!(p.message.is_some());
    }

    #[test]
    fn a_message_is_cleared_by_the_next_keypress() {
        let mut p = prompt();
        p.fail("nope".to_string(), Some("session 3".to_string()));
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
    fn an_empty_field_shows_the_label_and_the_default() {
        let lines = render(&prompt(), 50, 9);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("new session name: session 3")),
            "expected the labelled line with its default, got {lines:?}"
        );
    }

    #[test]
    fn typing_replaces_the_placeholder_entirely() {
        let mut p = prompt();
        type_in(&mut p, "my-project");
        let lines = render(&p, 50, 9);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("new session name: my-project")),
            "expected the typed name after the label, got {lines:?}"
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
            lines
                .iter()
                .any(|l| l.contains("new session name: session 3")),
            "expected the placeholder back, got {lines:?}"
        );
    }

    /// The bug this layout exists to avoid: if the default starts one column
    /// over from where typing lands, the field jumps on the first keystroke.
    ///
    /// It only holds because the anchor is measured from the *opening* line and
    /// ignores `input`, so this pins that too.
    #[test]
    fn the_default_sits_where_the_first_keystroke_will_land() {
        let mut typed = prompt();
        type_in(&mut typed, "n");

        assert_eq!(
            field_column(&prompt(), 50, 9),
            field_column(&typed, 50, 9),
            "the default and the first typed character must share a column"
        );
    }

    /// Four roles on one line, and modifiers are the only way to tell them
    /// apart given nothing here sets a colour: the label leads in bold, the
    /// cursor is an inverted cell, the default recedes into dim, and what you
    /// type is plain.
    #[test]
    fn the_label_leads_the_cursor_marks_the_field_and_the_default_recedes() {
        let label = "new session name: ".width();

        let empty = line_cells(&prompt(), 50, 9);
        assert_eq!(text_of(&empty), "new session name: session 3");
        assert!(
            empty[..label]
                .iter()
                .all(|(_, m)| m.contains(Modifier::BOLD)),
            "the label must be bold: {empty:?}"
        );
        assert_eq!(
            empty[label].1,
            Modifier::REVERSED,
            "the cursor inverts the default's first letter and nothing else"
        );
        assert!(
            empty[label + 1..].iter().all(|(_, m)| *m == Modifier::DIM),
            "the rest of the default must be dim and only dim: {empty:?}"
        );

        let mut p = prompt();
        type_in(&mut p, "notes");
        let typed = line_cells(&p, 50, 9);
        assert_eq!(text_of(&typed), "new session name: notes ");
        assert!(
            typed[..label]
                .iter()
                .all(|(_, m)| m.contains(Modifier::BOLD)),
            "the label must stay bold once typing starts: {typed:?}"
        );
        assert!(
            typed[label..label + 5].iter().all(|(_, m)| m.is_empty()),
            "typed text carries no modifier at all: {typed:?}"
        );
        assert_eq!(
            typed[label + 5].1,
            Modifier::REVERSED,
            "the cursor trails what was typed, over a blank"
        );
        assert!(
            typed.iter().all(|(_, m)| !m.contains(Modifier::DIM)),
            "nothing on a typed line is dim: {typed:?}"
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
            Some("session 4".to_string()),
        );
        let lines = render(&p, 50, 9);
        let field = lines
            .iter()
            .position(|l| l.contains("new session name: notes"))
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
        failed.fail(
            "something went wrong".to_string(),
            Some("session 3".to_string()),
        );

        for (w, h) in [(1, 1), (2, 1), (1, 2), (0, 0), (80, 1), (3, 3), (10, 2)] {
            for p in [&prompt(), &typed, &failed] {
                let _ = render(p, w.max(1), h.max(1));
            }
        }
    }

    /// Renaming hides the list the session was selected in, so the label has to
    /// say which session is being renamed.
    #[test]
    fn the_rename_prompt_names_the_session_and_starts_from_its_name() {
        let p = renaming("dotfiles");
        let lines = render(&p, 60, 9);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("rename \"dotfiles\" to: dotfiles")),
            "expected the session named in the label and pre-filled in the field, got {lines:?}"
        );
    }

    #[test]
    fn the_rename_prompt_says_rename_in_its_hints() {
        let lines = render(&renaming("dotfiles"), 60, 9);
        assert!(
            lines[8].contains("⏎ rename") && lines[8].contains("esc cancel"),
            "expected rename hints on the last row, got {:?}",
            lines[8]
        );
    }

    /// A pre-filled field has no placeholder to show — until you clear it, at
    /// which point the current name is exactly what enter would keep.
    #[test]
    fn clearing_a_rename_offers_the_current_name_back() {
        let mut p = renaming("dotfiles");
        for _ in 0.."dotfiles".len() {
            p.on_key(Key::Backspace);
        }
        assert_eq!(p.input, "");

        let cells = line_cells(&p, 60, 9);
        let label = "rename \"dotfiles\" to: ".width();
        assert_eq!(text_of(&cells), "rename \"dotfiles\" to: dotfiles");
        assert_eq!(
            cells[label].1,
            Modifier::REVERSED,
            "the cursor sits on the offered name's first letter"
        );
        assert!(
            cells[label + 1..].iter().all(|(_, m)| *m == Modifier::DIM),
            "the offered name recedes like any other default: {cells:?}"
        );

        assert_eq!(
            p.on_key(Key::Enter),
            Step::Submit("dotfiles".to_string()),
            "enter on a cleared rename keeps the name it had"
        );
    }

    /// A rejected rename must not move the default the way a rejected create
    /// does — the name the session already has has not changed.
    #[test]
    fn a_rejected_rename_keeps_offering_the_current_name() {
        let mut p = renaming("dotfiles");
        p.fail("a session named \"notes\" already exists".to_string(), None);
        assert_eq!(p.default_name, "dotfiles");
    }

    /// However little room is left, the cursor renders: it is the only sign that
    /// a keystroke landed, and a label long enough to fill the line must not
    /// push it off the edge.
    #[test]
    fn the_cursor_survives_a_label_wider_than_the_terminal() {
        for w in [1u16, 2, 8, 20, 30] {
            for p in [
                &prompt(),
                &renaming("a-really-long-session-name-here"),
                &renaming("dotfiles"),
            ] {
                let cells = line_cells(p, w, 9);
                assert!(
                    cells.iter().any(|(_, m)| m.contains(Modifier::REVERSED)),
                    "no cursor at {w} columns: {cells:?}"
                );
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
        p.fail("nope".to_string(), Some("session 3".to_string()));

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
