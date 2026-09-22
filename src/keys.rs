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
//! # How the prefix is spelled
//!
//! The prefix is a `Ctrl` chord, and a terminal has three ways of spelling one:
//! the control byte — `NUL` for the default `Ctrl-Space` — or, once Neovim's
//! TUI has asked it for the kitty keyboard protocol or xterm's
//! `modifyOtherKeys`, which it does at every start, an escape sequence.
//! [`crate::keyseq`] knows the spellings; the machine treats all of them as the
//! same key, so the prefix works in Windows Terminal, kitty, Ghostty, WezTerm or
//! xterm exactly as it does in a terminal that speaks neither protocol.
//!
//! Two consequences. A sequence can be cut in two by the end of a `read()`, so
//! an unfinished one is held back until the rest arrives, or until the caller's
//! short [`Wait::Sequence`] passes and it is passed on as it stands — which is
//! how a bare `Esc`, in a terminal that still sends one, reaches Neovim. And
//! whatever spelled the prefix is what a literal `<prefix>` replays: a terminal
//! that sent a sequence gets its sequence back, byte for byte, never a control
//! byte it did not send.
//!
//! Not everything on stdin is a keystroke. A terminal asked for key-release
//! reports (Neovim 0.12 asks) sends one for every key, the prefix included;
//! and the terminal answers the editor's queries on the same stream, at every
//! start and resume. Neither is the next key: both change nothing and go
//! through as they came — except the release of the prefix itself while its
//! press is still held back, which stays with that press and shares its fate.
//! Neovim only ever sees the input in its original order, less what the
//! machine consumed.
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

use crate::keyseq::{self, Sequence, ESC};

/// The byte `Ctrl-Space` sends: `NUL`. The one chord in the prefix's class
/// that is not a `Ctrl-<letter>`, so it is also the one whose label and key
/// code do not follow from a letter — see [`prefix_label`] and [`prefix_code`].
const CTRL_SPACE: u8 = 0x00;

/// What the space bar is called: in a config file, on the help screen, and in
/// the README — as the prefix chord's key and as the picker command's.
const SPACE_NAME: &str = "Space";

/// `Ctrl-Space`.
pub const PREFIX: u8 = CTRL_SPACE;

/// How the prefix is spelled for people: on the help screen, in the README and
/// in `--help`. A test ties it to [`PREFIX`].
pub const PREFIX_LABEL: &str = "Ctrl-Space";

/// Which way `<prefix> n` and `<prefix> p` step through the session list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Next,
    Prev,
}

/// Something the proxy must do instead of forwarding bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `<prefix> Space` — suspend the relay and show the picker. The child stays
    /// alive.
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
    /// `<prefix> n` / `<prefix> p` — attach to the session either side of this
    /// one in the picker's order, wrapping at both ends. One action rather than
    /// two, so the direction stays a value all the way to the session loop
    /// instead of being spelled out again at every hand-off.
    Cycle(Direction),
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
    /// The byte typed after the prefix. Printable ASCII; [`key_label`] spells
    /// it for the help screen, since one of them is the space bar and would
    /// otherwise be an empty cell.
    pub key: u8,
    pub action: Action,
    /// One line, in terms of what happens to the user.
    pub help: &'static str,
    /// The same thing in one lowercase word, for the hint bar the relay puts
    /// up while the prefix is armed ([`crate::hint`]). A field rather than a
    /// second table, so a command cannot be added without a label for the bar
    /// — the guarantee `help` already gives the help screen.
    pub hint: &'static str,
}

/// Every command, in the order the help screen lists them.
pub const BINDINGS: &[Binding] = &[
    Binding {
        // The prefix chord's own key, without the Ctrl: `<prefix> Space`, the
        // way `<prefix> <prefix>` is the prefix chord twice.
        key: b' ',
        action: Action::Picker,
        help: "back to the picker, session still attached",
        hint: "picker",
    },
    Binding {
        key: b'd',
        action: Action::Detach,
        help: "detach and exit, session still running",
        hint: "detach",
    },
    Binding {
        key: b'c',
        action: Action::Create,
        help: "start a new session and attach to it",
        hint: "new",
    },
    Binding {
        key: b'n',
        action: Action::Cycle(Direction::Next),
        help: "attach to the next session, wrapping",
        hint: "next",
    },
    Binding {
        key: b'p',
        action: Action::Cycle(Direction::Prev),
        help: "attach to the previous session, wrapping",
        hint: "prev",
    },
    Binding {
        key: b'?',
        action: Action::Help,
        help: "show this help",
        hint: "help",
    },
];

/// The command a second byte selects, if any. Linear over a handful of rows; a
/// `match` would be a second copy of the table.
pub fn command(byte: u8) -> Option<Action> {
    BINDINGS.iter().find(|b| b.key == byte).map(|b| b.action)
}

/// Parse a human prefix spelling like `"C-Space"`, `"C-t"` or `"Ctrl-a"` into
/// its control byte. Case-insensitive on both halves; the inverse of
/// [`prefix_label`].
///
/// Only a `Ctrl-<letter>` chord or `Ctrl-Space` is accepted, because the prefix
/// has to be a single byte the terminal delivers in raw mode, and a control
/// chord is the one class that is both typable and does not collide with
/// ordinary text. `Ctrl-Space` is that class's one non-letter member, and it
/// sends `NUL` — a byte no ordinary text contains. A few of the letters are
/// refused: they already mean something else on the wire and would never reach
/// the machine as a prefix (or, for `C-m`, would clash with the number-entry
/// terminator `ENTER`).
///
/// `C-c` and `C-z` are *allowed*, as in tmux: choosing them is the user's
/// explicit, reversible decision, and it only means that byte stops reaching
/// Neovim — the machine is unaffected.
pub fn parse_prefix(s: &str) -> Result<u8, String> {
    let lower = s.trim().to_ascii_lowercase();
    let key = lower
        .strip_prefix("ctrl-")
        .or_else(|| lower.strip_prefix("c-"))
        .ok_or_else(|| format!("prefix {s:?} must look like \"C-Space\" or \"Ctrl-a\""))?;
    if key.eq_ignore_ascii_case(SPACE_NAME) {
        return Ok(CTRL_SPACE);
    }
    let &[b] = key.as_bytes() else {
        return Err(format!(
            "prefix {s:?} must be Ctrl and a single ASCII letter, like \"C-t\", or \"C-Space\""
        ));
    };
    if !b.is_ascii_lowercase() {
        return Err(format!(
            "prefix {s:?} must be Ctrl and an ASCII letter a-z, or Space"
        ));
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

/// Spell a key the way people read it: itself for a printable character, and
/// a name for the space bar, which would otherwise be an invisible cell on the
/// help screen and an unreadable one in the README.
pub fn key_label(byte: u8) -> String {
    if byte == b' ' {
        SPACE_NAME.to_string()
    } else {
        (byte as char).to_string()
    }
}

/// Spell a prefix byte the way people read it: `0x00` -> `"Ctrl-Space"`,
/// `0x14` -> `"Ctrl-t"`. The inverse of [`parse_prefix`], used by the runtime
/// help screen and messages so a remapped prefix is described as the key the
/// user actually set.
pub fn prefix_label(byte: u8) -> String {
    format!("Ctrl-{}", key_label(prefix_code(byte)))
}

/// The key code a terminal reports the prefix chord under, see
/// [`crate::keyseq`]: the letter of a `Ctrl-<letter>` control byte, its low
/// five bits set back into ASCII, so `0x14` is `'t'`; and for `Ctrl-Space`,
/// whose byte is `NUL`, the space bar's own code.
pub fn prefix_code(byte: u8) -> u8 {
    if byte == CTRL_SPACE {
        b' '
    } else {
        byte | 0x60
    }
}

/// Carriage return, which is what Enter is in raw mode. Ends a number early
/// rather than waiting out `keys.timeout_ms`.
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

/// What the caller must put a clock on, if anything. See [`Prefix::wait`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// A lone `<prefix>` or a half-typed number: give the user
    /// `keys.timeout_ms` to finish it.
    Command,
    /// An escape sequence the last read cut short: give the rest of it a
    /// moment to arrive. Machine time, not human time — the bytes of one key
    /// are only ever split by a buffer boundary — so this wait should be
    /// short: a bare `Esc` in a terminal that still sends one is held for
    /// exactly this long.
    Sequence,
}

/// What the machine is waiting for, for something that wants to say so on the
/// screen — the hint bar the relay puts up while the prefix is armed (see
/// [`crate::hint`]).
///
/// Not [`Wait`], which is the same question asked about clocks: that says what
/// the caller must *time*, and an unfinished escape sequence is one of its
/// answers because it is a timer the caller must arm. This says what the user
/// has half-typed, which is a different set — and the set a bar can be drawn
/// from.
///
/// [`State`] stays private. This is the part of it that is anybody else's
/// business, and no more: a caller can tell a pending command from a pending
/// number, and cannot reach in and change either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// A lone `<prefix>`: the next key picks a command.
    Command,
    /// The digits typed so far, waiting to see whether another follows.
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
    /// The bytes that armed the machine — the control byte, or the sequence a
    /// terminal sent instead of it, and that key's release report if one has
    /// arrived since — replayed verbatim when the prefix turns out to be meant
    /// literally. Empty unless [`State::Armed`].
    armed: Vec<u8>,
    /// An escape sequence still being read. It could yet spell the prefix, so
    /// it is held back until it is complete or clearly something else; empty
    /// between sequences.
    seq: Vec<u8>,
}

/// The derive would zero `prefix`; the default machine must arm on [`PREFIX`].
impl Default for Prefix {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Prefix {
    /// A machine armed by the standard [`PREFIX`] (`Ctrl-Space`).
    pub fn new(highest: u32) -> Self {
        Self::with_prefix(highest, PREFIX)
    }

    /// A machine armed by `prefix`, for a config file that remaps the key.
    pub fn with_prefix(highest: u32, prefix: u8) -> Self {
        Self {
            state: State::Idle,
            highest,
            prefix,
            armed: Vec::new(),
            seq: Vec::new(),
        }
    }

    /// True if the machine is mid-sequence and a timeout must be armed —
    /// a lone `<prefix>`, a half-typed number or an unfinished escape
    /// sequence. [`wait`](Self::wait) says which.
    pub fn is_armed(&self) -> bool {
        self.wait().is_some()
    }

    /// What the caller must time, if anything. The caller polls on that wait
    /// while this is `Some`, and calls [`timeout`](Self::timeout) when it
    /// passes, so a number does not resolve late and a cut-off sequence does
    /// not hang.
    ///
    /// An unfinished sequence takes precedence: settling it settles whatever
    /// else was pending too, since it was the key after the prefix or the
    /// number.
    pub fn wait(&self) -> Option<Wait> {
        if !self.seq.is_empty() {
            Some(Wait::Sequence)
        } else if self.state != State::Idle {
            Some(Wait::Command)
        } else {
            None
        }
    }

    /// What the user has half-typed, if anything — for the hint bar, which says
    /// so on the screen while the machine waits (see [`crate::hint`]).
    ///
    /// Read from `state` and not from [`wait`](Self::wait), which would be a
    /// different answer: an escape sequence cut short *from idle* is the Escape
    /// key arriving and nothing is pending, while one arriving after the prefix
    /// is still the prefix waiting — so the bar goes up on the prefix and does
    /// not blink off while the bytes of the next key come in.
    pub fn pending(&self) -> Option<Pending> {
        match self.state {
            State::Idle => None,
            State::Armed => Some(Pending::Command),
            State::Number(n) => Some(Pending::Number(n)),
        }
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
            self.byte(b, &mut steps, &mut pending);
        }
        flush(&mut steps, &mut pending);
        steps
    }

    /// One byte, in whatever state the machine is in.
    fn byte(&mut self, b: u8, steps: &mut Vec<Step>, pending: &mut Vec<u8>) {
        if !self.seq.is_empty() {
            self.seq.push(b);
            match keyseq::classify(&self.seq, prefix_code(self.prefix)) {
                Sequence::Partial => {}
                Sequence::Prefix => {
                    let bytes = std::mem::take(&mut self.seq);
                    self.prefix_key(bytes, steps, pending);
                }
                Sequence::Release(code) => {
                    // Releasing a key is not pressing one: no state changes,
                    // and Neovim gets the report as it was sent. The one
                    // exception is the release of the prefix while its press
                    // is still held back: it stays with that press, so Neovim
                    // sees the two in order for a literal and neither for a
                    // command, never a release before its press.
                    let mut report = std::mem::take(&mut self.seq);
                    if self.state == State::Armed && code == u32::from(prefix_code(self.prefix)) {
                        self.armed.append(&mut report);
                    } else {
                        pending.append(&mut report);
                    }
                }
                Sequence::Reply => {
                    // The terminal answering the editor, not the user typing:
                    // no state changes, and the answer goes through as it
                    // came — even between a prefix and its command.
                    pending.append(&mut self.seq);
                }
                Sequence::Other(n) => {
                    let mut key = std::mem::take(&mut self.seq);
                    let rest = key.split_off(n);
                    self.other_key(key, steps, pending);
                    // What followed the key is input in its own right — it
                    // may even be the prefix byte, or the start of another
                    // sequence.
                    for r in rest {
                        self.byte(r, steps, pending);
                    }
                }
            }
        } else if b == ESC {
            self.seq.push(b);
        } else if b == self.prefix {
            self.prefix_key(vec![b], steps, pending);
        } else {
            self.plain_byte(b, steps, pending);
        }
    }

    /// The prefix, spelled as `bytes`.
    fn prefix_key(&mut self, bytes: Vec<u8>, steps: &mut Vec<Step>, pending: &mut Vec<u8>) {
        match self.state {
            State::Idle => {
                self.state = State::Armed;
                self.armed = bytes;
            }
            State::Armed => {
                // <prefix> <prefix>: one literal prefix reaches Neovim — the
                // one that was held back, in the terminal's own spelling.
                // Checked before everything else, so no rule and no row can
                // ever shadow it.
                self.state = State::Idle;
                pending.append(&mut self.armed);
            }
            State::Number(n) => {
                // The digits already typed were a complete command, and this
                // prefix starts the next one.
                self.state = State::Idle;
                flush(steps, pending);
                steps.push(Step::Act(Action::Switch(n)));
                self.state = State::Armed;
                self.armed = bytes;
            }
        }
    }

    /// A whole key that is not the prefix, arriving as an escape sequence —
    /// or a sequence that turned out to be no key at all. Handled like any
    /// other non-command: the machine's pending business is settled and the
    /// bytes go through unchanged.
    fn other_key(&mut self, bytes: Vec<u8>, steps: &mut Vec<Step>, pending: &mut Vec<u8>) {
        match self.state {
            State::Idle => {}
            State::Armed => {
                // Not a command, so the user's original keystrokes are
                // replayed in order and nothing is eaten.
                self.state = State::Idle;
                pending.append(&mut self.armed);
            }
            State::Number(n) => {
                self.state = State::Idle;
                flush(steps, pending);
                steps.push(Step::Act(Action::Switch(n)));
            }
        }
        pending.extend(bytes);
    }

    /// An ordinary byte: neither the prefix nor part of an escape sequence.
    fn plain_byte(&mut self, b: u8, steps: &mut Vec<Step>, pending: &mut Vec<u8>) {
        match self.state {
            State::Idle => pending.push(b),

            State::Armed => {
                self.state = State::Idle;
                let mut armed = std::mem::take(&mut self.armed);
                if (b'1'..=b'9').contains(&b) {
                    // A number never starts with 0, so `<prefix> 0` falls
                    // through to the replay branch below.
                    self.start_number(u32::from(b - b'0'), steps, pending);
                } else if let Some(action) = command(b) {
                    flush(steps, pending);
                    steps.push(Step::Act(action));
                } else {
                    // Not a command, so the user's original keystrokes are
                    // replayed in order and nothing is eaten.
                    pending.append(&mut armed);
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
                    self.start_number(wider, steps, pending);
                } else if b == ENTER {
                    // An explicit "that is the whole number", so a user who
                    // knows the id never waits out the timeout.
                    self.state = State::Idle;
                    flush(steps, pending);
                    steps.push(Step::Act(Action::Switch(n)));
                } else {
                    // The digits already typed were a complete command, and
                    // this byte is simply the next key. Acting and then
                    // handling `b` afresh is what keeps `<prefix> 1 x` equivalent
                    // to `<prefix> Space x`: the command runs, the `x` reaches Neovim,
                    // and no stray prefix byte is injected.
                    self.state = State::Idle;
                    flush(steps, pending);
                    steps.push(Step::Act(Action::Switch(n)));
                    pending.push(b);
                }
            }
        }
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

    /// Called when the [`wait`](Self::wait) the machine asked for has passed
    /// with no further byte.
    ///
    /// Resolves a lone `<prefix>` into a literal one, a half-typed number into
    /// the session it already names, and a cut-off escape sequence into the
    /// bytes it was — which, as the key after a prefix or a number, settles
    /// those too. Idempotent, so a caller that fires its timer spuriously does
    /// no harm.
    pub fn timeout(&mut self) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut pending = Vec::new();
        if !self.seq.is_empty() {
            // The rest never came: whatever it was, it was not the prefix.
            let held = std::mem::take(&mut self.seq);
            self.other_key(held, &mut steps, &mut pending);
        } else {
            match std::mem::replace(&mut self.state, State::Idle) {
                State::Idle => {}
                State::Armed => pending.append(&mut self.armed),
                State::Number(n) => steps.push(Step::Act(Action::Switch(n))),
            }
        }
        flush(&mut steps, &mut pending);
        steps
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

    /// The README's "While attached" table is a prose copy of [`BINDINGS`], and
    /// a reader picking the tool up has only that copy — the `<prefix> ?` screen
    /// needs a running session. So every command must have a row there.
    ///
    /// The key cell, not the description: the two word things differently on
    /// purpose (the screen has one line, the README has a column), and pinning
    /// the prose would only force them to drift together. What actually goes
    /// wrong is a command added here and never written down.
    #[test]
    fn every_binding_has_a_row_in_the_readme() {
        let readme = include_str!("../README.md");
        for b in BINDINGS {
            let cell = format!("| `<prefix>` `{}` |", key_label(b.key));
            assert!(
                readme.contains(&cell),
                "README has no row for `<prefix> {}` — add one to \
                 the \"While attached\" table",
                key_label(b.key)
            );
        }
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
        let first = p.feed(b"ab\x00");
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

    /// Sequences are looked at — that is how an encoded prefix is found — but
    /// everything that is not the prefix goes through byte for byte, and none
    /// of it leaves the machine waiting once the sequence is whole.
    #[test]
    fn other_escape_sequences_pass_through_untouched() {
        let mut p = Prefix::new(0);
        // A bracketed paste wrapper, the terminal's reply to the kitty
        // keyboard query, arrows, a control chord in legacy form, F5, a mouse
        // report, a focus event, and Ctrl-d spelled the kitty way.
        let raw = b"\x1b[200~pasted\x1b[201~\x1b[?0u\x1b[A\x1b[1;5A\x1b[15~\x1b[<35;10;20M\x1b[I\x1b[100;5u";
        let steps = p.feed(raw);
        assert_eq!(forwarded(&steps), raw);
        assert!(actions(&steps).is_empty());
        assert_eq!(p.wait(), None, "nothing should be held: {steps:?}");
    }

    // The prefix spelled the way a terminal in an extended keyboard mode
    // spells it — see `keyseq` for the grammar; these pin the machine's use
    // of it.

    /// `Ctrl-Space` as the kitty keyboard protocol and xterm's
    /// `modifyOtherKeys` send it. This is the bug that motivated `keyseq`:
    /// Windows Terminal 1.25 speaks the first, Neovim asks for it, and
    /// `<prefix> d` reached the editor as the prefix and a pending delete.
    const KITTY: &[u8] = b"\x1b[32;5u";
    const XTERM: &[u8] = b"\x1b[27;5;32~";

    #[test]
    fn an_encoded_prefix_arms_and_its_command_acts() {
        for spelling in [
            KITTY,
            XTERM,
            b"\x1b[32;5:1u",
            b"\x1b[32;5:2u",
            b"\x1b[32;133u",
        ] {
            let mut p = Prefix::new(0);
            let steps = p.feed(spelling);
            assert!(
                steps.is_empty(),
                "{spelling:?} must be swallowed, got {steps:?}"
            );
            assert_eq!(p.wait(), Some(Wait::Command), "{spelling:?} must arm");

            let steps = p.feed(b"d");
            assert_eq!(actions(&steps), vec![Action::Detach], "{spelling:?}");
            assert!(forwarded(&steps).is_empty(), "{spelling:?}");
            assert!(!p.is_armed());
        }
    }

    /// The picker key is the prefix chord's own key without the Ctrl, so both
    /// arrive under the same key code (32) in a terminal that reports releases.
    /// The chord's release belongs to its press and is held with it; the bare
    /// byte that follows is the command, and neither reaches Neovim.
    #[test]
    fn the_prefix_then_its_bare_key_opens_the_picker() {
        for spelling in [&[PREFIX][..], KITTY, XTERM] {
            let mut p = Prefix::new(0);
            assert!(p.feed(spelling).is_empty(), "{spelling:?}");
            let steps = p.feed(b" ");
            assert_eq!(actions(&steps), vec![Action::Picker], "{spelling:?}");
            assert!(forwarded(&steps).is_empty(), "{spelling:?}");
            assert!(!p.is_armed(), "{spelling:?}");
        }

        let mut p = Prefix::new(0);
        p.feed(KITTY);
        assert!(p.feed(b"\x1b[32;5:3u").is_empty(), "held with its press");
        let steps = p.feed(b" ");
        assert_eq!(actions(&steps), vec![Action::Picker]);
        assert!(forwarded(&steps).is_empty(), "neither reaches Neovim");
    }

    /// Both spellings in one read, with the command and ordinary text around
    /// them, in the right order.
    #[test]
    fn an_encoded_prefix_inside_a_larger_chunk_splits_correctly() {
        let mut p = Prefix::new(0);
        let mut input = b"before".to_vec();
        input.extend_from_slice(KITTY);
        input.extend_from_slice(b" middle");
        input.extend_from_slice(XTERM);
        input.extend_from_slice(b"dafter");
        let steps = p.feed(&input);
        assert_eq!(
            steps,
            vec![
                Step::Forward(b"before".to_vec()),
                Step::Act(Action::Picker),
                Step::Forward(b"middle".to_vec()),
                Step::Act(Action::Detach),
                Step::Forward(b"after".to_vec()),
            ]
        );
    }

    /// A read can end anywhere inside the sequence — including right after
    /// the `ESC`, which is also what a bare Escape key looks like. Every
    /// split must still find the prefix.
    #[test]
    fn an_encoded_prefix_survives_a_chunk_boundary_at_every_byte() {
        for spelling in [KITTY, XTERM] {
            for cut in 1..spelling.len() {
                let mut p = Prefix::new(0);
                let first = p.feed(&spelling[..cut]);
                assert!(first.is_empty(), "cut at {cut}: {first:?}");
                assert_eq!(
                    p.wait(),
                    Some(Wait::Sequence),
                    "cut at {cut}: the rest must be waited for, briefly"
                );
                let second = p.feed(&spelling[cut..]);
                assert!(second.is_empty(), "cut at {cut}: {second:?}");
                assert_eq!(p.wait(), Some(Wait::Command), "cut at {cut}");

                let steps = p.feed(b" ");
                assert_eq!(actions(&steps), vec![Action::Picker], "cut at {cut}");
            }
        }
    }

    /// The literal is what the terminal sent, not the control byte: a terminal
    /// that spells the chord as a sequence gets its sequence back.
    #[test]
    fn a_doubled_encoded_prefix_replays_the_terminal_s_own_spelling() {
        let mut p = Prefix::new(0);
        let mut both = KITTY.to_vec();
        both.extend_from_slice(KITTY);
        let steps = p.feed(&both);
        assert_eq!(forwarded(&steps), KITTY);
        assert!(actions(&steps).is_empty());
        assert!(!p.is_armed());

        // Mixed spellings are the same key: the first one is what is replayed.
        let mut p = Prefix::new(0);
        let mut mixed = XTERM.to_vec();
        mixed.push(PREFIX);
        assert_eq!(forwarded(&p.feed(&mixed)), XTERM);
        let mut p = Prefix::new(0);
        let mut mixed = vec![PREFIX];
        mixed.extend_from_slice(KITTY);
        assert_eq!(forwarded(&p.feed(&mixed)), vec![PREFIX]);
    }

    #[test]
    fn an_encoded_prefix_alone_is_replayed_on_the_timeout() {
        let mut p = Prefix::new(0);
        p.feed(KITTY);
        assert_eq!(forwarded(&p.timeout()), KITTY);
        assert!(!p.is_armed());
        assert!(p.timeout().is_empty(), "idempotent");
    }

    /// The key after an encoded prefix is not a command: both are replayed,
    /// each in the spelling it arrived in.
    #[test]
    fn an_encoded_prefix_then_another_key_replays_both() {
        // A plain letter, and a key that is itself a sequence (Escape, as the
        // kitty protocol spells it).
        for other in [&b"x"[..], b"\x1b[27u", b"\x1b[100;5u"] {
            let mut p = Prefix::new(0);
            let mut input = KITTY.to_vec();
            input.extend_from_slice(other);
            let steps = p.feed(&input);
            let mut want = KITTY.to_vec();
            want.extend_from_slice(other);
            assert_eq!(forwarded(&steps), want, "after {other:?}");
            assert!(actions(&steps).is_empty());
            assert!(!p.is_armed());
        }
    }

    /// The other direction: a byte prefix followed by a sequence that is not
    /// a command (an arrow) replays both, and `<prefix> <sequence-prefix>` is
    /// still a literal.
    #[test]
    fn a_byte_prefix_then_a_sequence_is_handled_like_any_other_key() {
        let mut p = Prefix::new(0);
        let steps = p.feed(b"\x00\x1b[A");
        assert_eq!(forwarded(&steps), b"\x00\x1b[A");
        assert!(!p.is_armed());

        let mut p = Prefix::new(0);
        let mut input = vec![PREFIX];
        input.extend_from_slice(KITTY);
        assert_eq!(forwarded(&p.feed(&input)), vec![PREFIX]);
        assert!(!p.is_armed());
    }

    /// A terminal asked for release events reports one for every key,
    /// including the prefix just swallowed. None of them is a keystroke:
    /// they go through, and the machine stays where it was.
    #[test]
    fn key_release_reports_are_transparent_in_every_state() {
        let release_prefix = b"\x1b[32;5:3u";
        let release_d = b"\x1b[100;1:3u";
        let release_one = b"\x1b[49;1:3u";

        // Idle.
        let mut p = Prefix::new(12);
        assert_eq!(forwarded(&p.feed(release_prefix)), release_prefix);
        assert!(!p.is_armed());

        // Armed: the prefix's own release must not count as the next key —
        // nor reach Neovim ahead of the press it belongs to, which is still
        // held back; another key's release goes straight through.
        let mut p = Prefix::new(12);
        p.feed(KITTY);
        assert!(p.feed(release_prefix).is_empty(), "held with its press");
        assert_eq!(p.wait(), Some(Wait::Command), "still armed");
        assert_eq!(forwarded(&p.feed(release_d)), release_d);
        assert_eq!(p.wait(), Some(Wait::Command), "still armed");
        let steps = p.feed(b"d");
        assert_eq!(actions(&steps), vec![Action::Detach]);
        assert!(forwarded(&steps).is_empty(), "a command drops both");

        // Number: releasing the `1` must not end the number.
        let mut p = Prefix::new(12);
        p.feed(KITTY);
        p.feed(b"1");
        let steps = p.feed(release_one);
        assert_eq!(forwarded(&steps), release_one);
        assert_eq!(p.wait(), Some(Wait::Command), "still a number");
        assert_eq!(actions(&p.feed(b"2")), vec![Action::Switch(12)]);
    }

    /// The editor queries the terminal at every start and resume, and the
    /// answer can land between a prefix and its command — as it does when a
    /// prefix is typed the moment the client comes up. An answer is not a key.
    #[test]
    fn a_terminal_reply_between_the_prefix_and_its_command_is_not_a_key() {
        let da1 = b"\x1b[?62;22c";
        let mut p = Prefix::new(12);
        p.feed(KITTY);
        assert_eq!(forwarded(&p.feed(da1)), da1, "the answer goes through");
        assert_eq!(p.wait(), Some(Wait::Command), "still armed");
        assert_eq!(actions(&p.feed(b"d")), vec![Action::Detach]);

        let mut p = Prefix::new(12);
        p.feed(&[PREFIX, b'1']);
        assert_eq!(forwarded(&p.feed(b"\x1b[?0u")), b"\x1b[?0u");
        assert_eq!(p.wait(), Some(Wait::Command), "still a number");
        assert_eq!(actions(&p.feed(b"2")), vec![Action::Switch(12)]);
    }

    /// The press and the release of a literal prefix reach Neovim in the
    /// order they were typed, whether the literal comes from a doubled
    /// prefix or from the wait passing.
    #[test]
    fn a_literal_prefix_replays_its_press_and_release_in_order() {
        let release = b"\x1b[32;1:3u";
        let mut both = KITTY.to_vec();
        both.extend_from_slice(release);

        let mut p = Prefix::new(0);
        p.feed(KITTY);
        p.feed(release);
        assert_eq!(forwarded(&p.feed(KITTY)), both);
        assert!(!p.is_armed());

        let mut p = Prefix::new(0);
        p.feed(KITTY);
        p.feed(release);
        assert_eq!(forwarded(&p.timeout()), both);
        assert!(!p.is_armed());
    }

    /// Holding the key down repeats it, and a repeat is a press: the first
    /// repeat makes a literal, the next arms again — as a held-down control
    /// byte does in a terminal without the protocol.
    #[test]
    fn a_held_down_encoded_prefix_alternates_like_a_held_down_byte() {
        let repeat = b"\x1b[32;5:2u";
        let mut p = Prefix::new(0);
        let mut input = KITTY.to_vec();
        input.extend_from_slice(repeat);
        input.extend_from_slice(repeat);
        let steps = p.feed(&input);
        assert_eq!(
            forwarded(&steps),
            KITTY,
            "one literal, in the press's spelling"
        );
        assert!(actions(&steps).is_empty());
        assert_eq!(p.wait(), Some(Wait::Command), "the third press arms again");

        let mut p = Prefix::new(0);
        assert_eq!(forwarded(&p.feed(&[PREFIX, PREFIX, PREFIX])), vec![PREFIX]);
        assert!(p.is_armed());
    }

    /// An encoded prefix after a half-typed number ends the number and arms
    /// again, exactly as the byte does.
    #[test]
    fn an_encoded_prefix_after_a_number_ends_it_and_arms_again() {
        let mut p = Prefix::new(12);
        p.feed(&[PREFIX]);
        p.feed(b"1");
        let steps = p.feed(KITTY);
        assert_eq!(actions(&steps), vec![Action::Switch(1)]);
        assert!(forwarded(&steps).is_empty());
        assert_eq!(p.wait(), Some(Wait::Command));
        assert_eq!(actions(&p.feed(b" ")), vec![Action::Picker]);
    }

    /// The wait for the rest of a sequence is the short one, and it wins over
    /// the command wait: settling the sequence settles the command too.
    #[test]
    fn a_cut_off_sequence_asks_for_the_short_wait_in_every_state() {
        // Idle: a bare ESC, which in a terminal without the protocol is the
        // Escape key. It is held, then goes through as it stands.
        let mut p = Prefix::new(12);
        assert!(p.feed(b"\x1b").is_empty());
        assert_eq!(p.wait(), Some(Wait::Sequence));
        assert_eq!(forwarded(&p.timeout()), b"\x1b");
        assert!(!p.is_armed());

        // Armed: it was the key after the prefix, so both are replayed.
        let mut p = Prefix::new(12);
        p.feed(KITTY);
        assert_eq!(p.wait(), Some(Wait::Command));
        p.feed(b"\x1b[1");
        assert_eq!(p.wait(), Some(Wait::Sequence));
        let mut want = KITTY.to_vec();
        want.extend_from_slice(b"\x1b[1");
        assert_eq!(forwarded(&p.timeout()), want);
        assert!(!p.is_armed());

        // Number: the digits were the whole command, and the sequence follows.
        let mut p = Prefix::new(12);
        p.feed(&[PREFIX, b'1']);
        p.feed(b"\x1b");
        assert_eq!(p.wait(), Some(Wait::Sequence));
        let steps = p.timeout();
        assert_eq!(actions(&steps), vec![Action::Switch(1)]);
        assert_eq!(forwarded(&steps), b"\x1b");
        assert!(!p.is_armed());
    }

    /// A held `ESC` followed by something that cannot continue it: the `ESC`
    /// was the Escape key, and the next byte is the next key — which may be
    /// the prefix.
    #[test]
    fn an_escape_then_an_unrelated_byte_forwards_the_escape_and_goes_on() {
        let mut p = Prefix::new(0);
        assert!(p.feed(b"\x1b").is_empty());
        let steps = p.feed(b":");
        assert_eq!(forwarded(&steps), b"\x1b:");
        assert!(!p.is_armed());

        let mut p = Prefix::new(0);
        assert!(p.feed(b"\x1b").is_empty());
        let steps = p.feed(&[PREFIX]);
        assert_eq!(forwarded(&steps), b"\x1b", "the ESC goes through");
        assert_eq!(p.wait(), Some(Wait::Command), "and the prefix byte arms");

        // A broken sequence, then the prefix byte: the prefix still arms.
        let mut p = Prefix::new(0);
        let steps = p.feed(b"\x1b[11\x00");
        assert_eq!(forwarded(&steps), b"\x1b[11");
        assert!(p.is_armed());
        assert_eq!(actions(&p.feed(b"d")), vec![Action::Detach]);
    }

    /// Only the exact chord: `Ctrl-Shift-Space`, `Ctrl-d`, and a key with an
    /// extra modifier all go through untouched.
    #[test]
    fn a_different_chord_in_the_same_spelling_is_not_the_prefix() {
        for seq in [
            &b"\x1b[32;6u"[..],
            b"\x1b[100;5u",
            b"\x1b[32;7u",
            b"\x1b[27;6;32~",
        ] {
            let mut p = Prefix::new(0);
            let mut input = seq.to_vec();
            input.push(b'd');
            let steps = p.feed(&input);
            assert_eq!(forwarded(&steps), input, "{seq:?}");
            assert!(actions(&steps).is_empty(), "{seq:?}");
            assert!(!p.is_armed(), "{seq:?}");
        }
    }

    /// A remapped prefix is recognised under its own letter's code, and the
    /// default's code is then just another key.
    #[test]
    fn a_remapped_prefix_is_recognised_in_every_spelling() {
        let ctrl_a = 0x01;
        for spelling in [&[ctrl_a][..], b"\x1b[97;5u", b"\x1b[27;5;97~"] {
            let mut p = Prefix::with_prefix(0, ctrl_a);
            assert!(p.feed(spelling).is_empty(), "{spelling:?}");
            assert_eq!(actions(&p.feed(b"d")), vec![Action::Detach], "{spelling:?}");
        }
        let mut p = Prefix::with_prefix(0, ctrl_a);
        assert_eq!(forwarded(&p.feed(KITTY)), KITTY);
        assert!(!p.is_armed());
    }

    /// The key a terminal reports the chord under is the one the label names,
    /// for every prefix the parser accepts.
    #[test]
    fn the_prefix_code_is_the_key_the_label_names() {
        for byte in 1u8..=26 {
            let label = prefix_label(byte);
            assert_eq!(
                label.as_bytes().last().copied(),
                Some(prefix_code(byte)),
                "for {label}"
            );
            assert!(prefix_code(byte).is_ascii_lowercase());
        }
        // The one prefix that is not a letter: the label spells the key out,
        // and the terminal reports it under the space bar's own code.
        assert_eq!(prefix_label(CTRL_SPACE), "Ctrl-Space");
        assert_eq!(prefix_code(CTRL_SPACE), b' ');
    }

    /// A pasted burst never arms the machine on its way through, and never
    /// leaves anything held once it is whole.
    #[test]
    fn a_large_paste_full_of_sequences_is_forwarded_whole() {
        let mut p = Prefix::new(30);
        let mut paste = b"\x1b[200~".to_vec();
        for _ in 0..500 {
            paste.extend_from_slice(b"line 12 \x1b[A\x1b[31mred\x1b[0m\n");
        }
        paste.extend_from_slice(b"\x1b[201~");
        let steps = p.feed(&paste);
        assert_eq!(forwarded(&steps), paste);
        assert!(actions(&steps).is_empty());
        assert!(!p.is_armed());
    }

    #[test]
    fn a_prefix_inside_a_larger_chunk_splits_correctly() {
        let mut p = Prefix::new(0);
        let steps = p.feed(b"before\x00 after");
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
        let steps = p.feed(b"\x00 \x00d\x00c\x00n\x00p\x00?");
        assert_eq!(
            actions(&steps),
            vec![
                Action::Picker,
                Action::Detach,
                Action::Create,
                Action::Cycle(Direction::Next),
                Action::Cycle(Direction::Prev),
                Action::Help,
            ]
        );
        assert!(forwarded(&steps).is_empty());
    }

    /// `n` and `p` are the two keys that name a *relative* target, so the
    /// direction has to survive the machine rather than being decided later.
    #[test]
    fn n_and_p_step_forwards_and_backwards() {
        for (key, want) in [(b'n', Direction::Next), (b'p', Direction::Prev)] {
            let mut p = Prefix::new(0);
            let steps = p.feed(&[PREFIX, key]);
            assert_eq!(
                actions(&steps),
                vec![Action::Cycle(want)],
                "{}",
                key as char
            );
            assert!(
                forwarded(&steps).is_empty(),
                "{} must not also reach Neovim",
                key as char
            );
        }
    }

    /// A doubled prefix is a literal before the table is consulted, so the two
    /// new keys are still typable in the editor.
    #[test]
    fn a_literal_prefix_then_n_still_reaches_neovim() {
        let mut p = Prefix::new(0);
        let steps = p.feed(&[PREFIX, PREFIX, b'n']);
        assert_eq!(forwarded(&steps), vec![PREFIX, b'n']);
        assert!(actions(&steps).is_empty());
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
    /// left mid-sequence — except `ESC`, which could be the start of another
    /// spelling of the prefix and is held until the wait passes.
    #[test]
    fn exhaustive_second_byte_table() {
        for b in 0u8..=255 {
            let mut p = Prefix::new(0);
            let steps = p.feed(&[PREFIX, b]);
            // Same order as `feed`: the literal rule, then digits, then the table.
            if b == PREFIX {
                assert_eq!(forwarded(&steps), vec![PREFIX]);
            } else if b == ESC {
                assert!(steps.is_empty(), "a bare ESC is held, not replayed yet");
                assert_eq!(p.wait(), Some(Wait::Sequence));
                assert_eq!(
                    forwarded(&p.timeout()),
                    vec![PREFIX, ESC],
                    "the wait passing replays both, in order"
                );
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
        let all = [
            Action::Picker,
            Action::Detach,
            Action::Create,
            Action::Cycle(Direction::Next),
            Action::Cycle(Direction::Prev),
            Action::Help,
        ];
        for action in all {
            match action {
                // `Cycle` carries data and is still listed twice above: unlike a
                // session number, each direction has one key of its own.
                Action::Picker
                | Action::Detach
                | Action::Create
                | Action::Cycle(_)
                | Action::Help => {}
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
                a.key.is_ascii() && !a.key.is_ascii_control(),
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

    /// The hint bar is one row and reads at a glance, so a label that wrapped,
    /// ran to two words or arrived capitalised would break the row rather than
    /// merely look wrong — the grammar is `src/ui/draw.rs`'s, which is
    /// uniformly lowercase and one word per key.
    #[test]
    fn every_binding_has_a_one_word_hint() {
        for b in BINDINGS {
            let hint = b.hint;
            assert!(!hint.is_empty(), "{:?} has no hint label", b.key as char);
            assert!(
                !hint.contains(char::is_whitespace),
                "{:?}'s hint {hint:?} is more than one word",
                b.key as char
            );
            assert_eq!(
                hint,
                hint.to_lowercase(),
                "{:?}'s hint is not lowercase",
                b.key as char
            );
        }
    }

    /// Two commands labelled the same thing make the bar say one of them
    /// twice and neither of them clearly.
    #[test]
    fn no_two_bindings_share_a_hint() {
        for (i, a) in BINDINGS.iter().enumerate() {
            for b in &BINDINGS[i + 1..] {
                assert_ne!(a.hint, b.hint, "two commands are labelled {:?}", a.hint);
            }
        }
    }

    /// `pending` is what the hint bar is drawn from, so it has to follow the
    /// machine exactly: a bar that appeared without the prefix, or stayed up
    /// after the command ran, would be drawn over an editor that owns the row.
    #[test]
    fn pending_follows_the_state_machine() {
        let mut p = Prefix::new(12);
        assert_eq!(p.pending(), None, "nothing is pending from idle");

        p.feed(&[PREFIX]);
        assert_eq!(p.pending(), Some(Pending::Command), "a lone prefix arms");
        p.feed(&[PREFIX]);
        assert_eq!(p.pending(), None, "a literal prefix resolves it");

        p.feed(&[PREFIX, b'1']);
        assert_eq!(p.pending(), Some(Pending::Number(1)), "a half-typed number");
        p.feed(b"2");
        assert_eq!(p.pending(), None, "the twelfth session is named");

        p.feed(&[PREFIX, b'd']);
        assert_eq!(p.pending(), None, "a command resolves it");

        p.feed(&[PREFIX]);
        p.timeout();
        assert_eq!(p.pending(), None, "so does the timeout");
    }

    /// The bar must not blink between the prefix and a next key the terminal
    /// spells as a sequence: those bytes arrive over more than one `read` at a
    /// human typing speed, and the prefix is still waiting throughout.
    ///
    /// And the other way round: a sequence cut short from *idle* is the Escape
    /// key arriving, which is not a pending command however long the machine
    /// holds it. That is the distinction `pending` reads `state` for rather
    /// than `wait`, which answers `Some` to both.
    #[test]
    fn pending_ignores_a_half_read_sequence() {
        let mut p = Prefix::new(0);
        p.feed(b"\x1b[1");
        assert_eq!(p.wait(), Some(Wait::Sequence), "mid-sequence");
        assert_eq!(p.pending(), None, "the Escape key is not a command");

        let mut p = Prefix::new(0);
        p.feed(&[PREFIX]);
        p.feed(b"\x1b[1");
        assert_eq!(p.wait(), Some(Wait::Sequence), "mid-sequence");
        assert_eq!(
            p.pending(),
            Some(Pending::Command),
            "the prefix is still waiting"
        );

        // The prefix's own release report, which is held back with its press:
        // not a key, so not the end of the wait either.
        let mut p = Prefix::new(0);
        p.feed(KITTY);
        p.feed(b"\x1b[32;5:3u");
        assert_eq!(
            p.pending(),
            Some(Pending::Command),
            "a release is not a key"
        );
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
    /// simply the next key, exactly as it is after `<prefix> Space`.
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
    /// what the machine matches. Tie the two together so they cannot disagree,
    /// in both directions.
    #[test]
    fn the_prefix_label_names_the_prefix_byte() {
        assert_eq!(prefix_label(PREFIX), PREFIX_LABEL);
        assert_eq!(parse_prefix(PREFIX_LABEL), Ok(PREFIX));
    }

    #[test]
    fn a_prefix_string_parses_to_its_control_byte() {
        assert_eq!(parse_prefix("C-Space"), Ok(PREFIX));
        assert_eq!(parse_prefix("C-t"), Ok(0x14));
        assert_eq!(parse_prefix("Ctrl-a"), Ok(0x01));
        assert_eq!(parse_prefix("C-z"), Ok(0x1a));
    }

    /// Both halves are case-insensitive, and surrounding space is ignored.
    #[test]
    fn a_prefix_string_is_case_insensitive() {
        assert_eq!(parse_prefix("ctrl-T"), Ok(0x14));
        assert_eq!(parse_prefix("c-A"), Ok(0x01));
        assert_eq!(parse_prefix("  Ctrl-B  "), Ok(0x02));
        assert_eq!(parse_prefix("  c-sPaCe  "), Ok(CTRL_SPACE));
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
        // And the one label that is a name rather than a letter.
        assert_eq!(parse_prefix(&prefix_label(CTRL_SPACE)), Ok(CTRL_SPACE));
    }

    /// Generalises [`the_prefix_label_names_the_prefix_byte`] beyond the default.
    #[test]
    fn the_prefix_label_names_any_control_byte() {
        assert_eq!(prefix_label(0x01), "Ctrl-a");
        assert_eq!(prefix_label(0x14), "Ctrl-t");
        assert_eq!(prefix_label(0x1a), "Ctrl-z");
        assert_eq!(prefix_label(CTRL_SPACE), "Ctrl-Space");
    }

    #[test]
    fn a_malformed_prefix_is_rejected() {
        for s in [
            "t", "C-1", "C-", "", "hyper-t", "C-ab", "C-é", "Space", "C-spa",
        ] {
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

        let steps = p.feed(b" ");
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
