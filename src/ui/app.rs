//! Picker state.
//!
//! Deliberately free of I/O and of ratatui: this is a state machine that takes
//! key events and returns [`Request`]s for the caller to carry out. That is what
//! makes every keybind, the filtering, and the prompt behaviour testable without
//! a terminal, a transport, or a running Neovim.

use std::cell::RefCell;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

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
        }
    }

    /// Replace the session list, keeping the selection on the same session where
    /// possible — by identity, not position, so a rename that re-sorts the list
    /// does not move the highlight to an unrelated row.
    pub fn set_sessions(&mut self, sessions: Vec<Session>) {
        let previously = self.selected_id();
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

    /// The session with this id, from the list the picker is showing. What a
    /// [`Request`] carrying an id resolves against, so acting on a row costs no
    /// round trip beyond the action itself.
    /// Every row the picker is holding, in the order it is holding them.
    ///
    /// For a caller that carries the listing on rather than asking for another
    /// — see [`crate::ui::Outcome::Attach`]. `visible` is the filtered view and
    /// answers a different question.
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    pub fn session(&self, id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn selected_session(&self) -> Option<&Session> {
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

    /// Every visible row's `(id, resolved number)`, in display order.
    fn snapshot(&self) -> Vec<(String, u32)> {
        self.visible()
            .iter()
            .map(|s| (s.id.clone(), s.state.num))
            .collect()
    }

    /// Move the grabbed row to `to`, sliding everything between it and where it
    /// came from one place the other way.
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
    fn restore(&mut self, was: &[(String, u32)], id: &str) {
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
    }

    /// Handle one key. Returns whatever the caller now has to do.
    pub fn on_key(&mut self, key: Key) -> Request {
        // A stale message must not linger over an unrelated action.
        self.message = None;

        match &self.mode {
            Mode::Normal => self.on_key_normal(key),
            Mode::Filter => self.on_key_filter(key),
            Mode::Confirm { .. } => self.on_key_confirm(key),
            Mode::Reorder { .. } => self.on_key_reorder(key),
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
        match &self.mode {
            Mode::Normal | Mode::Filter => self.on_mouse_normal(mouse),
            Mode::Confirm { .. } => {
                // Only an explicit `y` kills, and a click is not one; but a
                // click is a deliberate act, so it declines the question the
                // way any key but `y` does. The wheel is not deliberate enough
                // to dismiss a `[y/N]`.
                if matches!(mouse, Mouse::Press(_)) {
                    self.mode = Mode::Normal;
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
                    let was = self.snapshot();
                    self.mode = Mode::Reorder { id, was };
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
                self.mode = Mode::Normal;
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
                    let was = self.snapshot();
                    self.mode = Mode::Reorder { id, was };
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
                self.mode = Mode::Normal;
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
            Key::Char('y') | Key::Char('Y') => {
                self.mode = Mode::Normal;
                Request::Kill(id)
            }
            Key::CtrlC => Request::Quit,
            _ => {
                self.mode = Mode::Normal;
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
    /// Only the prompt binds these four: they move within and between its
    /// fields. The picker's handlers ignore them, as they do any other key
    /// they do not name.
    Left,
    Right,
    Tab,
    BackTab,
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

    fn app(names: &[&str]) -> App {
        App::new(
            names
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    let num = i as u32 + 1;
                    let mut s =
                        Session::new(format!("id{i:06}"), n.to_string(), 100 + i as u32, num);
                    s.state.num = num;
                    s
                })
                .collect(),
        )
    }

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

    fn filtering(names: &[&str], query: &str) -> App {
        let mut a = app(names);
        a.on_key(Key::Char('/'));
        for c in query.chars() {
            a.on_key(Key::Char(c));
        }
        a
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
}
