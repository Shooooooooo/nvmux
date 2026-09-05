//! Picker state.
//!
//! Deliberately free of I/O and of ratatui: this is a state machine that takes
//! key events and returns [`Request`]s for the caller to carry out. That is what
//! makes every keybind, the filtering, and the prompt behaviour testable without
//! a terminal, a transport, or a running Neovim.

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
    pub fn visible(&self) -> Vec<&Session> {
        if self.filter.is_empty() {
            return self.sessions.iter().collect();
        }
        let needle = self.filter.to_lowercase();
        self.sessions
            .iter()
            .filter(|s| s.name.to_lowercase().contains(&needle))
            .collect()
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn selected_session(&self) -> Option<&Session> {
        self.visible().get(self.selected).copied()
    }

    fn selected_id(&self) -> Option<String> {
        self.selected_session().map(|s| s.id.clone())
    }

    fn session_name(&self, id: &str) -> String {
        self.sessions
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.name.clone())
            .unwrap_or_default()
    }

    /// Move the selection, wrapping at both ends.
    fn move_by(&mut self, delta: isize) {
        let len = self.visible().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let len = len as isize;
        self.selected = (((self.selected as isize + delta) % len + len) % len) as usize;
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
        let Some(n) = self.pending.take().map(|p| p.saturating_mul(10) + d) else {
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

    /// Called by the driver once [`crate::keys::TIMEOUT`] has passed with a
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

    fn on_key_normal(&mut self, key: Key) -> Request {
        // Any key that is not a digit ends a half-typed number rather than
        // letting it linger into an unrelated keystroke.
        if !matches!(key, Key::Char('0'..='9')) {
            self.pending = None;
        }
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
            Key::Enter => match self.selected_session() {
                Some(s) => Request::Attach(s.id.clone()),
                None => Request::None,
            },
            Key::Char('c') => Request::NewSession,
            Key::Char('r') => match self.selected_session() {
                Some(s) => Request::RenameSession(s.id.clone()),
                None => Request::None,
            },
            Key::Char('x') => match self.selected_session() {
                Some(s) => {
                    let id = s.id.clone();
                    self.show_kill_confirm(&id);
                    Request::None
                }
                None => Request::None,
            },
            Key::Char('/') => {
                self.mode = Mode::Filter;
                Request::None
            }
            Key::Esc => {
                self.filter.clear();
                self.clamp_selection();
                Request::None
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
                match self.selected_session() {
                    Some(s) => Request::Attach(s.id.clone()),
                    None => Request::None,
                }
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
    Home,
    End,
    CtrlC,
    /// Ctrl-N — the readline-style companion to `j`/Down.
    CtrlN,
    /// Ctrl-P — the readline-style companion to `k`/Up.
    CtrlP,
    Other,
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

    #[test]
    fn movement_wraps_in_both_directions() {
        let mut a = app(&["one", "two", "three"]);
        assert_eq!(a.selected_index(), 0);
        a.on_key(Key::Char('j'));
        a.on_key(Key::Char('j'));
        assert_eq!(a.selected_index(), 2);
        a.on_key(Key::Char('j'));
        assert_eq!(a.selected_index(), 0, "should wrap forwards");
        a.on_key(Key::Char('k'));
        assert_eq!(a.selected_index(), 2, "should wrap backwards");
    }

    #[test]
    fn arrows_match_jk() {
        let mut a = app(&["one", "two"]);
        a.on_key(Key::Down);
        assert_eq!(a.selected_index(), 1);
        a.on_key(Key::Up);
        assert_eq!(a.selected_index(), 0);
    }

    #[test]
    fn ctrl_n_and_ctrl_p_match_jk() {
        let mut a = app(&["one", "two"]);
        a.on_key(Key::CtrlN);
        assert_eq!(a.selected_index(), 1);
        a.on_key(Key::CtrlP);
        assert_eq!(a.selected_index(), 0);
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
}
