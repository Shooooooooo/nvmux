//! Picker state.
//!
//! Deliberately free of I/O and of ratatui: this is a state machine that takes
//! key events and returns [`Request`]s for the caller to carry out. That is what
//! makes every keybind, the filtering, and the prompt behaviour testable without
//! a terminal, a transport, or a running Neovim.

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
use crate::config::Notes as NoteStyle;
use crate::notes::Note;
use crate::session::Session;

/// What the picker is currently doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Typing a filter. The list narrows live as the query changes.
    Filter,
    /// Waiting for y/N on a kill.
    Confirm {
        id: String,
        prompt: String,
    },
    /// A session has been picked up and is being moved. `was` is every visible
    /// row's `(id, state.num)` as it stood when the grab started: all `Esc`
    /// needs to put everything back, and all a placing `Space` needs to tell a
    /// real move from a grab that went nowhere.
    Reorder {
        id: String,
        was: Vec<(String, u32)>,
    },
}

/// Work the picker wants the caller to do.
///
/// The picker never performs I/O itself; it asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    None,
    Attach(String),
    /// Ask for a name for a new session; naming happens on its own screen.
    ///
    /// The session goes in just after the row highlighted when this was asked,
    /// which nothing moves while the prompt is up — see [`App::placement_of`].
    NewSession,
    /// Ask for a new name for this session.
    RenameSession(String),
    Kill(String),
    /// Persist this arrangement: every visible session paired with the number it
    /// should now store.
    ///
    /// The whole arrangement rather than only the rows that moved. What is
    /// persisted is [`Session::num`], and a row's stored number can already
    /// differ from the resolved one it is showing — `finish_listing` re-derives
    /// a number for a duplicate and for the `num == 0` of legacy metadata and
    /// orphans. A row skipped as "unchanged" would keep a stored number that
    /// then collides with one this batch just wrote, and the next listing would
    /// resolve the collision the other way round: not a partly-moved list but an
    /// arbitrary one. Sending the arrangement entire costs the same — one ssh
    /// round trip either way — and repairs those numbers on its way past.
    ///
    /// Never sent empty: a grab that changed nothing returns [`Request::None`],
    /// so picking a session up and putting it straight down costs no I/O.
    Reorder(Vec<(String, u32)>),
    /// Show the key bindings; help happens on its own screen.
    Help,
    Quit,
}

pub struct App {
    sessions: Vec<Session>,
    filter: String,
    /// Index into the *visible* (filtered) list.
    selected: usize,
    mode: Mode,
    /// A transient message shown where the hints normally are.
    message: Option<String>,
    /// Digits typed so far towards a session number, when more digits could
    /// still change which session is meant. See [`App::on_digit`].
    pending: Option<u32>,
    /// The session the picker was opened from and can be dismissed back to.
    /// See [`App::set_came_from`].
    came_from: Option<String>,
    /// What the left button last went down on, until it comes up. See
    /// [`App::on_mouse`].
    pressed: Option<Pressed>,
    /// Scratch buffers for the filter's scorer, kept across queries: building
    /// a `Matcher` allocates its whole matrix up front, and [`App::visible`]
    /// is asked several times per keystroke. In a `RefCell` because scoring
    /// takes `&mut` and `visible` is a read.
    matcher: RefCell<Matcher>,
    /// Whether a session in flight leaves a trail at all, and so trades places
    /// visibly and lands: `[effects.move] enabled`, under `[effects] enabled`,
    /// read once when the picker opens, as the rest of the config is.
    trail: bool,
    /// The trails a session in flight leaves (see [`starfield`]): off the end
    /// of its name, flying right, and off the other end of its row, flying
    /// left. Two fields rather than one drawn twice, so the two ends are not
    /// mirror images of each other. Kept between grabs, so each one carries on
    /// the same generators rather than drawing the same stars.
    after: Starfield,
    before: Starfield,
    /// The landing under way, from the key that put a session down until its
    /// dust settles. See [`super::landing`].
    landing: Option<Landing>,
    /// What the last moves of a session in flight are still doing to the rows
    /// it traded places with: stepping aside, letting its bar go, sparks off
    /// the seam. See [`super::swap`].
    swap: Swap,
    /// Whether the cursor lights the rows it moves between — the afterglow on
    /// the row it leaves, the glint across the one it lands on:
    /// `[effects.cursor] enabled`, under `[effects] enabled`. Read once when
    /// the picker opens, like `trail`.
    lights: bool,
    /// The row the cursor has just left, while it is still glowing. One at a
    /// time, like the glint: the next move takes the glow off it and puts it on
    /// the row that move left, so a held key never smears a tail of rows. See
    /// [`App::glow`].
    glow: Option<Glow>,
    /// The row the cursor last landed on, while the glint crosses it. One at
    /// a time: the next move takes it to the next row. See [`App::glint`].
    glint: Option<Glow>,
    /// Whether a filter keystroke's dropped rows fade out (`[effects.filter]
    /// enabled`).
    sift: bool,
    /// The rows the last filter keystroke dropped, while they fade. See
    /// [`App::rows`].
    leaving: Option<Leaving>,
    /// Whether a kill is drawn — the name struck through while the `[y/N]`
    /// asks, and the row erased once it is confirmed: `[effects.kill]
    /// enabled`.
    kill: bool,
    /// The line through the name a `[y/N]` is asking about, while it is drawn
    /// on and, once the answer is no, back off. See [`App::strike`].
    strike: Option<Strike>,
    /// The row a kill is erasing, by session id, until a fresh listing
    /// replaces it. See [`App::start_backspace`].
    backspace: Option<(String, Backspace)>,
    /// Whether coming back to the picker from a session marks that session's
    /// row: `[effects.back] enabled`. See [`App::back_to`].
    back: bool,
    /// Whether `c` opens a gap where the new session will go before the prompt
    /// comes up: `[effects.create] enabled`.
    create: bool,
    /// That gap, from `c` until the prompt is back down. See
    /// [`App::make_room`].
    room: Option<Room>,
    /// How a session's note is shown: `[picker] notes`, read once when the
    /// picker opens, like the effects. See [`super::note`].
    note_style: NoteStyle,
    /// What each session last said it had to report, by id (see
    /// [`crate::notes`]): replaced whole by each round of reads, which is the
    /// truth about them all.
    notes: HashMap<String, Note>,
    /// How long the spinner beside a busy session has been turning, which is
    /// all that says which of its frames is up. Moved on whether or not one is
    /// on screen: a spinner that comes up starts wherever this has got to.
    spin: Duration,
}

/// The id of the row standing in the gap [`App::make_room`] opens: no session's,
/// since `+` is not in the alphabet an id is written in (see
/// [`crate::ids::is_valid_id`]), so nothing that looks a row up by id can find
/// a real session there, or find this row by a real session's id.
pub const GHOST_ID: &str = "+";

/// The gap a session about to be created will land in.
#[derive(Debug, Clone)]
struct Room {
    /// The row in it: the session as it will be listed, under the name the
    /// prompt will offer and the number it will be given.
    ghost: Session,
    /// Where it goes among the visible rows.
    at: usize,
    /// The number one past every row's, which the last row takes once
    /// everything under the gap has moved down one.
    last: u32,
    /// How long the gap has been open.
    age: Duration,
}

/// A row the cursor has left or landed on, and how long ago.
#[derive(Debug, Clone)]
struct Glow {
    id: String,
    age: Duration,
}

/// The line through a name, by session id: how much of it is drawn, `0..=1`,
/// and whether it is going back off. Kept as an amount rather than an age so a
/// `[y/N]` answered before the line was all the way across draws it back from
/// where it had got to.
#[derive(Debug, Clone)]
struct Strike {
    id: String,
    drawn: f32,
    back: bool,
}

/// What one filter keystroke dropped, and how long ago.
#[derive(Debug, Clone)]
struct Leaving {
    ids: Vec<String>,
    age: Duration,
}

/// One row of the list as it is drawn: a session, whether it is one the
/// filter has just dropped and is only on screen while it fades, whether it is
/// the session about to be created standing in the gap made for it (see
/// [`App::make_room`]), and the number it shows.
///
/// The number is the session's own except under a gap, where every row below
/// it shows the number of the row below it: the numbers stay where they are on
/// the screen and the rows move down past them, which is what the create is
/// about to store (see [`App::placement_of`]).
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
    /// On a row that was not the selection: the press selected it, and the
    /// release does nothing. A drag in between picks the row up.
    Row,
    /// On the row already selected: the release attaches, unless a drag in
    /// between turned the gesture into a move.
    Selected,
}

/// What the filter is matched against: the name, and the last component of
/// the working directory — the project folder, for a session whose name does
/// not say (`scratch`, or the `session 3` the prompt suggested).
///
/// The last component only, never the path. Every session on a host shares
/// most of its path with every other, and a fuzzy match over `/home/you/...`
/// would admit the whole list for any query that could be spelled out of it.
/// The folder is the one part that tells sessions apart, and the one part a
/// user would think to type.
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
            pressed: None,
            matcher: RefCell::new(Matcher::new(Config::DEFAULT)),
            trail: crate::config::get().effects.move_enabled(),
            after: Starfield::seeded(),
            before: Starfield::seeded(),
            landing: None,
            swap: Swap::seeded(),
            lights: crate::config::get().effects.cursor_enabled(),
            glow: None,
            glint: None,
            sift: crate::config::get().effects.filter_enabled(),
            leaving: None,
            kill: crate::config::get().effects.kill_enabled(),
            strike: None,
            backspace: None,
            back: crate::config::get().effects.back_enabled(),
            create: crate::config::get().effects.create_enabled(),
            room: None,
            note_style: crate::config::get().picker.notes,
            notes: HashMap::new(),
            spin: Duration::ZERO,
        }
    }

    /// What every session has to report, as the latest round of reads found
    /// it (see [`crate::notes::Poller`]). A session missing from it has
    /// nothing to say.
    pub fn set_notes(&mut self, notes: HashMap<String, Note>) {
        self.notes = notes;
    }

    /// How a session's note is shown: `[picker] notes`.
    pub fn note_style(&self) -> NoteStyle {
        self.note_style
    }

    /// For the renderer's tests, which draw each way without a config.
    #[cfg(test)]
    pub(super) fn set_note_style(&mut self, style: NoteStyle) {
        self.note_style = style;
    }

    /// The note session `id` has to show, if it has one and notes are shown
    /// at all.
    pub fn note(&self, id: &str) -> Option<&Note> {
        if self.note_style == NoteStyle::Off {
            return None;
        }
        self.notes.get(id)
    }

    /// Which frame of the spinner is up, round and round: [`note::sign`] and
    /// [`note::column`] take it modulo their frames.
    pub fn spin_frame(&self) -> usize {
        (self.spin.as_millis() / note::SPIN.as_millis()) as usize
    }

    /// Whether a spinner is on screen, beside a busy session that has not
    /// said how far it is: the caller then draws at the spinner's pace, a
    /// frame every [`note::SPIN`], rather than waiting on the keyboard. Not
    /// [`App::animating`], which asks for a frame every sixtieth of a second
    /// and for the terminal to keep up with each, for something that moves
    /// once in five of those.
    pub fn spinning(&self) -> bool {
        self.rows()
            .iter()
            .filter(|r| !r.ghost)
            .filter_map(|r| self.note(&r.session.id))
            .any(note::turns)
    }

    /// Replace the session list, keeping the selection on the same session where
    /// possible — by identity, not position, so a rename that re-sorts the list
    /// does not move the highlight to an unrelated row.
    ///
    /// A fresh listing is the truth about the rows, so whatever was passing
    /// over the old ones — an erased row, a glow, a fade — ends with it.
    pub fn set_sessions(&mut self, sessions: Vec<Session>) {
        let previously = self.selected_id();
        self.backspace = None;
        self.strike = None;
        self.glow = None;
        self.glint = None;
        self.leaving = None;
        self.landing = None;
        self.swap.clear();
        self.room = None;
        self.sessions = sessions;
        self.selected = previously
            .and_then(|id| self.visible().iter().position(|s| s.id == id))
            .unwrap_or_else(|| self.selected.min(self.visible().len().saturating_sub(1)));
    }

    /// Put the cursor on this session, if the visible list still has it.
    ///
    /// How the picker opens on the session the user is attached to rather than
    /// on the first row: `<prefix> Space` leaves the client running, so coming
    /// back to a cursor on row one throws away the one thing the picker already
    /// knew. A session that has since gone — killed elsewhere, or the child
    /// exited — simply leaves the cursor where it was.
    pub fn select_session(&mut self, id: &str) {
        if let Some(at) = self.visible().iter().position(|s| s.id == id) {
            self.selected = at;
        }
    }

    /// Name the session the picker was opened from, so `Esc` can dismiss the
    /// picker back to it.
    ///
    /// Only worth setting when there is still a client behind that session —
    /// `<prefix> Space` leaves one running, a failed attach and an exited child
    /// do not. A session that has gone from the list since is not gone back to
    /// either: killing the session you came from leaves `Esc` with nothing to
    /// do, which is better than dismissing the picker onto a dead client.
    ///
    /// The picker is coming back up over that session, so this is also what
    /// marks its row as the one `Esc` returns to (see [`App::back_to`]).
    pub fn set_came_from(&mut self, id: &str) {
        self.came_from = Some(id.to_string());
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

    /// The half-typed session number, if one is waiting for another digit. The
    /// hint row shows it so the state is never invisible.
    pub fn pending(&self) -> Option<u32> {
        self.pending
    }

    pub fn set_message(&mut self, msg: impl Into<String>) {
        self.message = Some(msg.into());
    }

    /// Whether a session in flight is trailing stars: one is, and the trails
    /// are on. What the renderer draws them for, and what the caller keeps a
    /// faster clock running for, so they move — with the trails off, a move
    /// is drawn still and the clock never speeds up.
    pub fn trailing(&self) -> bool {
        self.trail && matches!(self.mode, Mode::Reorder { .. })
    }

    /// Move the trails behind a session in flight on by `elapsed`. Nothing
    /// happens while none is trailing.
    ///
    /// The clock is the caller's, as it is for a half-typed number: `App`
    /// stays free of it, and a test can step it by exactly the time it wants.
    ///
    /// The spinner beside a busy session moves on too, on screen or not.
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
        if let Some(glint) = &mut self.glint {
            glint.age += elapsed;
            if glint.age >= effects::GLINT {
                self.glint = None;
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

    /// Whether anything on screen is moving, and so whether the caller should
    /// draw at a frame's pace rather than wait on the keyboard: a trail, rows
    /// trading places, a glow, a glint, a fading row, a line being struck or
    /// drawn back, a row being erased, or a row coming up in a gap made for a
    /// new session. A line all the way across is still, and asks for nothing
    /// while the `[y/N]` waits; so is the mark on the session `Esc` goes back
    /// to, which never moves. With every effect off this is only ever false,
    /// and the picker changes on a key and nothing else.
    pub fn animating(&self) -> bool {
        self.trailing()
            || self.glow.is_some()
            || self.glint.is_some()
            || self.leaving.is_some()
            || self.striking()
            || self.erasing()
            || self.landing.is_some()
            || self.swap.moving()
            || self.room().is_some()
    }

    /// The landing under way, if a session has just been put down.
    pub fn landing(&self) -> Option<&Landing> {
        self.landing.as_ref()
    }

    /// What the last moves of a session in flight are still doing to the rows
    /// it traded places with, for the renderer and the effects' post-pass.
    pub fn swap(&self) -> &Swap {
        &self.swap
    }

    /// Put the session in flight down: back to the ordinary list, landing it
    /// first if the trail is on — the landing is how a trail ends. Every way of placing one comes through here —
    /// the keys and the mouse's release — so none of them can forget the
    /// landing. `Esc` does not: a cancelled move did not land anywhere.
    fn put_down(&mut self) {
        self.mode = Mode::Normal;
        if self.trail {
            self.landing = Some(Landing::seeded());
        }
    }

    /// The row the cursor last left, by session id, with how far through its
    /// glow it is, `0..1`, or `None` once it has settled.
    pub fn glow(&self) -> Option<(&str, f32)> {
        let length = effects::AFTERGLOW.as_secs_f32();
        self.glow
            .as_ref()
            .map(|g| (g.id.as_str(), g.age.as_secs_f32() / length))
    }

    /// The row the cursor has just landed on, by session id, with how far the
    /// glint is across it, `0..1`, or `None` once it has crossed.
    pub fn glint(&self) -> Option<(&str, f32)> {
        let length = effects::GLINT.as_secs_f32();
        self.glint
            .as_ref()
            .map(|g| (g.id.as_str(), g.age.as_secs_f32() / length))
    }

    /// How far through their fade the rows the last filter keystroke dropped
    /// are, `0..1`, or `None` when none are on screen.
    pub fn leaving(&self) -> Option<f32> {
        self.leaving
            .as_ref()
            .map(|l| l.age.as_secs_f32() / effects::SIFT.as_secs_f32())
    }

    /// The session a `[y/N]` is asking about, by id, with how much of the line
    /// through its name is drawn, `0..=1`: on its way across while the
    /// question is up, on its way back once it is answered no.
    pub fn strike(&self) -> Option<(&str, f32)> {
        self.strike.as_ref().map(|s| (s.id.as_str(), s.drawn))
    }

    /// Whether the line through a name is on its way across or back. All the
    /// way across, it is still.
    fn striking(&self) -> bool {
        self.strike
            .as_ref()
            .is_some_and(|s| s.back || s.drawn < 1.0)
    }

    /// Start the row `id` erasing, for a kill just confirmed. Says whether
    /// there is anything to watch: the effect is on, and the row is on screen.
    pub fn start_backspace(&mut self, id: &str) -> bool {
        if !self.kill || !self.visible().iter().any(|s| s.id == id) {
            return false;
        }
        self.backspace = Some((id.to_string(), Backspace::new()));
        true
    }

    /// The row being erased and its backspace, for the renderer. Still here
    /// once it is done — the row stays empty until a listing replaces it.
    pub fn backspace(&self) -> Option<(&str, &Backspace)> {
        self.backspace.as_ref().map(|(id, b)| (id.as_str(), b))
    }

    /// Whether a row is still being erased.
    pub fn erasing(&self) -> bool {
        self.backspace.as_ref().is_some_and(|(_, b)| !b.done())
    }

    /// Open a gap after the highlighted row for the session `c` is about to
    /// create, and stand it there, named `name`: what the prompt will offer.
    /// Says whether there is anything to watch — whether the effect is on.
    ///
    /// The gap goes where [`App::placement_of`] will put the session once it
    /// exists, and the rows show the numbers it will deal out: the new row the
    /// number of the row it pushes down, each row below it the next, and the
    /// last row the number one past every row's. With nothing highlighted, or
    /// the last row, the gap is at the end and only the new row's number is
    /// new. So the screen says, before the prompt asks anything, where the
    /// session will be and what it will be called.
    ///
    /// Whatever else was passing — a glow, a glint, a fading row, a line
    /// being drawn back off a name a `[y/N]` was declined for — ends here:
    /// the picker is on its way out, and the gap is all that should be moving.
    /// The gap stays until [`App::close_room`] or a fresh listing, so the
    /// picker can be drawn closing in onto it.
    pub fn make_room(&mut self, name: &str) -> bool {
        if !self.create {
            return false;
        }
        let (at, num) = self.slot();
        let last = self.last_number();
        let mut ghost = Session::new(GHOST_ID.to_string(), name.to_string(), 0, num);
        ghost.state.num = num;
        self.glow = None;
        self.glint = None;
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

    /// The number a session created now will be given, and so the one its
    /// default name is numbered after (see [`super::prompt::default_name`]):
    /// that of the row after the highlighted one, which it pushes down, or one
    /// past every row's when it goes at the end. What [`App::make_room`]
    /// shows on the row in the gap, and what [`App::placement_of`] stores.
    pub fn new_number(&self) -> u32 {
        self.slot().1
    }

    /// Where among the visible rows a session created now goes — after the
    /// highlighted row, or first in an empty list — and the number it will be
    /// given there. See [`App::new_number`].
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

    /// The number one past every row's: what a session appended to the list
    /// would be given.
    fn last_number(&self) -> u32 {
        self.sessions.iter().map(|s| s.state.num).max().unwrap_or(0) + 1
    }

    /// Close the gap [`App::make_room`] opened: the prompt has gone, and the
    /// rows go back to where they were — or, for a session that was created,
    /// to the listing that has it.
    pub fn close_room(&mut self) {
        self.room = None;
    }

    /// How far the row in the gap has come up out of the background, `0..1`,
    /// while it is still coming; `None` once it is all the way up, or there is
    /// no gap.
    pub fn room(&self) -> Option<f32> {
        let room = self.room.as_ref()?;
        let progress = room.age.as_secs_f32() / effects::ROOM.as_secs_f32();
        (progress < 1.0).then_some(progress)
    }

    /// Whether there is a gap open, coming up or all the way up.
    pub fn has_room(&self) -> bool {
        self.room.is_some()
    }

    /// The session the picker came back up over, by id — the one `Esc` goes
    /// back to — for the renderer to mark its row (see `draw::BACK`). `None`
    /// when the picker was opened from no session, or with `[effects.back]`
    /// off. A session that has gone since has no row, and so no mark: whether
    /// it is listed is the renderer's to find, as it finds every other row.
    pub fn back_to(&self) -> Option<&str> {
        self.came_from.as_deref().filter(|_| self.back)
    }

    /// The trail off the end of the name, for the renderer.
    pub fn trail_after(&self) -> &Starfield {
        &self.after
    }

    /// The trail off the other end of the row, for the renderer.
    pub fn trail_before(&self) -> &Starfield {
        &self.before
    }

    /// Turn the trails on or off whatever the config says, for a test that
    /// needs one or the other.
    #[cfg(test)]
    pub(super) fn set_trail(&mut self, on: bool) {
        self.trail = on;
    }

    /// Turn the picker's own effects — the afterglow and the glint, the
    /// filter's fade, the kill's strike and backspace, the mark on the session
    /// come back from — on or off whatever the config says, for a test that
    /// needs one or the other.
    #[cfg(test)]
    pub(super) fn set_effects(&mut self, on: bool) {
        self.lights = on;
        self.sift = on;
        self.kill = on;
        self.back = on;
        self.create = on;
        if !on {
            self.glow = None;
            self.glint = None;
            self.leaving = None;
            self.strike = None;
            self.backspace = None;
        }
    }

    /// Sessions matching the current filter, in display order.
    ///
    /// The match is fuzzy — `asv` finds `api-server` — and is the same scorer,
    /// with the same case and accent rules, that ranks the create prompt's
    /// directory completions ([`super::complete`]): case-insensitive unless the
    /// query has a capital in it, and `Pattern::parse`'s syntax throughout, so
    /// a query of two words wants both.
    ///
    /// Fuzzy in what it *admits*, and nothing else. A completion menu sorts by
    /// score because its rows are interchangeable; these are not. A row's number
    /// is its position in the list, `<prefix> 3` and the digit keys name it by
    /// that number, and a reorder under a filter deals the visible numbers back
    /// out down the visible rows — all of which assume the filtered view is the
    /// full list with rows removed. So the score is a yes or a no here, and the
    /// order is the list's own.
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

    /// The rows the list draws: the visible ones and, for a moment after a
    /// filter keystroke narrowed the list, the ones it dropped, where they
    /// stood; or, while [`App::make_room`] has a gap open, the visible ones
    /// with the session about to be created in it. In the list's own order.
    ///
    /// Only the drawing sees the dropped rows and the new one. Keys, numbers
    /// and the mouse go on resolving against [`App::visible`], so a row on its
    /// way out, or not yet in, can be seen and not touched.
    pub fn rows(&self) -> Vec<Row<'_>> {
        let visible = self.visible();
        if let Some(room) = &self.room {
            // Nothing is leaving under a gap: making room ended that.
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

    /// The selection's index into [`App::rows`]. The same as
    /// [`App::selected_index`] whenever no row is leaving.
    pub fn selected_row(&self) -> usize {
        let Some(id) = self.selected_id() else {
            return 0;
        };
        self.rows()
            .iter()
            .position(|r| !r.leaving && r.session.id == id)
            .unwrap_or(0)
    }

    /// The selected session's id, if anything is selected.
    pub fn selected_row_id(&self) -> Option<&str> {
        self.selected_session().map(|s| s.id.as_str())
    }

    /// Which characters of `name` the filter matched, as `char` indices in
    /// order — what the renderer underlines. Empty with no filter, or when the
    /// session matched on its folder rather than its name.
    ///
    /// The matcher counts grapheme clusters, which are `char`s for every name
    /// without combining marks; one with them can have its underline land a
    /// letter off, which is the price of not taking a dependency for it.
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

    /// Every row the picker is holding, in the order it is holding them.
    ///
    /// For a caller that carries the listing on rather than asking for another
    /// — see [`crate::ui::Outcome::Attach`]. `visible` is the filtered view and
    /// answers a different question.
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    /// The session with this id, from the list the picker is showing. What a
    /// [`Request`] carrying an id resolves against, so acting on a row costs no
    /// round trip beyond the action itself.
    pub fn session(&self, id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    fn selected_session(&self) -> Option<&Session> {
        self.visible().get(self.selected).copied()
    }

    /// A request naming whatever is selected, or nothing when the list is empty
    /// — every key that acts on a row has to answer for both.
    fn on_selection(&self, request: impl FnOnce(String) -> Request) -> Request {
        self.selected_id().map_or(Request::None, request)
    }

    fn selected_id(&self) -> Option<String> {
        self.selected_session().map(|s| s.id.clone())
    }

    fn session_name(&self, id: &str) -> String {
        self.session(id).map(|s| s.name.clone()).unwrap_or_default()
    }

    /// The row `delta` away from the selection, wrapping at both ends.
    ///
    /// Shared by the cursor and by a session being moved, so "the arrows wrap"
    /// stays one fact about the picker rather than two implementations that
    /// could drift apart.
    fn wrapped(&self, delta: isize) -> usize {
        let len = self.visible().len();
        if len == 0 {
            return 0;
        }
        let len = len as isize;
        (((self.selected as isize + delta) % len + len) % len) as usize
    }

    /// Move the selection, wrapping at both ends.
    fn move_by(&mut self, delta: isize) {
        self.selected = self.wrapped(delta);
    }

    /// The row `delta` away from the selection, stopping at both ends.
    ///
    /// For the wheel, which unlike the keys does not wrap: a list that jumps
    /// from its last row to its first under a scrolling finger reads as the
    /// list having lost its place, where a key pressed once too often reads as
    /// the key having done what it always does.
    fn clamped(&self, delta: isize) -> usize {
        let len = self.visible().len();
        if len == 0 {
            return 0;
        }
        (self.selected as isize + delta).clamp(0, len as isize - 1) as usize
    }

    /// Where the session `new_id` — just created from this picker — belongs, as
    /// the arrangement to store: every visible row and the new session, in
    /// their new order, each paired with the number it should now hold.
    ///
    /// Just after the highlighted row, so a session lands next to the one the
    /// user was looking at when they made it rather than at the bottom of the
    /// list, and everything after it moves down one.
    ///
    /// The new session is treated as though it had been appended and then moved
    /// up to that row, which is exactly what storing it this way does: it was
    /// created last, and this moves it. So its number is the one past every
    /// row's, and the visible numbers plus that one are dealt back out down the
    /// new order — the same deal a reorder makes (see
    /// [`App::shift_grabbed_to`]), with the same result under a filter: the
    /// hidden rows keep their numbers and stay out of the payload.
    ///
    /// Empty when there is nothing to write: no row was highlighted, or the
    /// last one was, and after it is where a new session goes anyway.
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

    /// Pick the session `id` up — the space bar and a drag both come here — with
    /// the arrangement as it stands, for `Esc` to put back, and fresh trails
    /// off both ends of it, which catch rather than appear whole (see
    /// [`Starfield::ignite`]).
    fn pick_up(&mut self, id: String) {
        let was = self.snapshot();
        // A grab holds an arrangement of the visible rows; nothing else may be
        // on screen while it does, the last grab's swap included.
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

    /// Move the grabbed row to `to`, sliding everything between it and where it
    /// came from one place the other way — and, with the trail on, start the
    /// rows that traded places stepping aside (see [`super::swap`]).
    ///
    /// The numbers stay where they are on the screen; it is the sessions that
    /// move between them. The list arrives from `finish_listing` sorted by
    /// `state.num`, so dealing those same numbers back out down the new row
    /// order leaves the column reading exactly as it did — which is what lets
    /// `visible`, `scroll_offset` and the renderer stay unaware that a move
    /// happened at all, and what keeps the numbers distinct and gap-preserving
    /// however long the drag runs.
    ///
    /// One re-assignment rather than a loop of adjacent swaps: a
    /// `while self.selected != to` around a mover that clamps by returning is an
    /// unguarded loop, and no key can interrupt one.
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
            // The rows passed, nearest the session's new row first, and the
            // seam it last crossed: its top edge going down, its bottom edge
            // going up.
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

        // Rows the filter is hiding keep their numbers, so the whole vector is
        // still sorted by number once the visible ones have been dealt.
        self.sessions.sort_by_key(|s| s.state.num);
        self.selected = to;
    }

    /// Put the numbers back exactly as they were when the grab started, and the
    /// cursor back on the session that was grabbed.
    ///
    /// By id rather than by the row it came from, so it stays right even if the
    /// list ever moved underneath.
    ///
    /// What the moves were still drawing goes too: the rows it was drawn over
    /// are back where they were, and a cancelled move did not happen.
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

    /// Show the kill confirmation. Killing is unconditional, so the `[y/N]` is
    /// the whole safeguard — nothing is asked of the session itself.
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

    /// The `[y/N]` answered no, by a key or a click: back to the list, and the
    /// line through the name drawn back off from wherever it had got to.
    fn decline(&mut self) {
        self.mode = Mode::Normal;
        if let Some(strike) = &mut self.strike {
            strike.back = true;
        }
    }

    /// Handle one key. Returns whatever the caller now has to do.
    pub fn on_key(&mut self, key: Key) -> Request {
        // A stale message must not linger over an unrelated action.
        self.message = None;
        // Nor the rows the last keystroke dropped: the list is the query's
        // again, so a fast typist never waits on rows ruled out a letter ago.
        self.leaving = None;
        let was = self.cursor();
        let visible = self.visible_ids();

        let request = match &self.mode {
            Mode::Normal => self.on_key_normal(key),
            Mode::Filter => self.on_key_filter(key),
            Mode::Confirm { .. } => self.on_key_confirm(key),
            Mode::Reorder { .. } => self.on_key_reorder(key),
        };
        self.glow_from(was);
        self.note_dropped(visible);
        request
    }

    /// The session under the cursor, where the cursor is one that glows when
    /// it leaves a row: in the list as it is browsed or filtered, not on a
    /// session in flight or under a `[y/N]`.
    fn cursor(&self) -> Option<String> {
        matches!(self.mode, Mode::Normal | Mode::Filter)
            .then(|| self.selected_id())
            .flatten()
    }

    /// If the cursor has moved off the session `was`, start that row glowing,
    /// and a glint across the row it moved onto. Whatever row was glowing
    /// before stops: only the row just left glows.
    fn glow_from(&mut self, was: Option<String>) {
        if !self.lights {
            return;
        }
        let now = self.cursor();
        // A session picked up or a `[y/N]` takes the bar over, and the glint
        // goes with it rather than carrying on across the question, or coming
        // back half done once it is answered.
        if now.is_none() {
            self.glint = None;
        }
        let (Some(was), Some(now)) = (was, now) else {
            return;
        };
        // A row the filter just took is leaving, not glowing.
        if was == now || !self.visible().iter().any(|s| s.id == was) {
            return;
        }
        self.glow = Some(Glow {
            id: was,
            age: Duration::ZERO,
        });
        self.glint = Some(Glow {
            id: now,
            age: Duration::ZERO,
        });
    }

    fn visible_ids(&self) -> Vec<String> {
        self.visible().iter().map(|s| s.id.clone()).collect()
    }

    /// Keep the rows that were visible and are not now on screen a moment
    /// longer, fading, so the eye sees which ones a filter keystroke took.
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

    /// Handle a digit typed in the picker.
    ///
    /// The numbers are resolved against the *visible* list, not the whole one:
    /// a filter can still be applied in normal mode, and a number belonging to a
    /// row the filter has hidden must not silently attach. You can press what
    /// you can see.
    ///
    /// Because `App` holds the sessions, most keystrokes need no timer at all —
    /// a digit that no longer number could extend is acted on at once. Only a
    /// genuinely ambiguous one (sessions 1 and 12 both present) waits, and the
    /// caller resolves that with [`App::resolve_pending`].
    fn on_digit(&mut self, d: u32) -> Request {
        let Some(n) = self
            .pending
            .take()
            .map(|p| p.saturating_mul(10).saturating_add(d))
        else {
            // A session number never starts with 0, so a leading one is not the
            // beginning of anything.
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
        // Could another digit still name a different session?
        let extendable = self
            .visible()
            .iter()
            .any(|s| s.state.num > n && s.state.num / 10 == n);

        match (exact, extendable) {
            // Ambiguous: 1 is a session but so is 12. Only this waits.
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

    /// Called by the driver once `keys.timeout_ms` has passed with a
    /// number half-typed: settle for the session it already names.
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

    /// Leave the picker for the session it was opened from.
    ///
    /// Nothing to go back to — opened from no session, or from one that has
    /// since gone — is not an error and not a quit: the picker stays, because
    /// dismissing it would leave the user looking at a terminal with nothing in
    /// it. `q` is how you leave for good.
    fn dismiss(&self) -> Request {
        match &self.came_from {
            Some(id) if self.session(id).is_some() => Request::Attach(id.clone()),
            _ => Request::None,
        }
    }

    /// Handle one mouse gesture. Returns whatever the caller now has to do.
    ///
    /// The rows a gesture names are indices into [`App::visible`], resolved by
    /// the caller from where the pointer was against the screen it last drew —
    /// so the picker knows rows, never coordinates, and stays as testable as it
    /// is for keys.
    ///
    /// The highlight follows the pointer: hovering over a row selects it, so
    /// the keyboard and the mouse share one cursor. A press selects the row
    /// under it; a press on the row already selected arms its release to
    /// attach. With the pointer's row always selected that makes a single
    /// click open the session it is over — and on a terminal that reports no
    /// motion (one without any-event tracking), the same two rules read as
    /// "click to select, click again to open", with no double-click clock
    /// either way. A drag with the button held picks the row up — the
    /// same [`Mode::Reorder`] the space bar enters — carries it, and the
    /// release places it, with the same request `Enter` would make. The wheel
    /// moves the selection, or the row in flight, one row at a time and stops
    /// at the ends.
    ///
    /// A press or a wheel step ends what a key would end: a stale message and a
    /// half-typed number. A hover, a drag or a release ends nothing, since none
    /// of them is something the user did on purpose *to* the picker.
    pub fn on_mouse(&mut self, mouse: Mouse) -> Request {
        if matches!(mouse, Mouse::Press(_) | Mouse::ScrollUp | Mouse::ScrollDown) {
            self.message = None;
            self.pending = None;
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
                // Only an explicit `y` kills, and a click is not one; but a
                // click is a deliberate act, so it declines the question the
                // way any key but `y` does. The wheel is not deliberate enough
                // to dismiss a `[y/N]`.
                if matches!(mouse, Mouse::Press(_)) {
                    self.decline();
                    self.pressed = None;
                }
                Request::None
            }
            Mode::Reorder { .. } => self.on_mouse_reorder(mouse),
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
            // Off the list, the cursor stays on the last row it was over.
            Mouse::Hover(_) => Request::None,
            // Dragged off the row it went down on: the row comes with it. Only
            // from a press that landed on a row — a drag that started on the
            // blank space must not pick up whatever happens to be selected.
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
                // As `Enter` does from the filter: the query stays applied,
                // the prompt closes.
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

    /// The mouse over a session in flight, whether a drag or the space bar
    /// picked it up: a press or a drag carries it to the row under the
    /// pointer, and a release places it — "click where you want it" — with the
    /// request `Enter` would make. The pointer merely passing over rows
    /// carries nothing: a row in flight moves on a button, the wheel or a key.
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
        // Any key that is not a digit ends a half-typed number rather than
        // letting it linger into an unrelated keystroke. Whether there was one
        // is what tells an `Esc` aimed at the number from one aimed at the
        // picker.
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
            // Guarded the way `x` is: without it, Space on an empty list would
            // enter the mode over the "no sessions" screen with nothing to move.
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
            // One layer at a time, innermost first: a half-typed number, then
            // a filter narrowing the list, then the picker itself. Each of the
            // first two is something the user put there and can see, and would
            // be thrown away unremarked by an Esc that left outright.
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
            Key::Char('q') | Key::CtrlC => Request::Quit,
            _ => Request::None,
        }
    }

    fn on_key_filter(&mut self, key: Key) -> Request {
        match key {
            Key::Char(c) => {
                self.filter.push(c);
                // The list narrows under the cursor, so the selection has to be
                // pulled back into range or it points past the end.
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
            Key::CtrlC => Request::Quit,
            _ => Request::None,
        }
    }

    /// Handle a key while a session is in flight.
    ///
    /// Anything not named here does nothing **and keeps the grab** — deliberately
    /// unlike [`Mode::Confirm`], where everything but `y` dismisses. A `[y/N]` is
    /// one question; a reorder is a multi-key edit holding an arrangement nothing
    /// has written yet, and a stray keystroke must not silently decide whether it
    /// is kept or thrown away. It also means `/` cannot re-filter under a grabbed
    /// session, so the snapshot describes the same rows for the whole edit.
    ///
    /// `Enter` places, and is the only key the hint row names for it. It is the
    /// confirm key everywhere else a mode is open, so it is the one to advertise;
    /// the older reading — that Enter means "attach" and so must not commit an
    /// arrangement — cost more than the ambiguity was worth, since nothing
    /// attaches while a session is in flight.
    ///
    /// Space places as well, unadvertised. It is the key that picked the session
    /// up, so a hand that found it once finds it again, and dropping the binding
    /// to match the row would punish exactly that habit. It also makes an
    /// autorepeat or a nervous double-tap grab-then-place on the spot, which
    /// moves nothing and so writes nothing — it ends back in normal mode rather
    /// than holding an edit the screen barely shows.
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
                    // Picked up and put straight back down, or moved and moved
                    // back: nothing to write and nothing to re-list.
                    return Request::None;
                }
                Request::Reorder(now)
            }
            Key::Esc => {
                self.restore(&was, &id);
                self.mode = Mode::Normal;
                Request::None
            }
            Key::CtrlC => Request::Quit,
            _ => Request::None,
        }
    }

    fn on_key_confirm(&mut self, key: Key) -> Request {
        let Mode::Confirm { id, .. } = &self.mode else {
            return Request::None;
        };
        let id = id.clone();
        match key {
            // Only an explicit `y` kills. Everything else, including Enter,
            // declines — that is what `[y/N]` promises.
            // The row is erased next, and the line goes with it.
            Key::Char('y') | Key::Char('Y') => {
                self.mode = Mode::Normal;
                self.strike = None;
                Request::Kill(id)
            }
            Key::CtrlC => Request::Quit,
            _ => {
                self.decline();
                Request::None
            }
        }
    }
}

/// A key press, decoupled from crossterm so the state machine can be tested
/// without constructing backend types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Up,
    Down,
    /// Only the prompt binds these three: `Left` and `Right` move within a
    /// field, and `Tab` completes where there is something to complete. The
    /// picker's handlers ignore them, as they do any other key they do not name.
    Left,
    Right,
    Tab,
    Home,
    End,
    CtrlC,
    /// Ctrl-N — the readline-style companion to `j`/Down.
    CtrlN,
    /// Ctrl-P — the readline-style companion to `k`/Up.
    CtrlP,
    Other,
}

/// A mouse gesture, with the visible row it landed on already resolved by the
/// caller — see [`App::on_mouse`]. Decoupled from crossterm, as [`Key`] is, and
/// from the screen's geometry too.
///
/// Only the left button and the wheel: nothing here has a use for the other
/// buttons, and the caller drops them before they get this far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mouse {
    /// The pointer moved with no button held, and is now over this row, or
    /// over nothing.
    Hover(Option<usize>),
    /// The left button went down on this row, or on nothing.
    Press(Option<usize>),
    /// The pointer moved with the left button held, and is now over this row,
    /// or over nothing.
    Drag(Option<usize>),
    /// The left button came up.
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

    /// The three ways to move are one behaviour, wrapping included — previously
    /// only `j`/`k` was checked for the wrap.
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
        // In Filter mode every printable key is query text, so Ctrl-N/P are the
        // only letters that can still move the cursor.
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

    /// A capital in the query asks for one in the name, as it does in the create
    /// prompt's completion. Lower-case queries stay as forgiving as ever.
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

    /// Two words are two requirements, both of which the same row has to meet.
    #[test]
    fn a_query_of_two_words_wants_both() {
        let a = filtering(&["api server", "api client", "web server"], "api ser");
        assert_eq!(names(&a), ["api server"]);
    }

    /// The rows a fuzzy filter admits keep the list's order and their numbers;
    /// nothing is sorted by how well it matched.
    #[test]
    fn a_filtered_list_keeps_its_order_rather_than_ranking_by_score() {
        // `ts` is a far better match for "ts" than for "notes-tests-server",
        // and comes after it in the list all the same.
        let a = filtering(&["notes-tests-server", "ts", "api"], "ts");
        assert_eq!(names(&a), ["notes-tests-server", "ts"]);
        let numbers: Vec<u32> = a.visible().iter().map(|s| s.state.num).collect();
        assert_eq!(numbers, [1, 2], "the numbers travel with the rows");
    }

    /// A session is where it runs as much as what it is called: `scratch` in
    /// `~/work/billing` is found by `billing`. Only the last component takes
    /// part, though — see `haystacks`.
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

    /// The list shrinks under the cursor as the query grows; the selection must
    /// not be left pointing past the end.
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

    /// `<prefix> Space` leaves the client running, so the picker is a layer over
    /// a session rather than a replacement for it, and `Esc` is the way back
    /// down — same as everywhere else it means "never mind".
    #[test]
    fn esc_dismisses_the_picker_back_to_the_session_it_came_from() {
        let mut a = app(&["one", "two", "three"]);
        a.set_came_from("id000001");
        a.on_key(Key::Char('G')); // the cursor need not be on it
        assert_eq!(a.on_key(Key::Esc), Request::Attach("id000001".into()));
    }

    /// The first screen of the program is the picker, with nothing behind it.
    /// Dismissing it would leave the user looking at an empty terminal, so `Esc`
    /// does nothing and `q` is still the way out.
    #[test]
    fn esc_does_nothing_when_the_picker_was_not_opened_from_a_session() {
        let mut a = app(&["one", "two"]);
        assert_eq!(a.on_key(Key::Esc), Request::None);
    }

    /// Killing the session you came from takes the client with it, so there is
    /// nothing left to go back to — and attaching to it would be attaching to
    /// something that is gone.
    #[test]
    fn esc_does_not_go_back_to_a_session_that_has_since_been_killed() {
        let mut a = app(&["one", "two"]);
        a.set_came_from("id000000");
        a.on_key(Key::Char('x'));
        assert_eq!(a.on_key(Key::Char('y')), Request::Kill("id000000".into()));
        a.set_sessions(vec![]);

        assert_eq!(a.on_key(Key::Esc), Request::None);
    }

    /// Innermost first. Both of these are things the user typed and can see, and
    /// an `Esc` that left outright would throw them away without saying so.
    #[test]
    fn esc_clears_what_is_half_typed_before_it_dismisses_anything() {
        let mut a = app(&["one", "two"]);
        a.set_came_from("id000000");

        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('o'));
        a.on_key(Key::Enter); // filter applied, back in normal mode
        assert_eq!(a.on_key(Key::Esc), Request::None, "the filter goes first");
        assert_eq!(a.filter(), "");

        // Sessions 1 and 12 both present, so the digit is genuinely half-typed.
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

    /// Naming a session happens on the prompt's own screen, so the picker's job
    /// is only to say it was asked for — no mode, no buffer, no name.
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

    /// With nothing selected there is nothing to rename, and `r` must not ask
    /// the caller to open a prompt for a session that does not exist.
    #[test]
    fn r_does_nothing_on_an_empty_list() {
        let mut a = app(&[]);
        assert_eq!(a.on_key(Key::Char('r')), Request::None);
    }

    /// `x` opens the confirm immediately and consults nothing.
    ///
    /// Killing is unconditional, so there is no unsaved-buffer count to fetch
    /// and no round trip to the session. That also means the picker cannot
    /// stall behind a busy session just to draw a prompt.
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

    /// `[y/N]` means the default is No. Only `y` may destroy a session.
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

    /// While the filter is open, ordinary keys are text, not commands: a query
    /// containing "x" must not trigger a kill, and one containing "c" or "r"
    /// must not open the naming prompt.
    ///
    /// Filter is the picker's only text entry now that naming has its own
    /// screen, so this is where the invariant lives.
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

    /// One session, numbered as a listing would have numbered it.
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

    /// Nine or fewer sessions means no digit can be extended, so none of them
    /// ever waits.
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

    /// A number never starts with zero.
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

        // 1 is ambiguous while 10, 11 and 12 exist, so it waits.
        assert_eq!(a.on_key(Key::Char('1')), Request::None);
        assert_eq!(a.pending(), Some(1));

        assert_eq!(
            a.on_key(Key::Char('2')),
            Request::Attach("id000011".into()),
            "12 is the twelfth session"
        );
        assert_eq!(a.pending(), None);
    }

    /// The ambiguous case is the only one that waits, and the caller settles it.
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

    /// `11` shares its first digit with 1, 10 and 12, so the first keystroke
    /// cannot decide anything and the second one settles it.
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

    /// Numbers address the rows on screen. A session the filter has hidden must
    /// not be reachable by a keystroke that names nothing visible.
    #[test]
    fn a_digit_only_reaches_a_session_the_filter_still_shows() {
        let mut a = app(&["alpha", "beta", "gamma"]);
        a.on_key(Key::Char('/'));
        for c in "beta".chars() {
            a.on_key(Key::Char(c));
        }
        a.on_key(Key::Enter); // back to normal mode, filter still applied

        // Only "beta" is visible, and it is number 2.
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

        // "aaa" is killed and something else takes its number, so "bbb" moves
        // to row 0; the highlight should travel with it rather than staying put.
        a.set_sessions(vec![
            session("id000001", "bbb", 1),
            session("id000002", "ccc", 2),
        ]);
        assert_eq!(a.selected_index(), 0, "it moved up with the session");
        assert_eq!(a.selected_session().expect("selected").id, "id000001");
        assert_eq!(a.selected_session().expect("selected").name, "bbb");
    }

    /// A rename no longer reorders the list — sorting is by number — but the
    /// selection must still be anchored to the session rather than the row.
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

    /// The arrangement on screen: names in display order, and the numbers
    /// beside them.
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

    /// Without the guard, Space would enter the mode over the "no sessions"
    /// screen, with a hint row offering to move something that is not there.
    #[test]
    fn space_on_an_empty_list_picks_nothing_up() {
        let mut a = app(&[]);
        assert_eq!(a.on_key(Key::Char(' ')), Request::None);
        assert_eq!(*a.mode(), Mode::Normal);
    }

    /// All three families carry the grabbed row, as all three move the cursor,
    /// and they wrap at both ends the same way.
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

    /// The invariant the whole design rests on. A move exchanges which session
    /// holds a number, never what the numbers are — so the column stays exactly
    /// as it was, gaps included, and no number is invented, lost, duplicated or
    /// zeroed however long the drag runs.
    #[test]
    fn the_numbers_on_screen_do_not_change_while_a_session_is_moving() {
        // Gapped on purpose, though a listing is dense: the picker is handed
        // whatever numbers it is handed, and a drag must deal those same ones
        // back out rather than renumber from the row order it happens to see.
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

    /// Every visible row, not only the ones whose number changed.
    ///
    /// A diff would be wrong: what is persisted is the *stored* number, and a
    /// row can already be showing a number `finish_listing` derived for it
    /// rather than one it stores. Skipping such a row as "unchanged" leaves it
    /// storing a number that collides with one this batch just wrote.
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

    /// The confirm key everywhere else a mode is open, so it confirms here too.
    /// It cannot mean "attach" while a session is in flight — nothing attaches
    /// from a grab — so the only reading left is the one the hint row offers.
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

        // And the same quiet exit when the arrangement came back unchanged.
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

    /// Unlike the kill confirm, where anything but `y` dismisses. A `[y/N]` is
    /// one question; this is a multi-key edit holding an arrangement nothing has
    /// written yet, and a stray keystroke must not decide its fate. `Enter` is
    /// not in the list: it places, alongside Space, and the hint row says so.
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

    /// Moving obeys the same rule digits do — "you can press what you can see".
    /// A row jumps over what the filter hides, and the hidden rows keep their
    /// numbers and stay out of the payload, so nothing writes them.
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

    /// Coming back from a session, the cursor is on the session that was
    /// attached — not on the first row.
    #[test]
    fn the_cursor_starts_on_the_session_it_is_told_to_focus() {
        let mut a = app(&["one", "two", "three"]);
        a.select_session("id000002");
        assert_eq!(a.selected_index(), 2);
        assert_eq!(a.selected_session().map(|s| s.name.as_str()), Some("three"));
    }

    /// A session that ended while it was attached, or was killed from
    /// elsewhere, is not in the listing the picker just read.
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

    /// The focused row keeps the cursor across a rename or a kill, which
    /// re-list: `set_sessions` restores by identity, and the id it restores is
    /// the focused one.
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

    /// A press and a release on the same row.
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

    /// Click to select, click again to open: the second click is the one that
    /// attaches, and it is told apart from the first by where the cursor was,
    /// not by a clock.
    #[test]
    fn a_click_on_the_selected_row_attaches() {
        let mut a = app(&["one", "two", "three"]);
        click(&mut a, 1);
        assert_eq!(click(&mut a, 1), Request::Attach("id000001".into()));

        // The cursor put there by a key counts the same as one put by a click.
        let mut b = app(&["one", "two", "three"]);
        b.on_key(Key::Char('j'));
        assert_eq!(click(&mut b, 1), Request::Attach("id000001".into()));
    }

    /// The attach is decided on the release, so a press that selected a row
    /// and a release that follows it are one click, not a click and a half.
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
        // A row the caller should never name, but the picker guards it anyway.
        assert_eq!(a.on_mouse(Mouse::Press(Some(7))), Request::None);
        assert_eq!(a.on_mouse(Mouse::Release), Request::None);
        assert_eq!(a.selected_index(), 1);
    }

    /// From the filter, a click accepts the query as `Enter` does: the list
    /// stays narrowed and the prompt closes on the attach.
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

    /// What a key ends, a press or a wheel step ends too: neither should leave
    /// a stale message over the row it just moved to, or a number waiting for
    /// a digit the mouse will never type.
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
        // A drag or a release is not something done to the picker on purpose.
        for gesture in [
            Mouse::Hover(Some(1)),
            Mouse::Hover(None),
            Mouse::Drag(Some(1)),
            Mouse::Drag(None),
            Mouse::Release,
        ] {
            let mut a = app(&refs);
            // The key first: a keypress ends a message, and the message is
            // what the gesture is being asked to leave alone.
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

    /// The cursor stays on the last row the pointer was over — it does not
    /// snap back, and it does not chase the pointer off the list.
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

    /// With the highlight following the pointer, the row a click lands on is
    /// already the selection, so one click opens it.
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

    /// A row in flight moves on a button, the wheel or a key — not because the
    /// pointer happened to pass over the list.
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

    /// A drag that starts on the selected row is a move, not an attach: the
    /// release that would have opened the session places it instead.
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

    /// Dragging within the row it went down on, or off the list altogether,
    /// picks nothing up — the pointer wobbles.
    #[test]
    fn a_drag_that_leaves_no_row_grabs_nothing() {
        let mut a = app(&["aaa", "bbb"]);
        a.on_mouse(Mouse::Press(Some(1)));
        a.on_mouse(Mouse::Drag(Some(1)));
        a.on_mouse(Mouse::Drag(None));
        assert_eq!(*a.mode(), Mode::Normal);
        assert_eq!(a.on_mouse(Mouse::Release), Request::None);

        // And a drag that began on the blank space must not pick up whatever
        // happened to be selected.
        let mut b = app(&["aaa", "bbb"]);
        b.on_mouse(Mouse::Press(None));
        b.on_mouse(Mouse::Drag(Some(1)));
        assert_eq!(*b.mode(), Mode::Normal);
        assert_eq!(arrangement(&b).0, ["aaa", "bbb"]);
    }

    /// Once a row is in flight, the pointer leaving the list does not drop it:
    /// it stays where it last was until the button comes up.
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

    /// The space bar picked it up; a click puts it down where the click is.
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

    /// A click on the grabbed row itself, or beside the list, is a release
    /// where it already is: picked up and put down.
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

    /// A drag can start from the filter, where the space bar cannot: the
    /// hint row already knows how to show a reorder with a query applied.
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

    /// `[y/N]` means the default is No: a click is any key but `y`, and the
    /// wheel is not even that.
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

    /// The trails run while a session is in flight, however it was picked up,
    /// and stop with the move — placed or put back.
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

    /// The trails are laid out when a session is picked up, and only then: the
    /// rest of the time nothing moves them, and nothing draws them.
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

    /// `[effects.move] enabled = false`: a move is still a move, but nothing trails
    /// it and nothing asks for the faster clock.
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

    /// On unless the config says otherwise — what an `App` built in a test, with
    /// no config loaded, reads.
    #[test]
    fn the_trail_follows_the_config() {
        assert!(crate::config::get().effects.move_enabled());
        assert!(app(&["aaa"]).trail);
    }

    // --- the swap -----------------------------------------------------------

    /// A move starts the rows that traded places stepping aside, the carried
    /// session one way and the one it passed the other, and the picker draws at
    /// a frame's pace until the swap is over — however the move was made.
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

    /// A jump passes every row between, and the row next to where the session
    /// lands is the nearest: last to let its bar go.
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

    /// Putting the session down leaves the swap to finish under the landing;
    /// cancelling the move takes it off at once, since the rows it was drawn
    /// over are back where they were.
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

    /// With the trail off, a move starts no swap and never asks for the faster
    /// clock.
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

    /// A fresh listing is the truth about the rows, and the swap goes with the
    /// rows it was drawn over.
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

    /// Straight after the highlighted row, with everything after it moving
    /// down one.
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

    /// After the last row is where a new session goes anyway, so there is
    /// nothing to write — and no round trip to pay for it. Nor is there with
    /// no rows at all.
    #[test]
    fn a_new_session_after_the_last_row_stores_nothing() {
        let mut a = app(&["aaa", "bbb"]);
        a.on_key(Key::Char('G'));
        assert!(a.placement_of("new").is_empty());
        assert!(app(&[]).placement_of("new").is_empty());
    }

    /// Under a filter the highlight is on a row the filter left, and the rows
    /// it hides keep their numbers and stay out of the payload — the rule a
    /// reorder follows.
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

    /// Asking for a name moves nothing: the highlight the session will go
    /// after is still where it was when the prompt comes back.
    #[test]
    fn asking_for_a_name_leaves_the_highlight_where_it_is() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char('j'));
        assert_eq!(a.on_key(Key::Char('c')), Request::NewSession);
        assert_eq!(a.selected_index(), 1);
        assert_eq!(*a.mode(), Mode::Normal);
    }

    // ---- the gap a new session opens ---------------------------------------

    /// Every row drawn, as `(id, number shown)`.
    fn drawn(a: &App) -> Vec<(String, u32)> {
        a.rows()
            .iter()
            .map(|r| (r.session.id.clone(), r.num))
            .collect()
    }

    /// The gap shows exactly what the create will store: the new row where
    /// [`App::placement_of`] puts it, and every row with the number it will
    /// be given — for every row the highlight could be on, filtered or not.
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

    /// The row in the gap is named as asked, after the highlighted row, and
    /// the rows below it move down a place and take the next number.
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

    /// With the last row highlighted, or none at all, the gap is at the end,
    /// and its number is the one past every row's.
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

    /// The row comes up over [`effects::ROOM`], asking for frames while it
    /// does, and then stands still in its gap until the gap is closed.
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

    /// A fresh listing is the truth about the rows, and ends the gap with
    /// everything else that was passing over them.
    #[test]
    fn a_fresh_listing_closes_the_gap() {
        let mut a = app(&["aaa", "bbb"]);
        assert!(a.make_room("session 3"));
        let listed = a.sessions().to_vec();
        a.set_sessions(listed);
        assert!(!a.has_room());
        assert!(a.rows().iter().all(|r| !r.ghost));
    }

    /// The picker is on its way out: whatever else was passing ends, so the
    /// gap is all that moves.
    #[test]
    fn making_room_ends_what_else_was_passing() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char('j'));
        assert!(a.glow().is_some() && a.glint().is_some());
        assert!(a.make_room("session 4"));
        assert!(a.glow().is_none() && a.glint().is_none());

        let mut a = app(&["aaa", "bbb"]);
        a.on_key(Key::Char('x'));
        a.on_key(Key::Char('n'));
        assert!(a.strike().is_some(), "the line on its way back off");
        assert!(a.make_room("session 3"));
        assert!(a.strike().is_none());
    }

    /// Off, `c` opens no gap and the rows are as they were.
    #[test]
    fn with_the_effect_off_no_gap_opens() {
        let mut a = app(&["aaa", "bbb"]);
        a.set_effects(false);
        assert!(!a.make_room("session 3"));
        assert!(!a.has_room());
        assert_eq!(a.rows().len(), 2);
    }

    /// The number a new session will be given is the one the gap shows on
    /// its row, and the one the create stores for it: after `dotfiles`, the
    /// 3 that `notes` had; after the last row, one past it.
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

    /// The id the row in the gap goes by is no session's, and could never be.
    #[test]
    fn the_gap_row_has_an_id_no_session_can_have() {
        assert!(!crate::ids::is_valid_id(GHOST_ID));
    }
}
