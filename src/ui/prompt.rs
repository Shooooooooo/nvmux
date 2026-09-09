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
//!     new session name:  session 3
//!     nvim command:      nvim --headless --listen {sock}
//!     working directory: /home/you/pro
//!     └─ bold             │ └─ dim
//!                         └─ the cursor, an inverted cell
//!                        ▸ projects        ← the menu, once `Tab` asks for it:
//!                          prototypes         reversed and bold, the picker's
//!                                             "selected row"
//! ```
//!
//! # One screen, three fields
//!
//! Creating asks three questions — what the session is called, how its Neovim is
//! started, and where it runs — and asks them together rather than one after the
//! other. Enter submits the whole form from whichever field it is pressed in, so
//! `<prefix> c` followed by enter still creates a session in one keystroke and
//! the second and third questions cost nothing to anyone who does not want them.
//! Renaming puts up the same screen with one field, since a rename starts
//! nothing.
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
//!
//! # One key, one job
//!
//! | Key | Job |
//! |---|---|
//! | `Tab` | completion, and nothing else |
//! | `↑` `↓` `Ctrl-p` `Ctrl-n` | move between the fields — or through the menu while it is open |
//! | `Enter` | create the session, always |
//! | `Esc` | close the menu if it is open, otherwise leave |
//!
//! Two earlier schemes failed here, and both failed the same way: a key that did
//! one thing or another depending on state nobody could see. So nothing is
//! conditional on anything invisible now. What the movement keys move through
//! depends on whether a menu is on screen, which you can see; `Tab` and `Enter`
//! and `Esc` each mean one thing wherever you press them.
//!
//! # The working directory has a menu, when you ask for it
//!
//! The third field is the one question with an answer nvmux can look up, so
//! `Tab` looks it up: a dropdown of the directories the field could become,
//! ranked fuzzily, so `nvmx` finds `nvmux-rs` and you need not know how a
//! directory starts to reach it. Nothing else shows it — the prompt opens as
//! three plain fields — and `Esc` puts it away again.
//!
//! `Tab` from inside the menu takes what is highlighted and writes it *without* a
//! trailing slash, so the path reads exactly as it would have been typed. The
//! next `Tab` appends the slash and opens the level below. That pairing is what
//! makes one key enough to walk down a tree: `Tab`, choose, `Tab`, `Tab`, choose,
//! `Tab` — and the `/` is never typed by hand.
//!
//! Stepping in is remembered from the accept rather than read off the path, and
//! deliberately: a directory whose name is also a prefix of its siblings — `pro`
//! beside `projects` — must not be stepped into merely because it exists. Only
//! having just chosen it means that.
//!
//! # Enter creates
//!
//! From any field, with the menu open or shut, and with a suggestion highlighted
//! or not: enter creates the session with the fields as they stand. So
//! `<prefix> c` then enter is a session in one keystroke, in the home directory,
//! and a half-typed query is refused by name rather than quietly becoming
//! whichever directory happened to be under the cursor.
//!
//! # Nothing waits for a keystroke
//!
//! The listing runs on a worker thread and reaches this loop through a channel;
//! [`super::complete`] is where that lives and why. It starts the moment the
//! prompt opens, so the first `Tab` is instant rather than paying a round trip to
//! say its first word. One listing serves a whole directory — the host stopped
//! filtering when the matching became fuzzy, since a subsequence cannot be a glob
//! — so a path costs about one round trip per `/` and everything in between is
//! answered from memory.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::app::Key;
use super::complete;
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
/// Where the working directory lives. Only [`Task::Create`] has one.
const DIRECTORY: usize = 2;

/// The tallest the dropdown gets. Enough to choose from without the form
/// climbing to the top of the screen to make room for it.
const MENU_ROWS: u16 = 6;

/// The highlighted row's marker, and the column the rest line up in — the
/// picker's, so the two lists read the same way.
const MARKER: &str = "▸ ";
const INDENT: &str = "  ";

/// What enter asked for.
#[derive(Debug, PartialEq, Eq)]
struct Submission {
    name: String,
    /// `None` when renaming, which starts nothing.
    command: Option<String>,
    /// `None` when renaming, for the same reason.
    directory: Option<String>,
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
    message: Option<String>,
    /// The dropdown under the fields. `None` on a prompt that has no directory
    /// to complete — a rename — and otherwise always present, empty or not,
    /// because the rows it occupies are reserved either way.
    menu: Option<Menu>,
}

/// The dropdown: the directories the working directory field could become, and
/// which of them is under the cursor.
///
/// The matches are kept rather than a finished block, because how many rows fit
/// is a question about the terminal, and the terminal is not known until the
/// frame is drawn.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Menu {
    /// What the query could mean, best first, as the scorer ranked it.
    matches: Vec<String>,
    /// The highlighted row. Always in range while `matches` is non-empty.
    selected: usize,
    /// Whether the menu is on screen. Nothing opens it but `Tab`, so the prompt
    /// is three plain fields until asked for suggestions.
    open: bool,
    /// The last thing `Tab` did was accept, so the next one steps *into* what it
    /// accepted rather than reopening on the same query. Cleared by every other
    /// key.
    ///
    /// A flag rather than a look at the path, because a directory whose name is
    /// also a prefix of its siblings — `pro` beside `projects` — must not be
    /// stepped into merely because it happens to exist. Only having just chosen
    /// it means that.
    stepped: bool,
    /// The host stopped short of listing the directory, so these are part of an
    /// answer rather than all of one.
    partial: bool,
    /// Said instead of the rows: waiting for an answer, or nothing matched.
    message: String,
}

impl Menu {
    /// Wrapping, like every other list in nvmux: one key reaches the far end of
    /// a short list rather than stopping dead at it.
    fn move_by(&mut self, delta: isize) {
        let len = self.matches.len();
        if len == 0 {
            return;
        }
        let len = len as isize;
        self.selected = (((self.selected as isize + delta) % len + len) % len) as usize;
    }

    fn highlighted(&self) -> Option<&str> {
        self.matches.get(self.selected).map(String::as_str)
    }
}

impl Prompt {
    fn create(default_name: String, default_command: String, default_directory: String) -> Self {
        let labels = aligned(&["new session name", "nvim command", "working directory"]);
        Self {
            fields: vec![
                Field::new(labels[0].clone(), String::new(), default_name),
                Field::new(labels[1].clone(), String::new(), default_command),
                Field::new(labels[2].clone(), String::new(), default_directory),
            ],
            focus: NAME,
            message: None,
            menu: Some(Menu::default()),
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
            message: None,
            menu: None,
        }
    }

    /// The bottom row, and the only thing on screen that says which keys are
    /// live. Every key here has one job; what changes is whether the movement
    /// keys are moving through fields or through suggestions.
    fn hints(&self) -> &'static str {
        if self.menu.is_none() {
            return "⏎ rename   esc cancel";
        }
        if self.choosing() {
            "⇥ accept   ↑↓ ^n ^p choose   esc close   ⏎ create"
        } else {
            "⏎ create   ↑↓ field   ⇥ complete   esc cancel"
        }
    }

    /// Is the menu on screen? Then it owns the movement keys, and `Tab` accepts
    /// from it rather than opening it.
    fn choosing(&self) -> bool {
        self.focus == DIRECTORY && self.menu.as_ref().is_some_and(|m| m.open)
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
            directory: self.fields.get(DIRECTORY).map(Field::value),
        }
    }

    fn on_key(&mut self, key: Key) -> Step {
        // A stale message must not linger over an unrelated action.
        self.message = None;

        // Whatever is open owns the movement keys. That is the whole of the
        // scheme: `Tab` completes, the movement keys move through whichever list
        // is in front of you, enter creates, Esc backs out of one thing at a
        // time. No key here does two jobs depending on state nobody can see.
        let choosing = self.choosing();

        match key {
            Key::Char(c) => {
                // The directory field inverts the rule the other two follow. In
                // them the default is a value being *replaced*, so typing
                // discards it; here it is a directory being navigated *from*, so
                // the first keystroke takes it in and the rest reads as a query
                // inside it. Typing `pro` on an untouched field means `~/pro`
                // rather than a relative path the prompt would go on to refuse.
                //
                // Unless that keystroke is `/`, which is how you say "somewhere
                // else entirely" and start an absolute path from nothing.
                if self.focus == DIRECTORY && self.fields[DIRECTORY].input.is_empty() && c != '/' {
                    self.fields[DIRECTORY].adopt_default();
                }
                self.focused().insert(c);
                // An open menu narrows rather than closing: filtering it is what
                // the query is for. `show` puts the selection back on the best
                // match when the matches change.
                self.stayed_put();
                Step::None
            }
            Key::Backspace => {
                self.focused().backspace();
                self.stayed_put();
                Step::None
            }
            Key::Left => {
                self.focused().left();
                self.stayed_put();
                Step::None
            }
            Key::Right => {
                self.focused().right();
                self.stayed_put();
                Step::None
            }
            Key::Home => {
                self.focused().home();
                self.stayed_put();
                Step::None
            }
            Key::End => {
                self.focused().end();
                self.stayed_put();
                Step::None
            }

            // `Tab` is for completion and for nothing else — it does not move
            // between fields, and in the two fields with nothing to complete it
            // does nothing at all.
            Key::Tab if self.menu.is_some() && self.focus == DIRECTORY => {
                if choosing {
                    self.accept();
                } else {
                    self.open_menu();
                }
                Step::None
            }

            Key::Down | Key::CtrlN if choosing => {
                self.menu().move_by(1);
                Step::None
            }
            Key::Up | Key::CtrlP if choosing => {
                self.menu().move_by(-1);
                Step::None
            }
            Key::Down | Key::CtrlN => {
                self.move_focus(1);
                self.stayed_put();
                Step::None
            }
            Key::Up | Key::CtrlP => {
                self.move_focus(self.fields.len() - 1);
                self.stayed_put();
                Step::None
            }

            // Always, from any field, with the menu open or shut. It is the one
            // thing enter means, so `c` then enter is still a session in one
            // keystroke — and a half-typed query is refused by name rather than
            // quietly becoming whichever suggestion happened to be highlighted.
            Key::Enter => Step::Submit(self.submission()),

            // One thing at a time on the way out: the menu you opened, then the
            // prompt. The picker's filter mode sets the same precedent, and
            // without it a menu opened by accident could only be dismissed by
            // choosing something out of it.
            Key::Esc if choosing => {
                self.close_menu();
                Step::None
            }
            // Not a quit here: it would tear the user out of a live session
            // they only meant to leave a prompt in.
            Key::Esc | Key::CtrlC => Step::Cancel,
            _ => Step::None,
        }
    }

    /// Open the menu on whatever the field is pointing at.
    ///
    /// After an accept, that means *inside* the directory just chosen, so the
    /// `/` is appended here rather than typed. This is the second half of the
    /// pairing that lets `Tab` walk down a tree: accept writes the name, and the
    /// next `Tab` steps into it.
    fn open_menu(&mut self) {
        let stepped = self.menu.as_ref().is_some_and(|m| m.stepped);
        if stepped {
            let field = &mut self.fields[DIRECTORY];
            let path = field.value();
            if !path.ends_with('/') {
                field.input = format!("{path}/");
                field.cursor = field.len();
            }
        }
        if let Some(menu) = self.menu.as_mut() {
            menu.open = true;
            menu.stepped = false;
            menu.selected = 0;
        }
    }

    fn close_menu(&mut self) {
        if let Some(menu) = self.menu.as_mut() {
            menu.open = false;
            menu.stepped = false;
            menu.matches.clear();
            menu.message.clear();
        }
    }

    /// Any key that was not an accept. `stepped` is only ever true for the one
    /// keystroke that follows one.
    fn stayed_put(&mut self) {
        if let Some(menu) = self.menu.as_mut() {
            menu.stepped = false;
        }
    }

    /// The menu, which every caller here has already established is there.
    fn menu(&mut self) -> &mut Menu {
        self.menu.as_mut().expect("a menu the caller checked for")
    }

    /// Take the highlighted directory into the field, and close the menu.
    ///
    /// The name alone, with no trailing `/`: the path then reads exactly as it
    /// would if it had been typed, and the slash is the next `Tab`'s job — which
    /// is what makes one key both "complete this" and "now go inside it".
    fn accept(&mut self) {
        let Some(name) = self
            .menu
            .as_ref()
            .and_then(Menu::highlighted)
            .map(str::to_string)
        else {
            return;
        };
        let field = &mut self.fields[DIRECTORY];
        // Against the value, not the input: an untouched field is showing its
        // default, and choosing from it means choosing inside that.
        let current = field.value();
        let parent = crate::dirs::split(&current).map_or(current.clone(), |(d, _)| d.to_string());
        let parent = parent.trim_end_matches('/');
        field.input = format!("{parent}/{name}");
        field.cursor = field.len();

        if let Some(menu) = self.menu.as_mut() {
            menu.open = false;
            menu.stepped = true;
            menu.matches.clear();
            menu.message.clear();
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
pub fn run(transport: &dyn Transport) -> Result<Outcome> {
    super::owning(|terminal| run_on(terminal, transport, Task::Create))
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
        Task::Create => Prompt::create(
            next_free_name(transport)?,
            default_command(transport),
            default_directory(transport),
        ),
        Task::Rename(session) => Prompt::rename(session),
    };

    // Only a create has a directory to complete, and only a create pays for the
    // worker. Dropped with the prompt, which is what ends it — see
    // `super::complete`.
    let mut completer = match task {
        Task::Create => Some(complete::Completer::new(transport.dir_source())),
        Task::Rename(_) => None,
    };
    // Ask about the default before a key is pressed. The answer is then usually
    // already in hand when the first one is, which is the difference between the
    // field feeling instant and it paying a round trip to say its first word.
    refresh(&mut prompt, completer.as_mut());

    loop {
        terminal.draw(|f| draw(f, &prompt))?;

        // A shorter wait only while an answer is outstanding: an idle prompt is
        // back on the ordinary tick and costs nothing.
        let waiting = completer.as_ref().is_some_and(complete::Completer::waiting);
        let tick = if waiting {
            super::BUSY_TICK
        } else {
            super::TICK
        };

        let Some(key) = super::poll_key_for(tick)? else {
            // Nothing was typed, so nothing new is being asked. What may have
            // arrived is an answer to what was.
            if completer.as_mut().is_some_and(complete::Completer::poll) {
                show(&mut prompt, completer.as_mut());
            }
            continue;
        };

        let step = prompt.on_key(key);
        // After the key, not before: `→` may have taken the suggestion, which
        // makes the input a new question.
        refresh(&mut prompt, completer.as_mut());

        let submission = match step {
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
                let typed = submission.directory.unwrap_or_default();
                Launch::parse(&line)
                    .map_err(Into::into)
                    .and_then(|launch| {
                        // Absolute by here, with any leading `~` already
                        // expanded against the *session host's* home — see
                        // `session::validate_directory`. Whether it exists is
                        // the spawn script's question.
                        let directory =
                            crate::session::validate_directory(&typed, transport.home())?;
                        transport
                            .create_session(&submission.name, &launch, &directory)
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

/// Put the directory field's current text to the completer, and show whatever
/// it can already answer.
///
/// Called after every key. It does no I/O: [`complete::Completer::ask`] looks in
/// memory and, failing that, posts a question for the worker — which is the
/// whole reason a keystroke is never slower than a keystroke.
///
/// What is offered follows the *value*, not the input, so an untouched field
/// completes inside the home directory it is showing rather than inside nothing.
fn refresh(prompt: &mut Prompt, completer: Option<&mut complete::Completer>) {
    let Some(completer) = completer else {
        return;
    };
    let typed = prompt.fields[DIRECTORY].value();
    completer.ask(&typed);
    show(prompt, Some(completer));
}

/// Copy what the completer is offering onto the screen: the suggestion after
/// the cursor, and the alternatives on the row below.
///
/// The ghost goes on the field only while that field is where the keystrokes
/// are going. Offering to extend a field the cursor is not in would be offering
/// something `→` would not do.
fn show(prompt: &mut Prompt, completer: Option<&mut complete::Completer>) {
    let Some(completer) = completer else {
        return;
    };
    // Nothing is matched, ranked or drawn until `Tab` has asked for it. The
    // listing still runs in the background regardless, so that first `Tab` is
    // instant rather than paying a round trip to say its first word.
    if !prompt.choosing() {
        if let Some(menu) = prompt.menu.as_mut() {
            menu.matches.clear();
            menu.message.clear();
        }
        return;
    }

    // Against the value rather than the input, matching what was asked: an
    // untouched field is showing its default, and completing it means completing
    // inside that.
    let typed = prompt.fields[DIRECTORY].value();
    let query = crate::dirs::split(&typed)
        .map_or("", |(_, q)| q)
        .to_string();

    let waiting = completer.waiting();
    let partial = completer.truncated();
    let matches = completer.matches(&query);

    let message = if waiting && matches.is_empty() {
        // Only when there is nothing to show. A cached listing answers instantly
        // while a fresh one is still in flight, and replacing real matches with
        // an ellipsis would be a step backwards.
        "…".to_string()
    } else if matches.is_empty() {
        // Said plainly rather than left blank: an open menu showing nothing
        // looks broken, and a query that matches nothing looks exactly like one
        // still being typed.
        if query.is_empty() {
            "nothing here to choose from".to_string()
        } else {
            format!("nothing here matches {query:?}")
        }
    } else {
        String::new()
    };

    let menu = prompt.menu.as_mut().expect("checked by `choosing`");
    // The best match, whenever the set of matches changes. Holding the index
    // still would point it at a different directory each keystroke, which is a
    // worse kind of stable.
    if menu.matches != matches {
        menu.selected = 0;
    }
    menu.matches = matches;
    menu.partial = partial;
    menu.message = message;
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
/// The directory a session gets when the user just presses enter: the session
/// host's home, with a trailing `/`.
///
/// The slash is not decoration. It is what makes the field's text mean "inside
/// this directory" rather than "this directory, among its siblings" — so the
/// menu opens listing the home directory's children, which is what someone
/// tabbing into the field wants to see, and choosing one descends rather than
/// stepping sideways. It is also what keeps enter submitting: with nothing after
/// the slash there is no half-typed name for the menu to resolve.
///
/// Empty stays empty. A host that could not say where home is has no default to
/// offer, and `/` alone would be a confident wrong answer.
fn default_directory(transport: &dyn Transport) -> String {
    let home = transport.home().trim_end_matches('/');
    if home.is_empty() {
        String::new()
    } else {
        format!("{home}/")
    }
}

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
    draw::draw_hint_row(frame, bottom, prompt.hints(), true);
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
    // The anchor counts the fields and the message and *not* the menu, so the
    // form sits in exactly the same place whether the menu is open or shut. A
    // block sized to include it would jump up the screen on every `Tab`, and one
    // that reserved its rows against that would leave a permanent blank gap
    // under a closed menu. Neither is worth having when the menu can simply hang
    // in the space below.
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

    let mut y = anchor.y + rows;

    // Routinely wider than the fields, so it gets the full width and its own
    // centring rather than hanging off the anchor. Drawn before the menu because
    // it belongs to the form: a rejected create must not be what a tall menu
    // pushes off the screen.
    if let Some(msg) = prompt.message.as_deref() {
        frame.render_widget(
            Paragraph::new(Line::from(draw::truncate(msg, area.width as usize)))
                .alignment(Alignment::Center),
            Rect {
                y,
                height: 1,
                ..area
            },
        );
        y += 1;
    }

    if let Some(menu) = prompt.menu.as_ref().filter(|m| m.open) {
        // Indented to where the field values start, so the menu reads as hanging
        // from the directory field rather than floating under the form.
        let indent = prompt
            .fields
            .get(DIRECTORY)
            .map_or(0, |f| f.label.width() as u16);
        // Whatever room is left under the form, capped so the menu cannot fill a
        // tall terminal end to end.
        let block = Rect {
            x: anchor.x.saturating_add(indent).min(area.x + area.width),
            y,
            width: width.saturating_sub(indent as usize) as u16,
            height: MENU_ROWS.min((area.y + area.height).saturating_sub(y)),
        };
        draw_menu(frame, menu, block);
    }
}

/// The dropdown, one directory per row, the highlighted one reversed.
///
/// `REVERSED | BOLD` is already what "the selected row in a list" means in the
/// picker, and it is deliberately not the field cursor's plain `REVERSED` — two
/// reversed things on one screen have to be told apart, and this is how the rest
/// of nvmux tells them apart.
fn draw_menu(frame: &mut Frame, menu: &Menu, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let dim = Style::default().add_modifier(Modifier::DIM);

    if !menu.message.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                draw::truncate(&menu.message, area.width as usize),
                dim,
            ))),
            Rect { height: 1, ..area },
        );
        return;
    }

    let height = area.height as usize;
    let offset = draw::scroll_offset(menu.selected, menu.matches.len(), height);
    let mut lines: Vec<Line> = menu
        .matches
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, name)| {
            let selected = i == menu.selected;
            let style = if selected {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                dim
            };
            let text = draw::truncate(
                &format!("{}{name}", if selected { MARKER } else { INDENT }),
                area.width as usize,
            );
            Line::from(Span::styled(text, style))
        })
        .collect();

    // The host stopped short, so the last row says so rather than letting a
    // partial list read as a complete one. It costs a match to say it, which is
    // the right trade: a list that is quietly missing entries is worse than a
    // list that is one shorter.
    if menu.partial && lines.len() == height && height > 0 {
        lines.pop();
        lines.push(Line::from(Span::styled(
            draw::truncate(&format!("{INDENT}(and more)"), area.width as usize),
            dim,
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
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
/// keep the cursor visible, so when a path outgrows it the text scrolls rather
/// than the part being edited disappearing. The cursor may sit one past the end,
/// so the line is treated as the input plus a trailing blank — which is the cell
/// it inverts there.
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
    const DIRECTORY_DEFAULT: &str = "/home/you/";

    fn prompt() -> Prompt {
        Prompt::create(
            "session 3".to_string(),
            COMMAND_DEFAULT.to_string(),
            DIRECTORY_DEFAULT.to_string(),
        )
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

    /// The zero-keystroke path `<prefix> c` used to be has to survive every
    /// field added since: enter on an untouched form means "every placeholder".
    #[test]
    fn enter_on_an_untouched_form_submits_every_default() {
        let mut p = prompt();
        assert_eq!(
            p.on_key(Key::Enter),
            Step::Submit(Submission {
                name: "session 3".to_string(),
                command: Some(COMMAND_DEFAULT.to_string()),
                directory: Some(DIRECTORY_DEFAULT.to_string()),
            })
        );
    }

    /// And from the other field too, so tabbing over to look at the command
    /// does not cost an extra keystroke to get back.
    #[test]
    fn enter_submits_the_whole_form_from_either_field() {
        let mut p = prompt();
        type_in(&mut p, "notes");
        p.on_key(Key::Down);
        type_in(&mut p, "nvim -u NONE --listen {sock}");
        assert_eq!(p.focus, COMMAND);
        assert_eq!(
            submitted(&mut p),
            Submission {
                name: "notes".to_string(),
                command: Some("nvim -u NONE --listen {sock}".to_string()),
                directory: Some(DIRECTORY_DEFAULT.to_string()),
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
        p.on_key(Key::Down);
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

    /// A rename has one field; moving within it must be a no-op rather than an
    /// index off the end.
    #[test]
    fn moving_focus_on_a_one_field_prompt_stays_put() {
        let mut p = renaming("dotfiles");
        for key in [Key::Up, Key::Down, Key::CtrlN, Key::CtrlP] {
            p.on_key(key);
            assert_eq!(p.focus, NAME, "{key:?}");
        }
    }

    #[test]
    fn typing_goes_to_the_focused_field_only() {
        let mut p = prompt();
        p.on_key(Key::Down);
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
            p.on_key(Key::Down);
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
                .any(|l| l.contains("new session name:  session 3")),
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
    fn the_three_fields_line_up() {
        let base = field_column(&prompt(), 60, 9);
        for tabs in 1..3 {
            let mut p = prompt();
            for _ in 0..tabs {
                p.on_key(Key::Down);
            }
            assert_eq!(field_column(&p, 60, 9), base, "field {tabs} is out of line");
        }
    }

    /// A prompt on the directory field with the menu open and filled in — the
    /// state every dropdown test below wants, without a worker to fill it.
    ///
    /// The focus moves with the movement keys and the menu is opened with `Tab`,
    /// which is the real route in: a helper that reached past them could pass
    /// while the keys that get you here were broken.
    fn menuing(typed: &str, matches: &[&str]) -> Prompt {
        let mut p = at_directory(typed);
        p.on_key(Key::Tab);
        assert!(p.choosing(), "Tab should have opened the menu");
        fill(&mut p, matches);
        p
    }

    /// The directory field, focused, with `typed` in it and no menu.
    fn at_directory(typed: &str) -> Prompt {
        let mut p = prompt();
        p.on_key(Key::Down);
        p.on_key(Key::Down);
        assert_eq!(p.focus, DIRECTORY);
        type_in(&mut p, typed);
        p
    }

    /// What the completer would have put there.
    fn fill(p: &mut Prompt, matches: &[&str]) {
        let menu = p.menu.as_mut().expect("a create prompt has a menu");
        menu.matches = matches.iter().map(|m| m.to_string()).collect();
        menu.selected = 0;
    }

    /// Four roles on one line, told apart by modifier alone since nothing here
    /// sets a colour.
    #[test]
    fn the_label_leads_the_cursor_marks_the_field_and_the_default_recedes() {
        let label = "new session name:  ".width();

        let empty = line_cells(&prompt(), 60, 9);
        assert_eq!(text_of(&empty), "new session name:  session 3");
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
        assert_eq!(text_of(&typed), "new session name:  notes ");
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
        for p in [
            prompt(),
            {
                let mut p = prompt();
                p.on_key(Key::Down);
                p
            },
            // With a menu on screen too: its highlight is reversed as well, and
            // the two must still be told apart.
            menuing("/home/", &["alpha", "beta"]),
        ] {
            // The field cursor is plain REVERSED; a selected row is
            // REVERSED|BOLD. That is the distinction `field_line` documents, and
            // it is what keeps two reversed things on one screen legible.
            let cursors: usize = cells_of(&p, 60, 9)
                .iter()
                .map(|row| {
                    row.iter()
                        .filter(|(_, m)| {
                            m.contains(Modifier::REVERSED) && !m.contains(Modifier::BOLD)
                        })
                        .count()
                })
                .sum();
            assert_eq!(cursors, 1, "exactly one cell is the field cursor");
        }
    }

    /// A menu with more matches than rows keeps the selection on screen, the
    /// same rule the picker's list follows and through the same function.
    #[test]
    fn a_menu_longer_than_its_rows_scrolls_to_keep_the_selection_visible() {
        let names: Vec<String> = (0..30).map(|i| format!("dir-{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut p = menuing("/home/", &refs);

        // Down past the bottom of the reserved rows.
        for _ in 0..12 {
            p.on_key(Key::Down);
        }
        let lines = render(&p, 60, 14);
        assert!(
            lines.iter().any(|l| l.contains("dir-12")),
            "the selection scrolled out of view: {lines:#?}"
        );

        // And wrapping to the end brings the far end into view.
        p.on_key(Key::Up);
        p.on_key(Key::Up);
        let lines = render(&p, 60, 14);
        assert!(
            lines.iter().any(|l| l.contains("dir-10")),
            "expected to be looking at the selection: {lines:#?}"
        );
    }

    /// A partial listing must not read as a complete one. It costs a row to say
    /// so, which is the right trade: a list quietly missing entries is worse
    /// than a list one shorter.
    #[test]
    fn a_partial_listing_says_so_rather_than_ending_quietly() {
        let names: Vec<String> = (0..30).map(|i| format!("dir-{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut p = menuing("/home/", &refs);
        p.menu.as_mut().expect("a menu").partial = true;

        let lines = render(&p, 60, 14);
        assert!(
            lines.iter().any(|l| l.contains("(and more)")),
            "expected the menu to admit it is partial: {lines:#?}"
        );
    }

    /// A query matching nothing is said plainly. Left blank it looks exactly
    /// like a path still being typed, and the difference is otherwise only
    /// discovered by pressing enter.
    #[test]
    fn a_query_that_matches_nothing_says_so() {
        let mut p = menuing("/home/zzz", &[]);
        p.menu.as_mut().expect("a menu").message = "nothing here matches \"zzz\"".to_string();
        let lines = render(&p, 60, 14);
        assert!(
            lines.iter().any(|l| l.contains("nothing here matches")),
            "expected the menu to say so: {lines:#?}"
        );
    }

    /// A rename has no directory to complete, so it has no menu and no rows
    /// reserved for one — a blank block under a one-field prompt would be odd.
    #[test]
    fn a_rename_has_no_menu_at_all() {
        assert!(renaming("notes").menu.is_none());
        let lines = render(&renaming("notes"), 60, 9);
        let filled = lines.iter().filter(|l| !l.is_empty()).count();
        assert_eq!(filled, 2, "just the field and the hint row: {lines:#?}");
    }

    /// The directory field inverts the placeholder rule the other two follow,
    /// because its default is a place being navigated from rather than a value
    /// being replaced — and because the menu underneath is already listing that
    /// place's children.
    #[test]
    fn typing_in_the_directory_field_continues_the_default_rather_than_replacing_it() {
        let mut p = prompt();
        p.on_key(Key::Down);
        p.on_key(Key::Down);
        type_in(&mut p, "pro");
        assert_eq!(p.fields[DIRECTORY].input, format!("{DIRECTORY_DEFAULT}pro"));

        // ...unless the first key is `/`, which is how you say "somewhere else
        // entirely".
        let mut p = prompt();
        p.on_key(Key::Down);
        p.on_key(Key::Down);
        type_in(&mut p, "/srv/www");
        assert_eq!(p.fields[DIRECTORY].input, "/srv/www");

        // The other two fields are unchanged: there the default is a value.
        let mut p = prompt();
        type_in(&mut p, "notes");
        assert_eq!(p.fields[NAME].input, "notes");
    }

    /// The prompt opens as three plain fields. Nothing conjures a list of
    /// directories — not focusing the field, not typing in it — until it is
    /// asked for.
    #[test]
    fn nothing_shows_the_menu_but_tab() {
        let mut p = at_directory("/home/pro");
        assert!(!p.choosing(), "typing must not open it");
        fill(&mut p, &["projects", "prototypes"]);
        let lines = render(&p, 60, 14);
        assert!(
            !lines.iter().any(|l| l.contains("projects")),
            "a closed menu draws nothing: {lines:#?}"
        );

        p.on_key(Key::Tab);
        assert!(p.choosing());
    }

    /// `Tab` opens the menu, and `Tab` again takes what is highlighted. One key,
    /// one job, in sequence.
    #[test]
    fn tab_opens_the_menu_then_accepts_from_it() {
        let mut p = menuing("/home/pro", &["projects", "prototypes"]);
        p.on_key(Key::CtrlN);
        p.on_key(Key::Tab);
        assert!(!p.choosing(), "accepting closes the menu");
        assert_eq!(p.fields[DIRECTORY].input, "/home/prototypes");
    }

    /// The name alone, so the path reads exactly as it would if it had been
    /// typed. The slash is the next `Tab`'s job.
    #[test]
    fn an_accept_writes_the_name_without_a_trailing_slash() {
        let mut p = menuing("/home/pro", &["projects"]);
        p.on_key(Key::Tab);
        assert_eq!(p.fields[DIRECTORY].input, "/home/projects");
        assert_eq!(
            p.fields[DIRECTORY].cursor,
            p.fields[DIRECTORY].len(),
            "the cursor follows, ready for the next component"
        );
    }

    /// The `/` nobody types. Accepting writes the name; the next `Tab` steps
    /// into it, so walking down a tree never leaves the keyboard's home row.
    #[test]
    fn tab_after_an_accept_steps_into_what_it_accepted() {
        let mut p = menuing("/home/pro", &["projects"]);
        p.on_key(Key::Tab);
        assert_eq!(p.fields[DIRECTORY].input, "/home/projects");

        p.on_key(Key::Tab);
        assert!(p.choosing(), "the menu reopened");
        assert_eq!(
            p.fields[DIRECTORY].input, "/home/projects/",
            "and stepped inside rather than reoffering the siblings"
        );
    }

    /// Only ever for the one keystroke after an accept. A directory whose name
    /// is also a prefix of its siblings must not be stepped into merely because
    /// it exists — only because it was just chosen.
    #[test]
    fn anything_but_tab_after_an_accept_forgets_the_step() {
        let mut p = menuing("/home/pro", &["pro", "projects"]);
        p.on_key(Key::Tab);
        assert_eq!(p.fields[DIRECTORY].input, "/home/pro");

        // A cursor move is enough to mean "I am not stepping in".
        p.on_key(Key::Left);
        p.on_key(Key::Tab);
        assert!(p.choosing());
        assert_eq!(
            p.fields[DIRECTORY].input, "/home/pro",
            "reopened on the query rather than stepping into `pro`"
        );
    }

    /// `Tab` is for completion, and the other two fields have nothing to
    /// complete. It does not move between fields any more either.
    #[test]
    fn tab_does_nothing_in_the_name_and_command_fields() {
        for focus in [NAME, COMMAND] {
            let mut p = prompt();
            for _ in 0..focus {
                p.on_key(Key::Down);
            }
            assert_eq!(p.focus, focus);
            let before = p.fields[focus].input.clone();

            assert_eq!(p.on_key(Key::Tab), Step::None);
            assert_eq!(p.focus, focus, "Tab must not move between fields");
            assert_eq!(p.fields[focus].input, before, "and must not type anything");
            assert!(!p.choosing(), "and there is nothing here to complete");
        }
    }

    /// All four movement keys, both states. Whatever is open owns them.
    #[test]
    fn the_movement_keys_change_field_until_the_menu_is_open() {
        for (down, up) in [(Key::Down, Key::Up), (Key::CtrlN, Key::CtrlP)] {
            let mut p = prompt();
            assert_eq!(p.focus, NAME);
            p.on_key(down);
            assert_eq!(p.focus, COMMAND);
            p.on_key(down);
            assert_eq!(p.focus, DIRECTORY);
            p.on_key(down);
            assert_eq!(p.focus, NAME, "three fields, so it wraps");
            p.on_key(up);
            assert_eq!(p.focus, DIRECTORY);

            // Open the menu and the same keys move through it instead.
            let mut p = menuing("/home/", &["alpha", "beta", "gamma"]);
            let at = |p: &Prompt| p.menu.as_ref().expect("a menu").selected;
            p.on_key(down);
            assert_eq!(at(&p), 1);
            p.on_key(down);
            assert_eq!(at(&p), 2);
            p.on_key(down);
            assert_eq!(at(&p), 0, "wraps at the end");
            p.on_key(up);
            assert_eq!(at(&p), 2, "and at the start");
            assert_eq!(p.focus, DIRECTORY, "and never changed field");
        }
    }

    /// Enter means one thing everywhere: create. Even with the menu open and a
    /// suggestion highlighted, it creates with the field as it stands — a
    /// half-typed query is refused by name rather than quietly becoming
    /// whichever directory happened to be under the cursor.
    #[test]
    fn enter_always_creates_even_with_the_menu_open() {
        let mut p = menuing("/home/nvmx", &["nvmux-rs"]);
        p.on_key(Key::CtrlN);
        assert!(p.choosing());

        let step = p.on_key(Key::Enter);
        match step {
            Step::Submit(s) => assert_eq!(s.directory, Some("/home/nvmx".to_string())),
            other => panic!("enter must create, got {other:?}"),
        }
    }

    /// One thing at a time on the way out: the menu you opened, then the prompt.
    #[test]
    fn esc_closes_the_menu_before_it_cancels_the_prompt() {
        let mut p = menuing("/home/pro", &["projects"]);
        assert!(p.choosing());

        assert_eq!(
            p.on_key(Key::Esc),
            Step::None,
            "the first Esc closes the menu"
        );
        assert!(!p.choosing());
        assert_eq!(
            p.fields[DIRECTORY].input, "/home/pro",
            "and leaves what was typed alone"
        );

        assert_eq!(p.on_key(Key::Esc), Step::Cancel, "the second leaves");
    }

    /// Narrowing is what the query is for, so typing filters an open menu rather
    /// than dismissing it.
    #[test]
    fn typing_narrows_an_open_menu_and_leaves_it_open() {
        let mut p = menuing("/home/", &["alpha", "beta"]);
        p.on_key(Key::CtrlN);
        assert_eq!(p.menu.as_ref().expect("a menu").selected, 1);

        type_in(&mut p, "a");
        assert!(p.choosing(), "still open");
        assert_eq!(p.fields[DIRECTORY].input, "/home/a");
    }

    /// The form must sit in exactly the same place whether the menu is open or
    /// shut. Sized to include it, the block would jump up the screen on every
    /// `Tab`; reserving its rows against that would leave a permanent gap.
    #[test]
    fn the_fields_do_not_move_when_the_menu_opens() {
        let row_of = |p: &Prompt| {
            cells_of(p, 60, 14)
                .iter()
                .position(|row| {
                    row.iter()
                        .any(|(_, m)| m.contains(Modifier::REVERSED) && !m.contains(Modifier::BOLD))
                })
                .expect("the field cursor")
        };

        let closed = row_of(&at_directory("/home/"));
        for n in [1usize, 3, 6, 40] {
            let names: Vec<String> = (0..n).map(|i| format!("dir-{i}")).collect();
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            assert_eq!(
                row_of(&menuing("/home/", &refs)),
                closed,
                "opening a menu of {n} moved the form"
            );
        }
    }

    /// The hint row is the only thing on screen that says which list the
    /// movement keys are moving through.
    #[test]
    fn the_hint_row_is_on_the_last_row_and_follows_the_menu() {
        let closed = render(&prompt(), 60, 14);
        assert!(
            closed[13].contains("⏎ create")
                && closed[13].contains("↑↓ field")
                && closed[13].contains("⇥ complete"),
            "expected the closed hints on the last row, got {:?}",
            closed[13]
        );

        let open = render(&menuing("/home/", &["alpha"]), 60, 14);
        assert!(
            open[13].contains("⇥ accept") && open[13].contains("choose"),
            "expected the hints to follow the menu, got {:?}",
            open[13]
        );
    }

    /// Two reversed things on one screen have to be told apart, or the eye reads
    /// the highlighted row as the place keystrokes are going.
    #[test]
    fn a_selected_row_is_not_mistaken_for_the_field_cursor() {
        let p = menuing("/home/", &["alpha", "beta"]);
        let rows = cells_of(&p, 60, 9);

        let bold_reversed: String = rows
            .iter()
            .flatten()
            .filter(|(_, m)| m.contains(Modifier::REVERSED) && m.contains(Modifier::BOLD))
            .map(|(sym, _)| sym.as_str())
            .collect();
        assert!(
            bold_reversed.contains("alpha"),
            "the best match should be the highlighted row, got {bold_reversed:?}"
        );
        assert!(
            !bold_reversed.contains("beta"),
            "only one row is highlighted: {bold_reversed:?}"
        );
    }

    #[test]
    fn the_command_field_shows_the_command_it_would_run() {
        let lines = render(&prompt(), 70, 9);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("nvim command:      nvim --headless --listen {sock}")),
            "expected the command default under the name, got {lines:?}"
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
        let first = lines
            .iter()
            .position(|l| l.contains("new session name:  notes"))
            .expect("the field should still show what was typed");
        // Counted off the last field rather than a fixed offset, so this keeps
        // saying "below the fields" however many of them there come to be.
        let below = first + p.fields.len();
        assert!(
            lines[below..].iter().any(|l| l.contains("already exists")),
            "expected the error below every field, got {lines:?}"
        );
    }

    #[test]
    fn a_value_longer_than_the_field_keeps_the_cursor_visible() {
        let mut p = prompt();
        p.on_key(Key::Down);
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

        // A menu taller and wider than any of these terminals, drawn from
        // lengths the screen never agreed to — which is exactly where an
        // off-by-one becomes a panic.
        let offering = menuing(
            "/home/you/a-very-long-directory/su",
            &[
                "subdir",
                "subdirectory",
                "submodule",
                "subproject",
                "substrate",
                "subsystem",
                "subtree",
            ],
        );
        let mut partial = menuing("/home/you/a-very-long-directory/su", &["subdir"]);
        partial.menu.as_mut().expect("a menu").partial = true;
        let mut both = menuing("/home/you/a-very-long-directory/su", &[]);
        both.menu.as_mut().expect("a menu").message = "nothing here matches \"su\"".to_string();
        both.fail("a session named \"notes\" already exists".to_string(), None);

        for &(w, h) in test_support::TINY_SIZES {
            for p in [
                &prompt(),
                &typed,
                &failed,
                &renaming("dotfiles"),
                &offering,
                &partial,
                &both,
            ] {
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
        p.on_key(Key::Down);
        p.on_key(Key::End);
        p.fail("nope".to_string(), Some("session 3".to_string()));
        test_support::assert_no_colour(60, 9, |f| draw(f, &p));

        // The menu is the newest thing on this screen, and its highlight is the
        // strongest — reversed and bold, which are modifiers, not colours.
        let offering = menuing("/home/you/pro", &["projects", "prototypes"]);
        test_support::assert_no_colour(60, 9, |f| draw(f, &offering));
    }
}
