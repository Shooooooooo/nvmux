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
    /// Typing a name for a new session.
    Create {
        input: String,
    },
    /// Editing an existing session's name, pre-filled with the current one.
    Rename {
        input: String,
        id: String,
    },
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
    Create(String),
    Rename { id: String, name: String },
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
}

impl App {
    pub fn new(sessions: Vec<Session>) -> Self {
        Self {
            sessions,
            filter: String::new(),
            selected: 0,
            mode: Mode::Normal,
            message: None,
        }
    }

    /// Replace the session list, keeping the selection on the same session
    /// where possible.
    ///
    /// Selection follows identity rather than position: after a rename the list
    /// re-sorts, and jumping the highlight to whatever landed at the old index
    /// would be disorienting.
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

    /// Show the kill confirmation.
    ///
    /// Killing is unconditional, so this asks nothing of the session: no RPC,
    /// no unsaved-buffer count, nothing that could be stale or unobtainable.
    /// The `[y/N]` is the whole safeguard.
    fn show_kill_confirm(&mut self, id: &str) {
        let name = self.session_name(id);
        self.mode = Mode::Confirm {
            id: id.to_string(),
            prompt: format!("kill {name:?}? [y/N]"),
        };
    }

    /// Handle one key. Returns whatever the caller now has to do.
    pub fn on_key(&mut self, key: Key) -> Request {
        // Any keypress clears a stale message, so it never lingers over an
        // unrelated action.
        self.message = None;

        match &self.mode {
            Mode::Normal => self.on_key_normal(key),
            Mode::Filter => self.on_key_filter(key),
            Mode::Create { .. } | Mode::Rename { .. } => self.on_key_input(key),
            Mode::Confirm { .. } => self.on_key_confirm(key),
        }
    }

    fn on_key_normal(&mut self, key: Key) -> Request {
        match key {
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
            Key::Char('c') => {
                self.mode = Mode::Create {
                    input: String::new(),
                };
                Request::None
            }
            Key::Char('r') => match self.selected_session() {
                Some(s) => {
                    // Pre-filled with the current name, as specified.
                    self.mode = Mode::Rename {
                        input: s.name.clone(),
                        id: s.id.clone(),
                    };
                    Request::None
                }
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
                // In Normal mode, Esc's job is to clear an active filter.
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
            // Enter means the same thing everywhere: attach to what is
            // highlighted. The filter stays applied.
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

    fn on_key_input(&mut self, key: Key) -> Request {
        let (input, id) = match &mut self.mode {
            Mode::Create { input } => (input, None),
            Mode::Rename { input, id } => {
                let id = id.clone();
                (input, Some(id))
            }
            _ => return Request::None,
        };

        match key {
            Key::Char(c) => {
                input.push(c);
                Request::None
            }
            Key::Backspace => {
                input.pop();
                Request::None
            }
            Key::Enter => {
                let text = input.trim().to_string();
                self.mode = Mode::Normal;
                if text.is_empty() {
                    return Request::None;
                }
                match id {
                    Some(id) => Request::Rename { id, name: text },
                    None => Request::Create(text),
                }
            }
            Key::Esc => {
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
                .map(|(i, n)| Session::new(format!("id{i:06}"), n.to_string(), 100 + i as u32))
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

    #[test]
    fn create_prompts_then_requests_creation() {
        let mut a = app(&[]);
        a.on_key(Key::Char('c'));
        assert!(matches!(a.mode(), Mode::Create { .. }));
        for c in "my-project".chars() {
            a.on_key(Key::Char(c));
        }
        assert_eq!(a.on_key(Key::Enter), Request::Create("my-project".into()));
        assert_eq!(*a.mode(), Mode::Normal);
    }

    #[test]
    fn an_empty_create_is_cancelled_not_submitted() {
        let mut a = app(&[]);
        a.on_key(Key::Char('c'));
        assert_eq!(a.on_key(Key::Enter), Request::None);
        assert_eq!(*a.mode(), Mode::Normal);

        // ...and neither is one that is only whitespace.
        a.on_key(Key::Char('c'));
        a.on_key(Key::Char(' '));
        assert_eq!(a.on_key(Key::Enter), Request::None);
    }

    #[test]
    fn esc_cancels_a_prompt_without_acting() {
        let mut a = app(&["one"]);
        a.on_key(Key::Char('c'));
        for c in "abandoned".chars() {
            a.on_key(Key::Char(c));
        }
        assert_eq!(a.on_key(Key::Esc), Request::None);
        assert_eq!(*a.mode(), Mode::Normal);
    }

    #[test]
    fn rename_is_prefilled_with_the_current_name() {
        let mut a = app(&["dotfiles", "notes"]);
        a.on_key(Key::Char('r'));
        match a.mode() {
            Mode::Rename { input, id } => {
                assert_eq!(input, "dotfiles", "prompt should be pre-filled");
                assert_eq!(id, "id000000");
            }
            other => panic!("expected Rename, got {other:?}"),
        }
        // Editing from the pre-filled value.
        for _ in 0..8 {
            a.on_key(Key::Backspace);
        }
        for c in "config".chars() {
            a.on_key(Key::Char(c));
        }
        assert_eq!(
            a.on_key(Key::Enter),
            Request::Rename {
                id: "id000000".into(),
                name: "config".into()
            }
        );
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

    /// While a prompt is open, ordinary keys are text, not commands. Typing a
    /// name containing "x" must not trigger a kill.
    #[test]
    fn prompt_keys_are_text_not_commands() {
        let mut a = app(&["one"]);
        a.on_key(Key::Char('c'));
        for c in "xqrc/".chars() {
            assert_eq!(a.on_key(Key::Char(c)), Request::None);
        }
        match a.mode() {
            Mode::Create { input } => assert_eq!(input, "xqrc/"),
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn selection_follows_the_session_not_the_index() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char('j')); // on "bbb"
        assert_eq!(a.selected_session().expect("selected").name, "bbb");

        // "bbb" is renamed to "zzz" and the list re-sorts; the highlight should
        // travel with it rather than staying on row 1.
        let mut renamed: Vec<Session> = vec![
            Session::new("id000000".into(), "aaa".into(), 100),
            Session::new("id000002".into(), "ccc".into(), 102),
            Session::new("id000001".into(), "zzz".into(), 101),
        ];
        renamed.sort_by(|x, y| x.name.cmp(&y.name));
        a.set_sessions(renamed);
        assert_eq!(a.selected_session().expect("selected").id, "id000001");
        assert_eq!(a.selected_session().expect("selected").name, "zzz");
    }

    #[test]
    fn selection_survives_the_selected_session_disappearing() {
        let mut a = app(&["aaa", "bbb", "ccc"]);
        a.on_key(Key::Char('G')); // "ccc"
        a.set_sessions(vec![
            Session::new("id000000".into(), "aaa".into(), 100),
            Session::new("id000001".into(), "bbb".into(), 101),
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
