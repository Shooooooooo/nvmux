//! The naming prompt — the only place a session is named, and the only place
//! one is created.
//!
//! Three ways in: `<prefix> c` from an attached session, and `c` or `r` from the
//! picker. [`run`] owns a terminal for the first, `run_on` borrows one for the
//! others; a `Task` says whether a session is being invented or renamed.
//!
//! It owns the whole screen rather than replacing a hint line, per the contract
//! in the parent module. Three weights carry the three kinds of text — bold for
//! the labels, plain for what you type, dim for the defaults and the hint row:
//!
//! ```text
//!     new session name: session 3
//!     nvim command:     nvim --headless --listen {sock}
//!     └─ bold            │ └─ dim
//!                        └─ the cursor, an inverted cell
//! ```
//!
//! # One screen, two fields
//!
//! Creating asks two questions — what the session is called, and how its Neovim
//! is started — and asks them together rather than one after the other.
//! Enter submits the whole form from whichever field it is pressed in, so
//! `<prefix> c` followed by enter still creates a session in one keystroke and
//! the second question costs nothing to anyone who does not want it. Renaming
//! puts up the same screen with one field, since a rename starts nothing.
//!
//! # The defaults are placeholders, not pre-filled values
//!
//! Enter on an empty field creates `session 1`, `session 2`, … with whatever
//! command was last used on this host. Each default is shown dimmed *inside* its
//! field, so it reads as what enter will do right now and costs nothing to
//! discard, rather than needing to be backspaced away.
//!
//! Renaming inverts that and the same field serves both: it *starts* pre-filled,
//! because a rename edits something that already exists. Clear it and the
//! current name reappears as the default — which is the truth either way, since
//! the default is always what enter would submit if you typed nothing.
//!
//! Being a placeholder is also why the cursor *inverts* its first letter rather
//! than taking a column in front of it: the default then occupies exactly the
//! columns your own text will, and nothing shifts when you start typing.
//!
//! A command, though, is usually a small edit of the default rather than
//! something written from scratch — and it is long. So `→` and `End` on an empty
//! field take the default into it, cursor at the end, instead of moving a cursor
//! that has nowhere to go. One keypress to edit the default; typing anything
//! else still replaces it outright.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::app::Key;
use super::draw;
use crate::error::Result;
use crate::launch::Launch;
use crate::session::Session;
use crate::state;
use crate::transport::Transport;

/// What the prompt returned.
#[derive(Debug)]
pub enum Outcome {
    /// Created; the caller attaches to it.
    Created(Session),
    /// Renamed; the caller relists.
    Renamed,
    /// Backed out.
    Cancelled,
}

impl Outcome {
    /// Whether the caller attaches next, and so needs the terminal given back
    /// cleared — see [`super::Screen::close_for_attach`].
    fn attaches(&self) -> bool {
        matches!(self, Self::Created(_))
    }
}

/// Which question is being asked. The two differ in how many fields there are,
/// what they start as, and what enter finally calls.
#[derive(Clone, Copy)]
pub(super) enum Task<'a> {
    Create,
    Rename(&'a Session),
}

/// Where the name lives in [`Prompt::fields`]. Both tasks have one.
const NAME: usize = 0;
/// Where the command lives. Only [`Task::Create`] has one.
const COMMAND: usize = 1;

/// What enter asked for.
#[derive(Debug, PartialEq, Eq)]
struct Submission {
    name: String,
    /// `None` when renaming, which starts nothing.
    command: Option<String>,
}

/// What one keypress meant.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    None,
    Submit(Submission),
    Cancel,
}

/// One labelled field: what has been typed in it, where the cursor is, and what
/// enter would submit if nothing had been.
struct Field {
    /// What the field is for, in front of it. A `label: value` line rather than
    /// a heading, because this screen covers whatever you were looking at and
    /// has to say both what is happening and what to type.
    label: String,
    input: String,
    /// A **character** index into `input`, from 0 to its length inclusive.
    /// Characters rather than bytes so arithmetic on it cannot land mid-glyph.
    cursor: usize,
    default: String,
}

impl Field {
    fn new(label: String, input: String, default: String) -> Self {
        let cursor = input.chars().count();
        Self {
            label,
            input,
            cursor,
            default,
        }
    }

    fn len(&self) -> usize {
        self.input.chars().count()
    }

    fn insert(&mut self, c: char) {
        self.input.insert(byte_at(&self.input, self.cursor), c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.input.remove(byte_at(&self.input, self.cursor - 1));
        self.cursor -= 1;
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        if self.adopt_default() {
            return;
        }
        self.cursor = (self.cursor + 1).min(self.len());
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.adopt_default();
        self.cursor = self.len();
    }

    /// Take the placeholder into the field so it can be edited. Only ever from
    /// an empty field: with anything typed there is a cursor to move instead.
    fn adopt_default(&mut self) -> bool {
        if !self.input.is_empty() {
            return false;
        }
        self.input.clone_from(&self.default);
        self.cursor = self.len();
        true
    }

    /// What this field submits: the trimmed text, or the default when there is
    /// none — which is exactly what the placeholder was showing.
    fn value(&self) -> String {
        let text = self.input.trim();
        if text.is_empty() {
            self.default.clone()
        } else {
            text.to_string()
        }
    }
}

/// The byte offset of a character index, or the end of the string.
fn byte_at(s: &str, chars: usize) -> usize {
    s.char_indices()
        .nth(chars)
        .map_or(s.len(), |(offset, _)| offset)
}

/// What has been typed, and what enter would do if nothing had been. Split from
/// the terminal so the state machine is testable without one.
struct Prompt {
    fields: Vec<Field>,
    focus: usize,
    hints: &'static str,
    message: Option<String>,
}

impl Prompt {
    fn create(default_name: String, default_command: String) -> Self {
        let labels = aligned(&["new session name", "nvim command"]);
        Self {
            fields: vec![
                Field::new(labels[0].clone(), String::new(), default_name),
                Field::new(labels[1].clone(), String::new(), default_command),
            ],
            focus: NAME,
            hints: "⏎ create   ⇥ field   → edit   esc cancel",
            message: None,
        }
    }

    /// Pre-filled: a rename edits something that already exists.
    fn rename(session: &Session) -> Self {
        Self {
            fields: vec![Field::new(
                format!("rename {:?} to: ", session.name),
                session.name.clone(),
                session.name.clone(),
            )],
            focus: NAME,
            hints: "⏎ rename   esc cancel",
            message: None,
        }
    }

    fn focused(&mut self) -> &mut Field {
        &mut self.fields[self.focus]
    }

    /// Wrapping, like the picker's list: with two fields, one key reaches the
    /// other whichever one is focused.
    fn move_focus(&mut self, delta: usize) {
        let n = self.fields.len();
        self.focus = (self.focus + delta) % n;
    }

    /// Report a rejected submission, keeping what was typed — a duplicate is
    /// usually one character from a good name. `default_name` is refreshed only
    /// when creating; a rename's default has not moved.
    fn fail(&mut self, message: String, default_name: Option<String>) {
        self.message = Some(message);
        if let Some(name) = default_name {
            self.fields[NAME].default = name;
        }
    }

    fn submission(&self) -> Submission {
        Submission {
            name: self.fields[NAME].value(),
            command: self.fields.get(COMMAND).map(Field::value),
        }
    }

    fn on_key(&mut self, key: Key) -> Step {
        // A stale message must not linger over an unrelated action.
        self.message = None;

        match key {
            Key::Char(c) => {
                self.focused().insert(c);
                Step::None
            }
            Key::Backspace => {
                self.focused().backspace();
                Step::None
            }
            Key::Left => {
                self.focused().left();
                Step::None
            }
            Key::Right => {
                self.focused().right();
                Step::None
            }
            Key::Home => {
                self.focused().home();
                Step::None
            }
            Key::End => {
                self.focused().end();
                Step::None
            }
            Key::Tab | Key::Down => {
                self.move_focus(1);
                Step::None
            }
            Key::BackTab | Key::Up => {
                self.move_focus(self.fields.len() - 1);
                Step::None
            }
            // From either field, so the one-keystroke create survives the
            // second question — see the module docs.
            Key::Enter => Step::Submit(self.submission()),
            // Not a quit here: it would tear the user out of a live session
            // they only meant to leave a prompt in.
            Key::Esc | Key::CtrlC => Step::Cancel,
            _ => Step::None,
        }
    }
}

/// `label: ` for each, padded so every field's value starts in the same column.
/// Labels of different lengths would otherwise stagger the fields and put the
/// cursor somewhere different on each row.
fn aligned(labels: &[&str]) -> Vec<String> {
    let width = labels.iter().map(|l| l.width()).max().unwrap_or(0);
    labels
        .iter()
        .map(|l| format!("{l}:{}", " ".repeat(width - l.width() + 1)))
        .collect()
}

/// Ask for a new session, owning the terminal while it does — for `<prefix> c`,
/// which arrives from an attached session with none to borrow.
///
/// [`Outcome::Created`] leads straight to a client spawn, exactly as the picker's
/// attach does, so the terminal goes back cleared on that one outcome and the
/// session being left is not what fills the spawn. A cancelled prompt goes back
/// to the same client, which is repainted, so it takes the ordinary close.
pub fn run(transport: &dyn Transport) -> Result<Outcome> {
    super::owning_for_attach(Outcome::attaches, |terminal| {
        run_on(terminal, transport, Task::Create)
    })
}

/// Ask on a terminal the caller already owns — how the picker drives this
/// screen for `c` and `r`.
///
/// Not `run`: a second terminal inside the picker's would enter the alternate
/// screen twice and leave it once. Sharing it also makes the handover
/// invisible, since `Terminal::draw` resets the frame each pass.
pub(super) fn run_on(
    terminal: &mut ratatui::DefaultTerminal,
    transport: &dyn Transport,
    task: Task,
) -> Result<Outcome> {
    let mut prompt = match task {
        Task::Create => Prompt::create(next_free_name(transport)?, default_command(transport)),
        Task::Rename(session) => Prompt::rename(session),
    };

    loop {
        terminal.draw(|f| draw(f, &prompt))?;

        let Some(key) = super::poll_key()? else {
            continue;
        };

        let submission = match prompt.on_key(key) {
            Step::None => continue,
            Step::Cancel => return Ok(Outcome::Cancelled),
            Step::Submit(submission) => submission,
        };

        // The name is not validated here: `session::validate_name` and the
        // transport's duplicate check are the rule, and a UI copy would drift
        // from them. The command is parsed here only because the transport
        // takes a `Launch` and cannot be handed a line that is not one; the
        // rules still live in `launch`.
        let committed = match task {
            Task::Create => {
                let line = submission.command.unwrap_or_default();
                Launch::parse(&line)
                    .map_err(Into::into)
                    .and_then(|launch| {
                        transport
                            .create_session(&submission.name, &launch)
                            // Only once a session has really started with it:
                            // offering back a command that never worked would
                            // make the same failure the default.
                            .inspect(|_| state::remember(transport.location(), launch.line()))
                    })
                    .map(Outcome::Created)
            }
            Task::Rename(session) => transport
                .rename_session(session, &submission.name)
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

/// The name a session gets when the user just presses enter. Compared
/// case-insensitively, because that is how the transports reject duplicates.
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

/// The command a session gets when the user just presses enter: what was last
/// used on this host, else what the config says.
///
/// Per host, because the answer is about a machine — a path to a nightly build
/// on one box means nothing on another, and `nvmux myhost` is a different
/// machine's `$PATH`.
fn default_command(transport: &dyn Transport) -> String {
    state::remembered(transport.location())
        .unwrap_or_else(|| crate::config::get().session.command.clone())
}

fn draw(frame: &mut Frame, prompt: &Prompt) {
    let area = frame.area();
    if area.height == 0 || area.width == 0 {
        return;
    }

    let (body, bottom) = draw::split_hint_row(area);

    draw_body(frame, prompt, body);
    draw::draw_hint_row(frame, bottom, prompt.hints, true);
}

/// Centre the block as it reads the moment the prompt opens, then hold it.
///
/// The anchor deliberately ignores what has been typed: sizing it to the current
/// contents would shuffle the block half a cell on every keystroke, and
/// anchoring on the labels alone would leave it right of centre. Measuring the
/// *opening* lines gets both, and keeps each field's first column where its
/// placeholder sits.
fn draw_body(frame: &mut Frame, prompt: &Prompt, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let rows = prompt.fields.len() as u16;
    let height = rows + u16::from(prompt.message.is_some());
    let opening = prompt
        .fields
        .iter()
        .map(|f| f.label.width() + f.default.width())
        .max()
        .unwrap_or(0) as u16;
    let anchor = draw::centre(area, opening, height);
    let width = (area.x + area.width).saturating_sub(anchor.x) as usize;

    for (i, field) in prompt.fields.iter().enumerate() {
        let y = anchor.y + i as u16;
        if i as u16 >= anchor.height {
            break;
        }
        frame.render_widget(
            Paragraph::new(field_line(field, prompt.focus == i, width)),
            Rect {
                x: anchor.x,
                y,
                width: width as u16,
                height: 1,
            },
        );
    }

    // Routinely wider than the fields, so it gets the full width and its own
    // centring rather than hanging off the anchor.
    if anchor.height > rows {
        if let Some(msg) = prompt.message.as_deref() {
            frame.render_widget(
                Paragraph::new(Line::from(draw::truncate(msg, area.width as usize)))
                    .alignment(Alignment::Center),
                Rect {
                    y: anchor.y + rows,
                    height: 1,
                    ..area
                },
            );
        }
    }
}

/// One field's whole line: bold label, then either what was typed or the dim
/// default. Derived from state rather than tracked, so backspacing to nothing
/// brings the placeholder back with nothing to keep in sync.
fn field_line(field: &Field, focused: bool, width: usize) -> Line<'static> {
    // The cursor is the only feedback that typing is doing anything, so it gets
    // a column before the label does — a long rename label on a narrow terminal
    // would otherwise fill the line and push it off the right edge.
    let label = draw::truncate(&field.label, width.saturating_sub(1));
    let room = width.saturating_sub(label.width() + 1);
    let mut spans = vec![Span::styled(
        label,
        Style::default().add_modifier(Modifier::BOLD),
    )];

    // Plain REVERSED, not the picker's REVERSED|BOLD: that pairing means "the
    // selected row", and a one-cell cursor wants the crisper form.
    let cursor = Style::default().add_modifier(Modifier::REVERSED);
    let dim = Style::default().add_modifier(Modifier::DIM);

    match (field.input.is_empty(), focused) {
        // The placeholder, with the cursor on its first letter so the default
        // sits in exactly the columns a typed value will.
        (true, true) => match split_first(&field.default) {
            Some((first, rest)) => {
                spans.push(Span::styled(first, cursor));
                spans.push(Span::styled(draw::truncate(rest, room), dim));
            }
            None => spans.push(Span::styled(" ", cursor)),
        },
        // Nothing typed and not where the keystrokes are going: no cursor, or
        // there would be two on screen and no telling which is live.
        (true, false) => spans.push(Span::styled(draw::truncate(&field.default, room), dim)),
        (false, true) => {
            let (before, at, after) = around_cursor(field, room);
            spans.push(Span::raw(before));
            spans.push(Span::styled(at, cursor));
            spans.push(Span::raw(after));
        }
        (false, false) => spans.push(Span::raw(draw::truncate(&field.input, room))),
    }

    Line::from(spans)
}

fn split_first(s: &str) -> Option<(String, &str)> {
    let first = s.chars().next()?;
    Some((first.to_string(), &s[first.len_utf8()..]))
}

/// The text either side of the cursor and the cell under it, windowed to `max`
/// display columns.
///
/// The opposite choice from `draw::truncate`, and for a reason: a field has to
/// keep the cursor visible, so when a command outgrows it the text scrolls
/// rather than the part being edited disappearing. The cursor may sit one past
/// the end, so the line is treated as the input plus a trailing blank — which is
/// the cell it inverts there.
fn around_cursor(field: &Field, max: usize) -> (String, String, String) {
    let cells: Vec<char> = field.input.chars().chain([' ']).collect();
    let at = field.cursor.min(cells.len() - 1);
    let width = |c: char| c.to_string().width().max(1);

    // The cursor's own cell first, and it is returned even when `max` is 0: a
    // field squeezed to nothing by a long label still has to show where the
    // keystrokes are going, which is why `field_line` reserves it a column.
    let mut used = width(cells[at]).min(max);
    let (mut start, mut end) = (at, at + 1);

    // Then backwards, so the text scrolls off the left as it grows past the
    // field, and only then forwards with whatever room is left.
    while start > 0 && used + width(cells[start - 1]) <= max {
        start -= 1;
        used += width(cells[start]);
    }
    while end < cells.len() && used + width(cells[end]) <= max {
        used += width(cells[end]);
        end += 1;
    }

    let text = |range: &[char]| range.iter().collect::<String>();
    (
        text(&cells[start..at]),
        text(&cells[at..at + 1]),
        text(&cells[at + 1..end]),
    )
}

#[cfg(test)]
mod tests {
    use super::super::test_support;
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const COMMAND_DEFAULT: &str = "nvim --headless --listen {sock}";

    /// Only a created session is attached to next. A rename relists and a
    /// cancel resumes the client that is still running, and both of those are
    /// drawn over by whatever comes next — clearing for them would put a blank
    /// screen in front of a picker that was about to paint anyway.
    #[test]
    fn only_a_created_session_hands_the_terminal_over() {
        let session = Session::new("id000000".into(), "a".into(), 100, 1);
        assert!(Outcome::Created(session).attaches());
        assert!(!Outcome::Renamed.attaches());
        assert!(!Outcome::Cancelled.attaches());
    }

    fn prompt() -> Prompt {
        Prompt::create("session 3".to_string(), COMMAND_DEFAULT.to_string())
    }

    fn renaming(name: &str) -> Prompt {
        Prompt::rename(&Session::new(
            "id000000".to_string(),
            name.to_string(),
            100,
            1,
        ))
    }

    fn type_in(p: &mut Prompt, text: &str) {
        for c in text.chars() {
            p.on_key(Key::Char(c));
        }
    }

    fn press(p: &mut Prompt, keys: &[Key]) {
        for &k in keys {
            p.on_key(k);
        }
    }

    fn render(p: &Prompt, w: u16, h: u16) -> Vec<String> {
        test_support::render(w, h, |f| draw(f, p))
    }

    fn submitted(p: &mut Prompt) -> Submission {
        match p.on_key(Key::Enter) {
            Step::Submit(s) => s,
            other => panic!("expected a submission, got {other:?}"),
        }
    }

    /// One rendered cell. `render` throws the modifier away, and the modifier is
    /// the point of most of these tests.
    type Cell = (String, Modifier);

    fn cells_of(p: &Prompt, w: u16, h: u16) -> Vec<Vec<Cell>> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        terminal.draw(|f| draw(f, p)).expect("draw");
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| {
                        let cell = &buf[(x, y)];
                        (cell.symbol().to_string(), cell.modifier)
                    })
                    .collect()
            })
            .collect()
    }

    /// The row holding the cursor, as cells, trimmed to its content. The right
    /// edge is the last cell that shows something *or* is the cursor — the
    /// cursor can be an inverted blank, which trimming on symbol alone would
    /// discard.
    fn line_cells(p: &Prompt, w: u16, h: u16) -> Vec<Cell> {
        let rows = cells_of(p, w, h);
        let row = rows
            .iter()
            .find(|row| row.iter().any(|(_, m)| m.contains(Modifier::REVERSED)))
            .expect("the focused field should be the row holding the cursor");

        let shown = |x: usize| row[x].0 != " " || row[x].1.contains(Modifier::REVERSED);
        let first = (0..row.len()).find(|&x| shown(x)).expect("content");
        let last = (0..row.len()).rposition(shown).expect("content");
        row[first..=last].to_vec()
    }

    /// The buffer column the focused field starts in — where the next character
    /// typed will land, and where the default has to be sitting before it does.
    fn field_column(p: &Prompt, w: u16, h: u16) -> usize {
        let rows = cells_of(p, w, h);
        for row in &rows {
            if let Some(x) = row.iter().position(|(_, m)| m.contains(Modifier::REVERSED)) {
                // The cursor trails what has been typed before it.
                let typed: usize = p.fields[p.focus]
                    .input
                    .chars()
                    .take(p.fields[p.focus].cursor)
                    .map(|c| c.to_string().width())
                    .sum();
                return x - typed;
            }
        }
        panic!("no cursor rendered");
    }

    fn text_of(cells: &[Cell]) -> String {
        cells.iter().map(|(s, _)| s.as_str()).collect()
    }

    // --- typing ------------------------------------------------------------

    #[test]
    fn typing_appends_and_backspace_removes() {
        let mut p = prompt();
        type_in(&mut p, "notes");
        assert_eq!(p.fields[NAME].input, "notes");
        p.on_key(Key::Backspace);
        assert_eq!(p.fields[NAME].input, "note");
    }

    #[test]
    fn backspace_on_an_empty_field_is_harmless() {
        let mut p = prompt();
        for _ in 0..5 {
            assert_eq!(p.on_key(Key::Backspace), Step::None);
        }
        assert_eq!(p.fields[NAME].input, "");
    }

    #[test]
    fn enter_submits_the_trimmed_text() {
        let mut p = prompt();
        type_in(&mut p, "  my-project  ");
        assert_eq!(
            submitted(&mut p).name,
            "my-project",
            "surrounding whitespace is not part of the name"
        );
    }

    /// The zero-keystroke path `<prefix> c` used to be has to survive the
    /// second field: enter on an untouched form means "both placeholders".
    #[test]
    fn enter_on_an_untouched_form_submits_both_defaults() {
        let mut p = prompt();
        assert_eq!(
            p.on_key(Key::Enter),
            Step::Submit(Submission {
                name: "session 3".to_string(),
                command: Some(COMMAND_DEFAULT.to_string()),
            })
        );
    }

    /// And from the other field too, so tabbing over to look at the command
    /// does not cost an extra keystroke to get back.
    #[test]
    fn enter_submits_the_whole_form_from_either_field() {
        let mut p = prompt();
        type_in(&mut p, "notes");
        p.on_key(Key::Tab);
        type_in(&mut p, "nvim -u NONE --listen {sock}");
        assert_eq!(p.focus, COMMAND);
        assert_eq!(
            submitted(&mut p),
            Submission {
                name: "notes".to_string(),
                command: Some("nvim -u NONE --listen {sock}".to_string()),
            }
        );
    }

    #[test]
    fn a_whitespace_only_field_counts_as_empty() {
        let mut p = prompt();
        type_in(&mut p, "   ");
        assert_eq!(submitted(&mut p).name, "session 3");
    }

    #[test]
    fn esc_cancels() {
        let mut p = prompt();
        type_in(&mut p, "notes");
        assert_eq!(p.on_key(Key::Esc), Step::Cancel);
    }

    /// Cancels like esc, rather than exiting nvmux out from under a live session.
    #[test]
    fn ctrl_c_cancels_rather_than_quitting() {
        let mut p = prompt();
        assert_eq!(p.on_key(Key::CtrlC), Step::Cancel);
    }

    #[test]
    fn a_rejected_submission_is_kept_so_it_can_be_edited() {
        let mut p = prompt();
        type_in(&mut p, "notes");
        p.on_key(Key::Tab);
        type_in(&mut p, "nvim");
        p.fail(
            "a session named \"notes\" already exists".to_string(),
            Some("session 4".to_string()),
        );
        assert_eq!(p.fields[NAME].input, "notes", "the typed name must survive");
        assert_eq!(p.fields[COMMAND].input, "nvim", "and so must the command");
        assert_eq!(p.fields[NAME].default, "session 4");
        assert!(p.message.is_some());
    }

    #[test]
    fn a_message_is_cleared_by_the_next_keypress() {
        let mut p = prompt();
        p.fail("nope".to_string(), Some("session 3".to_string()));
        p.on_key(Key::Char('x'));
        assert!(p.message.is_none());
    }

    // --- moving between fields ---------------------------------------------

    #[test]
    fn tab_and_the_arrows_move_between_the_fields_and_wrap() {
        let mut p = prompt();
        assert_eq!(p.focus, NAME);
        p.on_key(Key::Tab);
        assert_eq!(p.focus, COMMAND);
        p.on_key(Key::Tab);
        assert_eq!(p.focus, NAME, "two fields, so tab wraps");
        p.on_key(Key::BackTab);
        assert_eq!(p.focus, COMMAND);
        p.on_key(Key::Down);
        assert_eq!(p.focus, NAME);
        p.on_key(Key::Up);
        assert_eq!(p.focus, COMMAND);
    }

    /// A rename has one field; moving within it must be a no-op rather than an
    /// index off the end.
    #[test]
    fn moving_focus_on_a_one_field_prompt_stays_put() {
        let mut p = renaming("dotfiles");
        for key in [Key::Tab, Key::BackTab, Key::Up, Key::Down] {
            p.on_key(key);
            assert_eq!(p.focus, NAME, "{key:?}");
        }
    }

    #[test]
    fn typing_goes_to_the_focused_field_only() {
        let mut p = prompt();
        p.on_key(Key::Tab);
        type_in(&mut p, "nvim");
        assert_eq!(p.fields[NAME].input, "");
        assert_eq!(p.fields[COMMAND].input, "nvim");
    }

    // --- editing within a field --------------------------------------------

    /// A command line is long and usually wants a small change in the middle,
    /// which append-and-backspace could only reach by deleting everything after
    /// it.
    #[test]
    fn the_cursor_moves_and_text_is_inserted_where_it_is() {
        let mut p = prompt();
        type_in(&mut p, "nvim {sock}");
        press(&mut p, &[Key::Left; 7]);
        type_in(&mut p, " -u NONE");
        assert_eq!(p.fields[NAME].input, "nvim -u NONE {sock}");

        press(&mut p, &[Key::Home]);
        type_in(&mut p, "x");
        assert_eq!(p.fields[NAME].input, "xnvim -u NONE {sock}");

        press(&mut p, &[Key::End]);
        type_in(&mut p, "!");
        assert_eq!(p.fields[NAME].input, "xnvim -u NONE {sock}!");
    }

    #[test]
    fn backspace_deletes_before_the_cursor_not_at_the_end() {
        let mut p = prompt();
        type_in(&mut p, "abcd");
        press(&mut p, &[Key::Left, Key::Left, Key::Backspace]);
        assert_eq!(p.fields[NAME].input, "acd");
    }

    #[test]
    fn the_cursor_stops_at_both_ends() {
        let mut p = prompt();
        type_in(&mut p, "ab");
        press(&mut p, &[Key::Left; 5]);
        assert_eq!(p.fields[NAME].cursor, 0);
        press(&mut p, &[Key::Right; 5]);
        assert_eq!(p.fields[NAME].cursor, 2);
    }

    /// Cursor arithmetic is in characters, so a multi-byte name cannot put an
    /// insertion mid-glyph and panic.
    #[test]
    fn editing_a_multibyte_name_does_not_split_a_character() {
        let mut p = prompt();
        type_in(&mut p, "日本語");
        press(&mut p, &[Key::Left, Key::Backspace]);
        assert_eq!(p.fields[NAME].input, "日語");
        type_in(&mut p, "🎉");
        assert_eq!(p.fields[NAME].input, "日🎉語");
    }

    /// The affordance the long command line needs: one keypress to edit the
    /// default rather than retype it. Typing anything else still replaces it.
    #[test]
    fn right_or_end_takes_the_default_into_an_empty_field() {
        for key in [Key::Right, Key::End] {
            let mut p = prompt();
            p.on_key(Key::Tab);
            p.on_key(key);
            let field = &p.fields[COMMAND];
            assert_eq!(field.input, COMMAND_DEFAULT, "{key:?}");
            assert_eq!(field.cursor, field.len(), "{key:?}: cursor at the end");

            // …and now it is ordinary text, editable from the end.
            press(&mut p, &[Key::Backspace; 6]);
            type_in(&mut p, "{sock} --clean");
            assert_eq!(
                p.fields[COMMAND].input,
                "nvim --headless --listen {sock} --clean"
            );
        }
    }

    #[test]
    fn a_field_with_text_in_it_moves_the_cursor_instead() {
        let mut p = prompt();
        type_in(&mut p, "ab");
        press(&mut p, &[Key::Home, Key::Right]);
        assert_eq!(p.fields[NAME].input, "ab", "the default must not intrude");
        assert_eq!(p.fields[NAME].cursor, 1);
    }

    // --- defaults ----------------------------------------------------------

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

    /// Duplicates are rejected case-insensitively, so a default differing only
    /// in case would be offered and then refused.
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
    fn backspacing_to_empty_brings_the_placeholder_back() {
        let mut p = prompt();
        type_in(&mut p, "ab");
        press(&mut p, &[Key::Backspace, Key::Backspace]);
        let lines = render(&p, 60, 9);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("new session name: session 3")),
            "expected the placeholder back, got {lines:?}"
        );
    }

    // --- layout ------------------------------------------------------------

    /// The bug this layout exists to avoid: if the default starts one column
    /// over from where typing lands, the field jumps on the first keystroke.
    #[test]
    fn the_default_sits_where_the_first_keystroke_will_land() {
        let mut typed = prompt();
        type_in(&mut typed, "n");

        assert_eq!(
            field_column(&prompt(), 60, 9),
            field_column(&typed, 60, 9),
            "the default and the first typed character must share a column"
        );
    }

    /// Both fields' values start in the same column, so the cursor does not
    /// jump sideways when focus moves between them.
    #[test]
    fn the_two_fields_line_up() {
        let mut on_command = prompt();
        on_command.on_key(Key::Tab);
        assert_eq!(
            field_column(&prompt(), 60, 9),
            field_column(&on_command, 60, 9)
        );
    }

    /// Four roles on one line, told apart by modifier alone since nothing here
    /// sets a colour.
    #[test]
    fn the_label_leads_the_cursor_marks_the_field_and_the_default_recedes() {
        let label = "new session name: ".width();

        let empty = line_cells(&prompt(), 60, 9);
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
        let typed = line_cells(&p, 60, 9);
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

    /// Two cursors would leave no way to tell which field a keystroke reaches.
    #[test]
    fn only_the_focused_field_shows_a_cursor() {
        for p in [prompt(), {
            let mut p = prompt();
            p.on_key(Key::Tab);
            p
        }] {
            let reversed: usize = cells_of(&p, 60, 9)
                .iter()
                .map(|row| {
                    row.iter()
                        .filter(|(_, m)| m.contains(Modifier::REVERSED))
                        .count()
                })
                .sum();
            assert_eq!(reversed, 1, "exactly one cell is the cursor");
        }
    }

    #[test]
    fn the_command_field_shows_the_command_it_would_run() {
        let lines = render(&prompt(), 70, 9);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("nvim command:     nvim --headless --listen {sock}")),
            "expected the command default under the name, got {lines:?}"
        );
    }

    #[test]
    fn the_keybindings_are_on_the_last_row_and_do_not_change() {
        let empty = render(&prompt(), 60, 9);
        let mut p = prompt();
        type_in(&mut p, "notes");
        let typed = render(&p, 60, 9);

        assert!(
            empty[8].contains("⏎ create")
                && empty[8].contains("⇥ field")
                && empty[8].contains("esc cancel"),
            "expected the hints on the last row, got {:?}",
            empty[8]
        );
        assert_eq!(
            empty[8], typed[8],
            "the hint row says the same thing either way — the placeholders \
             carry the defaults, not the hints"
        );
    }

    #[test]
    fn a_rejected_submission_is_reported_under_the_fields() {
        let mut p = prompt();
        type_in(&mut p, "notes");
        p.fail(
            "a session named \"notes\" already exists".to_string(),
            Some("session 4".to_string()),
        );
        let lines = render(&p, 60, 9);
        let field = lines
            .iter()
            .position(|l| l.contains("new session name: notes"))
            .expect("the field should still show what was typed");
        assert!(
            lines[field + 2].contains("already exists"),
            "expected the error below both fields, got {lines:?}"
        );
    }

    #[test]
    fn a_value_longer_than_the_field_keeps_the_cursor_visible() {
        let mut p = prompt();
        p.on_key(Key::Tab);
        p.on_key(Key::End);
        type_in(&mut p, " --clean");

        let cells = line_cells(&p, 40, 9);
        assert!(
            text_of(&cells).ends_with("--clean "),
            "the end being edited must stay on screen: {cells:?}"
        );

        // …and moving back through it scrolls the other way.
        press(&mut p, &[Key::Home]);
        let cells = line_cells(&p, 40, 9);
        assert!(
            text_of(&cells).contains("nvim --head"),
            "the start must come back into view: {cells:?}"
        );
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let mut typed = prompt();
        type_in(&mut typed, "a-fairly-long-session-name");
        typed.on_key(Key::Tab);
        typed.on_key(Key::End);
        let mut failed = prompt();
        failed.fail(
            "something went wrong".to_string(),
            Some("session 3".to_string()),
        );

        for &(w, h) in test_support::TINY_SIZES {
            for p in [&prompt(), &typed, &failed, &renaming("dotfiles")] {
                let _ = render(p, w.max(1), h.max(1));
            }
        }
    }

    // --- renaming ----------------------------------------------------------

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

    /// A rename starts nothing, so it must not ask how to start it.
    #[test]
    fn the_rename_prompt_has_no_command_field() {
        let mut p = renaming("dotfiles");
        assert_eq!(p.fields.len(), 1);
        assert_eq!(submitted(&mut p).command, None);
        let lines = render(&renaming("dotfiles"), 60, 9);
        assert!(
            !lines.iter().any(|l| l.contains("nvim command")),
            "a rename must not offer a command field: {lines:?}"
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
        assert_eq!(p.fields[NAME].input, "");

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
            submitted(&mut p).name,
            "dotfiles",
            "enter on a cleared rename keeps the name it had"
        );
    }

    /// A rejected rename must not move the default the way a rejected create
    /// does — the name the session already has has not changed.
    #[test]
    fn a_rejected_rename_keeps_offering_the_current_name() {
        let mut p = renaming("dotfiles");
        p.fail("a session named \"notes\" already exists".to_string(), None);
        assert_eq!(p.fields[NAME].default, "dotfiles");
    }

    /// The cursor is the only sign a keystroke landed, so a label long enough to
    /// fill the line must not push it off the edge.
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

    /// No SGR colour, so the screen is correct under NO_COLOR by construction.
    #[test]
    fn nothing_sets_a_colour() {
        let mut p = prompt();
        type_in(&mut p, "notes");
        p.on_key(Key::Tab);
        p.on_key(Key::End);
        p.fail("nope".to_string(), Some("session 3".to_string()));
        test_support::assert_no_colour(60, 9, |f| draw(f, &p));
    }
}
