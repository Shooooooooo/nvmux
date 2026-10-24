//! The naming prompt: the only place a session is created or named.
//!
//! Entered by `<prefix> c` from a session ([`run`]) or `c`/`r` from the picker
//! ([`create_after`] and [`rename`] when floating; `run_on` on the picker's
//! full screen). A `Task` says whether it creates or renames.
//!
//! ```text
//!     new session name:  session 3
//!     nvim command:      nvim --headless --listen {sock}
//!     working directory: /home/you/pro
//!     └─ dim              │ └─ dim
//!                         └─ the cursor, an inverted cell
//!                        ▸ projects        ← the menu, once `Tab` asks for it:
//!                          prototypes         reversed and bold, the picker's
//!                                             "selected row"
//! ```
//!
//! # Fields
//!
//! Create shows name, command and working directory; rename shows only the
//! name. Enter submits from any field, so `<prefix> c` Enter creates a session
//! in one keystroke. Command and directory start pre-filled (you usually amend
//! them). The name starts as a dim placeholder you type over (`→`/`End` take
//! it); the cursor inverts its first letter so nothing shifts when typing. A
//! rename starts pre-filled. An emptied field shows its default dimmed, which
//! is what Enter would submit.
//!
//! # Styles
//!
//! No colours, only modifiers (see [`super::draw`]): **dim** for labels,
//! untouched defaults, the hint row and a path's discarded part; **plain** for
//! typed text; **reversed** for the cursor, and reversed+bold for the selected
//! menu row, as in the picker.
//!
//! # Keys
//!
//! | Key | Job |
//! |---|---|
//! | `Tab` | completion, and nothing else |
//! | `↑` `↓` `Ctrl-p` `Ctrl-n` | move between the fields — or through the menu while it is open |
//! | `Enter` | accept from the menu if one is open, otherwise create the session |
//! | `Esc` | close the menu if it is open, otherwise leave |
//!
//! No key depends on invisible state.
//!
//! # Directory completion
//!
//! `Tab` opens a fuzzy-ranked dropdown of directories (`nvmx` finds
//! `nvmux-rs`). Accepting writes the entry without a trailing slash; the next
//! `Tab` adds the slash and lists the level below. On an emptied field, `Tab`
//! first writes the dimmed default. Stepping in is remembered from the accept,
//! not inferred from the path, so `pro` beside `projects` is not stepped into
//! just because it exists.
//!
//! The field opens at the home directory, with a trailing `/`, as editable
//! text. The last `//` discards everything before its second slash
//! (`/home/shu//etc` is `/etc`), so typing `/` first means the root; in an
//! empty field a single `/` is the root. The discarded part is drawn dim.
//! [`crate::dirs::anchored`] resolves this here, because POSIX reads `a//b`
//! as `a/b` and the session would silently start elsewhere.
//!
//! Listing runs on a worker thread from the moment the prompt opens (see
//! [`super::complete`]). One listing serves a whole directory (fuzzy matching
//! cannot be a host-side glob), so a path costs about one round trip per `/`.
//!
//! # Floating
//!
//! With `[style] float` (the default) the prompt is drawn in a relay [`Panel`]
//! over the session, or in the picker's place for its `c` and `r` (the picker
//! returns if no session results). Behaviour is identical; only drawing and
//! key input differ.

use std::time::Instant;

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyEvent, KeyEventKind, MouseEvent};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use unicode_width::UnicodeWidthStr;

use super::app::{App, Key};
use super::complete;
use super::draw;
use super::float::{self, Did, Floating, Leave};
use crate::error::Result;
use crate::ids::Id;
use crate::launch::Launch;
use crate::session::Session;
use crate::state;
use crate::transport::{Shared, Transport};

#[derive(Debug)]
pub enum Outcome {
    /// The caller attaches to it.
    Created(Session),
    /// The new name; a rename changes nothing else.
    Renamed(String),
    Cancelled,
}

impl Outcome {
    /// Whether the caller attaches next and so needs the terminal cleared
    /// (see [`super::Screen::close_for_attach`]).
    fn attaches(&self) -> bool {
        matches!(self, Self::Created(_))
    }
}

/// Where a new session goes in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// At the end (`<prefix> c`).
    Last,
    /// After the picker's highlighted row (the picker's `c`); `None` (no
    /// rows) means first.
    After(Option<Id>),
}

#[derive(Clone, Copy)]
pub(super) enum Task<'a> {
    Create {
        /// The caller's listing, to compute the default name without another
        /// script run. `None` re-lists.
        listed: Option<&'a [Session]>,
        /// The number the session will get, used by [`default_name`]: the
        /// picker's slot after its highlighted row (see
        /// [`super::app::App::new_number`]). `None` for the end of the list.
        number: Option<u32>,
    },
    Rename(&'a Session),
}

/// Indices into [`Prompt::fields`]; only [`Task::Create`] has `COMMAND` and
/// `DIRECTORY`.
const NAME: usize = 0;
const COMMAND: usize = 1;
const DIRECTORY: usize = 2;

/// Maximum dropdown height, so the form need not climb far to fit it.
const MENU_ROWS: u16 = 6;

/// Same marker and indent as the picker.
const MARKER: &str = "▸ ";
const INDENT: &str = "  ";

/// Plain REVERSED; REVERSED|BOLD means "selected row".
const CURSOR: Style = Style::new().add_modifier(Modifier::REVERSED);

const DIM: Style = Style::new().add_modifier(Modifier::DIM);

#[derive(Debug, PartialEq, Eq)]
struct Submission {
    name: String,
    /// `None` when renaming.
    command: Option<String>,
    /// `None` when renaming.
    directory: Option<String>,
}

/// What one keypress meant.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    None,
    Submit(Submission),
    Cancel,
}

/// One labelled text field with a default submitted when it is empty. Also
/// used, without label or default, by [`super::settings`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Field {
    label: String,
    input: String,
    /// A **character** index into `input`, 0 to its length inclusive, so
    /// arithmetic cannot land mid-glyph.
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

    /// A field holding `input`, cursor at the end, with no label or default.
    pub(super) fn with(input: String) -> Self {
        Self::new(String::new(), input, String::new())
    }

    pub(super) fn input(&self) -> &str {
        &self.input
    }

    /// Cursor position in characters.
    pub(super) fn cursor(&self) -> usize {
        self.cursor
    }

    fn len(&self) -> usize {
        self.input.chars().count()
    }

    pub(super) fn insert(&mut self, c: char) {
        self.input.insert(byte_at(&self.input, self.cursor), c);
        self.cursor += 1;
    }

    pub(super) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.input.remove(byte_at(&self.input, self.cursor - 1));
        self.cursor -= 1;
    }

    pub(super) fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub(super) fn right(&mut self) {
        if self.adopt_default() {
            return;
        }
        self.cursor = (self.cursor + 1).min(self.len());
    }

    pub(super) fn home(&mut self) {
        self.cursor = 0;
    }

    pub(super) fn end(&mut self) {
        self.adopt_default();
        self.cursor = self.len();
    }

    /// Whether the dim default is shown, which `→`, `End` and (in the
    /// directory field) `Tab` adopt. `field_line` draws the placeholder on the
    /// same condition. The directory default can be empty if the host could not
    /// say where home is (see [`default_directory`]).
    fn showing_default(&self) -> bool {
        self.input.is_empty() && !self.default.is_empty()
    }

    /// Copy the shown default into the empty field for editing. Returns
    /// whether it did.
    fn adopt_default(&mut self) -> bool {
        if !self.showing_default() {
            return false;
        }
        self.input.clone_from(&self.default);
        self.cursor = self.len();
        true
    }

    /// The trimmed text, or the default if empty.
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

/// The prompt's state, separate from the terminal so it is testable.
struct Prompt {
    fields: Vec<Field>,
    focus: usize,
    message: Option<String>,
    /// `None` for a rename; otherwise always present, since its rows are
    /// reserved whether or not it is open.
    menu: Option<Menu>,
}

/// The directory dropdown. Holds matches rather than rendered rows, since the
/// row count depends on the terminal size at draw time.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Menu {
    /// Best first.
    matches: Vec<String>,
    /// In range while `matches` is non-empty.
    selected: usize,
    /// Only `Tab` opens it.
    open: bool,
    /// The last key accepted from the menu, so the next `Tab` steps into the
    /// accepted directory. Cleared by any other key. A flag, not inferred
    /// from the path (see the module docs).
    stepped: bool,
    /// The host's listing was truncated.
    partial: bool,
    /// Shown instead of rows: waiting, or no match.
    message: String,
}

impl Menu {
    /// Wraps, like every list in nvmux.
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
                // Command and directory are pre-filled (see module docs); the
                // default only matters once a field is emptied.
                Field::new(labels[1].clone(), default_command.clone(), default_command),
                Field::new(
                    labels[2].clone(),
                    default_directory.clone(),
                    default_directory,
                ),
            ],
            focus: NAME,
            message: None,
            menu: Some(Menu::default()),
        }
    }

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

    /// The hint row: the only on-screen list of live keys.
    fn hints(&self) -> &'static str {
        if self.menu.is_none() {
            return "⏎ rename   esc cancel";
        }
        if self.choosing() {
            "⇥ ⏎ accept   ↑↓ choose   esc close"
        } else if self.completes() {
            "⏎ create   ↑↓ field   ⇥ complete   esc cancel"
        } else {
            // No `⇥ complete`: nothing to complete in this field.
            "⏎ create   ↑↓ field   esc cancel"
        }
    }

    /// Whether `Tab` completes in the focused field. Used by both the hint row
    /// and the key handler so they cannot disagree.
    fn completes(&self) -> bool {
        self.menu.is_some() && self.focus == DIRECTORY
    }

    /// Whether the menu is open; it then owns the movement keys and `Tab`
    /// accepts.
    fn choosing(&self) -> bool {
        self.focus == DIRECTORY && self.menu.as_ref().is_some_and(|m| m.open)
    }

    fn focused(&mut self) -> &mut Field {
        &mut self.fields[self.focus]
    }

    /// Wraps; a no-op for a rename's single field.
    fn move_focus(&mut self, delta: usize) {
        let n = self.fields.len();
        self.focus = (self.focus + delta) % n;
    }

    /// Report a rejected submission, keeping the input (a duplicate is usually
    /// one character off). `default_name` is `Some` only when creating.
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
            // Resolve `//`: POSIX reads `/home/shu//etc` as `/home/shu/etc`.
            directory: self
                .fields
                .get(DIRECTORY)
                .map(|f| crate::dirs::anchored(&f.value()).1.to_string()),
        }
    }

    fn on_key(&mut self, key: Key) -> Step {
        self.message = None;

        let choosing = self.choosing();

        match key {
            Key::Char(c) => {
                self.focused().insert(c);
                // An open menu stays open and narrows; `show` reselects the
                // best match.
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

            // `Tab` only completes; elsewhere it does nothing.
            Key::Tab if self.completes() => {
                if choosing {
                    self.accept();
                } else if self.fields[DIRECTORY].adopt_default() {
                    // Write the dim default first; the next `Tab` lists it.
                    self.stayed_put();
                } else if self.complete_bare_tilde() {
                    self.stayed_put();
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

            // Accepting closes the menu, so a second Enter creates.
            Key::Enter if choosing => {
                self.accept();
                Step::None
            }
            Key::Enter => Step::Submit(self.submission()),

            // Esc closes the menu first, then the prompt, like the picker's
            // filter mode.
            Key::Esc if choosing => {
                self.close_menu(false);
                Step::None
            }
            // Cancel, not quit: quitting would pull the user out of a live
            // session.
            Key::Esc | Key::CtrlC => Step::Cancel,
            _ => Step::None,
        }
    }

    /// Open the menu on the field's path. Right after an accept, first append
    /// `/` to step into the chosen directory.
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

    /// `stepped` is true for an accept (the next `Tab` steps in), false for
    /// `Esc`.
    fn close_menu(&mut self, stepped: bool) {
        if let Some(menu) = self.menu.as_mut() {
            menu.open = false;
            menu.stepped = stepped;
            menu.matches.clear();
            menu.message.clear();
        }
    }

    /// Called on every key but an accept, so `stepped` lasts one keystroke.
    fn stayed_put(&mut self) {
        if let Some(menu) = self.menu.as_mut() {
            menu.stepped = false;
        }
    }

    /// Callers have already checked the menu exists.
    fn menu(&mut self) -> &mut Menu {
        self.menu.as_mut().expect("a menu the caller checked for")
    }

    /// `Tab` on a lone `~`: write `~/` and return true; the next `Tab` lists
    /// home.
    ///
    /// `~` has no `/` for [`crate::dirs::split`] to cut at, so it cannot be
    /// matched against siblings. Shells complete `~user` here, but
    /// [`crate::session::expand_tilde`] refuses `~user` (it would need a host
    /// round trip), so the slash is the only completion.
    fn complete_bare_tilde(&mut self) -> bool {
        let field = &mut self.fields[DIRECTORY];
        // The default is `~/`, so only a typed `~` gets here.
        if field.value() != "~" {
            return false;
        }
        field.input = "~/".to_string();
        field.cursor = field.len();
        true
    }

    /// Write the highlighted directory into the field, without a trailing `/`
    /// (the next `Tab` adds it), and close the menu.
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
        // The value, not the input: an emptied field means its default.
        let current = field.value();
        // Keep the discarded prefix before `//` as typed.
        let (inert, live) = crate::dirs::anchored(&current);
        let parent = crate::dirs::split(live).map_or(live, |(d, _)| d);
        let parent = parent.trim_end_matches('/');
        field.input = format!("{inert}{parent}/{name}");
        field.cursor = field.len();

        self.close_menu(true);
    }
}

/// `label: ` for each, padded so all values start in the same column.
fn aligned(labels: &[&str]) -> Vec<String> {
    let width = labels.iter().map(|l| l.width()).max().unwrap_or(0);
    labels
        .iter()
        .map(|l| format!("{l}:{}", " ".repeat(width - l.width() + 1)))
        .collect()
}

/// Ask for a new session on a terminal of its own, for `<prefix> c`.
///
/// On [`Outcome::Created`] a client is spawned next, so the terminal is
/// returned cleared; on cancel the old client is repainted, so the ordinary
/// close is used.
pub fn run(transport: &dyn Transport) -> Result<Outcome> {
    // The session just dissolved out, so dissolve in.
    super::owning_for_attach(Outcome::attaches, true, |terminal| {
        run_on(
            terminal,
            transport,
            Task::Create {
                listed: None,
                number: None,
            },
            Arrival::Dissolve,
        )
    })
}

/// Ask for a new session on a terminal of its own, for `c` on the floating
/// picker (see [`super::picker`]). Named and placed after row `after` of
/// `rows` as the full-screen picker would (see [`super::settle`]). Returns the
/// session and its rows, or `None` if cancelled.
pub fn create_after(
    transport: &dyn Transport,
    rows: &[Session],
    after: Option<&str>,
) -> Result<Option<(Session, Vec<Session>)>> {
    let mut picker = super::app::App::new(rows.to_vec());
    if let Some(id) = after {
        picker.select_session(id);
    }
    // The session just went to the background, so dissolve in.
    let outcome = super::owning_for_attach(Outcome::attaches, true, |terminal| {
        run_on(
            terminal,
            transport,
            Task::Create {
                listed: Some(picker.sessions()),
                number: Some(picker.new_number()),
            },
            Arrival::Dissolve,
        )
    })?;
    Ok(match outcome {
        Outcome::Created(session) => Some(super::settle(&picker, transport, session)),
        Outcome::Renamed(_) | Outcome::Cancelled => None,
    })
}

/// Ask for a new name on a terminal of its own, for `r` on the floating
/// picker (see [`super::picker`]). The same client returns either way, so the
/// ordinary close is used.
pub fn rename(transport: &dyn Transport, session: &Session) -> Result<Outcome> {
    // Dissolve in, and out again either way.
    super::owning(|terminal| {
        run_on(
            terminal,
            transport,
            Task::Rename(session),
            Arrival::Dissolve,
        )
    })
}

/// Minimum session margin around the floating prompt, per side (as the
/// picker and help).
const SESSION_COLS: u16 = 2;
const SESSION_ROWS: u16 = 1;

/// Extra width beyond the opening lines for typing, and the width bounds
/// (the settings panel's).
const PANEL_ROOM: u16 = 24;
const PANEL_NARROWEST: u16 = 48;
const PANEL_WIDEST: u16 = 72;

/// Panel heights: the body plus the last row, where the prompt has its hint
/// row full screen, left blank ([`float::inside`]). A create's form (three
/// fields and a message) is centred with the menu below it, so the body is
/// the form plus twice the menu, less the one row centring puts above. A
/// rename has its line and message plus three rows.
const CREATE_ROWS: u16 = (3 + 1) + 2 * MENU_ROWS - 1 + 1;
const RENAME_ROWS: u16 = (1 + 1) + 3 + 1;

/// The prompt floating over a session (see `crate::pty::panel`), fed the
/// relay's keys.
pub struct Panel {
    prompt: Prompt,
    job: Job,
    completer: Option<complete::Completer>,
    /// Lent at startup (see [`super::picker::lend`]); `None` only if nothing
    /// lent it.
    transport: Option<Shared>,
    /// The session to switch to after closing, if one was made.
    leave: Option<Leave>,
    /// Last tick or key; the next completer poll is scheduled from it.
    ticked: Instant,
}

enum Job {
    /// Create at `place`. The picker holds the current rows and cursor, used
    /// for naming and placement (see [`super::settle`]).
    Create { place: Place, picker: Box<App> },
    /// Rename this session; `None` if it is no longer in the rows.
    Rename(Option<Session>),
}

impl Panel {
    pub fn create(place: Place) -> Self {
        Self::create_on(place, super::picker::rows(), super::picker::transport())
    }

    pub fn rename(id: Id) -> Self {
        Self::rename_on(id, super::picker::rows(), super::picker::transport())
    }

    /// Offers the same default name as the full-screen picker would and
    /// starts listing the default directory right away.
    fn create_on(place: Place, rows: Vec<Session>, transport: Option<Shared>) -> Self {
        let mut picker = App::new(rows);
        let number = match place {
            Place::Last => None,
            Place::After(after) => {
                if let Some(id) = after {
                    picker.select_session(id.as_str());
                }
                Some(picker.new_number())
            }
        };
        let name = default_name(picker.sessions(), number);
        let (command, directory) = match transport.as_deref() {
            Some(t) => (default_command(t), default_directory(t.home())),
            None => (crate::config::get().session.command.clone(), String::new()),
        };
        let mut completer = transport
            .as_deref()
            .map(|t| complete::Completer::new(t.dir_source(), t.home()));
        let mut prompt = Prompt::create(name, command, directory);
        refresh(&mut prompt, completer.as_mut());
        Self {
            prompt,
            job: Job::Create {
                place,
                picker: Box::new(picker),
            },
            completer,
            transport,
            leave: None,
            ticked: Instant::now(),
        }
    }

    fn rename_on(id: Id, rows: Vec<Session>, transport: Option<Shared>) -> Self {
        let session = rows.into_iter().find(|s| s.id == id.as_str());
        let prompt = match &session {
            Some(session) => Prompt::rename(session),
            None => Prompt::rename(&Session::new(id.as_str().into(), id.as_str().into(), 0, 0)),
        };
        Self {
            prompt,
            job: Job::Rename(session),
            completer: None,
            transport,
            leave: None,
            ticked: Instant::now(),
        }
    }

    /// The new session's number (for its default name): after the picker's
    /// row, or `None` for the end.
    fn number(&self) -> Option<u32> {
        match &self.job {
            Job::Create {
                place: Place::After(_),
                picker,
            } => Some(picker.new_number()),
            Job::Create {
                place: Place::Last, ..
            }
            | Job::Rename(_) => None,
        }
    }

    /// Carry out a submission as the full-screen prompt does; on refusal the
    /// panel stays up with the reason.
    fn submit(&mut self, submission: Submission) -> Did {
        let Some(transport) = self.transport.clone() else {
            self.prompt.fail("nothing to make it with".into(), None);
            return Did::Stay;
        };
        let transport = transport.as_ref();
        let number = self.number();
        let task = match &self.job {
            Job::Create { picker, .. } => Task::Create {
                listed: Some(picker.sessions()),
                number,
            },
            Job::Rename(Some(session)) => Task::Rename(session),
            Job::Rename(None) => {
                self.prompt.fail("that session is gone".into(), None);
                return Did::Stay;
            }
        };
        match commit(transport, task, submission) {
            Ok(Outcome::Created(session)) => self.made(transport, session),
            Ok(Outcome::Renamed(name)) => self.renamed(transport, &name),
            Ok(Outcome::Cancelled) => Did::Close,
            Err(e) => {
                // The rows in hand may be stale, so re-list for the default.
                let refreshed = match self.job {
                    Job::Create { .. } => next_free_name(transport, None, number).ok(),
                    Job::Rename(_) => None,
                };
                self.prompt.fail(e.one_line(), refreshed);
                Did::Stay
            }
        }
    }

    /// Place the new session (after the picker's row, or last), leave the
    /// rows for the session loop, and switch to it.
    fn made(&mut self, transport: &dyn Transport, session: Session) -> Did {
        let (session, rows) = match &self.job {
            Job::Create {
                place: Place::After(_),
                picker,
            } => super::settle(picker, transport, session),
            Job::Create {
                place: Place::Last,
                picker,
            } => {
                let mut rows = picker.sessions().to_vec();
                rows.push(session.clone());
                (session, rows)
            }
            Job::Rename(_) => return Did::Close,
        };
        super::picker::leave_rows(&rows);
        self.leave = Id::new(&session.id).map(Leave::Switch);
        Did::Close
    }

    /// Re-list (or, failing that, patch the rows in hand) and leave the rows
    /// for the picker.
    fn renamed(&mut self, transport: &dyn Transport, name: &str) -> Did {
        let rows = transport.list_sessions().unwrap_or_else(|e| {
            tracing::debug!(error = %e, "could not list the sessions after a rename");
            let mut rows = super::picker::rows();
            if let Job::Rename(Some(renamed)) = &self.job {
                for s in rows.iter_mut().filter(|s| s.id == renamed.id) {
                    s.name = name.to_string();
                }
            }
            rows
        });
        super::picker::leave_rows(&rows);
        Did::Close
    }
}

impl Floating for Panel {
    /// Centred above the hint row; `None` if too little session would show
    /// around it, which means full screen.
    fn area(&self, screen: Rect) -> Option<Rect> {
        let (room, _) = float::split(screen);
        let opening = self
            .prompt
            .fields
            .iter()
            .map(|f| f.label.width() + f.default.width())
            .max()
            .unwrap_or(0) as u16;
        let width = (opening + PANEL_ROOM)
            .clamp(PANEL_NARROWEST, PANEL_WIDEST)
            .min(room.width.saturating_sub(2 * SESSION_COLS));
        let height = match self.job {
            Job::Create { .. } => CREATE_ROWS,
            Job::Rename(_) => RENAME_ROWS,
        };
        if width < PANEL_NARROWEST || room.height < height + 2 * SESSION_ROWS {
            return None;
        }
        Some(Rect::new(
            room.x + (room.width - width) / 2,
            room.y + (room.height - height) / 2,
            width,
            height,
        ))
    }

    /// Key releases are ignored.
    fn on_key(&mut self, key: KeyEvent, now: Instant) -> Did {
        if key.kind == KeyEventKind::Release {
            return Did::Stay;
        }
        self.ticked = now;
        let step = self.prompt.on_key(super::translate(key));
        refresh(&mut self.prompt, self.completer.as_mut());
        match step {
            Step::None => Did::Stay,
            Step::Cancel => Did::Close,
            Step::Submit(submission) => self.submit(submission),
        }
    }

    /// Nothing clickable; the relay handles presses on the session.
    fn on_mouse(&mut self, _mouse: MouseEvent, _now: Instant) -> Did {
        Did::Stay
    }

    /// Only while the completer owes an answer.
    fn wake_at(&self) -> Option<Instant> {
        self.completer
            .as_ref()
            .filter(|c| c.waiting())
            .map(|_| self.ticked + super::BUSY_TICK)
    }

    /// Show any new completer answers.
    fn tick(&mut self, now: Instant) -> bool {
        self.ticked = now;
        if self
            .completer
            .as_mut()
            .is_some_and(complete::Completer::poll)
        {
            show(&mut self.prompt, self.completer.as_mut());
            return true;
        }
        false
    }

    fn moving(&self) -> bool {
        false
    }

    /// Draw on an in-memory terminal the size of the panel's inside, themed,
    /// then copy all but the hint row into a buffer of `area`.
    fn draw(&mut self, area: Rect) -> Buffer {
        let mut buf = Buffer::empty(area);
        let inside = float::inside(area);
        if inside.width == 0 || inside.height == 0 {
            return buf;
        }
        let Ok(mut terminal) = Terminal::new(TestBackend::new(inside.width, inside.height));
        let prompt = &self.prompt;
        let Ok(_) = terminal.draw(|f| {
            draw(f, prompt);
            crate::theme::paint(f.buffer_mut());
        });
        float::lay(&mut buf, inside, terminal.backend().buffer());
        buf
    }

    fn draw_hints(&self, row: Rect) -> Buffer {
        let mut buf = Buffer::empty(row);
        draw::hint_row_on(&mut buf, row, self.prompt.hints(), true);
        crate::theme::paint(&mut buf);
        buf
    }

    fn close(&self) {}

    fn leave(&self) -> Option<Leave> {
        self.leave
    }
}

/// How the prompt appears, and so whether it fades out on cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Arrival {
    /// Cut in and out over a screen still up: the picker's `r`, and its `c`
    /// with `[effects.create]` off or no fade available.
    Cut,
    /// Dissolve in from the background a session just dissolved into
    /// (`<prefix> c`, or `c`/`r` on the floating picker), and out on cancel.
    Dissolve,
    /// Open out of the name row after the picker closed onto the gap it made
    /// (see [`super::app::App::make_room`]); dissolve out on cancel.
    FromName,
}

/// Run on a terminal the caller owns (the picker's `c` and `r`).
///
/// Not `run`: a nested terminal would enter the alternate screen twice and
/// leave once. Sharing it also hides the handover, since `Terminal::draw`
/// resets the frame each pass.
///
/// Every `arrival` but [`Arrival::Cut`] fades out on cancel; a completed
/// create always fades out, since a client spawn follows.
pub(super) fn run_on(
    terminal: &mut ratatui::DefaultTerminal,
    transport: &dyn Transport,
    task: Task,
    arrival: Arrival,
) -> Result<Outcome> {
    let animate = arrival != Arrival::Cut;
    let mut prompt = match task {
        Task::Create { listed, number } => Prompt::create(
            next_free_name(transport, listed, number)?,
            default_command(transport),
            default_directory(transport.home()),
        ),
        Task::Rename(session) => Prompt::rename(session),
    };

    // Only a create starts the worker; dropping the completer ends it (see
    // `super::complete`).
    let mut completer = match task {
        Task::Create { .. } => Some(complete::Completer::new(
            transport.dir_source(),
            transport.home(),
        )),
        Task::Rename(_) => None,
    };
    // List the default up front so the first `Tab` is usually instant.
    refresh(&mut prompt, completer.as_mut());

    match arrival {
        Arrival::Cut => {}
        Arrival::Dissolve => crate::fade::fade_in(terminal, |f| draw(f, &prompt))?,
        Arrival::FromName => {
            let size = terminal.size()?;
            let row = name_row(&prompt, Rect::new(0, 0, size.width, size.height));
            crate::fade::fade_in_from(terminal, |f| draw(f, &prompt), row)?;
        }
    }

    loop {
        terminal.draw(|f| {
            draw(f, &prompt);
            crate::theme::paint(f.buffer_mut());
        })?;

        // Poll faster only while an answer is outstanding.
        let waiting = completer.as_ref().is_some_and(complete::Completer::waiting);
        let tick = if waiting {
            super::BUSY_TICK
        } else {
            super::TICK
        };

        let Some(key) = super::poll_key_for(tick)? else {
            // No key; check for a completer answer.
            if completer.as_mut().is_some_and(complete::Completer::poll) {
                show(&mut prompt, completer.as_mut());
            }
            continue;
        };

        let step = prompt.on_key(key);
        // After the key: it may have changed the query (e.g. `→` adopting the
        // default).
        refresh(&mut prompt, completer.as_mut());

        let submission = match step {
            Step::None => continue,
            Step::Cancel => {
                if animate {
                    crate::fade::fade_out(terminal, |f| draw(f, &prompt))?;
                }
                return Ok(Outcome::Cancelled);
            }
            Step::Submit(submission) => submission,
        };

        match commit(transport, task, submission) {
            Ok(outcome) => {
                // Always fade before a client spawn; a rename returns to the
                // picker, so it fades only if it faded in.
                if outcome.attaches() || animate {
                    crate::fade::fade_out(terminal, |f| draw(f, &prompt))?;
                }
                return Ok(outcome);
            }
            Err(e) => {
                // The default may have been taken since; re-list rather than
                // trust the picker's listing on a retry.
                let refreshed = match task {
                    Task::Create { number, .. } => Some(next_free_name(transport, None, number)?),
                    Task::Rename(_) => None,
                };
                prompt.fail(e.one_line(), refreshed);
            }
        }
    }
}

/// Create or rename the session.
///
/// Names are validated only by `session::validate_name` and the transport's
/// duplicate check, so no UI copy can drift. The command is parsed here only
/// because the transport takes a `Launch`.
fn commit(transport: &dyn Transport, task: Task, submission: Submission) -> Result<Outcome> {
    match task {
        Task::Create { .. } => {
            let line = submission.command.unwrap_or_default();
            let typed = submission.directory.unwrap_or_default();
            Launch::parse(&line)
                .map_err(Into::into)
                .and_then(|launch| {
                    // Absolute, `~` expanded against the session host's home.
                    // Existence is checked by the spawn script.
                    let directory = crate::session::validate_directory(&typed, transport.home())?;
                    transport
                        .create_session(&submission.name, &launch, &directory)
                        // Remember only a command that started a session.
                        .inspect(|_| state::remember(transport.location(), launch.line()))
                })
                .map(Outcome::Created)
        }
        Task::Rename(session) => transport
            .rename_session(session, &submission.name)
            .map(|_| Outcome::Renamed(submission.name)),
    }
}

/// Ask the completer about the directory field's value (so an emptied field
/// completes its default) and show what it has.
///
/// Called after every key and does no I/O: [`complete::Completer::ask`]
/// answers from memory or queues the question for the worker.
fn refresh(prompt: &mut Prompt, completer: Option<&mut complete::Completer>) {
    let Some(completer) = completer else {
        return;
    };
    // Only the part after any `//`.
    let typed = prompt.fields[DIRECTORY].value();
    completer.ask(crate::dirs::anchored(&typed).1);
    show(prompt, Some(completer));
}

/// Copy the completer's matches into the menu.
fn show(prompt: &mut Prompt, completer: Option<&mut complete::Completer>) {
    let Some(completer) = completer else {
        return;
    };
    // Nothing is matched until `Tab` opens the menu, though listing runs
    // in the background regardless.
    if !prompt.choosing() {
        if let Some(menu) = prompt.menu.as_mut() {
            menu.matches.clear();
            menu.message.clear();
        }
        return;
    }

    // The value, as in `refresh`.
    let typed = prompt.fields[DIRECTORY].value();
    let live = crate::dirs::anchored(&typed).1;
    let query = crate::dirs::split(live).map_or("", |(_, q)| q);

    let waiting = completer.waiting();
    let partial = completer.truncated();
    let matches = completer.matches(query);

    let message = if waiting && matches.is_empty() {
        // Only with nothing to show: cached matches beat an ellipsis.
        "…".to_string()
    } else if matches.is_empty() {
        // An empty open menu would look broken.
        if query.is_empty() {
            "nothing here to choose from".to_string()
        } else {
            format!("nothing here matches {query:?}")
        }
    } else {
        String::new()
    };

    let menu = prompt.menu.as_mut().expect("checked by `choosing`");
    // Reselect the best match when the matches change; a fixed index would
    // point at a different directory each keystroke.
    if menu.matches != matches {
        menu.selected = 0;
    }
    menu.matches = matches;
    menu.partial = partial;
    menu.message = message;
}

/// `session N`, where `N` is the number the session will get (made between
/// 2 and 3, it is `session 3`). If taken, the next free number above, else
/// the first free one below. Case-insensitive, like the transports'
/// duplicate check.
fn next_name(taken: &[String], number: u32) -> String {
    let number = number.max(1);
    for n in (number..number.saturating_add(1000)).chain(1..number) {
        let candidate = format!("session {n}");
        if !taken.iter().any(|t| t.eq_ignore_ascii_case(&candidate)) {
            return candidate;
        }
    }
    "session".to_string()
}

/// [`default_name`] against `listed`, or a fresh listing if `None`.
fn next_free_name(
    transport: &dyn Transport,
    listed: Option<&[Session]>,
    number: Option<u32>,
) -> Result<String> {
    match listed {
        Some(sessions) => Ok(default_name(sessions, number)),
        None => Ok(default_name(&transport.list_sessions()?, number)),
    }
}

/// The default name for a session numbered `number` (`None`: last). Also
/// shown by the picker in the gap it makes (see
/// [`super::app::App::make_room`]), so the two always agree.
pub(super) fn default_name(listed: &[Session], number: Option<u32>) -> String {
    let taken: Vec<String> = listed.iter().map(|s| s.name.clone()).collect();
    let last = listed.iter().map(|s| s.state.num).max().unwrap_or(0) + 1;
    next_name(&taken, number.unwrap_or(last))
}

/// `~/`, which [`crate::session::expand_tilde`] resolves against the session
/// host's home. The slash makes the menu list home's children. Empty if the
/// host has no known home: `~/` would then fail on Enter, and `/` would be
/// wrong.
fn default_directory(home: &str) -> String {
    if home.trim_end_matches('/').is_empty() {
        String::new()
    } else {
        "~/".to_string()
    }
}

/// The command last used on this host, else the configured one. Per host,
/// since paths and `$PATH` differ between machines.
fn default_command(transport: &dyn Transport) -> String {
    state::remembered(transport.location())
        .unwrap_or_else(|| crate::config::get().session.command.clone())
}

fn draw(frame: &mut Frame, prompt: &Prompt) {
    draw::screen(frame, prompt.hints(), true, |frame, body| {
        draw_body(frame, prompt, body)
    });
}

/// The form's rect in `area`, centred on the opening lines (labels plus
/// defaults) rather than the current input, so it does not shift while
/// typing. The menu is excluded so the form stays put when it opens; the menu
/// hangs below.
fn anchor(prompt: &Prompt, area: Rect) -> Rect {
    let height = prompt.fields.len() as u16 + u16::from(prompt.message.is_some());
    let opening = prompt
        .fields
        .iter()
        .map(|f| f.label.width() + f.default.width())
        .max()
        .unwrap_or(0) as u16;
    draw::centre(area, opening, height)
}

/// The name field's screen row, for [`Arrival::FromName`].
fn name_row(prompt: &Prompt, area: Rect) -> u16 {
    let (body, _) = draw::split_hint_row(area);
    anchor(prompt, body).y + NAME as u16
}

fn draw_body(frame: &mut Frame, prompt: &Prompt, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let rows = prompt.fields.len() as u16;
    let anchor = anchor(prompt, area);
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

    // Full width and centred, as it is often wider than the fields. Drawn
    // before the menu so a tall menu cannot push it off screen.
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
        // Aligned with the field values.
        let indent = prompt
            .fields
            .get(DIRECTORY)
            .map_or(0, |f| f.label.width() as u16);
        // The room left below, capped at `MENU_ROWS`.
        let block = Rect {
            x: anchor.x.saturating_add(indent).min(area.x + area.width),
            y,
            width: width.saturating_sub(indent as usize) as u16,
            height: MENU_ROWS.min((area.y + area.height).saturating_sub(y)),
        };
        draw_menu(frame, menu, block);
    }
}

/// The selected row is `REVERSED | BOLD`, as in the picker, distinct from the
/// cursor's plain `REVERSED`.
fn draw_menu(frame: &mut Frame, menu: &Menu, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    if !menu.message.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                draw::truncate(&menu.message, area.width as usize),
                DIM,
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
                DIM
            };
            let text = draw::truncate(
                &format!("{}{name}", if selected { MARKER } else { INDENT }),
                area.width as usize,
            );
            Line::from(Span::styled(text, style))
        })
        .collect();

    // A truncated listing gives up its last row to say so.
    if menu.partial && lines.len() == height && height > 0 {
        lines.pop();
        lines.push(Line::from(Span::styled(
            draw::truncate(&format!("{INDENT}(and more)"), area.width as usize),
            DIM,
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// One field's line: dim label, then the input or the dim default.
fn field_line(field: &Field, focused: bool, width: usize) -> Line<'static> {
    // Reserve a column for the cursor so a long label cannot push it off.
    let label = draw::truncate(&field.label, width.saturating_sub(1));
    let room = width.saturating_sub(label.width() + 1);
    let mut spans = vec![Span::styled(label, DIM)];

    // Characters discarded by a `//`, drawn dim.
    let inert = crate::dirs::anchored(&field.input).0.chars().count();

    match (field.input.is_empty(), focused) {
        // Cursor on the placeholder's first letter.
        (true, true) => match split_first(&field.default) {
            Some((first, rest)) => {
                spans.push(Span::styled(first, CURSOR));
                spans.push(Span::styled(draw::truncate(rest, room), DIM));
            }
            None => spans.push(Span::styled(" ", CURSOR)),
        },
        // Unfocused: no cursor.
        (true, false) => spans.push(Span::styled(draw::truncate(&field.default, room), DIM)),
        (false, true) => spans.extend(windowed(field, room, inert)),
        (false, false) => {
            // No cursor to keep in view: truncate, keeping the dim prefix.
            let text = draw::truncate(&field.input, room);
            let shown = text.chars().count();
            let cut = inert.min(shown);
            let (before, after): (String, String) = (
                text.chars().take(cut).collect(),
                text.chars().skip(cut).collect(),
            );
            if !before.is_empty() {
                spans.push(Span::styled(before, DIM));
            }
            if !after.is_empty() {
                spans.push(Span::raw(after));
            }
        }
    }

    Line::from(spans)
}

fn split_first(s: &str) -> Option<(String, &str)> {
    let first = s.chars().next()?;
    Some((first.to_string(), &s[first.len_utf8()..]))
}

/// The visible window of `input` as character indices `(start, cursor, end)`.
/// Indices, not strings, so `windowed` can place the dim/plain boundary.
///
/// Unlike `draw::truncate`, it scrolls to keep the cursor visible. The input
/// gets a trailing blank cell for a cursor past the end.
fn around_cursor(field: &Field, max: usize) -> (usize, usize, usize) {
    let cells: Vec<char> = field.input.chars().chain([' ']).collect();
    let at = field.cursor.min(cells.len() - 1);
    let width = |c: char| c.to_string().width().max(1);

    // The cursor cell is kept even when `max` is 0.
    let mut used = width(cells[at]).min(max);
    let (mut start, mut end) = (at, at + 1);

    // Extend left first, so long text scrolls off the left, then right.
    while start > 0 && used + width(cells[start - 1]) <= max {
        start -= 1;
        used += width(cells[start]);
    }
    while end < cells.len() && used + width(cells[end]) <= max {
        used += width(cells[end]);
        end += 1;
    }

    (start, at, end)
}

/// The windowed text as spans, coalescing runs of equal style (cursor and
/// dim prefix vary independently).
fn windowed(field: &Field, max: usize, inert: usize) -> Vec<Span<'static>> {
    let cells: Vec<char> = field.input.chars().chain([' ']).collect();
    let (start, at, end) = around_cursor(field, max);

    let style = |i: usize| match (i == at, i < inert) {
        // The cursor wins, even in the dim prefix.
        (true, _) => CURSOR,
        (false, true) => DIM,
        (false, false) => Style::default(),
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut current = style(start);
    for (i, c) in cells.iter().enumerate().take(end).skip(start) {
        let s = style(i);
        if s != current && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), current));
        }
        current = s;
        run.push(*c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, current));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::super::test_support;
    use super::*;

    const COMMAND_DEFAULT: &str = "nvim --headless --listen {sock}";
    const DIRECTORY_DEFAULT: &str = "~/";

    /// Clearing after a rename or cancel would flash a blank screen before the
    /// picker or client repaints.
    #[test]
    fn only_a_created_session_hands_the_terminal_over() {
        let session = Session::new("id000000".into(), "a".into(), 100, 1);
        assert!(Outcome::Created(session).attaches());
        assert!(!Outcome::Renamed("b".into()).attaches());
        assert!(!Outcome::Cancelled.attaches());
    }

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

    /// A rendered cell with its modifier, which `render` drops.
    type Cell = (String, Modifier);

    fn cells_of(p: &Prompt, w: u16, h: u16) -> Vec<Vec<Cell>> {
        let buf = test_support::buffer(w, h, |f| draw(f, p));
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

    /// The cursor's row, trimmed to content; an inverted blank cursor counts
    /// as content.
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

    /// The buffer column where the focused field's value starts.
    fn field_column(p: &Prompt, w: u16, h: u16) -> usize {
        let rows = cells_of(p, w, h);
        for row in &rows {
            if let Some(x) = row.iter().position(|(_, m)| m.contains(Modifier::REVERSED)) {
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

    /// `<prefix> c` Enter must still create with every default.
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

    #[test]
    fn enter_submits_the_whole_form_from_either_field() {
        let mut p = at_command("nvim -u NONE --listen {sock}");
        p.on_key(Key::Up);
        type_in(&mut p, "notes");
        p.on_key(Key::Down);
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

    #[test]
    fn ctrl_c_cancels_rather_than_quitting() {
        let mut p = prompt();
        assert_eq!(p.on_key(Key::CtrlC), Step::Cancel);
    }

    #[test]
    fn a_rejected_submission_is_kept_so_it_can_be_edited() {
        let mut p = at_command("nvim");
        p.on_key(Key::Up);
        type_in(&mut p, "notes");
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
        assert_eq!(p.focus, COMMAND);
        type_in(&mut p, " --clean");
        assert_eq!(p.fields[NAME].input, "", "the name is untouched");
        assert_eq!(
            p.fields[COMMAND].input,
            format!("{COMMAND_DEFAULT} --clean"),
            "and the command took every keystroke, appended to what was there"
        );
    }

    // --- editing within a field --------------------------------------------

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

    #[test]
    fn editing_a_multibyte_name_does_not_split_a_character() {
        let mut p = prompt();
        type_in(&mut p, "日本語");
        press(&mut p, &[Key::Left, Key::Backspace]);
        assert_eq!(p.fields[NAME].input, "日語");
        type_in(&mut p, "🎉");
        assert_eq!(p.fields[NAME].input, "日🎉語");
    }

    #[test]
    fn right_or_end_takes_the_default_into_an_empty_field() {
        // Only the name has a placeholder.
        for key in [Key::Right, Key::End] {
            let mut p = prompt();
            p.on_key(key);
            let field = &p.fields[NAME];
            assert_eq!(field.input, "session 3", "{key:?}");
            assert_eq!(field.cursor, field.len(), "{key:?}: cursor at the end");

            press(&mut p, &[Key::Backspace]);
            type_in(&mut p, "9");
            assert_eq!(p.fields[NAME].input, "session 9");
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

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn the_default_is_numbered_where_the_session_goes() {
        assert_eq!(next_name(&[], 1), "session 1");
        let taken = names(&["api-server", "dotfiles", "notes"]);
        assert_eq!(next_name(&taken, 3), "session 3");
        assert_eq!(next_name(&taken, 1), "session 1");
        assert_eq!(next_name(&taken, 4), "session 4");
        assert_eq!(next_name(&[], 0), "session 1", "there is no session 0");
    }

    #[test]
    fn a_taken_default_gives_way_to_the_next_free_number_up() {
        let taken = names(&["session 1", "session 2", "session 3"]);
        assert_eq!(next_name(&taken, 3), "session 4");
        assert_eq!(next_name(&taken, 2), "session 4");
        let taken: Vec<String> = (5..1005).map(|n| format!("session {n}")).collect();
        assert_eq!(next_name(&taken, 5), "session 1", "every one above taken");
    }

    /// Duplicates are rejected case-insensitively.
    #[test]
    fn the_default_avoids_names_that_differ_only_in_case() {
        assert_eq!(next_name(&names(&["Session 1"]), 1), "session 2");
    }

    #[test]
    fn the_default_gives_up_gracefully_when_every_slot_is_taken() {
        let taken: Vec<String> = (1..=1000).map(|n| format!("session {n}")).collect();
        assert_eq!(next_name(&taken, 1), "session");
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

    /// Otherwise the field jumps on the first keystroke.
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

    /// The directory field with the menu opened by `Tab` (the real route, so
    /// broken keys fail tests) and filled with `matches`.
    fn menuing(typed: &str, matches: &[&str]) -> Prompt {
        let mut p = at_directory(typed);
        p.on_key(Key::Tab);
        assert!(p.choosing(), "Tab should have opened the menu");
        fill(&mut p, matches);
        p
    }

    /// The command field, focused, holding exactly `line` (set, since typing
    /// would append to the pre-filled default).
    fn at_command(line: &str) -> Prompt {
        let mut p = prompt();
        p.on_key(Key::Down);
        assert_eq!(p.focus, COMMAND);
        let field = &mut p.fields[COMMAND];
        field.input = line.to_string();
        field.cursor = field.len();
        p
    }

    /// The line's cells after the label, skipped by width since the label
    /// is dim like a discarded path prefix.
    fn after_label(p: &Prompt, w: u16, h: u16) -> Vec<Cell> {
        let width = p.fields[p.focus].label.width();
        line_cells(p, w, h).into_iter().skip(width).collect()
    }

    /// The directory field, focused via the movement keys, holding exactly
    /// `path` (set, since typing would append to the pre-filled home).
    fn at_directory(path: &str) -> Prompt {
        let mut p = prompt();
        p.on_key(Key::Down);
        p.on_key(Key::Down);
        assert_eq!(p.focus, DIRECTORY);
        let field = &mut p.fields[DIRECTORY];
        field.input = path.to_string();
        field.cursor = field.len();
        p
    }

    /// Stand in for the completer.
    fn fill(p: &mut Prompt, matches: &[&str]) {
        let menu = p.menu.as_mut().expect("a create prompt has a menu");
        menu.matches = matches.iter().map(|m| m.to_string()).collect();
        menu.selected = 0;
    }

    #[test]
    fn the_label_leads_the_cursor_marks_the_field_and_the_default_recedes() {
        let label = "new session name:  ".width();

        let empty = line_cells(&prompt(), 60, 9);
        assert_eq!(text_of(&empty), "new session name:  session 3");
        assert!(
            empty[..label]
                .iter()
                .all(|(_, m)| m.contains(Modifier::DIM)),
            "the label must recede: {empty:?}"
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
                .all(|(_, m)| m.contains(Modifier::DIM)),
            "the label must stay receded once typing starts: {typed:?}"
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
            typed[label..]
                .iter()
                .all(|(_, m)| !m.contains(Modifier::DIM)),
            "past the label, nothing on a typed line is dim: {typed:?}"
        );
    }

    #[test]
    fn the_labels_recede_so_the_values_lead() {
        for focus in [NAME, COMMAND, DIRECTORY] {
            let mut p = prompt();
            for _ in 0..focus {
                p.on_key(Key::Down);
            }
            let width = p.fields[focus].label.width();
            let cells = line_cells(&p, 70, 14);
            assert!(
                cells[..width]
                    .iter()
                    .all(|(_, m)| m.contains(Modifier::DIM)),
                "field {focus}'s label must recede: {:?}",
                &cells[..width]
            );
        }

        let p = renaming("dotfiles");
        let width = p.fields[NAME].label.width();
        let cells = line_cells(&p, 70, 14);
        assert!(
            cells[..width]
                .iter()
                .all(|(_, m)| m.contains(Modifier::DIM)),
            "the rename label must recede too: {:?}",
            &cells[..width]
        );
    }

    /// Appending to it is covered by `typing_goes_to_the_focused_field_only`.
    #[test]
    fn the_command_field_starts_at_its_default_rather_than_offering_it() {
        let p = prompt();
        assert_eq!(p.fields[COMMAND].input, COMMAND_DEFAULT, "really there");
        assert_eq!(
            p.fields[COMMAND].cursor,
            p.fields[COMMAND].len(),
            "with the cursor after it"
        );
    }

    #[test]
    fn only_the_session_name_is_still_a_placeholder() {
        let p = prompt();
        assert!(
            p.fields[NAME].input.is_empty(),
            "the name is offered, not prefilled"
        );
        assert!(!p.fields[COMMAND].input.is_empty());
        assert!(!p.fields[DIRECTORY].input.is_empty());

        let mut p = prompt();
        p.on_key(Key::Right);
        assert_eq!(p.fields[NAME].input, p.fields[NAME].default);
    }

    #[test]
    fn only_the_focused_field_shows_a_cursor() {
        for p in [
            prompt(),
            {
                let mut p = prompt();
                p.on_key(Key::Down);
                p
            },
            // The menu's highlight is reversed too.
            menuing("/home/", &["alpha", "beta"]),
        ] {
            // Cursor: REVERSED; selected row: REVERSED|BOLD.
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

        p.on_key(Key::Up);
        p.on_key(Key::Up);
        let lines = render(&p, 60, 14);
        assert!(
            lines.iter().any(|l| l.contains("dir-10")),
            "expected to be looking at the selection: {lines:#?}"
        );
    }

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

    /// No rows reserved for a menu either.
    #[test]
    fn a_rename_has_no_menu_at_all() {
        assert!(renaming("notes").menu.is_none());
        let lines = render(&renaming("notes"), 60, 9);
        let filled = lines.iter().filter(|l| !l.is_empty()).count();
        assert_eq!(filled, 2, "just the field and the hint row: {lines:#?}");
    }

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

    #[test]
    fn tab_opens_the_menu_then_accepts_from_it() {
        let mut p = menuing("/home/pro", &["projects", "prototypes"]);
        p.on_key(Key::CtrlN);
        p.on_key(Key::Tab);
        assert!(!p.choosing(), "accepting closes the menu");
        assert_eq!(p.fields[DIRECTORY].input, "/home/prototypes");
    }

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

    /// `pro` beside `projects` must not be stepped into just because it exists.
    #[test]
    fn anything_but_tab_after_an_accept_forgets_the_step() {
        let mut p = menuing("/home/pro", &["pro", "projects"]);
        p.on_key(Key::Tab);
        assert_eq!(p.fields[DIRECTORY].input, "/home/pro");

        p.on_key(Key::Left);
        p.on_key(Key::Tab);
        assert!(p.choosing());
        assert_eq!(
            p.fields[DIRECTORY].input, "/home/pro",
            "reopened on the query rather than stepping into `pro`"
        );
    }

    /// Fails if the hint row and the key handler drift apart.
    #[test]
    fn the_hint_row_offers_tab_only_where_tab_completes() {
        for focus in [NAME, COMMAND, DIRECTORY] {
            let mut p = prompt();
            for _ in 0..focus {
                p.on_key(Key::Down);
            }
            assert_eq!(p.focus, focus);
            let advertised = p.hints().contains('⇥');

            let mut pressed = prompt();
            for _ in 0..focus {
                pressed.on_key(Key::Down);
            }
            let before = pressed.fields[focus].input.clone();
            pressed.on_key(Key::Tab);
            // Opening the menu or adopting the default both count.
            let did_something = pressed.choosing() || pressed.fields[focus].input != before;

            assert_eq!(
                advertised, did_something,
                "field {focus}: the row says {advertised} and the key does \
                 {did_something}"
            );
        }
    }

    /// The directory field, backspaced to empty, showing the dim default.
    fn emptied_directory() -> Prompt {
        let mut p = prompt();
        press(&mut p, &[Key::Down, Key::Down]);
        assert_eq!(p.focus, DIRECTORY);
        for _ in 0..p.fields[DIRECTORY].len() {
            p.on_key(Key::Backspace);
        }
        assert!(p.fields[DIRECTORY].input.is_empty(), "cleared to nothing");
        p
    }

    /// Regression: `Tab` listed home's children under a field that still
    /// looked empty.
    #[test]
    fn tab_takes_the_home_path_back_into_an_emptied_field() {
        let mut p = emptied_directory();

        p.on_key(Key::Tab);
        let field = &p.fields[DIRECTORY];
        assert_eq!(field.input, DIRECTORY_DEFAULT, "the dim path, taken in");
        assert_eq!(field.cursor, field.len(), "with the cursor after it");
        assert!(
            !p.choosing(),
            "and no menu: this `Tab` was the one that filled the field"
        );

        p.on_key(Key::Tab);
        assert!(p.choosing());
        assert_eq!(
            p.fields[DIRECTORY].input, DIRECTORY_DEFAULT,
            "opening the menu does not move the path it opened on"
        );
    }

    #[test]
    fn tab_on_a_bare_tilde_writes_the_slash_that_steps_into_it() {
        let mut p = emptied_directory();
        type_in(&mut p, "~");

        p.on_key(Key::Tab);
        let field = &p.fields[DIRECTORY];
        assert_eq!(field.input, "~/", "the slash, written for you");
        assert_eq!(field.cursor, field.len(), "with the cursor after it");
        assert!(
            !p.choosing(),
            "and no menu: this `Tab` was the one that wrote the slash"
        );

        p.on_key(Key::Tab);
        assert!(p.choosing(), "the next one opens on the home directory");
        assert_eq!(
            p.fields[DIRECTORY].input, "~/",
            "opening the menu does not move the path it opened on"
        );
    }

    /// A second slash would make `//`, meaning the root.
    #[test]
    fn tab_never_adds_a_second_slash_to_a_tilde() {
        let mut p = prompt();
        press(
            &mut p,
            &[Key::Down, Key::Down, Key::Tab, Key::Tab, Key::Tab],
        );
        assert_eq!(p.fields[DIRECTORY].value(), "~/");
        assert_eq!(
            crate::dirs::anchored(&p.fields[DIRECTORY].value()).1,
            "~/",
            "still the home directory, not the root"
        );
    }

    /// `~r` is a half-typed name for the menu, not a lone `~`.
    #[test]
    fn tab_writes_no_slash_after_a_tilde_with_anything_on_it() {
        let mut p = emptied_directory();
        type_in(&mut p, "~r");

        p.on_key(Key::Tab);
        assert_eq!(p.fields[DIRECTORY].input, "~r", "left as typed");
        assert!(p.choosing(), "the ordinary `Tab`, which opens the menu");
    }

    /// The hint row still offers `⇥ complete`, so `Tab` must do something.
    #[test]
    fn tab_still_opens_the_menu_when_there_is_no_home_to_take() {
        let mut p = Prompt::create(
            "session 3".to_string(),
            COMMAND_DEFAULT.to_string(),
            String::new(),
        );
        press(&mut p, &[Key::Down, Key::Down]);
        assert_eq!(p.focus, DIRECTORY);
        assert!(p.fields[DIRECTORY].input.is_empty());

        p.on_key(Key::Tab);
        assert!(p.choosing(), "nothing to take, so the menu opens");
    }

    #[test]
    fn a_slash_in_an_emptied_field_is_the_root_itself() {
        let mut p = emptied_directory();
        type_in(&mut p, "/");
        assert_eq!(p.fields[DIRECTORY].input, "/");

        // Nothing dim, and `refresh` asks about the root.
        let value = p.fields[DIRECTORY].value();
        let (inert, live) = crate::dirs::anchored(&value);
        assert_eq!((inert, live), ("", "/"));
        assert_eq!(crate::dirs::split(live), Some(("/", "")));

        match p.on_key(Key::Enter) {
            Step::Submit(s) => assert_eq!(s.directory, Some("/".to_string())),
            other => panic!("expected a create, got {other:?}"),
        }
    }

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

    #[test]
    fn enter_accepts_from_the_menu_and_a_second_enter_creates() {
        let mut p = menuing("/home/nvmx", &["nvmux-rs"]);
        assert!(p.choosing());

        assert_eq!(p.on_key(Key::Enter), Step::None, "the first enter accepts");
        assert!(!p.choosing(), "and closes the menu");
        assert_eq!(p.fields[DIRECTORY].input, "/home/nvmux-rs");

        match p.on_key(Key::Enter) {
            Step::Submit(s) => assert_eq!(s.directory, Some("/home/nvmux-rs".to_string())),
            other => panic!("the second enter must create, got {other:?}"),
        }
    }

    /// Including the step the next `Tab` takes.
    #[test]
    fn enter_and_tab_accept_the_same_way() {
        let mut by_tab = menuing("/home/pro", &["projects"]);
        by_tab.on_key(Key::Tab);
        let mut by_enter = menuing("/home/pro", &["projects"]);
        by_enter.on_key(Key::Enter);
        assert_eq!(
            by_tab.fields[DIRECTORY].input,
            by_enter.fields[DIRECTORY].input
        );

        by_tab.on_key(Key::Tab);
        by_enter.on_key(Key::Tab);
        assert_eq!(
            by_tab.fields[DIRECTORY].input, by_enter.fields[DIRECTORY].input,
            "and both leave the next Tab able to step in"
        );
        assert_eq!(by_enter.fields[DIRECTORY].input, "/home/projects/");
    }

    #[test]
    fn enter_creates_when_no_menu_is_open() {
        let mut p = at_directory("/home/you/");
        assert!(!p.choosing());
        assert!(matches!(p.on_key(Key::Enter), Step::Submit(_)));
    }

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

    #[test]
    fn typing_narrows_an_open_menu_and_leaves_it_open() {
        let mut p = menuing("/home/", &["alpha", "beta"]);
        p.on_key(Key::CtrlN);
        assert_eq!(p.menu.as_ref().expect("a menu").selected, 1);

        type_in(&mut p, "a");
        assert!(p.choosing(), "still open");
        assert_eq!(p.fields[DIRECTORY].input, "/home/a");
    }

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

    #[test]
    fn the_hint_row_is_on_the_last_row_and_follows_the_menu() {
        let closed = render(&prompt(), 60, 14);
        assert!(
            closed[13].contains("⏎ create") && closed[13].contains("↑↓ field"),
            "expected the closed hints on the last row, got {:?}",
            closed[13]
        );
        assert!(
            !closed[13].contains("⇥"),
            "the name field has nothing to complete: {:?}",
            closed[13]
        );

        let at_dir = render(&at_directory("/home/"), 60, 14);
        assert!(
            at_dir[13].contains("⇥ complete"),
            "the working directory field does: {:?}",
            at_dir[13]
        );

        let open = render(&menuing("/home/", &["alpha"]), 60, 14);
        assert!(
            open[13].contains("⇥ ⏎ accept") && open[13].contains("↑↓ choose"),
            "expected the hints to follow the menu, got {:?}",
            open[13]
        );
        assert!(
            !open[13].contains('^'),
            "the arrows say it in half the width; the chords still work but do \
             not need a place on a row that has to fit a narrow terminal: {:?}",
            open[13]
        );
    }

    #[test]
    fn the_directory_field_starts_at_home_rather_than_offering_it() {
        let p = prompt();
        assert_eq!(
            p.fields[DIRECTORY].input, DIRECTORY_DEFAULT,
            "pre-filled, not a placeholder"
        );
        assert_eq!(
            p.fields[DIRECTORY].cursor,
            p.fields[DIRECTORY].len(),
            "with the cursor after it, ready to type"
        );
        // Ordinary editable text.
        let mut p = prompt();
        p.on_key(Key::Down);
        p.on_key(Key::Down);
        p.on_key(Key::Backspace);
        assert_eq!(
            p.fields[DIRECTORY].input,
            DIRECTORY_DEFAULT.trim_end_matches('/')
        );
    }

    /// The pre-filled text ends in `/`, so typing one makes `//`.
    #[test]
    fn a_slash_at_the_end_of_the_home_path_means_the_root() {
        let mut p = prompt();
        p.on_key(Key::Down);
        p.on_key(Key::Down);
        type_in(&mut p, "/etc");
        assert_eq!(
            p.fields[DIRECTORY].input,
            format!("{DIRECTORY_DEFAULT}/etc")
        );

        match p.on_key(Key::Enter) {
            Step::Submit(s) => assert_eq!(s.directory, Some("/etc".to_string())),
            other => panic!("expected a create, got {other:?}"),
        }
    }

    #[test]
    fn the_default_directory_is_the_tilde() {
        assert_eq!(default_directory("/home/you"), "~/");
        assert_eq!(
            default_directory("/home/you/"),
            "~/",
            "a home already ending in a slash does not make a `//`"
        );
    }

    /// `~` would be refused on Enter.
    #[test]
    fn a_host_with_no_home_gets_no_default_directory() {
        assert!(default_directory("").is_empty());
        assert!(
            default_directory("/").is_empty(),
            "not a home, just the root"
        );
    }

    /// Uses the real expansion: a mistake here would be invisible until a
    /// session started elsewhere.
    #[test]
    fn pressing_enter_on_the_default_still_creates_in_the_home_directory() {
        const HOME: &str = "/home/them";
        let mut p = Prompt::create(
            "session 3".to_string(),
            COMMAND_DEFAULT.to_string(),
            default_directory(HOME),
        );
        let Step::Submit(s) = p.on_key(Key::Enter) else {
            panic!("expected a create");
        };
        let typed = s.directory.expect("a directory");
        assert_eq!(typed, "~/", "what the untouched field submits");
        assert_eq!(
            crate::session::validate_directory(&typed, HOME).expect("expands"),
            "/home/them/",
            "the same absolute path the spelled-out default used to send"
        );
    }

    /// POSIX reads `a//b` as `a/b`, so a raw submission would silently start
    /// in the wrong directory.
    #[test]
    fn what_is_submitted_is_the_path_that_counts() {
        for (typed, expected) in [
            ("/home/you//etc", "/etc"),
            ("/home/you/projects//srv/www", "/srv/www"),
            ("/home/you//", "/"),
            ("/home/you/projects", "/home/you/projects"),
        ] {
            let mut p = at_directory(typed);
            match p.on_key(Key::Enter) {
                Step::Submit(s) => {
                    assert_eq!(s.directory, Some(expected.to_string()), "from {typed:?}")
                }
                other => panic!("expected a create, got {other:?}"),
            }
        }
    }

    #[test]
    fn accepting_after_a_double_slash_keeps_the_discarded_prefix() {
        let mut p = menuing("/home/you//e", &["etc"]);
        p.on_key(Key::Tab);
        assert_eq!(p.fields[DIRECTORY].input, "/home/you//etc");

        match p.on_key(Key::Enter) {
            Step::Submit(s) => assert_eq!(s.directory, Some("/etc".to_string())),
            other => panic!("expected a create, got {other:?}"),
        }
    }

    #[test]
    fn the_part_that_no_longer_counts_is_dim_and_the_rest_is_not() {
        let dim_text = |p: &Prompt| -> String {
            after_label(p, 60, 14)
                .iter()
                .filter(|(_, m)| m.contains(Modifier::DIM))
                .map(|(s, _)| s.as_str())
                .collect()
        };

        assert_eq!(
            dim_text(&at_directory("/home/you/projects")),
            "",
            "an ordinary path is all live text"
        );

        // The cursor is at the end, outside the dim prefix.
        assert_eq!(dim_text(&at_directory("/home/you//etc")), "/home/you/");
        assert_eq!(
            dim_text(&at_directory("/home/you/projects//etc")),
            "/home/you/projects/",
            "everything discarded, not just the home part"
        );
    }

    /// Including with the cursor inside the dim prefix.
    #[test]
    fn a_dim_prefix_survives_a_field_too_narrow_for_it() {
        for width in [24u16, 30, 40, 60] {
            let mut p = at_directory("/home/you/a-long-directory-name//etc");
            // After the label, every dim cell precedes every live one.
            let value: Vec<_> = after_label(&p, width, 14);
            let last_dim = value.iter().rposition(|(_, m)| m.contains(Modifier::DIM));
            let first_live = value.iter().position(|(_, m)| !m.contains(Modifier::DIM));
            if let (Some(last), Some(first)) = (last_dim, first_live) {
                assert!(
                    last < first,
                    "the dim half is not contiguous at {width}: {value:?}"
                );
            }

            let live: String = value
                .iter()
                .filter(|(_, m)| !m.contains(Modifier::DIM))
                .map(|(s, _)| s.as_str())
                .collect();
            assert!(
                live.trim().ends_with("etc"),
                "the live half should be on screen at {width}: {live:?}"
            );

            // Cursor inside the dim prefix.
            p.on_key(Key::Home);
            let cells = after_label(&p, width, 14);
            let cursors = cells
                .iter()
                .filter(|(_, m)| m.contains(Modifier::REVERSED) && !m.contains(Modifier::BOLD))
                .count();
            assert_eq!(cursors, 1, "one cursor at {width} columns: {cells:?}");
        }
    }

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

        // A menu larger than any of these terminals, where off-by-ones panic.
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

    /// The picker list is hidden, so the label must name the session.
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

    /// Unlike a rejected create, the default does not change.
    #[test]
    fn a_rejected_rename_keeps_offering_the_current_name() {
        let mut p = renaming("dotfiles");
        p.fail("a session named \"notes\" already exists".to_string(), None);
        assert_eq!(p.fields[NAME].default, "dotfiles");
    }

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

        let offering = menuing("/home/you/pro", &["projects", "prototypes"]);
        test_support::assert_no_colour(60, 9, |f| draw(f, &offering));
    }

    // --- out of the gap the picker makes -------------------------------------

    /// `names` as a listing, numbered from 1.
    fn listing(names: &[&str]) -> Vec<Session> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let num = i as u32 + 1;
                let mut s = Session::new(format!("id{i:06}"), n.to_string(), 100, num);
                s.state.num = num;
                s
            })
            .collect()
    }

    #[test]
    fn the_default_name_is_numbered_where_the_session_goes_in_the_listing() {
        let listed = listing(&["api-server", "dotfiles", "notes"]);
        assert_eq!(
            default_name(&listed, Some(3)),
            "session 3",
            "between dotfiles and notes"
        );
        assert_eq!(default_name(&listed, None), "session 4", "at the end");
        assert_eq!(default_name(&[], None), "session 1");
        let listed = listing(&["session 1", "Session 2", "notes"]);
        assert_eq!(default_name(&listed, Some(2)), "session 3", "2 is taken");
    }

    // --- floating -----------------------------------------------------------

    /// Like `listing`, but with valid nvmux ids.
    fn hosted(names: &[&str]) -> Vec<Session> {
        const IDS: [&str; 5] = ["aaaaaaaa", "bbbbbbbb", "cccccccc", "dddddddd", "eeeeeeee"];
        names
            .iter()
            .zip(IDS)
            .zip(1..)
            .map(|((name, id), num)| {
                let mut s = Session::new(id.to_string(), name.to_string(), 100, num);
                s.state.num = num;
                s
            })
            .collect()
    }

    fn id(id: &str) -> Id {
        Id::new(id).expect("an id")
    }

    fn named(rows: &[Session]) -> Vec<String> {
        rows.iter().map(|s| s.name.clone()).collect()
    }

    /// An in-memory host that lists, renames and renumbers.
    struct Host {
        location: crate::transport::Location,
        rows: std::sync::Mutex<Vec<Session>>,
    }

    impl Host {
        fn new(rows: Vec<Session>) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                location: crate::transport::Location::Local,
                rows: std::sync::Mutex::new(rows),
            })
        }
    }

    impl Transport for Host {
        fn location(&self) -> &crate::transport::Location {
            &self.location
        }
        fn list_sessions(&self) -> Result<Vec<Session>> {
            let mut rows = self.rows.lock().expect("rows").clone();
            rows.sort_by_key(|s| s.state.num);
            Ok(rows)
        }
        fn create_session(&self, _: &str, _: &Launch, _: &str) -> Result<Session> {
            unimplemented!()
        }
        fn kill_session(&self, _: &Session) -> Result<()> {
            unimplemented!()
        }
        fn rename_session(&self, s: &Session, name: &str) -> Result<()> {
            let mut rows = self.rows.lock().expect("rows");
            for row in rows.iter_mut().filter(|r| r.id == s.id) {
                row.name = name.to_string();
            }
            Ok(())
        }
        fn renumber(&self, batch: &[Session]) -> Result<()> {
            let mut rows = self.rows.lock().expect("rows");
            for b in batch {
                for row in rows.iter_mut().filter(|r| r.id == b.id) {
                    row.num = b.num;
                    row.state.num = b.num;
                }
            }
            Ok(())
        }
        fn home(&self) -> &str {
            "/home/you"
        }
        fn dir_source(&self) -> crate::dirs::DirSource {
            crate::dirs::DirSource::Local
        }
        fn local_socket_for(&self, _: &Session) -> Result<std::path::PathBuf> {
            unimplemented!()
        }
    }

    fn key(panel: &mut Panel, code: ratatui::crossterm::event::KeyCode) -> Did {
        let key = KeyEvent::new(code, ratatui::crossterm::event::KeyModifiers::NONE);
        panel.on_key(key, Instant::now())
    }

    fn type_into(panel: &mut Panel, text: &str) {
        for c in text.chars() {
            assert_eq!(
                key(panel, ratatui::crossterm::event::KeyCode::Char(c)),
                Did::Stay
            );
        }
    }

    #[test]
    fn the_floating_create_prompt_offers_the_name_for_where_the_session_goes() {
        let rows = hosted(&["one", "two", "three"]);
        let after = Panel::create_on(Place::After(Some(id("aaaaaaaa"))), rows.clone(), None);
        assert_eq!(after.prompt.fields[NAME].default, "session 2");
        let last = Panel::create_on(Place::Last, rows, None);
        assert_eq!(last.prompt.fields[NAME].default, "session 4");
    }

    /// The panel ends at least a row of the session above the hint row.
    #[test]
    fn the_floating_prompt_fits_where_it_can() {
        let rows = hosted(&["one", "two"]);
        let create = Panel::create_on(Place::Last, rows.clone(), None);
        let rename = Panel::rename_on(id("aaaaaaaa"), rows, None);
        let screen = Rect::new(0, 0, 80, 24);
        let c = create.area(screen).expect("room for a create");
        let r = rename.area(screen).expect("room for a rename");
        for a in [c, r] {
            assert!(
                a.x >= SESSION_COLS && a.right() + SESSION_COLS <= 80,
                "{a:?}"
            );
            assert!(
                a.y >= SESSION_ROWS && a.bottom() + SESSION_ROWS <= 23,
                "{a:?}"
            );
        }
        assert!(r.height < c.height && r.width <= c.width, "{r:?} {c:?}");
        assert_eq!(create.area(Rect::new(0, 0, 50, 24)), None);
        assert_eq!(create.area(Rect::new(0, 0, 80, 18)), None);
    }

    #[test]
    fn the_floating_create_prompt_draws_its_form_and_its_hints() {
        let mut panel = Panel::create_on(Place::Last, hosted(&["one"]), None);
        let screen = Rect::new(0, 0, 80, 24);
        let area = panel.area(screen).expect("room");
        let buf = panel.draw(area);
        let rows: Vec<String> = (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect()
            })
            .collect();
        for want in ["new session name", "nvim command", "working directory"] {
            assert!(rows.iter().any(|r| r.contains(want)), "{want}: {rows:#?}");
        }
        assert!(!rows.iter().any(|r| r.contains("⏎ create")), "{rows:#?}");
        assert!(rows[rows.len() - 1].trim().is_empty(), "{rows:#?}");

        let (_, row) = float::split(screen);
        let hints = panel.draw_hints(row);
        let whole = test_support::buffer(80, 24, |f| draw(f, &panel.prompt));
        for x in 0..80 {
            assert_eq!(hints[(x, 23)], whole[(x, 23)], "column {x}");
        }
        let said: String = (0..80).map(|x| hints[(x, 23)].symbol()).collect();
        assert!(said.contains("⏎ create"), "{said:?}");
    }

    #[test]
    fn the_floating_prompt_takes_keys_and_esc_takes_it_down() {
        let mut panel = Panel::create_on(Place::Last, hosted(&["one"]), None);
        type_into(&mut panel, "blog");
        assert_eq!(panel.prompt.fields[NAME].input, "blog");
        assert_eq!(
            key(&mut panel, ratatui::crossterm::event::KeyCode::Esc),
            Did::Close
        );
        assert_eq!(panel.leave(), None);
    }

    #[test]
    fn a_session_made_floating_goes_after_the_row_and_is_gone_to() {
        let _turn = super::super::test_support::PANELS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rows = hosted(&["one", "two", "three"]);
        let mut made = Session::new("dddddddd".into(), "blog".into(), 100, 4);
        made.state.num = 4;
        let host = Host::new(rows.clone());
        host.rows.lock().expect("rows").push(made.clone());
        let mut panel =
            Panel::create_on(Place::After(Some(id("aaaaaaaa"))), rows, Some(host.clone()));
        assert_eq!(panel.made(host.as_ref(), made), Did::Close);
        assert_eq!(panel.leave(), Some(Leave::Switch(id("dddddddd"))));
        let left = super::super::picker::take_changed().expect("rows left");
        assert_eq!(named(&left), ["one", "blog", "two", "three"]);
    }

    #[test]
    fn a_session_made_floating_from_a_session_goes_at_the_end() {
        let _turn = super::super::test_support::PANELS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rows = hosted(&["one", "two"]);
        let mut made = Session::new("cccccccc".into(), "blog".into(), 100, 3);
        made.state.num = 3;
        let host = Host::new(rows.clone());
        let mut panel = Panel::create_on(Place::Last, rows, Some(host.clone()));
        assert_eq!(panel.made(host.as_ref(), made), Did::Close);
        assert_eq!(panel.leave(), Some(Leave::Switch(id("cccccccc"))));
        let left = super::super::picker::take_changed().expect("rows left");
        assert_eq!(named(&left), ["one", "two", "blog"]);
        let numbers: Vec<u32> = host
            .list_sessions()
            .expect("rows")
            .iter()
            .map(|s| s.num)
            .collect();
        assert_eq!(numbers, [1, 2], "nothing renumbered");
    }

    #[test]
    fn a_rename_floating_leaves_the_rows_renamed() {
        let _turn = super::super::test_support::PANELS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rows = hosted(&["one", "two"]);
        let host = Host::new(rows.clone());
        let mut panel = Panel::rename_on(id("bbbbbbbb"), rows, Some(host.clone()));
        for _ in "two".chars() {
            key(&mut panel, ratatui::crossterm::event::KeyCode::Backspace);
        }
        type_into(&mut panel, "blog");
        assert_eq!(
            key(&mut panel, ratatui::crossterm::event::KeyCode::Enter),
            Did::Close
        );
        assert_eq!(panel.leave(), None);
        let left = super::super::picker::take_changed().expect("rows left");
        assert_eq!(named(&left), ["one", "blog"]);
    }

    #[test]
    fn a_rename_of_a_session_gone_says_so() {
        let rows = hosted(&["one"]);
        let host = Host::new(rows.clone());
        let mut panel = Panel::rename_on(id("cccccccc"), rows, Some(host));
        assert_eq!(
            key(&mut panel, ratatui::crossterm::event::KeyCode::Enter),
            Did::Stay
        );
        assert_eq!(
            panel.prompt.message.as_deref(),
            Some("that session is gone")
        );
    }

    #[test]
    fn the_name_row_is_where_the_name_field_is_drawn() {
        let p = Prompt::create("session 4".into(), "nvim".into(), "~/".into());
        for (w, h) in [(72, 15), (80, 24), (40, 6), (120, 50)] {
            let lines = super::super::test_support::render(w, h, |f| draw(f, &p));
            let drawn = lines
                .iter()
                .position(|l| l.contains("new session name"))
                .expect("the name field is drawn");
            assert_eq!(
                usize::from(name_row(&p, Rect::new(0, 0, w, h))),
                drawn,
                "{w}x{h}: {lines:#?}"
            );
        }
    }
}
