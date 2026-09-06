//! The `<prefix>` prefix state machine.
//!
//! This sits in the stdin half of the PTY proxy and is the *only* thing that
//! inspects the byte stream on the way to Neovim; the child-to-terminal
//! direction is never parsed at all — see [`crate::pty`].
//!
//! Deliberately pure: timing is the caller's job, via [`Prefix::timeout`], so
//! the machine is unit-testable without a PTY, a terminal or a clock.
//!
//! [`BINDINGS`] is the one table. [`Prefix::feed`] consults it, the tests
//! iterate it, and the help screen ([`crate::ui::help`]) renders it, so a
//! command cannot be added without appearing in the help. The README's "While
//! attached" table is a prose copy, and it is what can go stale.
//!
//! Note what is *not* here: `Ctrl-z` (0x1a) is not special-cased. It is
//! forwarded like any other byte and Neovim receives it as a key.
//!
//! # Digits
//!
//! `<prefix> 1` through `<prefix> 9` select a session by its number, and a number may run
//! to several digits. That means a digit after the prefix is *always* a command
//! and no longer reaches Neovim — `<prefix> <prefix> 1` is the way to send it. `0` is the
//! exception: no session number starts with one, so `<prefix> 0` replays both bytes
//! like any other non-command.
//!
//! Multi-digit numbers need a moment to see whether another digit follows, and
//! [`Prefix::new`] takes the highest live session number so that moment is only
//! ever spent when it could change the answer. With nine or fewer sessions a
//! single digit acts at once. The hint is a latency optimisation and nothing
//! more: whether a session actually exists is settled by the caller.

/// `Ctrl-t`.
pub const PREFIX: u8 = 0x14;

/// How the prefix is spelled for people: on the help screen, in the README and
/// in `--help`. A test ties it to [`PREFIX`].
pub const PREFIX_LABEL: &str = "Ctrl-t";

/// How long to wait for the second byte of a prefix sequence before deciding
/// the user meant a literal `<prefix>`.
pub const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Something the proxy must do instead of forwarding bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `<prefix> t` — suspend the relay and show the picker. The child stays alive.
    Picker,
    /// `<prefix> d` — terminate the local UI and exit, leaving the server running.
    Detach,
    /// `<prefix> c` — create a new session and attach to it.
    Create,
    /// `<prefix> ?` — show the key bindings. The child stays alive.
    Help,
    /// `<prefix> <number>` — attach to the session with that number, leaving this
    /// one running. The only action with no [`BINDINGS`] row: one row cannot
    /// stand for nine keys, so digits are a rule in [`Prefix::feed`] instead.
    Switch(u32),
}

/// One instruction from the machine, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Write these bytes to the PTY master, verbatim.
    Forward(Vec<u8>),
    /// Do this.
    Act(Action),
}

/// One `<prefix>` command: the byte that selects it, what it does, and how the help
/// screen describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    /// The byte typed after the prefix. Printable ASCII, so the help screen
    /// can show it as itself.
    pub key: u8,
    pub action: Action,
    /// One line, in terms of what happens to the user.
    pub help: &'static str,
}

/// Every command, in the order the help screen lists them.
pub const BINDINGS: &[Binding] = &[
    Binding {
        key: b't',
        action: Action::Picker,
        help: "back to the picker, session still attached",
    },
    Binding {
        key: b'd',
        action: Action::Detach,
        help: "detach and exit, session still running",
    },
    Binding {
        key: b'c',
        action: Action::Create,
        help: "name a new session and attach to it",
    },
    Binding {
        key: b'?',
        action: Action::Help,
        help: "show this help",
    },
];

/// The command a second byte selects, if any. Linear over a handful of rows; a
/// `match` would be a second copy of the table.
pub fn command(byte: u8) -> Option<Action> {
    BINDINGS.iter().find(|b| b.key == byte).map(|b| b.action)
}

/// Parse a human prefix spelling like `"C-t"` or `"Ctrl-a"` into its control
/// byte. Case-insensitive on both halves; the inverse of [`prefix_label`].
///
/// Only a `Ctrl-<letter>` chord is accepted, because the prefix has to be a
/// single byte the terminal delivers in raw mode, and a control chord is the one
/// class that is both typable and does not collide with ordinary text. A few of
/// those bytes are refused: they already mean something else on the wire and
/// would never reach the machine as a prefix (or, for `C-m`, would clash with
/// the number-entry terminator [`ENTER`]).
///
/// `C-c` and `C-z` are *allowed*, as in tmux: choosing them is the user's
/// explicit, reversible decision, and it only means that byte stops reaching
/// Neovim — the machine is unaffected.
pub fn parse_prefix(s: &str) -> Result<u8, String> {
    let lower = s.trim().to_ascii_lowercase();
    let letter = lower
        .strip_prefix("ctrl-")
        .or_else(|| lower.strip_prefix("c-"))
        .ok_or_else(|| format!("prefix {s:?} must look like \"C-t\" or \"Ctrl-a\""))?;
    let &[b] = letter.as_bytes() else {
        return Err(format!(
            "prefix {s:?} must be Ctrl and a single ASCII letter, like \"C-t\""
        ));
    };
    if !b.is_ascii_lowercase() {
        return Err(format!("prefix {s:?} must be Ctrl and an ASCII letter a-z"));
    }
    // 'a' (0x61) -> 0x01 ... 't' (0x74) -> 0x14 ... 'z' (0x7a) -> 0x1a.
    let byte = b & 0x1f;
    match byte {
        0x08 => Err("C-h is Backspace and would never reach the prefix machine".into()),
        0x09 => Err("C-i is Tab and would never reach the prefix machine".into()),
        0x0a => Err("C-j is a line feed and would never reach the prefix machine".into()),
        0x0d => Err("C-m is Enter and would collide with number entry".into()),
        _ => Ok(byte),
    }
}

/// Spell a prefix byte the way people read it: `0x14` -> `"Ctrl-t"`. The inverse
/// of [`parse_prefix`], used by the runtime help screen and messages so a
/// remapped prefix is described as the key the user actually set.
pub fn prefix_label(byte: u8) -> String {
    // The control byte's letter is the low five bits set back into ASCII.
    format!("Ctrl-{}", (byte | 0x60) as char)
}

/// Carriage return, which is what Enter is in raw mode. Ends a number early
/// rather than waiting out the [`TIMEOUT`].
const ENTER: u8 = 0x0d;

/// Where the machine is between keystrokes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Ordinary bytes, passing straight through.
    #[default]
    Idle,
    /// A `<prefix>` has been swallowed and we are waiting to see what follows.
    Armed,
    /// Digits are accumulating into a session number.
    Number(u32),
}

/// The prefix state machine.
///
/// The prefix byte is state, not a global read: the machine stays pure and
/// clock-free (see the module docs), and the configured key enters through the
/// constructor rather than a settings lookup buried in [`feed`](Prefix::feed).
#[derive(Debug)]
pub struct Prefix {
    state: State,
    /// The highest live session number, used only to decide whether a further
    /// digit could still change the answer. Stale is harmless — see the module
    /// docs.
    highest: u32,
    /// The byte that arms the machine. [`PREFIX`] by default; a config file can
    /// remap it (see [`crate::config`]).
    prefix: u8,
}

/// The derive would zero `prefix`; the default machine must arm on [`PREFIX`].
impl Default for Prefix {
    fn default() -> Self {
        Self {
            state: State::Idle,
            highest: 0,
            prefix: PREFIX,
        }
    }
}

impl Prefix {
    /// A machine armed by the standard [`PREFIX`] (`Ctrl-t`).
    pub fn new(highest: u32) -> Self {
        Self::with_prefix(highest, PREFIX)
    }

    /// A machine armed by `prefix`, for a config file that remaps the key.
    pub fn with_prefix(highest: u32, prefix: u8) -> Self {
        Self {
            state: State::Idle,
            highest,
            prefix,
        }
    }

    /// True if the machine is mid-sequence and a [`TIMEOUT`] must be armed —
    /// either a lone `<prefix>` or a half-typed number. The caller polls on the
    /// short timeout while this holds, so a number does not resolve late.
    pub fn is_armed(&self) -> bool {
        self.state != State::Idle
    }

    /// Whether another digit could still extend `n` into a live session number.
    /// `checked_mul` because a held-down digit key would otherwise overflow.
    fn can_extend(&self, n: u32) -> bool {
        n.checked_mul(10).is_some_and(|wider| wider <= self.highest)
    }

    /// Feed a chunk of bytes read from the user's terminal.
    ///
    /// Runs of ordinary bytes are coalesced into a single [`Step::Forward`], so
    /// a paste of 8 KB does not become 8192 writes.
    pub fn feed(&mut self, input: &[u8]) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut pending: Vec<u8> = Vec::new();

        for &b in input {
            match self.state {
                State::Idle => {
                    if b == self.prefix {
                        self.state = State::Armed;
                    } else {
                        pending.push(b);
                    }
                }

                State::Armed => {
                    self.state = State::Idle;
                    if b == self.prefix {
                        // <prefix> <prefix>: one literal prefix byte reaches Neovim.
                        // Checked before everything else, so no rule and no row
                        // can ever shadow it.
                        pending.push(self.prefix);
                    } else if (b'1'..=b'9').contains(&b) {
                        // A number never starts with 0, so `<prefix> 0` falls
                        // through to the replay branch below.
                        self.start_number(u32::from(b - b'0'), &mut steps, &mut pending);
                    } else if let Some(action) = command(b) {
                        flush(&mut steps, &mut pending);
                        steps.push(Step::Act(action));
                    } else {
                        // Not a command, so the user's original keystrokes are
                        // replayed in order and nothing is eaten.
                        pending.push(self.prefix);
                        pending.push(b);
                    }
                }

                State::Number(n) => {
                    if b.is_ascii_digit() {
                        self.state = State::Idle;
                        // Saturating throughout: a held-down digit key must
                        // neither panic nor wrap a huge number back around onto
                        // a live one.
                        let wider = n.saturating_mul(10).saturating_add(u32::from(b - b'0'));
                        self.start_number(wider, &mut steps, &mut pending);
                    } else if b == ENTER {
                        // An explicit "that is the whole number", so a user who
                        // knows the id never waits out the timeout.
                        self.state = State::Idle;
                        flush(&mut steps, &mut pending);
                        steps.push(Step::Act(Action::Switch(n)));
                    } else {
                        // The digits already typed were a complete command, and
                        // this byte is simply the next key. Acting and then
                        // handling `b` afresh is what keeps `<prefix> 1 x` equivalent
                        // to `<prefix> t x`: the command runs, the `x` reaches Neovim,
                        // and no stray prefix byte is injected.
                        self.state = State::Idle;
                        flush(&mut steps, &mut pending);
                        steps.push(Step::Act(Action::Switch(n)));
                        if b == self.prefix {
                            self.state = State::Armed;
                        } else {
                            pending.push(b);
                        }
                    }
                }
            }
        }

        flush(&mut steps, &mut pending);
        steps
    }

    /// Begin, or extend, a session number: wait for another digit only when one
    /// could still change which session is meant.
    fn start_number(&mut self, n: u32, steps: &mut Vec<Step>, pending: &mut Vec<u8>) {
        if self.can_extend(n) {
            self.state = State::Number(n);
        } else {
            self.state = State::Idle;
            flush(steps, pending);
            steps.push(Step::Act(Action::Switch(n)));
        }
    }

    /// Called when no byte arrived within [`TIMEOUT`] of the machine arming.
    ///
    /// Resolves a lone `<prefix>` into a literal one, and a half-typed number into
    /// the session it already names. Idempotent, so a caller that fires its
    /// timer spuriously does no harm.
    pub fn timeout(&mut self) -> Vec<Step> {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Idle => Vec::new(),
            State::Armed => vec![Step::Forward(vec![self.prefix])],
            State::Number(n) => vec![Step::Act(Action::Switch(n))],
        }
    }
}

fn flush(steps: &mut Vec<Step>, pending: &mut Vec<u8>) {
    if !pending.is_empty() {
        steps.push(Step::Forward(std::mem::take(pending)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concatenate everything the machine would forward, dropping actions.
    fn forwarded(steps: &[Step]) -> Vec<u8> {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Forward(b) => Some(b.clone()),
                Step::Act(_) => None,
            })
            .flatten()
            .collect()
    }

    fn actions(steps: &[Step]) -> Vec<Action> {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Act(a) => Some(*a),
                Step::Forward(_) => None,
            })
            .collect()
    }

    #[test]
    fn ordinary_bytes_pass_through_untouched() {
        let mut p = Prefix::new(0);
        let steps = p.feed(b"hello world");
        assert_eq!(forwarded(&steps), b"hello world");
        assert!(actions(&steps).is_empty());
        assert!(!p.is_armed());
    }

    #[test]
    fn a_run_of_bytes_is_one_write() {
        let mut p = Prefix::new(0);
        let steps = p.feed(b"abcdef");
        assert_eq!(steps.len(), 1, "should coalesce: {steps:?}");
    }

    #[test]
    fn prefix_alone_is_swallowed_and_arms() {
        let mut p = Prefix::new(0);
        let steps = p.feed(&[PREFIX]);
        assert!(
            forwarded(&steps).is_empty(),
            "prefix must not reach nvim yet"
        );
        assert!(p.is_armed());
    }

    #[test]
    fn doubled_prefix_sends_one_literal() {
        let mut p = Prefix::new(0);
        let steps = p.feed(&[PREFIX, PREFIX]);
        assert_eq!(forwarded(&steps), vec![PREFIX]);
        assert!(!p.is_armed());
    }

    #[test]
    fn commands_produce_actions_and_no_bytes() {
        for &Binding { key, action, .. } in BINDINGS {
            let mut p = Prefix::new(0);
            let steps = p.feed(&[PREFIX, key]);
            assert_eq!(actions(&steps), vec![action], "for {:?}", key as char);
            assert!(
                forwarded(&steps).is_empty(),
                "command bytes must not reach nvim: {:?}",
                key as char
            );
            assert!(!p.is_armed());
        }
    }

    #[test]
    fn unknown_command_replays_both_bytes_in_order() {
        assert!(
            command(b'x').is_none(),
            "this test needs a byte that is not a command"
        );
        let mut p = Prefix::new(0);
        let steps = p.feed(&[PREFIX, b'x']);
        assert_eq!(forwarded(&steps), vec![PREFIX, b'x']);
        assert!(actions(&steps).is_empty());
    }

    #[test]
    fn timeout_resolves_a_lone_prefix_to_a_literal() {
        let mut p = Prefix::new(0);
        p.feed(&[PREFIX]);
        assert!(p.is_armed());
        assert_eq!(forwarded(&p.timeout()), vec![PREFIX]);
        assert!(!p.is_armed());
    }

    #[test]
    fn timeout_when_not_armed_does_nothing() {
        let mut p = Prefix::new(0);
        assert!(p.timeout().is_empty());
        // Idempotent: a spurious second timer must not inject a stray byte.
        p.feed(&[PREFIX]);
        p.timeout();
        assert!(p.timeout().is_empty());
    }

    /// The prefix may be the last byte of one `read()` and the command the
    /// first byte of the next. This is the common case at a human typing speed,
    /// not an edge case.
    #[test]
    fn state_survives_a_chunk_boundary() {
        let mut p = Prefix::new(0);
        let first = p.feed(b"ab\x14");
        assert_eq!(forwarded(&first), b"ab");
        assert!(p.is_armed());

        let second = p.feed(b"d");
        assert_eq!(actions(&second), vec![Action::Detach]);
        assert!(forwarded(&second).is_empty());
    }

    #[test]
    fn ctrl_z_is_not_special() {
        // 0x1a must reach Neovim as an ordinary key. Whether the server then
        // emits a `suspend` UI event is the child TUI's business, not ours.
        let mut p = Prefix::new(0);
        let steps = p.feed(&[0x1a]);
        assert_eq!(forwarded(&steps), vec![0x1a]);
        assert!(actions(&steps).is_empty());
    }

    #[test]
    fn ctrl_c_and_ctrl_s_are_not_special() {
        // These only reach us as bytes because the terminal is in raw mode with
        // ISIG off and IXON cleared; the machine itself must not intercept them.
        let mut p = Prefix::new(0);
        let steps = p.feed(&[0x03, 0x13, 0x1c]);
        assert_eq!(forwarded(&steps), vec![0x03, 0x13, 0x1c]);
    }

    #[test]
    fn escape_sequences_are_never_parsed() {
        let mut p = Prefix::new(0);
        // A bracketed paste wrapper plus a kitty keyboard query.
        let raw = b"\x1b[200~pasted\x1b[201~\x1b[?u";
        let steps = p.feed(raw);
        assert_eq!(forwarded(&steps), raw);
    }

    #[test]
    fn a_prefix_inside_a_larger_chunk_splits_correctly() {
        let mut p = Prefix::new(0);
        let steps = p.feed(b"before\x14tafter");
        assert_eq!(forwarded(&steps), b"beforeafter");
        assert_eq!(actions(&steps), vec![Action::Picker]);
        // Ordering matters: the bytes before the command must be written first.
        assert!(matches!(steps[0], Step::Forward(_)));
        assert!(matches!(steps[1], Step::Act(Action::Picker)));
        assert!(matches!(steps[2], Step::Forward(_)));
    }

    #[test]
    fn several_commands_in_one_chunk() {
        let mut p = Prefix::new(0);
        // Literal bytes on purpose: this is what pins each letter to its
        // action, independently of what `BINDINGS` says.
        let steps = p.feed(b"\x14t\x14d\x14c\x14?");
        assert_eq!(
            actions(&steps),
            vec![Action::Picker, Action::Detach, Action::Create, Action::Help]
        );
        assert!(forwarded(&steps).is_empty());
    }

    #[test]
    fn triple_prefix_is_a_literal_then_arms_again() {
        let mut p = Prefix::new(0);
        let steps = p.feed(&[PREFIX, PREFIX, PREFIX]);
        assert_eq!(forwarded(&steps), vec![PREFIX]);
        assert!(p.is_armed(), "the third byte should re-arm");
    }

    #[test]
    fn empty_input_is_a_no_op() {
        let mut p = Prefix::new(0);
        assert!(p.feed(b"").is_empty());
        p.feed(&[PREFIX]);
        assert!(p.feed(b"").is_empty());
        assert!(p.is_armed(), "an empty read must not disarm");
    }

    /// Every byte that is not a command must survive a prefix unchanged.
    ///
    /// With no sessions known, every digit resolves at once, so nothing here is
    /// left mid-sequence.
    #[test]
    fn exhaustive_second_byte_table() {
        for b in 0u8..=255 {
            let mut p = Prefix::new(0);
            let steps = p.feed(&[PREFIX, b]);
            // Same order as `feed`: the literal rule, then digits, then the table.
            if b == PREFIX {
                assert_eq!(forwarded(&steps), vec![PREFIX]);
            } else if (b'1'..=b'9').contains(&b) {
                assert_eq!(
                    actions(&steps),
                    vec![Action::Switch(u32::from(b - b'0'))],
                    "digit {:?} should select a session",
                    b as char
                );
                assert!(forwarded(&steps).is_empty(), "byte {b:#04x} leaked bytes");
            } else if let Some(action) = command(b) {
                assert_eq!(actions(&steps), vec![action], "byte {b:#04x} should act");
                assert!(forwarded(&steps).is_empty(), "byte {b:#04x} leaked bytes");
            } else {
                assert_eq!(
                    forwarded(&steps),
                    vec![PREFIX, b],
                    "byte {b:#04x} must be replayed after the prefix"
                );
            }
            assert!(!p.is_armed(), "byte {b:#04x} left the machine armed");
        }
    }

    #[test]
    fn question_mark_after_the_prefix_is_help_not_a_replayed_byte() {
        let mut p = Prefix::new(0);
        let steps = p.feed(&[PREFIX, b'?']);
        assert_eq!(actions(&steps), vec![Action::Help]);
        assert!(forwarded(&steps).is_empty());
        assert!(!p.is_armed());
    }

    /// `<prefix> ?` used to be replayed to Neovim as two bytes. This is the way to
    /// still send it, and it must keep working.
    #[test]
    fn a_literal_prefix_then_question_mark_still_reaches_neovim() {
        let mut p = Prefix::new(0);
        let steps = p.feed(&[PREFIX, PREFIX, b'?']);
        assert_eq!(forwarded(&steps), vec![PREFIX, b'?']);
        assert!(actions(&steps).is_empty());
    }

    /// Rust cannot enumerate an enum, so the list is kept here by hand. The
    /// `match` is exhaustive on purpose: a new variant fails to compile until
    /// it is added to the list, and then the count fails until it has a row.
    ///
    /// `Switch` is the one deliberate exception, asserted below rather than
    /// waived: a single row cannot stand for nine keys, so digits are a rule in
    /// `feed` and a literal row on the help screen.
    #[test]
    fn the_table_binds_every_action_exactly_once() {
        let all = [Action::Picker, Action::Detach, Action::Create, Action::Help];
        for action in all {
            match action {
                Action::Picker | Action::Detach | Action::Create | Action::Help => {}
                // Not in `all`: it carries a number, so it has no fixed key.
                Action::Switch(_) => unreachable!("Switch is not a table action"),
            }
            assert_eq!(
                BINDINGS.iter().filter(|b| b.action == action).count(),
                1,
                "{action:?} must have exactly one key"
            );
        }
        assert_eq!(
            BINDINGS.len(),
            all.len(),
            "a new command needs a row here and above"
        );
        assert!(
            !BINDINGS
                .iter()
                .any(|b| matches!(b.action, Action::Switch(_))),
            "Switch is a rule in `feed`, not a row"
        );
    }

    /// No row may claim a digit either, or the help screen would promise a key
    /// the digit rule has already taken.
    #[test]
    fn no_command_key_is_a_digit() {
        for b in BINDINGS {
            assert!(
                !b.key.is_ascii_digit(),
                "{:?} collides with the session-number rule",
                b.key as char
            );
        }
    }

    #[test]
    fn command_keys_are_distinct_printable_and_never_the_prefix() {
        for (i, a) in BINDINGS.iter().enumerate() {
            assert!(
                a.key.is_ascii_graphic(),
                "byte {:#04x} cannot be shown on the help screen",
                a.key
            );
            // The literal rule runs before the table, so a PREFIX row would be
            // a help-screen lie that never fires.
            assert_ne!(a.key, PREFIX);
            for b in &BINDINGS[i + 1..] {
                assert_ne!(a.key, b.key, "two commands share {:?}", a.key as char);
            }
        }
    }

    #[test]
    fn every_binding_has_a_one_line_description() {
        for b in BINDINGS {
            assert!(!b.help.is_empty(), "{:?} has no description", b.key as char);
            assert!(
                !b.help.contains('\n'),
                "{:?} has a multi-line description",
                b.key as char
            );
        }
    }

    #[test]
    fn a_single_digit_selects_a_session_without_waiting() {
        // Nine sessions: no second digit could name a different one, so the
        // common case costs nothing.
        let mut p = Prefix::new(9);
        let steps = p.feed(&[PREFIX, b'3']);
        assert_eq!(actions(&steps), vec![Action::Switch(3)]);
        assert!(
            forwarded(&steps).is_empty(),
            "the digit must not reach nvim"
        );
        assert!(!p.is_armed(), "nothing should still be pending");
    }

    #[test]
    fn several_digits_make_one_number() {
        let mut p = Prefix::new(30);
        let steps = p.feed(&[PREFIX, b'1', b'2']);
        assert_eq!(actions(&steps), vec![Action::Switch(12)]);
        assert!(forwarded(&steps).is_empty());
        assert!(!p.is_armed());
    }

    /// The wait exists only while another digit could still change the answer.
    #[test]
    fn a_digit_waits_only_while_a_longer_number_is_possible() {
        let mut p = Prefix::new(12);
        assert!(
            p.feed(&[PREFIX, b'1']).is_empty(),
            "1 could still become 12"
        );
        assert!(p.is_armed(), "the caller must arm a timeout");

        let mut p = Prefix::new(12);
        let steps = p.feed(&[PREFIX, b'2']);
        assert_eq!(
            actions(&steps),
            vec![Action::Switch(2)],
            "20 is past the end, so 2 is already the whole answer"
        );
        assert!(!p.is_armed());
    }

    #[test]
    fn a_half_typed_number_resolves_on_the_timeout() {
        let mut p = Prefix::new(12);
        p.feed(&[PREFIX, b'1']);
        assert_eq!(actions(&p.timeout()), vec![Action::Switch(1)]);
        assert!(!p.is_armed());
        // Idempotent, like the lone-prefix case.
        assert!(p.timeout().is_empty());
    }

    #[test]
    fn enter_ends_a_number_early() {
        let mut p = Prefix::new(12);
        p.feed(&[PREFIX, b'1']);
        let steps = p.feed(&[ENTER]);
        assert_eq!(actions(&steps), vec![Action::Switch(1)]);
        assert!(
            forwarded(&steps).is_empty(),
            "the terminator must not reach nvim"
        );
    }

    /// A number never starts with zero, so this is not a command at all.
    #[test]
    fn a_leading_zero_is_replayed_like_any_other_non_command() {
        let mut p = Prefix::new(30);
        let steps = p.feed(&[PREFIX, b'0']);
        assert_eq!(forwarded(&steps), vec![PREFIX, b'0']);
        assert!(actions(&steps).is_empty());
        assert!(!p.is_armed());
    }

    #[test]
    fn zero_is_still_a_digit_inside_a_number() {
        let mut p = Prefix::new(30);
        let steps = p.feed(&[PREFIX, b'1', b'0']);
        assert_eq!(actions(&steps), vec![Action::Switch(10)]);
    }

    /// The digits already typed were a complete command; the key after them is
    /// simply the next key, exactly as it is after `<prefix> t`.
    #[test]
    fn a_key_after_a_number_ends_it_and_then_reaches_neovim() {
        let mut p = Prefix::new(12);
        let steps = p.feed(&[PREFIX, b'1', b'x']);
        assert_eq!(actions(&steps), vec![Action::Switch(1)]);
        assert_eq!(
            forwarded(&steps),
            vec![b'x'],
            "no stray prefix byte may be injected"
        );
        assert!(!p.is_armed());
    }

    #[test]
    fn a_prefix_after_a_number_ends_it_and_arms_again() {
        let mut p = Prefix::new(12);
        let steps = p.feed(&[PREFIX, b'1', PREFIX]);
        assert_eq!(actions(&steps), vec![Action::Switch(1)]);
        assert!(forwarded(&steps).is_empty());
        assert!(p.is_armed(), "the second prefix should arm");
    }

    /// The prefix may end one `read()` and the digits begin the next, at any
    /// point in the sequence.
    #[test]
    fn a_number_survives_a_chunk_boundary() {
        let mut p = Prefix::new(30);
        assert!(p.feed(&[PREFIX]).is_empty());
        assert!(p.feed(b"1").is_empty(), "1 could still become 12");
        let steps = p.feed(b"2");
        assert_eq!(actions(&steps), vec![Action::Switch(12)]);
    }

    /// This is the documented way to send a digit to Neovim now that `<prefix> 1` is
    /// a command, so it must keep working.
    #[test]
    fn a_literal_prefix_then_a_digit_still_reaches_neovim() {
        let mut p = Prefix::new(9);
        let steps = p.feed(&[PREFIX, PREFIX, b'1']);
        assert_eq!(forwarded(&steps), vec![PREFIX, b'1']);
        assert!(actions(&steps).is_empty());
    }

    /// Ordinary typing contains digits. They must only ever be a command
    /// directly after the prefix.
    #[test]
    fn digits_in_ordinary_input_are_not_commands() {
        let mut p = Prefix::new(30);
        let steps = p.feed(b"buffer 12 line 30");
        assert_eq!(forwarded(&steps), b"buffer 12 line 30");
        assert!(actions(&steps).is_empty());
        assert!(!p.is_armed());
    }

    /// A held-down digit key must not wrap around into a live session number.
    #[test]
    fn an_absurdly_long_number_does_not_overflow() {
        let mut p = Prefix::new(u32::MAX);
        let steps = p.feed(&[PREFIX]);
        assert!(steps.is_empty());
        let steps = p.feed(&[b'9'; 64]);
        // Whatever it settles on, it must be a switch and not a panic.
        assert!(
            actions(&steps)
                .iter()
                .all(|a| matches!(a, Action::Switch(_))),
            "unexpected actions: {steps:?}"
        );
    }

    /// The label is what the help screen and the README print; the byte is
    /// what the machine matches. Derive one from the other so they cannot
    /// disagree.
    #[test]
    fn the_prefix_label_names_the_prefix_byte() {
        let letter = PREFIX_LABEL.chars().last().expect("a label");
        assert_eq!(letter as u8 & 0x1f, PREFIX);
    }

    #[test]
    fn a_prefix_string_parses_to_its_control_byte() {
        assert_eq!(parse_prefix("C-t"), Ok(PREFIX));
        assert_eq!(parse_prefix("Ctrl-a"), Ok(0x01));
        assert_eq!(parse_prefix("C-z"), Ok(0x1a));
    }

    /// Both halves are case-insensitive, and surrounding space is ignored.
    #[test]
    fn a_prefix_string_is_case_insensitive() {
        assert_eq!(parse_prefix("ctrl-T"), Ok(PREFIX));
        assert_eq!(parse_prefix("c-A"), Ok(0x01));
        assert_eq!(parse_prefix("  Ctrl-B  "), Ok(0x02));
    }

    /// The label and the parser are inverses for every control byte, so what the
    /// help screen prints round-trips back to the byte the machine matches.
    #[test]
    fn the_prefix_string_round_trips() {
        for byte in 1u8..=26 {
            // The four bytes that are also Enter/newline/Tab/Backspace are
            // refused on the way back, by design; they never round-trip.
            if matches!(byte, 0x08 | 0x09 | 0x0a | 0x0d) {
                continue;
            }
            let label = prefix_label(byte);
            assert_eq!(parse_prefix(&label), Ok(byte), "for {label}");
        }
        assert_eq!(parse_prefix(&prefix_label(PREFIX)), Ok(PREFIX));
    }

    /// Generalises [`the_prefix_label_names_the_prefix_byte`] beyond the default.
    #[test]
    fn the_prefix_label_names_any_control_byte() {
        assert_eq!(prefix_label(0x01), "Ctrl-a");
        assert_eq!(prefix_label(PREFIX), "Ctrl-t");
        assert_eq!(prefix_label(0x1a), "Ctrl-z");
    }

    #[test]
    fn a_malformed_prefix_is_rejected() {
        for s in ["t", "C-1", "C-", "", "hyper-t", "C-ab", "C-é"] {
            assert!(parse_prefix(s).is_err(), "{s:?} should be rejected");
        }
    }

    /// These control bytes are already Enter/newline/Tab/Backspace on the wire,
    /// so they could never act as a prefix; the parser refuses them by name.
    #[test]
    fn prefixes_that_break_the_terminal_are_rejected() {
        for s in ["C-m", "C-j", "C-i", "C-h"] {
            assert!(parse_prefix(s).is_err(), "{s:?} should be rejected");
        }
    }

    /// Allowed on purpose (tmux does the same): the byte simply stops reaching
    /// Neovim. Documented, reversible, and harmless to the machine.
    #[test]
    fn ctrl_c_and_ctrl_z_are_accepted_as_prefixes() {
        assert_eq!(parse_prefix("C-c"), Ok(0x03));
        assert_eq!(parse_prefix("C-z"), Ok(0x1a));
    }

    /// A remapped machine arms on its own byte, and the old default is then just
    /// an ordinary key forwarded to Neovim.
    #[test]
    fn a_remapped_prefix_arms_on_its_own_byte() {
        let ctrl_a = 0x01;
        let mut p = Prefix::with_prefix(0, ctrl_a);
        let armed = p.feed(&[ctrl_a]);
        assert!(forwarded(&armed).is_empty(), "the new prefix must be eaten");
        assert!(p.is_armed());

        let steps = p.feed(b"t");
        assert_eq!(actions(&steps), vec![Action::Picker]);

        // The former default is no longer special.
        let mut p = Prefix::with_prefix(0, ctrl_a);
        let steps = p.feed(&[PREFIX]);
        assert_eq!(forwarded(&steps), vec![PREFIX]);
        assert!(!p.is_armed());
    }

    #[test]
    fn a_remapped_doubled_prefix_sends_one_literal() {
        let ctrl_a = 0x01;
        let mut p = Prefix::with_prefix(0, ctrl_a);
        let steps = p.feed(&[ctrl_a, ctrl_a]);
        assert_eq!(forwarded(&steps), vec![ctrl_a]);
        assert!(!p.is_armed());
    }

    /// `new` and the derived-less `Default` both arm on the standard prefix, so
    /// the default machine is unchanged.
    #[test]
    fn new_and_default_use_the_standard_prefix() {
        for mut p in [Prefix::new(0), Prefix::default()] {
            assert!(p.feed(&[PREFIX]).is_empty());
            assert!(p.is_armed(), "the standard prefix must arm the machine");
        }
    }
}
