//! Picker state.
//!
//! Free of I/O and ratatui: a state machine that takes key events and returns
//! [`Request`]s for the caller, so keybinds, filtering and prompts are testable
//! without a terminal, transport or Neovim.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use super::backspace::Backspace;
use super::effects;
use super::landing::Landing;
use super::note;
use super::starfield::{self, Starfield};
use super::swap::Swap;
use crate::notes::Note;
use crate::session::Session;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Typing a filter; the list narrows live.
    Filter,
    Confirm {
        id: String,
        prompt: String,
    },
    /// A session picked up and being moved. `was` is every visible row's
    /// `(id, state.num)` at the start of the grab, for `Esc` to restore and
    /// for `Space` to tell a real move from a no-op.
    Reorder {
        id: String,
        was: Vec<(String, u32)>,
    },
    /// Confirming a quit with unsaved settings (see [`App::set_unsaved`]).
    /// `overwrite`: the file changed the same settings and `w` overwrites.
    Leave {
        prompt: String,
        overwrite: bool,
    },
}

/// Work for the caller; the picker never performs I/O itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    None,
    Attach(String),
    /// Prompt for a name; the session goes after the highlighted row, which
    /// stays put meanwhile (see [`App::placement_of`]).
    NewSession,
    RenameSession(String),
    Kill(String),
    /// Every visible session with the number to store. The whole arrangement,
    /// not a diff: a stored [`Session::num`] can differ from the shown one
    /// (`finish_listing` numbers rows by position), and a skipped row could
    /// then collide with a number just written. Never empty; a no-op grab
    /// returns [`Request::None`].
    Reorder(Vec<(String, u32)>),
    Help,
    Settings,
    /// Save unsaved settings and quit if that worked; `overwrite` also
    /// overwrites the file's own changes to them.
    SaveAndQuit {
        overwrite: bool,
    },
    Quit,
}

pub struct App {
    sessions: Vec<Session>,
    filter: String,
    /// Index into the *visible* (filtered) list.
    selected: usize,
    mode: Mode,
    /// Transient message shown in place of the hints.
    message: Option<String>,
    /// Digits typed while more could change the match (see
    /// [`App::on_digit`]).
    pending: Option<u32>,
    /// The session `Esc` returns to (see [`App::set_came_from`]).
    came_from: Option<String>,
    /// Session asked for before it was listed, and the row the cursor was on
    /// (see [`App::select_session`]).
    wanted: Option<(String, usize)>,
    pressed: Option<Pressed>,
    /// Reused: building a `Matcher` allocates its whole matrix, and
    /// [`App::visible`] (`&self`) runs several times per keystroke.
    matcher: RefCell<Matcher>,
    /// `[effects.move] enabled`: trails, swap and landing. Cached like the
    /// flags below (see [`App::reload_effects`]).
    trail: bool,
    /// Trails off each end of a moved row (see [`starfield`]); two fields so
    /// the ends are not mirror images. Kept across grabs so each grab draws
    /// new stars.
    after: Starfield,
    before: Starfield,
    landing: Option<Landing>,
    swap: Swap,
    /// `[effects.cursor] enabled`.
    lights: bool,
    /// Only one row glows at a time, so a held key leaves no smear.
    glow: Option<Glow>,
    /// `[effects.filter] enabled`.
    sift: bool,
    leaving: Option<Leaving>,
    /// `[effects.kill] enabled`.
    kill: bool,
    strike: Option<Strike>,
    /// Row being erased, by session id, until a fresh listing replaces it.
    backspace: Option<(String, Backspace)>,
    /// `[effects.create] enabled`.
    create: bool,
    room: Option<Room>,
    /// Latest note per session id (see [`crate::notes`]).
    notes: HashMap<String, Note>,
    /// Spinner clock; runs even with no spinner shown.
    spin: Duration,
    /// Unsaved settings count, asked about on quit (see [`Mode::Leave`]).
    unsaved: usize,
}

/// Id of the placeholder row in the gap [`App::make_room`] opens. `+` is not
/// a valid id character (see [`crate::ids::is_valid_id`]), so it never
/// collides with a real session.
pub const GHOST_ID: &str = "+";

/// The gap a session about to be created will land in.
#[derive(Debug, Clone)]
struct Room {
    /// The placeholder row, with the name the prompt will offer and the
    /// number the session will get.
    ghost: Session,
    /// Index among the visible rows.
    at: usize,
    /// One past every row's number; the last row takes it once everything
    /// below the gap has moved down.
    last: u32,
    age: Duration,
}

/// A row the cursor has left, and how long ago.
#[derive(Debug, Clone)]
struct Glow {
    id: String,
    age: Duration,
}

/// Strike-through on a session's name: fraction drawn (`0..=1`) and whether
/// it is retracting. An amount, not an age, so a `[y/N]` answered mid-draw
/// retracts from where it got to.
#[derive(Debug, Clone)]
struct Strike {
    id: String,
    drawn: f32,
    back: bool,
}

/// Rows one filter keystroke dropped, and how long ago.
#[derive(Debug, Clone)]
struct Leaving {
    ids: Vec<String>,
    age: Duration,
}

/// One drawn row. `leaving`: dropped by the filter and fading. `ghost`: the
/// placeholder in a create gap (see [`App::make_room`]).
///
/// `num` is the session's own, except below a gap, where each row shows the
/// number of the row below: numbers stay put on screen while rows move down
/// past them, matching what the create will store (see
/// [`App::placement_of`]).
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    pub session: &'a Session,
    pub leaving: bool,
    pub ghost: bool,
    pub num: u32,
}

impl<'a> Row<'a> {
    fn of(session: &'a Session, leaving: bool) -> Self {
        Self {
            session,
            leaving,
            ghost: false,
            num: session.state.num,
        }
    }
}

/// Where a left press landed, which decides what its release means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pressed {
    /// An unselected row: the press selected it and the release does
    /// nothing. A drag picks it up.
    Row,
    /// The selected row: the release attaches, unless a drag made it a move.
    Selected,
}

/// Filter targets: the name and the last component of the working directory
/// (the project folder, for sessions named `scratch` or `session 3`).
///
/// Not the full path: sessions share most of it, so a fuzzy match over
/// `/home/you/...` would admit nearly every row.
fn haystacks(s: &Session) -> impl Iterator<Item = &str> {
    let folder = s
        .directory
        .rsplit('/')
        .find(|component| !component.is_empty());
    std::iter::once(s.name.as_str()).chain(folder)
}

impl App {
    pub fn new(sessions: Vec<Session>) -> Self {
        Self {
            sessions,
            filter: String::new(),
            selected: 0,
            mode: Mode::Normal,
            message: None,
            pending: None,
            came_from: None,
            wanted: None,
            pressed: None,
            matcher: RefCell::new(Matcher::new(Config::DEFAULT)),
            trail: crate::config::get().effects.move_enabled(),
            after: Starfield::seeded(),
            before: Starfield::seeded(),
            landing: None,
            swap: Swap::seeded(),
            lights: crate::config::get().effects.cursor_enabled(),
            glow: None,
            sift: crate::config::get().effects.filter_enabled(),
            leaving: None,
            kill: crate::config::get().effects.kill_enabled(),
            strike: None,
            backspace: None,
            create: crate::config::get().effects.create_enabled(),
            room: None,
            notes: HashMap::new(),
            spin: Duration::ZERO,
            unsaved: 0,
        }
    }

    /// Set by the caller, since the picker reads nothing itself.
    pub fn set_unsaved(&mut self, unsaved: usize) {
        self.unsaved = unsaved;
    }

    /// The save on quit failed with `why`. On a `clash` (the file changed the
    /// same settings) the question stays up so a second `w` overwrites;
    /// otherwise the picker returns, showing `why`.
    pub fn not_saved(&mut self, why: String, clash: bool) {
        if clash {
            self.mode = Mode::Leave {
                prompt: format!("{why} — w overwrites and quits  q quit anyway  esc stay"),
                overwrite: true,
            };
        } else {
            self.mode = Mode::Normal;
            self.set_message(why);
        }
    }

    /// `q`, or `Ctrl-c` anywhere. With unsaved settings, first cancels the
    /// current mode as `Esc` would and asks on the hint row.
    fn quit(&mut self) -> Request {
        if self.unsaved == 0 {
            return Request::Quit;
        }
        match self.mode.clone() {
            Mode::Reorder { id, was } => self.restore(&was, &id),
            Mode::Confirm { .. } => self.decline(),
            Mode::Normal | Mode::Filter | Mode::Leave { .. } => {}
        }
        self.pending = None;
        let n = self.unsaved;
        self.mode = Mode::Leave {
            prompt: format!(
                "{n} unsaved setting{} — w save and quit  q quit anyway  esc stay",
                if n == 1 { "" } else { "s" }
            ),
            overwrite: false,
        };
        Request::None
    }

    fn on_key_leave(&mut self, key: Key) -> Request {
        let Mode::Leave { overwrite, .. } = &self.mode else {
            return Request::None;
        };
        let overwrite = *overwrite;
        match key {
            Key::Char('w') => Request::SaveAndQuit { overwrite },
            Key::Char('q') | Key::CtrlC => Request::Quit,
            Key::Esc => {
                self.mode = Mode::Normal;
                Request::None
            }
            _ => Request::None,
        }
    }

    /// Re-read the cached effect flags after the settings screen closes (see
    /// [`crate::ui::settings`]).
    pub fn reload_effects(&mut self) {
        let effects = crate::config::get().effects;
        self.trail = effects.move_enabled();
        self.lights = effects.cursor_enabled();
        self.sift = effects.filter_enabled();
        self.kill = effects.kill_enabled();
        self.create = effects.create_enabled();
    }

    /// The latest round of notes (see [`crate::notes::Poller`]); a session
    /// missing from it has none.
    pub fn set_notes(&mut self, notes: HashMap<String, Note>) {
        self.notes = notes;
    }

    pub fn note(&self, id: &str) -> Option<&Note> {
        self.notes.get(id)
    }

    /// Unbounded; [`note::sign`] takes it modulo its frame count.
    pub fn spin_frame(&self) -> usize {
        (self.spin.as_millis() / note::SPIN.as_millis()) as usize
    }

    /// Whether a spinner is on screen (a busy session with no progress). The
    /// caller then redraws every [`note::SPIN`] instead of waiting on keys.
    /// Separate from [`App::animating`], which asks for 60 fps.
    pub fn spinning(&self) -> bool {
        self.rows()
            .iter()
            .filter(|r| !r.ghost)
            .filter_map(|r| self.note(&r.session.id))
            .any(note::turns)
    }

    /// Replace the session list, keeping the selection on the same session
    /// (by id, not position, so it follows the session when rows shift).
    ///
    /// Ends any effect on the old rows (erase, glow, fade, ...).
    pub fn set_sessions(&mut self, sessions: Vec<Session>) {
        let previously = self.selected_id();
        let wanted = self
            .wanted
            .take()
            .filter(|(_, at)| *at == self.selected)
            .map(|(id, _)| id);
        self.backspace = None;
        self.strike = None;
        self.glow = None;
        self.leaving = None;
        self.landing = None;
        self.swap.clear();
        self.room = None;
        self.sessions = sessions;
        let at = |app: &Self, id: &str| app.visible().iter().position(|s| s.id == id);
        self.selected = wanted
            .and_then(|id| at(self, &id))
            .or_else(|| previously.and_then(|id| at(self, &id)))
            .unwrap_or_else(|| self.selected.min(self.visible().len().saturating_sub(1)));
    }

    /// Select this session if visible. If not, the next listing selects it
    /// if present, the cursor has not moved and no key, click or scroll came
    /// since: a client ended by `:detach` is missing from the held rows
    /// though its session lives (see `main`).
    pub fn select_session(&mut self, id: &str) {
        match self.visible().iter().position(|s| s.id == id) {
            Some(at) => {
                self.selected = at;
                self.wanted = None;
            }
            None => self.wanted = Some((id.to_string(), self.selected)),
        }
    }

    /// The session `Esc` returns to; set only while its client still runs.
    /// Once it leaves the list, `Esc` does nothing.
    pub fn set_came_from(&mut self, id: &str) {
        self.came_from = Some(id.to_string());
    }

    /// The [`App::set_came_from`] session while still listed; also what the
    /// settings opened with `,` float over if switched to floating.
    pub fn came_from(&self) -> Option<&Session> {
        self.came_from.as_deref().and_then(|id| self.session(id))
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// The half-typed session number, shown on the hint row.
    pub fn pending(&self) -> Option<u32> {
        self.pending
    }

    pub fn set_message(&mut self, msg: impl Into<String>) {
        self.message = Some(msg.into());
    }

    /// Whether a moved session is trailing stars (a move with trails on). The
    /// caller runs a faster clock while true.
    pub fn trailing(&self) -> bool {
        self.trail && matches!(self.mode, Mode::Reorder { .. })
    }

    /// Advance every animation and the spinner by `elapsed`. The clock is the
    /// caller's, so tests can step it exactly.
    pub fn tick(&mut self, elapsed: Duration) {
        self.spin = self.spin.saturating_add(elapsed);
        if self.trailing() {
            self.after.advance(elapsed, starfield::TRAIL);
            self.before.advance(elapsed, starfield::TRAIL);
        }
        if let Some(glow) = &mut self.glow {
            glow.age += elapsed;
            if glow.age >= effects::AFTERGLOW {
                self.glow = None;
            }
        }
        if let Some(leaving) = &mut self.leaving {
            leaving.age += elapsed;
            if leaving.age >= effects::SIFT {
                self.leaving = None;
            }
        }
        if let Some(strike) = &mut self.strike {
            let step = elapsed.as_secs_f32() / effects::STRIKE.as_secs_f32();
            if strike.back {
                strike.drawn -= step;
                if strike.drawn <= 0.0 {
                    self.strike = None;
                }
            } else {
                strike.drawn = (strike.drawn + step).min(1.0);
            }
        }
        if let Some((_, backspace)) = &mut self.backspace {
            backspace.advance(elapsed);
        }
        if let Some(landing) = &mut self.landing {
            landing.advance(elapsed);
            if landing.done() {
                self.landing = None;
            }
        }
        self.swap.advance(elapsed);
        if let Some(room) = &mut self.room {
            room.age += elapsed;
        }
    }

    /// Whether anything is moving, so the caller should draw at frame rate
    /// instead of waiting on keys. A completed strike-through is still. With
    /// every effect off this is always false.
    pub fn animating(&self) -> bool {
        self.trailing()
            || self.glow.is_some()
            || self.leaving.is_some()
            || self.striking()
            || self.erasing()
            || self.landing.is_some()
            || self.swap.moving()
            || self.room().is_some()
    }

    pub fn landing(&self) -> Option<&Landing> {
        self.landing.as_ref()
    }

    pub fn swap(&self) -> &Swap {
        &self.swap
    }

    /// Drop the moved session, starting the landing if trails are on. Every
    /// placing path (keys, mouse release) goes through here; `Esc` does not,
    /// as a cancelled move lands nowhere.
    fn put_down(&mut self) {
        self.mode = Mode::Normal;
        if self.trail {
            self.landing = Some(Landing::seeded());
        }
    }

    /// Session id of the row the cursor last left, and glow progress `0..1`.
    pub fn glow(&self) -> Option<(&str, f32)> {
        let length = effects::AFTERGLOW.as_secs_f32();
        self.glow
            .as_ref()
            .map(|g| (g.id.as_str(), g.age.as_secs_f32() / length))
    }

    /// Fade progress `0..1` of rows the last filter keystroke dropped.
    pub fn leaving(&self) -> Option<f32> {
        self.leaving
            .as_ref()
            .map(|l| l.age.as_secs_f32() / effects::SIFT.as_secs_f32())
    }

    /// Session id under a `[y/N]` and how much of the strike-through is
    /// drawn, `0..=1` (retracting after a no).
    pub fn strike(&self) -> Option<(&str, f32)> {
        self.strike.as_ref().map(|s| (s.id.as_str(), s.drawn))
    }

    /// Whether the strike-through is moving; fully drawn, it is still.
    fn striking(&self) -> bool {
        self.strike
            .as_ref()
            .is_some_and(|s| s.back || s.drawn < 1.0)
    }

    /// Start erasing row `id` after a confirmed kill. Returns whether there
    /// is anything to show (effect on and row visible).
    pub fn start_backspace(&mut self, id: &str) -> bool {
        if !self.kill || !self.visible().iter().any(|s| s.id == id) {
            return false;
        }
        self.backspace = Some((id.to_string(), Backspace::new()));
        true
    }

    /// Kept after it finishes: the row stays empty until a listing replaces
    /// it.
    pub fn backspace(&self) -> Option<(&str, &Backspace)> {
        self.backspace.as_ref().map(|(id, b)| (id.as_str(), b))
    }

    pub fn erasing(&self) -> bool {
        self.backspace.as_ref().is_some_and(|(_, b)| !b.done())
    }

    /// Open a gap with a placeholder named `name` where [`App::placement_of`]
    /// will put the new session, showing the numbers it will assign. Returns
    /// whether the effect is on.
    ///
    /// Ends the glow, fade, strike and swap so only the gap moves. Lasts
    /// until [`App::close_room`] or a fresh listing.
    pub fn make_room(&mut self, name: &str) -> bool {
        if !self.create {
            return false;
        }
        let (at, num) = self.slot();
        let last = self.last_number();
        let mut ghost = Session::new(GHOST_ID.to_string(), name.to_string(), 0, num);
        ghost.state.num = num;
        self.glow = None;
        self.leaving = None;
        self.strike = None;
        self.swap.clear();
        self.room = Some(Room {
            ghost,
            at,
            last,
            age: Duration::ZERO,
        });
        true
    }

    /// Number a session created now gets (also used for its default name,
    /// see [`super::prompt::default_name`]): that of the row after the
    /// highlighted one, or one past every row's at the end.
    pub fn new_number(&self) -> u32 {
        self.slot().1
    }

    /// Visible index (after the highlighted row, or 0 if empty) and number
    /// for a session created now.
    fn slot(&self) -> (usize, u32) {
        let rows = self.snapshot();
        let at = if rows.is_empty() {
            0
        } else {
            (self.selected + 1).min(rows.len())
        };
        let num = rows
            .get(at)
            .map_or_else(|| self.last_number(), |(_, num)| *num);
        (at, num)
    }

    /// One past every row's number.
    fn last_number(&self) -> u32 {
        self.sessions.iter().map(|s| s.state.num).max().unwrap_or(0) + 1
    }

    pub fn close_room(&mut self) {
        self.room = None;
    }

    /// Fade-in progress `0..1` of the gap's row; `None` once complete or with
    /// no gap.
    pub fn room(&self) -> Option<f32> {
        let room = self.room.as_ref()?;
        let progress = room.age.as_secs_f32() / effects::ROOM.as_secs_f32();
        (progress < 1.0).then_some(progress)
    }

    pub fn has_room(&self) -> bool {
        self.room.is_some()
    }

    /// Trail off the end of the name.
    pub fn trail_after(&self) -> &Starfield {
        &self.after
    }

    /// Trail off the other end of the row.
    pub fn trail_before(&self) -> &Starfield {
        &self.before
    }

    #[cfg(test)]
    pub(super) fn set_trail(&mut self, on: bool) {
        self.trail = on;
    }

    /// Override every effect except trails, regardless of config.
    #[cfg(test)]
    pub(super) fn set_effects(&mut self, on: bool) {
        self.lights = on;
        self.sift = on;
        self.kill = on;
        self.create = on;
        if !on {
            self.glow = None;
            self.leaving = None;
            self.strike = None;
            self.backspace = None;
        }
    }

    /// Sessions matching the filter, fuzzily, with the same scorer and smart
    /// case as [`super::complete`].
    ///
    /// The score only filters, never reorders: row numbers, digit keys and
    /// filtered reordering assume the full list with rows removed.
    pub fn visible(&self) -> Vec<&Session> {
        if self.filter.is_empty() {
            return self.sessions.iter().collect();
        }
        let pattern = Pattern::parse(&self.filter, CaseMatching::Smart, Normalization::Smart);
        let mut matcher = self.matcher.borrow_mut();
        let mut buf = Vec::new();
        self.sessions
            .iter()
            .filter(|s| {
                haystacks(s).any(|text| {
                    pattern
                        .score(Utf32Str::new(text, &mut buf), &mut matcher)
                        .is_some()
                })
            })
            .collect()
    }

    /// Rows to draw: the visible ones plus fading or placeholder rows. Input
    /// resolves against [`App::visible`] instead.
    pub fn rows(&self) -> Vec<Row<'_>> {
        let visible = self.visible();
        if let Some(room) = &self.room {
            // `make_room` ended any fade.
            let mut rows: Vec<Row> = visible.into_iter().map(|s| Row::of(s, false)).collect();
            let at = room.at.min(rows.len());
            let below: Vec<u32> = rows[at..].iter().map(|r| r.num).collect();
            for (row, num) in rows[at..]
                .iter_mut()
                .zip(below.into_iter().skip(1).chain([room.last]))
            {
                row.num = num;
            }
            rows.insert(
                at,
                Row {
                    session: &room.ghost,
                    leaving: false,
                    ghost: true,
                    num: room.ghost.state.num,
                },
            );
            return rows;
        }
        let Some(leaving) = &self.leaving else {
            return visible
                .into_iter()
                .map(|session| Row::of(session, false))
                .collect();
        };
        self.sessions
            .iter()
            .filter_map(|session| {
                if visible.iter().any(|v| v.id == session.id) {
                    Some(Row::of(session, false))
                } else {
                    leaving
                        .ids
                        .contains(&session.id)
                        .then(|| Row::of(session, true))
                }
            })
            .collect()
    }

    /// The selection's index into [`App::rows`].
    pub fn selected_row(&self) -> usize {
        let Some(id) = self.selected_id() else {
            return 0;
        };
        self.rows()
            .iter()
            .position(|r| !r.leaving && r.session.id == id)
            .unwrap_or(0)
    }

    pub fn selected_row_id(&self) -> Option<&str> {
        self.selected_session().map(|s| s.id.as_str())
    }

    /// Sorted `char` indices of `name` the filter matched; empty with no
    /// filter or when only the folder matched. The matcher counts graphemes,
    /// so combining marks may shift the underline.
    pub fn matched(&self, name: &str) -> Vec<usize> {
        if self.filter.is_empty() {
            return Vec::new();
        }
        let pattern = Pattern::parse(&self.filter, CaseMatching::Smart, Normalization::Smart);
        let mut matcher = self.matcher.borrow_mut();
        let mut buf = Vec::new();
        let mut indices = Vec::new();
        if pattern
            .indices(Utf32Str::new(name, &mut buf), &mut matcher, &mut indices)
            .is_none()
        {
            return Vec::new();
        }
        indices.sort_unstable();
        indices.dedup();
        indices.into_iter().map(|i| i as usize).collect()
    }

    /// The full, unfiltered listing, for callers that carry it on (see
    /// [`crate::ui::Outcome::Attach`]).
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    /// Lookup in the held listing, so acting on a [`Request`]'s id needs no
    /// extra round trip.
    pub fn session(&self, id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    fn selected_session(&self) -> Option<&Session> {
        self.visible().get(self.selected).copied()
    }

    /// `request` for the selected id, or `Request::None` if the list is
    /// empty.
    fn on_selection(&self, request: impl FnOnce(String) -> Request) -> Request {
        self.selected_id().map_or(Request::None, request)
    }

    fn selected_id(&self) -> Option<String> {
        self.selected_session().map(|s| s.id.clone())
    }

    fn session_name(&self, id: &str) -> String {
        self.session(id).map(|s| s.name.clone()).unwrap_or_default()
    }

    /// The row `delta` from the selection, wrapping; shared by cursor and
    /// move so both wrap the same way.
    fn wrapped(&self, delta: isize) -> usize {
        let len = self.visible().len();
        if len == 0 {
            return 0;
        }
        let len = len as isize;
        (((self.selected as isize + delta) % len + len) % len) as usize
    }

    fn move_by(&mut self, delta: isize) {
        self.selected = self.wrapped(delta);
    }

    /// The row `delta` from the selection, stopping at the ends. For the
    /// wheel, where wrapping would look like the list lost its place.
    fn clamped(&self, delta: isize) -> usize {
        let len = self.visible().len();
        if len == 0 {
            return 0;
        }
        (self.selected as isize + delta).clamp(0, len as isize - 1) as usize
    }

    /// Arrangement to store for the just-created `new_id`, placed after the
    /// highlighted row. Treated as appended then moved up: visible numbers
    /// plus a new last one are dealt down the new order, as in
    /// [`App::shift_grabbed_to`]. Empty if it goes at the end anyway.
    pub fn placement_of(&self, new_id: &str) -> Vec<(String, u32)> {
        let rows = self.snapshot();
        let at = self.selected + 1;
        if at >= rows.len() {
            return Vec::new();
        }
        let numbers = rows.iter().map(|(_, num)| *num).chain([self.last_number()]);
        let mut ids: Vec<String> = rows.iter().map(|(id, _)| id.clone()).collect();
        ids.insert(at, new_id.to_string());
        ids.into_iter().zip(numbers).collect()
    }

    /// Pick up `id` (space or drag), saving the arrangement for `Esc` and
    /// igniting fresh trails (see [`Starfield::ignite`]).
    fn pick_up(&mut self, id: String) {
        let was = self.snapshot();
        // Nothing else may be on screen during a grab, including the last
        // grab's swap.
        self.leaving = None;
        self.swap.clear();
        self.mode = Mode::Reorder { id, was };
        if self.trail {
            self.after.ignite(starfield::TRAIL);
            self.before.ignite(starfield::TRAIL);
        }
    }

    /// Every visible row's `(id, resolved number)`, in display order.
    fn snapshot(&self) -> Vec<(String, u32)> {
        self.visible()
            .iter()
            .map(|s| (s.id.clone(), s.state.num))
            .collect()
    }

    /// Move the grabbed row to `to`. Numbers stay put and sessions move
    /// between them: the same numbers are dealt down the new order, so they
    /// stay distinct with gaps kept. One reassignment rather than a loop of
    /// adjacent swaps, which could spin forever.
    fn shift_grabbed_to(&mut self, to: usize) {
        let rows = self.snapshot();
        if rows.len() < 2 {
            return;
        }
        let to = to.min(rows.len() - 1);
        let from = self.selected.min(rows.len() - 1);
        if from == to {
            return;
        }

        let mut ids: Vec<&str> = rows.iter().map(|(id, _)| id.as_str()).collect();
        let moved = ids.remove(from);
        ids.insert(to, moved);

        if self.trail {
            // Rows passed, nearest the new position first, and the last seam
            // crossed: the row's top edge going down, bottom edge going up.
            let passed: Vec<String> = if to > from {
                rows[from + 1..=to]
                    .iter()
                    .rev()
                    .map(|(id, _)| id.clone())
                    .collect()
            } else {
                rows[to..from].iter().map(|(id, _)| id.clone()).collect()
            };
            let seam = if to > from { to } else { to + 1 };
            self.swap.start(moved.to_string(), passed, seam);
        }

        let dealt: Vec<(String, u32)> = ids
            .into_iter()
            .map(str::to_string)
            .zip(rows.iter().map(|(_, num)| *num))
            .collect();
        for (id, num) in dealt {
            if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) {
                s.state.num = num;
            }
        }

        // Hidden rows keep their numbers, so re-sorting keeps the whole list
        // ordered.
        self.sessions.sort_by_key(|s| s.state.num);
        self.selected = to;
    }

    /// Restore the numbers from the grab's start and reselect `id` (by id, in
    /// case the list moved underneath). Clears the swap effect.
    fn restore(&mut self, was: &[(String, u32)], id: &str) {
        self.swap.clear();
        for (row, num) in was {
            if let Some(s) = self.sessions.iter_mut().find(|s| &s.id == row) {
                s.state.num = *num;
            }
        }
        self.sessions.sort_by_key(|s| s.state.num);
        self.select_session(id);
    }

    fn clamp_selection(&mut self) {
        let len = self.visible().len();
        if self.selected >= len {
            self.selected = len.saturating_sub(1);
        }
    }

    /// Killing is unconditional, so this `[y/N]` is the only safeguard.
    fn show_kill_confirm(&mut self, id: &str) {
        let name = self.session_name(id);
        self.mode = Mode::Confirm {
            id: id.to_string(),
            prompt: format!("kill {name:?}? [y/N]"),
        };
        if self.kill {
            self.strike = Some(Strike {
                id: id.to_string(),
                drawn: 0.0,
                back: false,
            });
        }
    }

    /// `[y/N]` answered no (key or click): retract the strike-through.
    fn decline(&mut self) {
        self.mode = Mode::Normal;
        if let Some(strike) = &mut self.strike {
            strike.back = true;
        }
    }

    pub fn on_key(&mut self, key: Key) -> Request {
        // Any key clears a stale message, a pending `wanted` selection, and
        // the previous keystroke's fading rows (so fast typing never waits).
        self.message = None;
        self.wanted = None;
        self.leaving = None;
        let was = self.cursor();
        let visible = self.visible_ids();

        let request = match &self.mode {
            Mode::Normal => self.on_key_normal(key),
            Mode::Filter => self.on_key_filter(key),
            Mode::Confirm { .. } => self.on_key_confirm(key),
            Mode::Reorder { .. } => self.on_key_reorder(key),
            Mode::Leave { .. } => self.on_key_leave(key),
        };
        self.glow_from(was);
        self.note_dropped(visible);
        request
    }

    /// The selected id in Normal or Filter mode, where leaving a row glows.
    fn cursor(&self) -> Option<String> {
        matches!(self.mode, Mode::Normal | Mode::Filter)
            .then(|| self.selected_id())
            .flatten()
    }

    /// If the cursor left `was`, make it the (only) glowing row.
    fn glow_from(&mut self, was: Option<String>) {
        if !self.lights {
            return;
        }
        let (Some(was), Some(now)) = (was, self.cursor()) else {
            return;
        };
        // A row the filter just dropped fades instead.
        if was == now || !self.visible().iter().any(|s| s.id == was) {
            return;
        }
        self.glow = Some(Glow {
            id: was,
            age: Duration::ZERO,
        });
    }

    fn visible_ids(&self) -> Vec<String> {
        self.visible().iter().map(|s| s.id.clone()).collect()
    }

    /// Start fading the rows a filter keystroke dropped.
    fn note_dropped(&mut self, before: Vec<String>) {
        if !self.sift || !matches!(self.mode, Mode::Filter | Mode::Normal) {
            return;
        }
        let now = self.visible_ids();
        let ids: Vec<String> = before.into_iter().filter(|id| !now.contains(id)).collect();
        if !ids.is_empty() {
            self.leaving = Some(Leaving {
                ids,
                age: Duration::ZERO,
            });
        }
    }

    /// Numbers resolve against the *visible* list, so a row hidden by a
    /// filter (still applied in Normal mode) is never attached.
    ///
    /// A digit acts at once unless another digit could still change the
    /// match (sessions 1 and 12): then it waits for the caller's
    /// [`App::resolve_pending`].
    fn on_digit(&mut self, d: u32) -> Request {
        let Some(n) = self
            .pending
            .take()
            .map(|p| p.saturating_mul(10).saturating_add(d))
        else {
            // Session numbers never start with 0.
            if d == 0 {
                return Request::None;
            }
            return self.select_number(d);
        };
        self.select_number(n)
    }

    fn select_number(&mut self, n: u32) -> Request {
        let exact = self
            .visible()
            .into_iter()
            .find(|s| s.state.num == n)
            .map(|s| s.id.clone());
        let extendable = self
            .visible()
            .iter()
            .any(|s| s.state.num > n && s.state.num / 10 == n);

        match (exact, extendable) {
            // Ambiguous: 1 is a session but so is 12.
            (Some(_), true) => {
                self.pending = Some(n);
                Request::None
            }
            (Some(id), false) => Request::Attach(id),
            (None, true) => {
                self.pending = Some(n);
                Request::None
            }
            (None, false) => {
                self.set_message(format!("no session {n}"));
                Request::None
            }
        }
    }

    /// Called once `keys.timeout_ms` passes with a number half-typed: attach
    /// to the session it already names.
    pub fn resolve_pending(&mut self) -> Request {
        match self.pending.take() {
            Some(n) => self
                .visible()
                .into_iter()
                .find(|s| s.state.num == n)
                .map(|s| Request::Attach(s.id.clone()))
                .unwrap_or(Request::None),
            None => Request::None,
        }
    }

    /// Return to the session the picker was opened from. With none, the
    /// picker stays rather than leave an empty terminal; `q` quits.
    fn dismiss(&self) -> Request {
        self.came_from()
            .map_or(Request::None, |s| Request::Attach(s.id.clone()))
    }

    /// Rows are indices into [`App::visible`], resolved by the caller.
    ///
    /// Hover selects; a press on the selected row arms its release to attach,
    /// so one click opens the hovered row, and without motion reporting it is
    /// "click to select, click again to open" with no timer. A drag enters
    /// [`Mode::Reorder`]. Only press and wheel clear the message and pending
    /// number, as a key does.
    pub fn on_mouse(&mut self, mouse: Mouse) -> Request {
        if matches!(mouse, Mouse::Press(_) | Mouse::ScrollUp | Mouse::ScrollDown) {
            self.message = None;
            self.pending = None;
            self.wanted = None;
        }
        let was = self.cursor();
        let request = self.on_mouse_mode(mouse);
        self.glow_from(was);
        request
    }

    fn on_mouse_mode(&mut self, mouse: Mouse) -> Request {
        match &self.mode {
            Mode::Normal | Mode::Filter => self.on_mouse_normal(mouse),
            Mode::Confirm { .. } => {
                // Only `y` kills. A click declines, like any other key; the
                // wheel does nothing.
                if matches!(mouse, Mouse::Press(_)) {
                    self.decline();
                    self.pressed = None;
                }
                Request::None
            }
            Mode::Reorder { .. } => self.on_mouse_reorder(mouse),
            // A click means "stay", as it declines a `[y/N]`.
            Mode::Leave { .. } => {
                if matches!(mouse, Mouse::Press(_)) {
                    self.mode = Mode::Normal;
                    self.pressed = None;
                }
                Request::None
            }
        }
    }

    fn on_mouse_normal(&mut self, mouse: Mouse) -> Request {
        let len = self.visible().len();
        match mouse {
            Mouse::Press(Some(row)) if row < len => {
                if row == self.selected {
                    self.pressed = Some(Pressed::Selected);
                } else {
                    self.selected = row;
                    self.pressed = Some(Pressed::Row);
                }
                Request::None
            }
            Mouse::Press(_) => {
                self.pressed = None;
                Request::None
            }
            Mouse::Hover(Some(row)) if row < len => {
                self.selected = row;
                Request::None
            }
            // Off the list, the selection stays.
            Mouse::Hover(_) => Request::None,
            // Dragging off the pressed row picks it up. Requires a press on a
            // row, so a drag from blank space picks up nothing.
            Mouse::Drag(Some(row))
                if row < len && self.pressed.is_some() && row != self.selected =>
            {
                if let Some(id) = self.selected_id() {
                    self.pick_up(id);
                    self.shift_grabbed_to(row);
                }
                self.pressed = Some(Pressed::Row);
                Request::None
            }
            Mouse::Drag(_) => Request::None,
            Mouse::Release => {
                let attach = self.pressed.take() == Some(Pressed::Selected);
                if !attach {
                    return Request::None;
                }
                // As `Enter` from the filter: the query stays applied.
                self.mode = Mode::Normal;
                self.on_selection(Request::Attach)
            }
            Mouse::ScrollDown => {
                self.selected = self.clamped(1);
                Request::None
            }
            Mouse::ScrollUp => {
                self.selected = self.clamped(-1);
                Request::None
            }
        }
    }

    /// During a move (from drag or space): press or drag carries the row to
    /// the pointer, release places it as `Enter` would. Hover does nothing.
    fn on_mouse_reorder(&mut self, mouse: Mouse) -> Request {
        let Mode::Reorder { was, .. } = &self.mode else {
            return Request::None;
        };
        let was = was.clone();
        let len = self.visible().len();
        match mouse {
            Mouse::Press(Some(row)) | Mouse::Drag(Some(row)) if row < len => {
                if matches!(mouse, Mouse::Press(_)) {
                    self.pressed = Some(Pressed::Row);
                }
                self.shift_grabbed_to(row);
                Request::None
            }
            Mouse::Press(_) | Mouse::Drag(_) | Mouse::Hover(_) => Request::None,
            Mouse::Release => {
                self.pressed = None;
                let now = self.snapshot();
                self.put_down();
                if now == was {
                    return Request::None;
                }
                Request::Reorder(now)
            }
            Mouse::ScrollDown => {
                self.shift_grabbed_to(self.clamped(1));
                Request::None
            }
            Mouse::ScrollUp => {
                self.shift_grabbed_to(self.clamped(-1));
                Request::None
            }
        }
    }

    fn on_key_normal(&mut self, key: Key) -> Request {
        // A non-digit ends a half-typed number; if there was one, `Esc` only
        // cancels it.
        let was_pending = !matches!(key, Key::Char('0'..='9')) && self.pending.take().is_some();
        match key {
            Key::Char(c @ '0'..='9') => self.on_digit(u32::from(c) - u32::from('0')),
            Key::Char('j') | Key::Down | Key::CtrlN => {
                self.move_by(1);
                Request::None
            }
            Key::Char('k') | Key::Up | Key::CtrlP => {
                self.move_by(-1);
                Request::None
            }
            Key::Char('g') | Key::Home => {
                self.selected = 0;
                Request::None
            }
            Key::Char('G') | Key::End => {
                self.selected = self.visible().len().saturating_sub(1);
                Request::None
            }
            Key::Enter => self.on_selection(Request::Attach),
            Key::Char('c') => Request::NewSession,
            Key::Char('r') => self.on_selection(Request::RenameSession),
            Key::Char('x') => {
                if let Some(id) = self.selected_id() {
                    self.show_kill_confirm(&id);
                }
                Request::None
            }
            // Guarded like `x`, so an empty list never enters Reorder.
            Key::Char(' ') => {
                if let Some(id) = self.selected_id() {
                    self.pick_up(id);
                }
                Request::None
            }
            Key::Char('/') => {
                self.mode = Mode::Filter;
                Request::None
            }
            Key::Char('?') => Request::Help,
            Key::Char(',') => Request::Settings,
            // One layer at a time: a half-typed number, then the filter, then
            // the picker.
            Key::Esc => {
                if was_pending {
                    return Request::None;
                }
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.clamp_selection();
                    return Request::None;
                }
                self.dismiss()
            }
            Key::Char('q') | Key::CtrlC => self.quit(),
            _ => Request::None,
        }
    }

    fn on_key_filter(&mut self, key: Key) -> Request {
        match key {
            Key::Char(c) => {
                self.filter.push(c);
                // The list narrows under the cursor.
                self.clamp_selection();
                Request::None
            }
            Key::Backspace => {
                self.filter.pop();
                self.clamp_selection();
                Request::None
            }
            Key::Down | Key::CtrlN => {
                self.move_by(1);
                Request::None
            }
            Key::Up | Key::CtrlP => {
                self.move_by(-1);
                Request::None
            }
            Key::Enter => {
                self.mode = Mode::Normal;
                self.on_selection(Request::Attach)
            }
            Key::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
                self.clamp_selection();
                Request::None
            }
            Key::CtrlC => self.quit(),
            _ => Request::None,
        }
    }

    /// Unlisted keys do nothing **and keep the grab** (unlike
    /// [`Mode::Confirm`]): a stray key must not decide the edit, and `/`
    /// cannot re-filter under the snapshot. `Enter` and Space both place.
    fn on_key_reorder(&mut self, key: Key) -> Request {
        let Mode::Reorder { id, was } = &self.mode else {
            return Request::None;
        };
        let id = id.clone();
        let was = was.clone();
        let last = self.visible().len().saturating_sub(1);

        match key {
            Key::Char('j') | Key::Down | Key::CtrlN => {
                self.shift_grabbed_to(self.wrapped(1));
                Request::None
            }
            Key::Char('k') | Key::Up | Key::CtrlP => {
                self.shift_grabbed_to(self.wrapped(-1));
                Request::None
            }
            Key::Char('g') | Key::Home => {
                self.shift_grabbed_to(0);
                Request::None
            }
            Key::Char('G') | Key::End => {
                self.shift_grabbed_to(last);
                Request::None
            }
            Key::Char(' ') | Key::Enter => {
                let now = self.snapshot();
                self.put_down();
                if now == was {
                    // No net move: nothing to write.
                    return Request::None;
                }
                Request::Reorder(now)
            }
            Key::Esc => {
                self.restore(&was, &id);
                self.mode = Mode::Normal;
                Request::None
            }
            Key::CtrlC => self.quit(),
            _ => Request::None,
        }
    }

    fn on_key_confirm(&mut self, key: Key) -> Request {
        let Mode::Confirm { id, .. } = &self.mode else {
            return Request::None;
        };
        let id = id.clone();
        match key {
            // Only `y` kills; everything else, Enter included, declines. The
            // strike goes because the row is erased next.
            Key::Char('y') | Key::Char('Y') => {
                self.mode = Mode::Normal;
                self.strike = None;
                Request::Kill(id)
            }
            Key::CtrlC => self.quit(),
            _ => {
                self.decline();
                Request::None
            }
        }
    }
}

/// A key press, decoupled from crossterm for testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Up,
    Down,
    /// Used only by the prompt (cursor movement, completion).
    Left,
    Right,
    Tab,
    /// Shift-Tab, used by the settings screen.
    BackTab,
    Home,
    End,
    CtrlC,
    CtrlN,
    CtrlP,
    Other,
}

/// A mouse gesture with its visible row already resolved by the caller (see
/// [`App::on_mouse`]); decoupled from crossterm and screen geometry. Left
/// button and wheel only; the caller drops other buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mouse {
    /// Motion with no button held; the row under the pointer, if any.
    Hover(Option<usize>),
    /// Left button down.
    Press(Option<usize>),
    /// Motion with the left button held.
    Drag(Option<usize>),
    /// Left button up.
    Release,
    ScrollUp,
    ScrollDown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::swap;
    use crate::ui::test_support::{filtering, picker as app};

    fn names(app: &App) -> Vec<String> {
        app.visible().iter().map(|s| s.name.clone()).collect()
    }

    #[test]
    fn every_movement_key_moves_and_wraps_in_both_directions() {
        for (down, up) in [
            (Key::Char('j'), Key::Char('k')),
            (Key::Down, Key::Up),
            (Key::CtrlN, Key::CtrlP),
        ] {
            let mut a = app(&["one", "two", "three"]);
            assert_eq!(a.selected_index(), 0);
            a.on_key(down);
            a.on_key(down);
            assert_eq!(a.selected_index(), 2, "{down:?} should move down");
            a.on_key(down);
            assert_eq!(a.selected_index(), 0, "{down:?} should wrap forwards");
            a.on_key(up);
            assert_eq!(a.selected_index(), 2, "{up:?} should wrap backwards");
            a.on_key(up);
            assert_eq!(a.selected_index(), 1, "{up:?} should move up");
        }
    }

    #[test]
    fn ctrl_n_and_ctrl_p_move_while_filtering() {
        // In Filter mode printable keys are query text.
        let mut a = app(&["one", "two"]);
        a.on_key(Key::Char('/'));
        a.on_key(Key::CtrlN);
        assert_eq!(a.selected_index(), 1);
        assert_eq!(a.filter(), "", "the chord should not land in the query");
        a.on_key(Key::CtrlP);
        assert_eq!(a.selected_index(), 0);
    }

    #[test]
    fn g_and_shift_g_jump_to_the_ends() {
        let mut a = app(&["a", "b", "c", "d"]);
        a.on_key(Key::Char('G'));
        assert_eq!(a.selected_index(), 3);
        a.on_key(Key::Char('g'));
        assert_eq!(a.selected_index(), 0);
    }

    #[test]
    fn movement_on_an_empty_list_is_harmless() {
        let mut a = app(&[]);
        for k in [
            Key::Char('j'),
            Key::Char('k'),
            Key::Char('g'),
            Key::Char('G'),
        ] {
            a.on_key(k);
            assert_eq!(a.selected_index(), 0);
        }
        assert_eq!(a.on_key(Key::Enter), Request::None, "nothing to attach to");
    }

    #[test]
    fn enter_attaches_to_the_selection() {
        let mut a = app(&["one", "two"]);
        a.on_key(Key::Char('j'));
        assert_eq!(a.on_key(Key::Enter), Request::Attach("id000001".into()));
    }

    #[test]
    fn q_and_ctrl_c_quit() {
        assert_eq!(app(&["x"]).on_key(Key::Char('q')), Request::Quit);
        assert_eq!(app(&["x"]).on_key(Key::CtrlC), Request::Quit);
        assert_eq!(app(&["x"]).on_key(Key::Char('?')), Request::Help);
        assert_eq!(app(&["x"]).on_key(Key::Char(',')), Request::Settings);
    }

    #[test]
    fn quitting_with_settings_unsaved_asks_first() {
        let mut a = app(&["x"]);
        a.set_unsaved(2);
        assert_eq!(a.on_key(Key::Char('q')), Request::None);
        assert_eq!(
            a.mode(),
            &Mode::Leave {
                prompt: "2 unsaved settings — w save and quit  q quit anyway  esc stay".into(),
                overwrite: false
            }
        );
        assert_eq!(
            a.on_key(Key::Char('x')),
            Request::None,
            "only its keys answer"
        );
        assert_eq!(
            a.on_key(Key::Char('w')),
            Request::SaveAndQuit { overwrite: false }
        );
        assert_eq!(a.on_key(Key::Char('q')), Request::Quit);

        let mut a = app(&["x"]);
        a.set_unsaved(1);
        assert_eq!(a.on_key(Key::CtrlC), Request::None);
        assert_eq!(a.on_key(Key::Esc), Request::None);
        assert_eq!(a.mode(), &Mode::Normal, "esc stays");
        assert_eq!(a.on_key(Key::Char('q')), Request::None, "and asks again");
        assert_eq!(a.on_key(Key::CtrlC), Request::Quit, "Ctrl-c quits anyway");
    }

    /// A clash keeps the question up for a second `w`; other failures return
    /// to the picker with the reason.
    #[test]
    fn a_save_on_the_way_out_that_did_not_happen_says_why() {
        let mut a = app(&["x"]);
        a.set_unsaved(1);
        a.on_key(Key::Char('q'));
        a.not_saved("client.lazy changed in config.toml too".into(), true);
        assert_eq!(
            a.mode(),
            &Mode::Leave {
                prompt: "client.lazy changed in config.toml too — w overwrites and quits  \
                         q quit anyway  esc stay"
                    .into(),
                overwrite: true
            }
        );
        assert_eq!(
            a.on_key(Key::Char('w')),
            Request::SaveAndQuit { overwrite: true }
        );
        a.not_saved("not saved: it no longer reads".into(), false);
        assert_eq!(a.mode(), &Mode::Normal);
        assert_eq!(a.message(), Some("not saved: it no longer reads"));
    }

    #[test]
    fn quitting_mid_move_puts_the_move_back_and_a_click_stays() {
        let mut a = app(&["one", "two"]);
        a.set_unsaved(1);
        a.on_key(Key::Char(' '));
        a.on_key(Key::Char('j'));
        assert_eq!(a.on_key(Key::CtrlC), Request::None);
        assert!(matches!(a.mode(), Mode::Leave { .. }));
        assert_eq!(a.visible()[0].name, "one", "the move was put back");
        a.on_mouse(Mouse::ScrollDown);
        assert!(
            matches!(a.mode(), Mode::Leave { .. }),
            "the wheel does not stay"
        );
        a.on_mouse(Mouse::Press(None));
        assert_eq!(a.mode(), &Mode::Normal);
    }

    #[test]
    fn filtering_narrows_live_and_esc_clears_it() {
        let mut a = app(&["api-server", "dotfiles", "notes", "scratch"]);
        a.on_key(Key::Char('/'));
        assert_eq!(*a.mode(), Mode::Filter);

        a.on_key(Key::Char('o'));
        assert_eq!(
            names(&a),
            ["dotfiles", "notes"],
            "filter should apply per keystroke"
        );
        a.on_key(Key::Char('t'));
        assert_eq!(names(&a), ["dotfiles", "notes"]);
        a.on_key(Key::Char('f'));
        assert_eq!(names(&a), ["dotfiles"]);

        a.on_key(Key::Backspace);
        assert_eq!(
            names(&a),
            ["dotfiles", "notes"],
            "backspace should widen it again"
        );

        a.on_key(Key::Esc);
        assert_eq!(*a.mode(), Mode::Normal);
        assert_eq!(a.filter(), "");
        assert_eq!(names(&a).len(), 4, "esc should clear the filter");
    }

    #[test]
    fn filtering_is_case_insensitive() {
        let mut a = app(&["API-Server", "dotfiles"]);
        a.on_key(Key::Char('/'));
        for c in "api".chars() {
            a.on_key(Key::Char(c));
        }
        assert_eq!(names(&a), ["API-Server"]);
    }

    /// Smart case, as in the create prompt's completion.
    #[test]
    fn a_capital_in_the_query_makes_it_case_sensitive() {
        let a = filtering(&["API-Server", "api-client"], "API");
        assert_eq!(names(&a), ["API-Server"]);
        let a = filtering(&["API-Server", "api-client"], "api");
        assert_eq!(names(&a), ["API-Server", "api-client"]);
    }

    #[test]
    fn the_filter_is_fuzzy() {
        let a = filtering(&["api-server", "dotfiles", "notes", "scratch"], "asv");
        assert_eq!(names(&a), ["api-server"], "letters in order, gaps allowed");
        let a = filtering(&["api-server", "dotfiles", "notes", "scratch"], "sva");
        assert!(names(&a).is_empty(), "but not out of order");
    }

    #[test]
    fn a_query_of_two_words_wants_both() {
        let a = filtering(&["api server", "api client", "web server"], "api ser");
        assert_eq!(names(&a), ["api server"]);
    }

    #[test]
    fn a_filtered_list_keeps_its_order_rather_than_ranking_by_score() {
        // "ts" matches `ts` far better, yet stays second.
        let a = filtering(&["notes-tests-server", "ts", "api"], "ts");
        assert_eq!(names(&a), ["notes-tests-server", "ts"]);
        let numbers: Vec<u32> = a.visible().iter().map(|s| s.state.num).collect();
        assert_eq!(numbers, [1, 2], "the numbers travel with the rows");
    }

    /// Only the last component counts (see `haystacks`).
    #[test]
    fn the_filter_also_matches_the_working_directorys_last_component() {
        let mut a = app(&["scratch", "notes", "session 3"]);
        let mut sessions = a.sessions().to_vec();
        sessions[0].directory = "/home/you/work/billing".into();
        sessions[1].directory = "/home/you/notes".into();
        sessions[2].directory = "/home/you/work/api/".into();
        a.set_sessions(sessions);

        a.on_key(Key::Char('/'));
        for c in "billing".chars() {
            a.on_key(Key::Char(c));
        }
        assert_eq!(names(&a), ["scratch"], "found by its folder");

        a.on_key(Key::Esc);
        a.on_key(Key::Char('/'));
        for c in "api".chars() {
            a.on_key(Key::Char(c));
        }
        assert_eq!(
            names(&a),
            ["session 3"],
            "a trailing slash is not a component"
        );

        a.on_key(Key::Esc);
        a.on_key(Key::Char('/'));
        for c in "you".chars() {
            a.on_key(Key::Char(c));
        }
        assert!(
            names(&a).is_empty(),
            "the shared part of the path admits nothing: {:?}",
            names(&a)
        );
    }

    #[test]
    fn selection_stays_in_range_while_filtering() {
        let mut a = app(&["aaa", "bbb", "ccc", "abc"]);
        a.on_key(Key::Char('G'));
        assert_eq!(a.selected_index(), 3);

        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('b'));
        assert!(
            a.selected_index() < a.visible().len(),
            "selection {} is past the end of {} visible",
            a.selected_index(),
            a.visible().len()
        );
        assert!(a.selected_session().is_some());
    }

    #[test]
    fn filtering_to_nothing_is_safe() {
        let mut a = app(&["one", "two"]);
        a.on_key(Key::Char('/'));
        for c in "zzzz".chars() {
            a.on_key(Key::Char(c));
        }
        assert!(a.visible().is_empty());
        assert!(a.selected_session().is_none());
        assert_eq!(
            a.on_key(Key::Enter),
            Request::None,
            "must not attach to nothing"
        );
    }

    #[test]
    fn esc_in_normal_mode_clears_an_applied_filter() {
        let mut a = app(&["one", "two"]);
        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('o'));
        a.on_key(Key::Enter); // accepts and attaches, filter stays applied
        assert_eq!(a.filter(), "o");
        a.on_key(Key::Esc);
        assert_eq!(a.filter(), "", "esc in normal mode clears the filter");
    }

    #[test]
    fn esc_dismisses_the_picker_back_to_the_session_it_came_from() {
        let mut a = app(&["one", "two", "three"]);
        a.set_came_from("id000001");
        a.on_key(Key::Char('G')); // the cursor need not be on it
        assert_eq!(
            a.came_from().map(|s| s.id.as_str()),
            Some("id000001"),
            "and the settings float over it, turned to"
        );
        assert_eq!(a.on_key(Key::Esc), Request::Attach("id000001".into()));
    }

    /// At startup nothing is behind the picker; dismissing it would leave an
    /// empty terminal.
    #[test]
    fn esc_does_nothing_when_the_picker_was_not_opened_from_a_session() {
        let mut a = app(&["one", "two"]);
        assert!(
            a.came_from().is_none(),
            "nothing to float the settings over"
        );
        assert_eq!(a.on_key(Key::Esc), Request::None);
    }

    #[test]
    fn esc_does_not_go_back_to_a_session_that_has_since_been_killed() {
        let mut a = app(&["one", "two"]);
        a.set_came_from("id000000");
        a.on_key(Key::Char('x'));
        assert_eq!(a.on_key(Key::Char('y')), Request::Kill("id000000".into()));
        a.set_sessions(vec![]);

        assert!(a.came_from().is_none());
        assert_eq!(a.on_key(Key::Esc), Request::None);
    }

    #[test]
    fn esc_clears_what_is_half_typed_before_it_dismisses_anything() {
        let mut a = app(&["one", "two"]);
        a.set_came_from("id000000");

        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('o'));
        a.on_key(Key::Enter); // filter applied, back in normal mode
        assert_eq!(a.on_key(Key::Esc), Request::None, "the filter goes first");
        assert_eq!(a.filter(), "");

        // Sessions 1 and 12 both present, so the digit is half-typed.
        let mut a = app(&[
            "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
            "eleven", "twelve",
        ]);
        a.set_came_from("id000000");
        a.on_key(Key::Char('1'));
        assert_eq!(a.pending(), Some(1));
        assert_eq!(a.on_key(Key::Esc), Request::None, "the number goes first");
        assert_eq!(a.pending(), None);

        assert_eq!(
            a.on_key(Key::Esc),
            Request::Attach("id000000".into()),
            "with nothing left to clear, esc dismisses the picker"
        );
    }

    /// Naming happens on the prompt's own screen; the picker only asks.
    #[test]
    fn c_asks_for_a_new_session() {
        let mut a = app(&[]);
        assert_eq!(a.on_key(Key::Char('c')), Request::NewSession);
        assert_eq!(*a.mode(), Mode::Normal, "the picker stays where it is");
    }

    #[test]
    fn r_asks_to_rename_the_selected_session() {
        let mut a = app(&["dotfiles", "notes"]);
        a.on_key(Key::Char('j'));
        assert_eq!(
            a.on_key(Key::Char('r')),
            Request::RenameSession("id000001".into())
        );
        assert_eq!(*a.mode(), Mode::Normal);
    }

    #[test]
    fn r_does_nothing_on_an_empty_list() {
        let mut a = app(&[]);
        assert_eq!(a.on_key(Key::Char('r')), Request::None);
    }

    /// Killing is unconditional, so no round trip to a possibly busy session.
    #[test]
    fn x_opens_the_confirm_without_asking_the_session_anything() {
        let mut a = app(&["dotfiles"]);
        assert_eq!(
            a.on_key(Key::Char('x')),
            Request::None,
            "no I/O should be requested"
        );
        match a.mode() {
            Mode::Confirm { prompt, id } => {
                assert_eq!(id, "id000000");
                assert_eq!(prompt, r#"kill "dotfiles"? [y/N]"#);
            }
            other => panic!("expected Confirm, got {other:?}"),
        }
    }

    #[test]
    fn x_on_an_empty_list_does_nothing() {
        let mut a = app(&[]);
        assert_eq!(a.on_key(Key::Char('x')), Request::None);
        assert_eq!(*a.mode(), Mode::Normal);
    }

    #[test]
    fn only_y_confirms_a_kill() {
        for key in [
            Key::Enter,
            Key::Esc,
            Key::Char('n'),
            Key::Char('N'),
            Key::Char('x'),
            Key::Char(' '),
        ] {
            let mut a = app(&["dotfiles"]);
            a.on_key(Key::Char('x'));
            assert_eq!(a.on_key(key), Request::None, "{key:?} must not kill");
            assert_eq!(*a.mode(), Mode::Normal);
        }
        for key in [Key::Char('y'), Key::Char('Y')] {
            let mut a = app(&["dotfiles"]);
            a.on_key(Key::Char('x'));
            assert_eq!(
                a.on_key(key),
                Request::Kill("id000000".into()),
                "{key:?} should kill"
            );
        }
    }

    /// "x", "c" and "r" in a query must not kill or open the prompt.
    #[test]
    fn filter_keys_are_text_not_commands() {
        let mut a = app(&["one"]);
        a.on_key(Key::Char('/'));
        for c in "xqrc".chars() {
            assert_eq!(a.on_key(Key::Char(c)), Request::None);
        }
        assert_eq!(a.filter(), "xqrc");
        assert_eq!(*a.mode(), Mode::Filter);
    }

    fn session(id: &str, name: &str, num: u32) -> Session {
        let mut s = Session::new(id.to_string(), name.to_string(), 100, num);
        s.state.num = num;
        s
    }

    #[test]
    fn a_digit_attaches_to_the_session_with_that_number() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        assert_eq!(
            a.on_key(Key::Char('2')),
            Request::Attach("id000001".into()),
            "one keystroke, no Enter"
        );
        assert_eq!(a.pending(), None);
    }

    #[test]
    fn every_digit_resolves_at_once_when_no_number_can_be_extended() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        for (key, id) in [('1', "id000000"), ('2', "id000001"), ('3', "id000002")] {
            assert_eq!(a.on_key(Key::Char(key)), Request::Attach(id.into()));
            assert_eq!(a.pending(), None, "{key} should not have waited");
        }
    }

    #[test]
    fn a_digit_naming_no_session_reports_instead_of_attaching() {
        let mut a = app(&["aaa", "bbb"]);
        assert_eq!(a.on_key(Key::Char('7')), Request::None);
        assert_eq!(a.message(), Some("no session 7"));
        assert_eq!(a.pending(), None);
    }

    #[test]
    fn zero_does_nothing_on_its_own() {
        let mut a = app(&["aaa", "bbb"]);
        assert_eq!(a.on_key(Key::Char('0')), Request::None);
        assert_eq!(a.pending(), None);
        assert_eq!(a.message(), None, "it is a no-op, not an error");
    }

    #[test]
    fn two_digits_reach_a_session_past_the_ninth() {
        let names: Vec<String> = (0..12).map(|i| format!("s{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut a = app(&refs);

        // 1 waits while 10, 11 and 12 exist.
        assert_eq!(a.on_key(Key::Char('1')), Request::None);
        assert_eq!(a.pending(), Some(1));

        assert_eq!(
            a.on_key(Key::Char('2')),
            Request::Attach("id000011".into()),
            "12 is the twelfth session"
        );
        assert_eq!(a.pending(), None);
    }

    #[test]
    fn a_half_typed_number_settles_on_the_session_it_already_names() {
        let names: Vec<String> = (0..12).map(|i| format!("s{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut a = app(&refs);

        a.on_key(Key::Char('1'));
        assert_eq!(a.pending(), Some(1));
        assert_eq!(a.resolve_pending(), Request::Attach("id000000".into()));
        assert_eq!(a.pending(), None);
        assert_eq!(a.resolve_pending(), Request::None, "idempotent");
    }

    #[test]
    fn a_repeated_digit_reaches_the_session_it_spells() {
        let names: Vec<String> = (0..12).map(|i| format!("s{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut a = app(&refs);
        assert_eq!(a.on_key(Key::Char('1')), Request::None);
        assert_eq!(a.on_key(Key::Char('1')), Request::Attach("id000010".into()));
    }

    #[test]
    fn any_other_key_abandons_a_half_typed_number() {
        let names: Vec<String> = (0..12).map(|i| format!("s{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut a = app(&refs);

        a.on_key(Key::Char('1'));
        assert_eq!(a.pending(), Some(1));
        a.on_key(Key::Char('j'));
        assert_eq!(a.pending(), None, "a movement key ends the number");

        a.on_key(Key::Char('1'));
        a.on_key(Key::Esc);
        assert_eq!(a.pending(), None, "esc ends it too");
    }

    #[test]
    fn a_digit_only_reaches_a_session_the_filter_still_shows() {
        let mut a = app(&["alpha", "beta", "gamma"]);
        a.on_key(Key::Char('/'));
        for c in "beta".chars() {
            a.on_key(Key::Char(c));
        }
        a.on_key(Key::Enter); // back to normal mode, filter still applied

        // Only "beta" (number 2) is visible.
        assert_eq!(a.on_key(Key::Char('2')), Request::Attach("id000001".into()));

        let mut a = app(&["alpha", "beta", "gamma"]);
        a.on_key(Key::Char('/'));
        for c in "beta".chars() {
            a.on_key(Key::Char(c));
        }
        a.on_key(Key::Enter);
        assert_eq!(
            a.on_key(Key::Char('1')),
            Request::None,
            "alpha is filtered out, so 1 names nothing on screen"
        );
        assert_eq!(a.message(), Some("no session 1"));
    }

    #[test]
    fn digits_are_filter_text_not_commands_while_filtering() {
        let mut a = app(&["log1", "log2", "other"]);
        a.on_key(Key::Char('/'));
        assert_eq!(a.on_key(Key::Char('1')), Request::None);
        assert_eq!(a.filter(), "1", "the digit typed into the query");
        assert_eq!(a.visible().len(), 1);
        assert_eq!(a.visible()[0].name, "log1");
    }

    #[test]
    fn digits_still_decline_a_kill_confirmation() {
        let mut a = app(&["aaa", "bbb"]);
        a.on_key(Key::Char('x'));
        assert!(matches!(a.mode(), Mode::Confirm { .. }));
        assert_eq!(a.on_key(Key::Char('1')), Request::None);
        assert_eq!(a.mode(), &Mode::Normal, "anything but y declines");
    }

    #[test]
    fn selection_follows_the_session_not_the_index() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char('j')); // on "bbb"
        assert_eq!(a.selected_session().expect("selected").name, "bbb");

        // "aaa" is killed and "bbb" moves to row 0; the highlight follows it.
        a.set_sessions(vec![
            session("id000001", "bbb", 1),
            session("id000002", "ccc", 2),
        ]);
        assert_eq!(a.selected_index(), 0, "it moved up with the session");
        assert_eq!(a.selected_session().expect("selected").id, "id000001");
        assert_eq!(a.selected_session().expect("selected").name, "bbb");
    }

    #[test]
    fn selection_follows_a_renamed_session() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char('j')); // on "bbb"

        a.set_sessions(vec![
            session("id000000", "aaa", 1),
            session("id000001", "zzz", 2),
            session("id000002", "ccc", 3),
        ]);
        assert_eq!(a.selected_session().expect("selected").id, "id000001");
        assert_eq!(a.selected_session().expect("selected").name, "zzz");
    }

    #[test]
    fn selection_survives_the_selected_session_disappearing() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char('G')); // "ccc"
        a.set_sessions(vec![
            session("id000000", "aaa", 1),
            session("id000001", "bbb", 2),
        ]);
        assert!(
            a.selected_index() < 2,
            "selection {} left dangling after the list shrank",
            a.selected_index()
        );
        assert!(a.selected_session().is_some());
    }

    #[test]
    fn set_sessions_on_an_empty_result_does_not_panic() {
        let mut a = app(&["aaa", "bbb"]);
        a.on_key(Key::Char('G'));
        a.set_sessions(vec![]);
        assert!(a.selected_session().is_none());
        assert_eq!(a.on_key(Key::Enter), Request::None);
    }

    #[test]
    fn a_message_is_cleared_by_the_next_keypress() {
        let mut a = app(&["one"]);
        a.set_message("something went wrong");
        assert!(a.message().is_some());
        a.on_key(Key::Char('j'));
        assert!(a.message().is_none(), "a stale message must not linger");
    }

    // --- reordering ---------------------------------------------------------

    /// Visible names and their numbers, in display order.
    fn arrangement(app: &App) -> (Vec<String>, Vec<u32>) {
        (
            app.visible().iter().map(|s| s.name.clone()).collect(),
            app.visible().iter().map(|s| s.state.num).collect(),
        )
    }

    #[test]
    fn space_picks_up_the_selected_session() {
        let mut a = app(&["aaa", "bbb"]);
        a.on_key(Key::Char('j'));
        assert_eq!(a.on_key(Key::Char(' ')), Request::None, "no I/O to request");
        match a.mode() {
            Mode::Reorder { id, was } => {
                assert_eq!(id, "id000001", "the row under the cursor");
                assert_eq!(was.len(), 2, "the whole visible list is snapshotted");
            }
            other => panic!("expected Reorder, got {other:?}"),
        }
    }

    #[test]
    fn space_on_an_empty_list_picks_nothing_up() {
        let mut a = app(&[]);
        assert_eq!(a.on_key(Key::Char(' ')), Request::None);
        assert_eq!(*a.mode(), Mode::Normal);
    }

    #[test]
    fn every_movement_key_carries_the_grabbed_session() {
        for (down, up) in [
            (Key::Char('j'), Key::Char('k')),
            (Key::Down, Key::Up),
            (Key::CtrlN, Key::CtrlP),
        ] {
            let mut a = app(&["aaa", "bbb", "ccc"]);
            a.on_key(Key::Char(' '));

            a.on_key(down);
            assert_eq!(
                arrangement(&a).0,
                ["bbb", "aaa", "ccc"],
                "{down:?} down one"
            );
            assert_eq!(a.selected_index(), 1, "{down:?} keeps the cursor on it");

            a.on_key(down);
            assert_eq!(arrangement(&a).0, ["bbb", "ccc", "aaa"]);

            a.on_key(down);
            assert_eq!(
                arrangement(&a).0,
                ["aaa", "bbb", "ccc"],
                "{down:?} should wrap to the top"
            );
            assert_eq!(a.selected_index(), 0);

            a.on_key(up);
            assert_eq!(
                arrangement(&a).0,
                ["bbb", "ccc", "aaa"],
                "{up:?} should wrap to the bottom"
            );
            assert_eq!(a.selected_index(), 2);
        }
    }

    #[test]
    fn g_and_shift_g_send_the_grabbed_session_to_the_ends() {
        for (top, bottom) in [(Key::Char('g'), Key::Char('G')), (Key::Home, Key::End)] {
            let mut a = app(&["aaa", "bbb", "ccc", "ddd"]);
            a.on_key(Key::Char(' '));
            a.on_key(bottom);
            assert_eq!(arrangement(&a).0, ["bbb", "ccc", "ddd", "aaa"]);
            assert_eq!(a.selected_index(), 3);
            a.on_key(top);
            assert_eq!(arrangement(&a).0, ["aaa", "bbb", "ccc", "ddd"]);
            assert_eq!(a.selected_index(), 0);
        }
    }

    /// Core invariant: a move changes which session holds a number, never the
    /// set of numbers (gaps included).
    #[test]
    fn the_numbers_on_screen_do_not_change_while_a_session_is_moving() {
        // Gapped on purpose: a drag must reuse the given numbers, not
        // renumber by row.
        let mut a = App::new(vec![
            session("id000000", "aaa", 1),
            session("id000001", "bbb", 2),
            session("id000002", "ccc", 5),
        ]);
        a.on_key(Key::Char(' '));
        for key in [Key::Down, Key::Down, Key::Down, Key::Up, Key::Char('G')] {
            a.on_key(key);
            assert_eq!(
                arrangement(&a).1,
                [1, 2, 5],
                "the numbers moved when {key:?} was pressed"
            );
        }
        let (names, _) = arrangement(&a);
        assert_eq!(names, ["bbb", "ccc", "aaa"], "the sessions moved, though");
    }

    /// A diff would skip rows showing a derived number, leaving a stored one
    /// that collides (see `Request::Reorder`).
    #[test]
    fn placing_asks_for_the_whole_arrangement_not_a_diff() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char(' '));
        a.on_key(Key::Down);
        let request = a.on_key(Key::Char(' '));
        assert_eq!(*a.mode(), Mode::Normal);
        assert_eq!(
            request,
            Request::Reorder(vec![
                ("id000001".into(), 1),
                ("id000000".into(), 2),
                ("id000002".into(), 3),
            ]),
            "ccc keeps its number and is still in the payload"
        );
    }

    #[test]
    fn enter_places_like_space() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char(' '));
        a.on_key(Key::Down);
        let request = a.on_key(Key::Enter);
        assert_eq!(*a.mode(), Mode::Normal, "the grab ended");
        assert_eq!(
            request,
            Request::Reorder(vec![
                ("id000001".into(), 1),
                ("id000000".into(), 2),
                ("id000002".into(), 3),
            ]),
            "the same payload Space would have asked for"
        );

        // No request when the arrangement is unchanged.
        let mut b = app(&["aaa", "bbb"]);
        b.on_key(Key::Char(' '));
        assert_eq!(
            b.on_key(Key::Enter),
            Request::None,
            "picked up and put down"
        );
        assert_eq!(*b.mode(), Mode::Normal);
    }

    #[test]
    fn a_grab_that_moved_nothing_asks_for_no_work() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char(' '));
        assert_eq!(
            a.on_key(Key::Char(' ')),
            Request::None,
            "picked up and put down"
        );
        assert_eq!(*a.mode(), Mode::Normal, "a double-tap leaves no edit open");

        a.on_key(Key::Char(' '));
        a.on_key(Key::Down);
        a.on_key(Key::Up);
        assert_eq!(
            a.on_key(Key::Char(' ')),
            Request::None,
            "moved and moved back is not a reorder"
        );
    }

    #[test]
    fn esc_puts_a_grabbed_session_back_where_it_came_from() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char('j')); // on "bbb"
        a.on_key(Key::Char(' '));
        a.on_key(Key::Down);
        a.on_key(Key::Down);
        assert_ne!(arrangement(&a).0, ["aaa", "bbb", "ccc"], "it did move");

        assert_eq!(a.on_key(Key::Esc), Request::None, "nothing was written");
        assert_eq!(*a.mode(), Mode::Normal);
        assert_eq!(
            arrangement(&a),
            (
                vec!["aaa".to_string(), "bbb".to_string(), "ccc".to_string()],
                vec![1, 2, 3]
            )
        );
        assert_eq!(
            a.selected_session().expect("selected").name,
            "bbb",
            "the cursor comes back with it"
        );
    }

    #[test]
    fn a_stray_key_does_not_end_a_reorder() {
        for key in [
            Key::Char('1'),
            Key::Char('x'),
            Key::Char('r'),
            Key::Char('c'),
            Key::Char('/'),
            Key::Char('?'),
            Key::Char('q'),
            Key::Tab,
            Key::Backspace,
            Key::Other,
        ] {
            let mut a = app(&["aaa", "bbb"]);
            a.on_key(Key::Char(' '));
            a.on_key(Key::Down);
            let before = arrangement(&a);

            assert_eq!(a.on_key(key), Request::None, "{key:?} must ask for nothing");
            assert!(
                matches!(a.mode(), Mode::Reorder { .. }),
                "{key:?} dropped the session"
            );
            assert_eq!(arrangement(&a), before, "{key:?} disturbed the arrangement");
        }
    }

    #[test]
    fn ctrl_c_still_quits_from_a_reorder() {
        let mut a = app(&["aaa", "bbb"]);
        a.on_key(Key::Char(' '));
        assert_eq!(a.on_key(Key::CtrlC), Request::Quit);
    }

    #[test]
    fn a_filtered_reorder_jumps_the_hidden_rows_and_leaves_their_numbers_alone() {
        let mut a = app(&["alpha", "zzz", "gamma"]);
        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('a'));
        a.on_key(Key::Enter); // back to normal mode, filter still applied
        assert_eq!(arrangement(&a).0, ["alpha", "gamma"]);

        a.on_key(Key::Char(' '));
        a.on_key(Key::Down);
        assert_eq!(arrangement(&a).0, ["gamma", "alpha"]);

        let request = a.on_key(Key::Char(' '));
        assert_eq!(
            request,
            Request::Reorder(vec![("id000002".into(), 1), ("id000000".into(), 3)]),
            "only the rows on screen, and zzz is not one of them"
        );
        assert_eq!(
            a.session("id000001").expect("zzz").state.num,
            2,
            "the hidden row kept its number"
        );
    }

    #[test]
    fn reordering_a_list_of_one_is_harmless() {
        let mut a = app(&["only"]);
        a.on_key(Key::Char(' '));
        for key in [Key::Down, Key::Up, Key::Char('g'), Key::Char('G')] {
            assert_eq!(a.on_key(key), Request::None);
            assert_eq!(a.selected_index(), 0);
        }
        assert_eq!(a.on_key(Key::Char(' ')), Request::None, "nothing moved");
        assert_eq!(*a.mode(), Mode::Normal);
    }

    #[test]
    fn the_cursor_starts_on_the_session_it_is_told_to_focus() {
        let mut a = app(&["one", "two", "three"]);
        a.select_session("id000002");
        assert_eq!(a.selected_index(), 2);
        assert_eq!(a.selected_session().map(|s| s.name.as_str()), Some("three"));
    }

    #[test]
    fn focusing_a_session_that_is_gone_leaves_the_cursor_alone() {
        let mut a = app(&["one", "two"]);
        a.on_key(Key::Char('j'));
        a.select_session("id000009");
        assert_eq!(a.selected_index(), 1, "an absent id must not move anything");

        let mut empty = App::new(Vec::new());
        empty.select_session("id000000");
        assert_eq!(empty.selected_index(), 0);
    }

    #[test]
    fn a_focused_row_survives_a_refresh_that_reorders_the_list() {
        let mut a = app(&["one", "two", "three"]);
        a.select_session("id000002");

        let reversed = app(&["one", "two", "three"])
            .visible()
            .into_iter()
            .rev()
            .cloned()
            .collect();
        a.set_sessions(reversed);

        assert_eq!(a.selected_session().map(|s| s.name.as_str()), Some("three"));
    }

    // --- the mouse ----------------------------------------------------------

    fn click(a: &mut App, row: usize) -> Request {
        let down = a.on_mouse(Mouse::Press(Some(row)));
        assert_eq!(down, Request::None, "a press alone asks for nothing");
        a.on_mouse(Mouse::Release)
    }

    #[test]
    fn a_click_selects_the_row_under_it() {
        let mut a = app(&["one", "two", "three"]);
        assert_eq!(click(&mut a, 2), Request::None);
        assert_eq!(a.selected_index(), 2);
        assert_eq!(*a.mode(), Mode::Normal);
        assert_eq!(click(&mut a, 0), Request::None);
        assert_eq!(a.selected_index(), 0);
    }

    /// Click to select, click again to open, decided by the selection, not a
    /// timer.
    #[test]
    fn a_click_on_the_selected_row_attaches() {
        let mut a = app(&["one", "two", "three"]);
        click(&mut a, 1);
        assert_eq!(click(&mut a, 1), Request::Attach("id000001".into()));

        // A key-placed cursor counts the same.
        let mut b = app(&["one", "two", "three"]);
        b.on_key(Key::Char('j'));
        assert_eq!(click(&mut b, 1), Request::Attach("id000001".into()));
    }

    #[test]
    fn a_release_after_selecting_does_not_attach() {
        let mut a = app(&["one", "two"]);
        a.on_mouse(Mouse::Press(Some(1)));
        assert_eq!(a.on_mouse(Mouse::Release), Request::None);
        assert_eq!(a.on_mouse(Mouse::Release), Request::None, "nor a stray one");
    }

    #[test]
    fn a_click_on_nothing_selects_nothing_and_attaches_nothing() {
        let mut a = app(&["one", "two"]);
        a.on_key(Key::Char('j'));
        assert_eq!(a.on_mouse(Mouse::Press(None)), Request::None);
        assert_eq!(a.on_mouse(Mouse::Release), Request::None);
        assert_eq!(a.selected_index(), 1, "the cursor stayed");
        // Out of range; the caller never sends it, but it is guarded.
        assert_eq!(a.on_mouse(Mouse::Press(Some(7))), Request::None);
        assert_eq!(a.on_mouse(Mouse::Release), Request::None);
        assert_eq!(a.selected_index(), 1);
    }

    #[test]
    fn a_click_attaches_from_the_filter_and_leaves_the_query_applied() {
        let mut a = app(&["api-server", "dotfiles", "notes"]);
        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('o'));
        assert_eq!(names(&a), ["dotfiles", "notes"]);
        assert_eq!(click(&mut a, 1), Request::None, "selects notes");
        assert_eq!(*a.mode(), Mode::Filter, "a single click keeps the prompt");
        assert_eq!(click(&mut a, 1), Request::Attach("id000002".into()));
        assert_eq!(*a.mode(), Mode::Normal);
        assert_eq!(a.filter(), "o");
    }

    #[test]
    fn the_wheel_moves_the_selection_and_stops_at_the_ends() {
        let mut a = app(&["one", "two", "three"]);
        assert_eq!(a.on_mouse(Mouse::ScrollUp), Request::None);
        assert_eq!(a.selected_index(), 0, "no wrap at the top");
        a.on_mouse(Mouse::ScrollDown);
        a.on_mouse(Mouse::ScrollDown);
        assert_eq!(a.selected_index(), 2);
        a.on_mouse(Mouse::ScrollDown);
        assert_eq!(a.selected_index(), 2, "no wrap at the bottom");
        a.on_mouse(Mouse::ScrollUp);
        assert_eq!(a.selected_index(), 1);
    }

    #[test]
    fn the_wheel_moves_within_the_filtered_list() {
        let mut a = app(&["aaa", "bbb", "abc"]);
        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('a'));
        assert_eq!(names(&a), ["aaa", "abc"]);
        a.on_mouse(Mouse::ScrollDown);
        a.on_mouse(Mouse::ScrollDown);
        assert_eq!(a.selected_session().expect("selected").name, "abc");
        assert_eq!(
            *a.mode(),
            Mode::Filter,
            "the wheel does not close the prompt"
        );
    }

    #[test]
    fn a_press_or_a_wheel_step_ends_a_message_and_a_half_typed_number() {
        let names: Vec<String> = (0..12).map(|i| format!("s{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        for gesture in [Mouse::Press(Some(0)), Mouse::Press(None), Mouse::ScrollDown] {
            let mut a = app(&refs);
            a.set_message("something went wrong");
            a.on_key(Key::Char('1'));
            assert_eq!(a.pending(), Some(1), "1 is ambiguous while 10-12 exist");
            assert_eq!(a.on_mouse(gesture), Request::None);
            assert_eq!(a.message(), None, "{gesture:?}");
            assert_eq!(a.pending(), None, "{gesture:?}");
        }
        // Hover, drag and release are not deliberate enough.
        for gesture in [
            Mouse::Hover(Some(1)),
            Mouse::Hover(None),
            Mouse::Drag(Some(1)),
            Mouse::Drag(None),
            Mouse::Release,
        ] {
            let mut a = app(&refs);
            // Key first, since a key clears the message.
            a.on_key(Key::Char('1'));
            a.set_message("still here");
            a.on_mouse(gesture);
            assert_eq!(a.message(), Some("still here"), "{gesture:?}");
            assert_eq!(a.pending(), Some(1), "{gesture:?}");
        }
    }

    // --- hover --------------------------------------------------------------

    #[test]
    fn hovering_over_a_row_selects_it() {
        let mut a = app(&["one", "two", "three"]);
        assert_eq!(a.on_mouse(Mouse::Hover(Some(2))), Request::None);
        assert_eq!(a.selected_index(), 2);
        assert_eq!(*a.mode(), Mode::Normal);
        a.on_mouse(Mouse::Hover(Some(0)));
        assert_eq!(a.selected_index(), 0);
    }

    #[test]
    fn hovering_off_the_list_leaves_the_cursor_where_it_was() {
        let mut a = app(&["one", "two", "three"]);
        a.on_mouse(Mouse::Hover(Some(1)));
        assert_eq!(a.on_mouse(Mouse::Hover(None)), Request::None);
        assert_eq!(a.selected_index(), 1);
        assert_eq!(
            a.on_mouse(Mouse::Hover(Some(9))),
            Request::None,
            "out of range"
        );
        assert_eq!(a.selected_index(), 1);
    }

    #[test]
    fn a_single_click_on_a_hovered_row_attaches() {
        let mut a = app(&["one", "two", "three"]);
        a.on_mouse(Mouse::Hover(Some(2)));
        assert_eq!(click(&mut a, 2), Request::Attach("id000002".into()));

        let mut b = app(&["api-server", "dotfiles", "notes"]);
        b.on_key(Key::Char('/'));
        b.on_key(Key::Char('o'));
        b.on_mouse(Mouse::Hover(Some(1)));
        assert_eq!(click(&mut b, 1), Request::Attach("id000002".into()));
        assert_eq!(*b.mode(), Mode::Normal);
        assert_eq!(b.filter(), "o", "the query stays applied");
    }

    #[test]
    fn a_drag_after_a_hover_still_picks_the_row_up() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_mouse(Mouse::Hover(Some(1)));
        a.on_mouse(Mouse::Press(Some(1)));
        a.on_mouse(Mouse::Drag(Some(2)));
        assert!(matches!(a.mode(), Mode::Reorder { .. }));
        assert_eq!(arrangement(&a).0, ["aaa", "ccc", "bbb"]);
        assert_eq!(
            a.on_mouse(Mouse::Release),
            Request::Reorder(vec![
                ("id000000".into(), 1),
                ("id000002".into(), 2),
                ("id000001".into(), 3),
            ])
        );
    }

    #[test]
    fn hovering_does_not_carry_a_grabbed_row() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char(' '));
        assert_eq!(a.on_mouse(Mouse::Hover(Some(2))), Request::None);
        assert!(matches!(a.mode(), Mode::Reorder { .. }));
        assert_eq!(arrangement(&a).0, ["aaa", "bbb", "ccc"]);
        assert_eq!(a.selected_index(), 0);
    }

    #[test]
    fn hovering_does_not_move_under_a_kill_confirm() {
        let mut a = app(&["dotfiles", "notes"]);
        a.on_key(Key::Char('x'));
        assert_eq!(a.on_mouse(Mouse::Hover(Some(1))), Request::None);
        assert!(matches!(a.mode(), Mode::Confirm { .. }));
        assert_eq!(a.selected_index(), 0);
    }

    #[test]
    fn a_drag_picks_the_row_up_and_a_release_places_it() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_mouse(Mouse::Press(Some(0)));
        assert_eq!(*a.mode(), Mode::Normal, "a press alone grabs nothing");

        assert_eq!(a.on_mouse(Mouse::Drag(Some(1))), Request::None);
        assert!(
            matches!(a.mode(), Mode::Reorder { .. }),
            "the drag grabbed it"
        );
        assert_eq!(arrangement(&a).0, ["bbb", "aaa", "ccc"]);
        assert_eq!(a.selected_index(), 1, "the cursor travels with it");

        a.on_mouse(Mouse::Drag(Some(2)));
        assert_eq!(arrangement(&a).0, ["bbb", "ccc", "aaa"]);
        assert_eq!(
            arrangement(&a).1,
            [1, 2, 3],
            "the numbers on screen do not change while a session is moving"
        );

        assert_eq!(
            a.on_mouse(Mouse::Release),
            Request::Reorder(vec![
                ("id000001".into(), 1),
                ("id000002".into(), 2),
                ("id000000".into(), 3),
            ]),
            "the whole arrangement, as Enter would ask for it"
        );
        assert_eq!(*a.mode(), Mode::Normal);
    }

    #[test]
    fn dragging_the_selected_row_moves_it_rather_than_attaching() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_mouse(Mouse::Press(Some(0))); // the selection: armed to attach
        a.on_mouse(Mouse::Drag(Some(2)));
        assert_eq!(
            a.on_mouse(Mouse::Release),
            Request::Reorder(vec![
                ("id000001".into(), 1),
                ("id000002".into(), 2),
                ("id000000".into(), 3),
            ])
        );
        assert_eq!(*a.mode(), Mode::Normal);
    }

    #[test]
    fn a_drag_that_comes_back_to_where_it_started_writes_nothing() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_mouse(Mouse::Press(Some(1)));
        a.on_mouse(Mouse::Drag(Some(2)));
        a.on_mouse(Mouse::Drag(Some(1)));
        assert_eq!(
            a.on_mouse(Mouse::Release),
            Request::None,
            "moved and moved back"
        );
        assert_eq!(*a.mode(), Mode::Normal);
        assert_eq!(arrangement(&a).0, ["aaa", "bbb", "ccc"]);
        assert_eq!(a.selected_index(), 1);
    }

    /// Pointer wobble within the pressed row, or off the list, grabs nothing.
    #[test]
    fn a_drag_that_leaves_no_row_grabs_nothing() {
        let mut a = app(&["aaa", "bbb"]);
        a.on_mouse(Mouse::Press(Some(1)));
        a.on_mouse(Mouse::Drag(Some(1)));
        a.on_mouse(Mouse::Drag(None));
        assert_eq!(*a.mode(), Mode::Normal);
        assert_eq!(a.on_mouse(Mouse::Release), Request::None);

        // A drag from blank space must not grab the selection.
        let mut b = app(&["aaa", "bbb"]);
        b.on_mouse(Mouse::Press(None));
        b.on_mouse(Mouse::Drag(Some(1)));
        assert_eq!(*b.mode(), Mode::Normal);
        assert_eq!(arrangement(&b).0, ["aaa", "bbb"]);
    }

    #[test]
    fn a_row_in_flight_stays_put_while_the_pointer_is_off_the_list() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_mouse(Mouse::Press(Some(0)));
        a.on_mouse(Mouse::Drag(Some(1)));
        a.on_mouse(Mouse::Drag(None));
        assert!(matches!(a.mode(), Mode::Reorder { .. }));
        assert_eq!(arrangement(&a).0, ["bbb", "aaa", "ccc"]);
        assert_eq!(
            a.on_mouse(Mouse::Release),
            Request::Reorder(vec![
                ("id000001".into(), 1),
                ("id000000".into(), 2),
                ("id000002".into(), 3)
            ])
        );
    }

    #[test]
    fn a_keyboard_grab_can_be_dropped_with_a_click() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char(' '));
        assert_eq!(a.on_mouse(Mouse::Press(Some(2))), Request::None);
        assert_eq!(
            arrangement(&a).0,
            ["bbb", "ccc", "aaa"],
            "carried on the press"
        );
        assert!(matches!(a.mode(), Mode::Reorder { .. }));
        assert_eq!(
            a.on_mouse(Mouse::Release),
            Request::Reorder(vec![
                ("id000001".into(), 1),
                ("id000002".into(), 2),
                ("id000000".into(), 3),
            ])
        );
        assert_eq!(*a.mode(), Mode::Normal);
    }

    /// Clicking the grabbed row, or beside the list, places it where it is.
    #[test]
    fn a_click_that_moves_a_grabbed_row_nowhere_just_places_it() {
        let mut a = app(&["aaa", "bbb"]);
        a.on_key(Key::Char(' '));
        a.on_mouse(Mouse::Press(Some(0)));
        assert_eq!(a.on_mouse(Mouse::Release), Request::None);
        assert_eq!(*a.mode(), Mode::Normal);

        let mut b = app(&["aaa", "bbb"]);
        b.on_key(Key::Char(' '));
        b.on_mouse(Mouse::Press(None));
        assert_eq!(b.on_mouse(Mouse::Release), Request::None);
        assert_eq!(*b.mode(), Mode::Normal);
    }

    #[test]
    fn the_wheel_carries_a_grabbed_row_and_stops_at_the_ends() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char(' '));
        a.on_mouse(Mouse::ScrollUp);
        assert_eq!(
            arrangement(&a).0,
            ["aaa", "bbb", "ccc"],
            "no wrap at the top"
        );
        a.on_mouse(Mouse::ScrollDown);
        a.on_mouse(Mouse::ScrollDown);
        a.on_mouse(Mouse::ScrollDown);
        assert_eq!(
            arrangement(&a).0,
            ["bbb", "ccc", "aaa"],
            "no wrap at the bottom"
        );
        assert!(
            matches!(a.mode(), Mode::Reorder { .. }),
            "the wheel does not place"
        );
        assert_eq!(a.on_key(Key::Esc), Request::None);
        assert_eq!(
            arrangement(&a).0,
            ["aaa", "bbb", "ccc"],
            "esc still puts it back"
        );
    }

    /// Unlike Space, a drag can start from the filter.
    #[test]
    fn a_drag_reorders_the_filtered_rows_and_leaves_hidden_ones_alone() {
        let mut a = app(&["alpha", "zzz", "gamma"]);
        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('a'));
        assert_eq!(arrangement(&a).0, ["alpha", "gamma"]);
        a.on_mouse(Mouse::Press(Some(0)));
        a.on_mouse(Mouse::Drag(Some(1)));
        assert_eq!(
            a.on_mouse(Mouse::Release),
            Request::Reorder(vec![("id000002".into(), 1), ("id000000".into(), 3)])
        );
        assert_eq!(a.session("id000001").expect("zzz").state.num, 2);
        assert_eq!(*a.mode(), Mode::Normal);
        assert_eq!(a.filter(), "a", "the query stays applied");
    }

    #[test]
    fn a_click_declines_a_kill_confirm_and_the_wheel_leaves_it_open() {
        let mut a = app(&["dotfiles", "notes"]);
        a.on_key(Key::Char('x'));
        assert_eq!(a.on_mouse(Mouse::ScrollDown), Request::None);
        assert!(matches!(a.mode(), Mode::Confirm { .. }));
        assert_eq!(a.selected_index(), 0, "nor does it move under the question");
        assert_eq!(a.on_mouse(Mouse::Press(Some(1))), Request::None);
        assert_eq!(*a.mode(), Mode::Normal, "declined");
        assert_eq!(a.selected_index(), 0, "declined, not acted on");
        assert_eq!(
            a.on_mouse(Mouse::Release),
            Request::None,
            "and not attached"
        );
    }

    #[test]
    fn every_gesture_on_an_empty_list_is_harmless() {
        let mut a = app(&[]);
        for gesture in [
            Mouse::Hover(Some(0)),
            Mouse::Hover(None),
            Mouse::Press(Some(0)),
            Mouse::Press(None),
            Mouse::Drag(Some(0)),
            Mouse::Drag(None),
            Mouse::Release,
            Mouse::ScrollUp,
            Mouse::ScrollDown,
        ] {
            assert_eq!(a.on_mouse(gesture), Request::None, "{gesture:?}");
            assert_eq!(*a.mode(), Mode::Normal, "{gesture:?}");
            assert_eq!(a.selected_index(), 0, "{gesture:?}");
        }
    }

    #[test]
    fn a_drag_on_a_list_of_one_is_harmless() {
        let mut a = app(&["only"]);
        a.on_mouse(Mouse::Press(Some(0)));
        a.on_mouse(Mouse::Drag(Some(0)));
        a.on_mouse(Mouse::Drag(None));
        assert_eq!(*a.mode(), Mode::Normal);
        assert_eq!(
            a.on_mouse(Mouse::Release),
            Request::Attach("id000000".into()),
            "a click on the one row, which was already selected"
        );
    }

    // ---- the trail behind a session in flight ------------------------------

    #[test]
    fn a_session_in_flight_keeps_the_clock_running() {
        let mut a = app(&["aaa", "bbb"]);
        a.set_trail(true);
        assert!(!a.trailing());
        a.on_key(Key::Char(' '));
        assert!(a.trailing(), "picked up with the space bar");
        a.on_key(Key::Down);
        a.on_key(Key::Enter);
        assert!(!a.trailing(), "placed");

        a.on_key(Key::Char(' '));
        a.on_key(Key::Esc);
        assert!(!a.trailing(), "put back");

        let mut a = app(&["aaa", "bbb"]);
        a.set_trail(true);
        a.on_mouse(Mouse::Press(Some(0)));
        a.on_mouse(Mouse::Drag(Some(1)));
        assert!(a.trailing(), "picked up with a drag");
        a.on_mouse(Mouse::Release);
        assert!(!a.trailing());
    }

    #[test]
    fn the_trails_are_laid_out_when_a_session_is_picked_up() {
        let mut a = app(&["aaa"]);
        a.set_trail(true);
        a.tick(Duration::from_millis(100));
        for trail in [a.trail_after(), a.trail_before()] {
            assert!(
                trail.render(starfield::TRAIL).trim().is_empty(),
                "a trail before anything was picked up"
            );
        }
        a.on_key(Key::Char(' '));
        a.tick(Duration::from_millis(16));
        for trail in [a.trail_after(), a.trail_before()] {
            assert_eq!(
                trail.render(starfield::TRAIL).chars().count(),
                starfield::TRAIL
            );
        }
    }

    /// `[effects.move] enabled = false`.
    #[test]
    fn with_the_trail_off_a_move_is_drawn_still() {
        let mut a = app(&["aaa", "bbb"]);
        a.set_trail(false);
        a.on_key(Key::Char(' '));
        assert!(matches!(a.mode(), Mode::Reorder { .. }), "still a move");
        assert!(!a.trailing());
        a.tick(Duration::from_millis(100));
        assert!(a.trail_after().render(starfield::TRAIL).trim().is_empty());
    }

    /// On by default; tests load no config.
    #[test]
    fn the_trail_follows_the_config() {
        assert!(crate::config::get().effects.move_enabled());
        assert!(app(&["aaa"]).trail);
    }

    // --- the swap -----------------------------------------------------------

    #[test]
    fn a_move_starts_a_swap_by_key_or_by_mouse() {
        for by_mouse in [false, true] {
            let mut a = app(&["aaa", "bbb", "ccc"]);
            a.set_trail(true);
            a.on_key(Key::Char(' '));
            if by_mouse {
                a.on_mouse(Mouse::Drag(Some(1)));
            } else {
                a.on_key(Key::Char('j'));
            }
            let swap = a.swap();
            assert_eq!(swap.aside("id000000"), swap::ASIDE as i16, "carried");
            assert_eq!(swap.aside("id000001"), -(swap::ASIDE as i16), "passed");
            assert_eq!(swap.aside("id000002"), 0, "not passed");
            let echoes: Vec<&str> = swap.echoes().map(|(id, _)| id).collect();
            assert_eq!(echoes, ["id000001"], "the row it left lets its bar go");
            assert!(a.animating());
        }
    }

    #[test]
    fn a_jump_passes_every_row_between_nearest_first() {
        let mut a = app(&["aaa", "bbb", "ccc", "ddd"]);
        a.set_trail(true);
        a.on_key(Key::Char(' '));
        a.on_key(Key::Char('G'));
        let echoes: Vec<&str> = a.swap().echoes().map(|(id, _)| id).collect();
        assert_eq!(echoes, ["id000003", "id000002", "id000001"]);

        let mut a = app(&["aaa", "bbb", "ccc", "ddd"]);
        a.set_trail(true);
        a.on_key(Key::Char('G'));
        a.on_key(Key::Char(' '));
        a.on_key(Key::Char('g'));
        let echoes: Vec<&str> = a.swap().echoes().map(|(id, _)| id).collect();
        assert_eq!(echoes, ["id000000", "id000001", "id000002"], "and going up");
    }

    #[test]
    fn placing_lets_the_swap_finish_and_cancelling_ends_it() {
        let mut a = app(&["aaa", "bbb"]);
        a.set_trail(true);
        a.on_key(Key::Char(' '));
        a.on_key(Key::Char('j'));
        a.on_key(Key::Enter);
        assert!(a.landing().is_some());
        assert!(a.swap().moving(), "the swap goes on under the landing");
        a.tick(swap::SPARKING.max(swap::SIDESTEP).max(swap::ECHO));
        assert!(!a.swap().moving());

        let mut a = app(&["aaa", "bbb"]);
        a.set_trail(true);
        a.on_key(Key::Char(' '));
        a.on_key(Key::Char('j'));
        a.on_key(Key::Esc);
        assert!(!a.swap().moving(), "a cancelled move leaves nothing behind");
    }

    #[test]
    fn with_the_trail_off_a_move_starts_no_swap() {
        let mut a = app(&["aaa", "bbb"]);
        a.set_trail(false);
        a.on_key(Key::Char(' '));
        a.on_key(Key::Char('j'));
        assert!(!a.swap().moving());
        assert_eq!(a.swap().aside("id000001"), 0);
        assert!(!a.animating());
    }

    #[test]
    fn a_fresh_listing_ends_the_swap() {
        let mut a = app(&["aaa", "bbb"]);
        a.set_trail(true);
        a.on_key(Key::Char(' '));
        a.on_key(Key::Char('j'));
        a.on_key(Key::Enter);
        let sessions = a.sessions().to_vec();
        a.set_sessions(sessions);
        assert!(!a.swap().moving());
    }

    // ---- where a new session goes ------------------------------------------

    #[test]
    fn a_new_session_goes_in_after_the_highlighted_row() {
        let a = app(&["aaa", "bbb", "ccc"]);
        assert_eq!(
            a.placement_of("new"),
            [
                ("id000000".to_string(), 1),
                ("new".to_string(), 2),
                ("id000001".to_string(), 3),
                ("id000002".to_string(), 4),
            ]
        );

        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char('j'));
        assert_eq!(
            a.placement_of("new"),
            [
                ("id000000".to_string(), 1),
                ("id000001".to_string(), 2),
                ("new".to_string(), 3),
                ("id000002".to_string(), 4),
            ]
        );
    }

    #[test]
    fn a_new_session_after_the_last_row_stores_nothing() {
        let mut a = app(&["aaa", "bbb"]);
        a.on_key(Key::Char('G'));
        assert!(a.placement_of("new").is_empty());
        assert!(app(&[]).placement_of("new").is_empty());
    }

    #[test]
    fn a_filtered_placement_leaves_the_hidden_rows_alone() {
        let mut a = app(&["alpha", "zzz", "gamma"]);
        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('a'));
        a.on_key(Key::Enter); // back to normal mode, filter still applied
        assert_eq!(names(&a), ["alpha", "gamma"]);
        assert_eq!(
            a.placement_of("new"),
            [
                ("id000000".to_string(), 1),
                ("new".to_string(), 3),
                ("id000002".to_string(), 4),
            ]
        );
    }

    #[test]
    fn asking_for_a_name_leaves_the_highlight_where_it_is() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char('j'));
        assert_eq!(a.on_key(Key::Char('c')), Request::NewSession);
        assert_eq!(a.selected_index(), 1);
        assert_eq!(*a.mode(), Mode::Normal);
    }

    // ---- the gap a new session opens ---------------------------------------

    /// Every drawn row as `(id, number shown)`.
    fn drawn(a: &App) -> Vec<(String, u32)> {
        a.rows()
            .iter()
            .map(|r| (r.session.id.clone(), r.num))
            .collect()
    }

    /// For every highlight position, filtered or not, the gap matches
    /// [`App::placement_of`].
    #[test]
    fn the_gap_shows_the_placement_the_create_will_store() {
        let filtered = || {
            let mut a = app(&["alpha", "zzz", "gamma", "delta"]);
            a.on_key(Key::Char('/'));
            a.on_key(Key::Char('a'));
            a.on_key(Key::Enter);
            a
        };
        for build in [|| app(&["aaa", "bbb", "ccc"]), filtered] {
            let rows = build().visible().len();
            for at in 0..rows {
                let mut a = build();
                a.select_session(&a.visible()[at].id.clone());
                let placement = a.placement_of(GHOST_ID);
                assert!(a.make_room("session 9"));
                if placement.is_empty() {
                    continue;
                }
                assert_eq!(drawn(&a), placement, "highlight on row {at}");
            }
        }
    }

    #[test]
    fn the_rows_under_the_highlight_move_down_for_the_new_one() {
        let mut a = app(&["api-server", "dotfiles", "notes"]);
        a.on_key(Key::Char('j'));
        assert!(a.make_room("session 4"));
        let rows = a.rows();
        let shown: Vec<(&str, u32, bool)> = rows
            .iter()
            .map(|r| (r.session.name.as_str(), r.num, r.ghost))
            .collect();
        assert_eq!(
            shown,
            [
                ("api-server", 1, false),
                ("dotfiles", 2, false),
                ("session 4", 3, true),
                ("notes", 4, false),
            ]
        );
        assert_eq!(a.selected_row(), 1, "the highlight has not moved");
        assert_eq!(a.visible().len(), 3, "and nothing can be chosen in the gap");
        assert_eq!(a.session("id000002").expect("notes").state.num, 3);
    }

    #[test]
    fn with_nothing_after_the_highlight_the_gap_is_at_the_end() {
        let mut a = app(&["aaa", "bbb"]);
        a.on_key(Key::Char('G'));
        assert!(a.make_room("session 3"));
        assert_eq!(
            drawn(&a),
            [
                ("id000000".to_string(), 1),
                ("id000001".to_string(), 2),
                (GHOST_ID.to_string(), 3),
            ]
        );

        let mut a = app(&[]);
        assert!(a.make_room("session 1"));
        assert_eq!(drawn(&a), [(GHOST_ID.to_string(), 1)]);
    }

    /// Animates over [`effects::ROOM`], then stays until closed.
    #[test]
    fn the_row_in_the_gap_comes_up_and_then_stands() {
        let mut a = app(&["aaa", "bbb"]);
        assert!(a.room().is_none() && !a.has_room());
        assert!(a.make_room("session 3"));
        assert_eq!(a.room(), Some(0.0));
        assert!(a.animating());
        a.tick(effects::ROOM / 2);
        let halfway = a.room().expect("still coming");
        assert!((halfway - 0.5).abs() < 0.01, "{halfway}");
        a.tick(effects::ROOM);
        assert!(a.room().is_none(), "all the way up");
        assert!(a.has_room(), "and still there");
        assert!(!a.animating());
        a.close_room();
        assert!(!a.has_room());
        assert_eq!(a.rows().len(), 2);
    }

    #[test]
    fn a_fresh_listing_closes_the_gap() {
        let mut a = app(&["aaa", "bbb"]);
        assert!(a.make_room("session 3"));
        let listed = a.sessions().to_vec();
        a.set_sessions(listed);
        assert!(!a.has_room());
        assert!(a.rows().iter().all(|r| !r.ghost));
    }

    #[test]
    fn making_room_ends_what_else_was_passing() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char('j'));
        assert!(a.glow().is_some());
        assert!(a.make_room("session 4"));
        assert!(a.glow().is_none());

        let mut a = app(&["aaa", "bbb"]);
        a.on_key(Key::Char('x'));
        a.on_key(Key::Char('n'));
        assert!(a.strike().is_some(), "the line on its way back off");
        assert!(a.make_room("session 3"));
        assert!(a.strike().is_none());
    }

    #[test]
    fn with_the_effect_off_no_gap_opens() {
        let mut a = app(&["aaa", "bbb"]);
        a.set_effects(false);
        assert!(!a.make_room("session 3"));
        assert!(!a.has_room());
        assert_eq!(a.rows().len(), 2);
    }

    /// After `dotfiles`, the 3 that `notes` had; after the last row, one past
    /// it.
    #[test]
    fn the_new_number_is_the_one_the_session_will_be_given() {
        for at in 0..3 {
            let mut a = app(&["api-server", "dotfiles", "notes"]);
            a.select_session(&format!("id{at:06}"));
            let number = a.new_number();
            assert_eq!(number, at as u32 + 2, "after row {at}");
            let stored = a.placement_of(GHOST_ID);
            assert!(stored.is_empty() || stored.contains(&(GHOST_ID.to_string(), number)));
            assert!(a.make_room("x"));
            let shown = a.rows().iter().find(|r| r.ghost).map(|r| r.num);
            assert_eq!(shown, Some(number));
        }
        assert_eq!(app(&[]).new_number(), 1);

        let mut a = app(&["alpha", "zzz", "gamma"]);
        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('a'));
        a.on_key(Key::Enter);
        assert_eq!(a.new_number(), 3, "after alpha, the number gamma shows");
    }

    #[test]
    fn the_gap_row_has_an_id_no_session_can_have() {
        assert!(!crate::ids::is_valid_id(GHOST_ID));
    }
}
