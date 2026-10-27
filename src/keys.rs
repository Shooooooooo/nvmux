//! The `<prefix>` state machine.
//!
//! It sits in the stdin half of the PTY proxy and is the only thing that
//! inspects bytes going to Neovim; output is never rewritten (see
//! [`crate::pty`]). Pure: timing is the caller's, via [`Prefix::timeout`], so
//! it is testable without a PTY, terminal or clock.
//!
//! [`BINDINGS`] is the one command table (default key, name, help).
//! `[keys.bind]` can move keys ([`crate::config`]); a [`Keymap`] is the table
//! as moved, read by [`Prefix::feed`], the help screen ([`crate::ui::help`])
//! and the hint bar ([`crate::hint`]), so the help cannot miss a command. The
//! README's "While attached" table is a hand-kept copy; a test checks only its
//! names and default keys.
//!
//! `Ctrl-z` (0x1a) is not special: it is forwarded to Neovim.
//!
//! # How the prefix is spelled
//!
//! The prefix is a `Ctrl` chord, sent as a control byte (`NUL` for the default
//! `Ctrl-Space`) or, once Neovim's TUI enables the kitty keyboard protocol or
//! `modifyOtherKeys` (at every start), as an escape sequence. [`crate::keyseq`]
//! knows the spellings; all are the same key here.
//!
//! A sequence may be split across `read()`s, so an unfinished one is held
//! until the rest arrives or the caller's [`Wait::Sequence`] passes, then
//! forwarded as is (this is how a bare `Esc` reaches Neovim). A literal
//! `<prefix>` replays the exact bytes that spelled the prefix.
//!
//! Key-release reports (Neovim 0.12 asks for them) and replies to the editor's
//! terminal queries also arrive on stdin. They are not the next key and pass
//! through unchanged, except the prefix's own release while its press is held,
//! which shares the press's fate. Neovim sees input in its original order,
//! minus what the machine consumed.
//!
//! # Digits
//!
//! `<prefix> 1`-`9` select a session by number, possibly multi-digit, so a
//! digit after the prefix never reaches Neovim (`<prefix> <prefix> 1` sends
//! it). No number starts with `0`, so `<prefix> 0` replays both bytes.
//! [`Prefix::new`] takes the highest session number so the wait for another
//! digit happens only when it could change the answer (never with nine or
//! fewer sessions). Whether the session exists is the caller's concern.

use crate::keyseq::{self, Sequence, ESC};

/// The byte `Ctrl-Space` sends: `NUL`. The only prefix chord that is not
/// `Ctrl-<letter>`, so [`prefix_label`] and [`prefix_code`] special-case it.
const CTRL_SPACE: u8 = 0x00;

/// The space bar's name in config files, the help screen and the README.
const SPACE_NAME: &str = "Space";

/// `Ctrl-Space`.
pub const PREFIX: u8 = CTRL_SPACE;

/// The prefix as shown on the help screen, the README and `--help`. A test
/// ties it to [`PREFIX`].
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
    /// `<prefix> Space`: suspend the relay and show the picker. The child
    /// stays alive.
    Picker,
    /// `<prefix> d`: exit the local UI, leaving the server running.
    Detach,
    /// `<prefix> c`: create a new session and attach to it.
    Create,
    /// `<prefix> ?`: show the key bindings. The child stays alive.
    Help,
    /// `<prefix> <number>`: attach to that session, leaving this one running.
    /// Has no [`BINDINGS`] row; digits are a rule in [`Prefix::feed`].
    Switch(u32),
    /// `<prefix> n` / `<prefix> p`: attach to the neighbouring session in the
    /// picker's order, wrapping.
    Cycle(Direction),
    /// `<prefix> Enter`: go where the newest margin note is about (its
    /// session, or this session's terminal; see [`crate::card`]). Nothing if
    /// no such note is up.
    Go,
    /// `<prefix> ,`: change settings while running (see
    /// [`crate::ui::settings`]). The child stays alive.
    Settings,
}

/// One instruction from the machine, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Write these bytes to the PTY master, verbatim.
    Forward(Vec<u8>),
    Act(Action),
}

/// One `<prefix>` command: its key, action, and help text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    /// The byte after the prefix: the default in [`BINDINGS`], the current key
    /// from [`Keymap::bindings`]. Printable ASCII or Enter; [`key_label`]
    /// names Space (else a blank cell) and Enter (not a character).
    pub key: u8,
    pub action: Action,
    /// One line, in terms of what happens to the user.
    pub help: &'static str,
    /// One lowercase word for the hint bar ([`crate::hint`]), and the
    /// command's name in `[keys.bind]`. Renaming it breaks config files using
    /// the old name, starting with the README's, which a test parses.
    pub hint: &'static str,
}

/// Every command, in the order the help screen lists them.
pub const BINDINGS: &[Binding] = &[
    Binding {
        // The prefix chord's own key, without the Ctrl.
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
        key: ENTER,
        action: Action::Go,
        help: "go where the newest note in the margin is about",
        hint: "go",
    },
    Binding {
        key: b'?',
        action: Action::Help,
        help: "show these keys",
        hint: "help",
    },
    // Last: the newest, and the one a narrow hint bar can best do without.
    Binding {
        key: b',',
        action: Action::Settings,
        help: "change nvmux's settings while it runs",
        hint: "settings",
    },
];

const COMMANDS: usize = BINDINGS.len();

/// Each command's `[keys.bind]` name ([`hint`](Binding::hint)) in
/// [`BINDINGS`] order, listed when a config names an unknown command.
pub(crate) static NAMES: [&str; COMMANDS] = {
    let mut names = [""; COMMANDS];
    let mut i = 0;
    while i < COMMANDS {
        names[i] = BINDINGS[i].hint;
        i += 1;
    }
    names
};

/// The key each command is on: the default, or where `[keys.bind]` moved it
/// ([`crate::config`]). One key per [`BINDINGS`] row; only keys can move. The
/// machine, help screen and hint bar all read through this, so they agree.
///
/// Two commands may share a key here; [`clash`](Keymap::clash) finds them and
/// the config refuses to load. No command can be on a digit or the prefix:
/// [`parse_key`] refuses the one and cannot spell the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keymap([u8; COMMANDS]);

/// The derive would put every command on `NUL`; the default is the table's.
impl Default for Keymap {
    fn default() -> Self {
        Self(std::array::from_fn(|i| BINDINGS[i].key))
    }
}

impl Keymap {
    /// [`BINDINGS`] with each key as currently bound.
    pub fn bindings(&self) -> impl Iterator<Item = Binding> {
        BINDINGS
            .iter()
            .zip(self.0)
            .map(|(b, key)| Binding { key, ..*b })
    }

    /// The command a second byte selects, if any. A linear scan; a `match`
    /// would duplicate the table.
    pub fn command(&self, byte: u8) -> Option<Action> {
        self.bindings().find(|b| b.key == byte).map(|b| b.action)
    }

    /// `None` only for [`Action::Switch`], which has no row.
    pub fn key(&self, action: Action) -> Option<u8> {
        self.bindings().find(|b| b.action == action).map(|b| b.key)
    }

    /// Put the command named `name` (its hint word) on `key`. False, and
    /// nothing moved, if there is no such command.
    pub fn bind(&mut self, name: &str, key: u8) -> bool {
        match NAMES.iter().position(|n| *n == name) {
            Some(i) => {
                self.0[i] = key;
                true
            }
            None => false,
        }
    }

    /// The first two commands (by name, in table order) that share a key, and
    /// the key. The second could never be reached.
    pub fn clash(&self) -> Option<(&'static str, &'static str, u8)> {
        (0..COMMANDS).find_map(|i| {
            (i + 1..COMMANDS)
                .find(|&j| self.0[j] == self.0[i])
                .map(|j| (NAMES[i], NAMES[j], self.0[i]))
        })
    }
}

/// Parse a prefix like `"C-Space"`, `"C-t"` or `"Ctrl-a"` into its control
/// byte. Case-insensitive; the inverse of [`prefix_label`].
///
/// Only `Ctrl-<letter>` or `Ctrl-Space` (`NUL`): a single raw-mode byte that
/// is typable and never ordinary text. `C-h`, `C-i`, `C-j` are refused (they
/// mean something else on the wire), and `C-m` would clash with `ENTER`, which
/// ends a number. `C-c` and `C-z` are allowed, as in tmux; that byte just stops
/// reaching Neovim.
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

/// Parse a `[keys.bind]` key: one character, or `Space` / `Enter` in any case
/// (as [`key_label`] prints them).
///
/// Only keys that arrive as a single byte in every keyboard protocol
/// ([`crate::keyseq`]): printable ASCII (sent as text, even shifted), Space,
/// and Enter (a carriage return even under kitty). Not digits (session
/// numbers), not chords (escape sequences the machine passes on), and never
/// the prefix (Enter is the only control byte, and [`parse_prefix`] refuses
/// it). Case matters: `D` is Shift-`d`, a key of its own.
pub fn parse_key(s: &str) -> Result<u8, String> {
    if s.eq_ignore_ascii_case(SPACE_NAME) {
        return Ok(b' ');
    }
    if s.eq_ignore_ascii_case(ENTER_NAME) {
        return Ok(ENTER);
    }
    let mut chars = s.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return Err(format!(
            "key {s:?} must be one character, \"Space\" or \"Enter\""
        ));
    };
    let Some(byte) = u8::try_from(c)
        .ok()
        .filter(|b| *b == b' ' || b.is_ascii_graphic())
    else {
        return Err(format!(
            "key {s:?} must be a printable ASCII character, \"Space\" or \"Enter\""
        ));
    };
    if byte.is_ascii_digit() {
        return Err(format!(
            "key {s:?} is a digit, and a digit after the prefix picks a session"
        ));
    }
    Ok(byte)
}

/// A key as people read it: the character itself, or a name for Space
/// (otherwise invisible) and Enter (not a character).
pub fn key_label(byte: u8) -> String {
    match byte {
        b' ' => SPACE_NAME.to_string(),
        ENTER => ENTER_NAME.to_string(),
        _ => (byte as char).to_string(),
    }
}

/// What the Enter key is called on the help screen and in the README.
pub const ENTER_NAME: &str = "Enter";

/// Enter on a one-line hint row or a card, as on the picker's row.
pub const ENTER_GLYPH: &str = "⏎";

/// Space on a one-line hint row (U+2423), instead of [`SPACE_NAME`]: those rows
/// are all lowercase and a glyph is neither case. One column wide (no Emoji
/// property).
pub const SPACE_GLYPH: &str = "␣";

/// A key as one-line hint rows spell it (the picker's in `src/ui/draw.rs`, and
/// [`crate::hint`]): glyphs for Space and Enter, else the character with its
/// case, since `D` and `d` are different keys. [`key_label`] is the named form
/// for the help screen and README.
pub fn key_glyph(byte: u8) -> String {
    match byte {
        b' ' => SPACE_GLYPH.to_string(),
        ENTER => ENTER_GLYPH.to_string(),
        _ => key_label(byte),
    }
}

/// `0x00` -> `"Ctrl-Space"`, `0x14` -> `"Ctrl-t"`; the inverse of
/// [`parse_prefix`], so a remapped prefix is shown as configured.
pub fn prefix_label(byte: u8) -> String {
    format!("Ctrl-{}", key_label(prefix_code(byte)))
}

/// The key code terminals report the prefix chord under ([`crate::keyseq`]):
/// the letter for `Ctrl-<letter>` (`0x14` is `'t'`), Space's code for `NUL`.
fn prefix_code(byte: u8) -> u8 {
    if byte == CTRL_SPACE {
        b' '
    } else {
        byte | 0x60
    }
}

/// Enter in raw mode (carriage return). Ends a number without waiting out
/// `keys.timeout_ms`.
const ENTER: u8 = 0x0d;

/// Where the machine is between keystrokes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    /// A `<prefix>` was swallowed; waiting for the next key.
    Armed,
    /// Digits accumulating into a session number.
    Number(u32),
}

/// What the caller must put a clock on, if anything. See [`Prefix::wait`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// A lone `<prefix>` or a half-typed number: give the user
    /// `keys.timeout_ms` to finish it.
    Command,
    /// An escape sequence cut short by the last read: give the rest a moment.
    /// Keep it short (machine time, as only a buffer boundary splits a key's
    /// bytes): a bare `Esc` is held for exactly this long.
    Sequence,
}

/// What the user has half-typed, for the hint bar ([`crate::hint`]). Not
/// [`Wait`], which is what the caller must time and so also covers unfinished
/// escape sequences. A read-only view of the private [`State`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// A lone `<prefix>`: the next key picks a command.
    Command,
    /// The digits typed so far, waiting to see whether another follows.
    Number(u32),
}

/// The prefix state machine. The prefix byte and keymap come in through the
/// constructor rather than a global read, keeping it pure.
#[derive(Debug)]
pub struct Prefix {
    state: State,
    /// The highest live session number, used only to decide whether another
    /// digit could change the answer; stale is harmless.
    highest: u32,
    /// The byte that arms the machine ([`PREFIX`] unless remapped).
    prefix: u8,
    keymap: Keymap,
    /// The bytes that armed the machine (control byte or sequence, plus its
    /// release report if one arrived), replayed verbatim if the prefix was
    /// meant literally. Empty unless [`State::Armed`].
    armed: Vec<u8>,
    /// An escape sequence still being read, held back since it could spell
    /// the prefix. Empty between sequences.
    seq: Vec<u8>,
}

/// The derive would zero `prefix`; the default machine must arm on [`PREFIX`].
impl Default for Prefix {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Prefix {
    /// A machine armed by [`PREFIX`] with the default keymap.
    pub fn new(highest: u32) -> Self {
        Self::with_prefix(highest, PREFIX)
    }

    pub fn with_prefix(highest: u32, prefix: u8) -> Self {
        Self::with_keys(highest, prefix, Keymap::default())
    }

    pub fn with_keys(highest: u32, prefix: u8, keymap: Keymap) -> Self {
        Self {
            state: State::Idle,
            highest,
            prefix,
            keymap,
            armed: Vec::new(),
            seq: Vec::new(),
        }
    }

    #[cfg(test)]
    fn is_armed(&self) -> bool {
        self.wait().is_some()
    }

    /// What the caller must time, if anything; call
    /// [`timeout`](Self::timeout) when it passes. An unfinished sequence takes
    /// precedence: settling it, as the next key, settles the rest too.
    pub fn wait(&self) -> Option<Wait> {
        if !self.seq.is_empty() {
            Some(Wait::Sequence)
        } else if self.state != State::Idle {
            Some(Wait::Command)
        } else {
            None
        }
    }

    /// What the user has half-typed, for the hint bar ([`crate::hint`]). From
    /// `state`, not [`wait`](Self::wait): a partial sequence from idle is just
    /// `Esc`, and one after the prefix must not make the bar blink off.
    pub fn pending(&self) -> Option<Pending> {
        match self.state {
            State::Idle => None,
            State::Armed => Some(Pending::Command),
            State::Number(n) => Some(Pending::Number(n)),
        }
    }

    /// Whether another digit could extend `n` into a live session number.
    /// `checked_mul`: a held-down digit key would overflow.
    fn can_extend(&self, n: u32) -> bool {
        n.checked_mul(10).is_some_and(|wider| wider <= self.highest)
    }

    /// Feed bytes read from the terminal. Ordinary runs coalesce into one
    /// [`Step::Forward`], so an 8 KB paste is not 8192 writes.
    pub fn feed(&mut self, input: &[u8]) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        for &b in input {
            self.byte(b, &mut steps, &mut pending);
        }
        flush(&mut steps, &mut pending);
        steps
    }

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
                    // No state change; forwarded as sent. Except the prefix's
                    // release while its press is held: it stays with the press,
                    // so Neovim never sees a release before its press.
                    let mut report = std::mem::take(&mut self.seq);
                    if self.state == State::Armed && code == u32::from(prefix_code(self.prefix)) {
                        self.armed.append(&mut report);
                    } else {
                        pending.append(&mut report);
                    }
                }
                Sequence::Reply => {
                    // A terminal reply, not typing: forwarded unchanged, even
                    // between a prefix and its command.
                    pending.append(&mut self.seq);
                }
                Sequence::Other(n) => {
                    let mut key = std::mem::take(&mut self.seq);
                    let rest = key.split_off(n);
                    self.other_key(key, steps, pending);
                    // The rest is new input; it may be the prefix or another
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

    fn prefix_key(&mut self, bytes: Vec<u8>, steps: &mut Vec<Step>, pending: &mut Vec<u8>) {
        match self.state {
            State::Idle => {
                self.state = State::Armed;
                self.armed = bytes;
            }
            State::Armed => {
                // <prefix> <prefix>: forward the held one, in the terminal's
                // spelling. Checked first, so no binding can shadow it.
                self.state = State::Idle;
                pending.append(&mut self.armed);
            }
            State::Number(n) => {
                // The digits were a complete command; this prefix starts the
                // next.
                self.state = State::Idle;
                flush(steps, pending);
                steps.push(Step::Act(Action::Switch(n)));
                self.state = State::Armed;
                self.armed = bytes;
            }
        }
    }

    /// A complete non-prefix escape sequence (or one that is no key): settle
    /// what is pending and forward the bytes unchanged.
    fn other_key(&mut self, bytes: Vec<u8>, steps: &mut Vec<Step>, pending: &mut Vec<u8>) {
        match self.state {
            State::Idle => {}
            State::Armed => {
                // Not a command: replay the original keystrokes in order.
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

    fn plain_byte(&mut self, b: u8, steps: &mut Vec<Step>, pending: &mut Vec<u8>) {
        match self.state {
            State::Idle => pending.push(b),

            State::Armed => {
                self.state = State::Idle;
                let mut armed = std::mem::take(&mut self.armed);
                if (b'1'..=b'9').contains(&b) {
                    // No number starts with 0, so `<prefix> 0` is replayed.
                    self.start_number(u32::from(b - b'0'), steps, pending);
                } else if let Some(action) = self.keymap.command(b) {
                    flush(steps, pending);
                    steps.push(Step::Act(action));
                } else {
                    // Not a command: replay the original keystrokes in order.
                    pending.append(&mut armed);
                    pending.push(b);
                }
            }

            State::Number(n) => {
                if b.is_ascii_digit() {
                    self.state = State::Idle;
                    // Saturating: a held digit must not panic or wrap around
                    // onto a live number.
                    let wider = n.saturating_mul(10).saturating_add(u32::from(b - b'0'));
                    self.start_number(wider, steps, pending);
                } else if b == ENTER {
                    self.state = State::Idle;
                    flush(steps, pending);
                    steps.push(Step::Act(Action::Switch(n)));
                } else {
                    // The digits were a complete command and `b` is the next
                    // key, so `<prefix> 1 x` acts like `<prefix> Space x`: the
                    // command runs and `x` reaches Neovim.
                    self.state = State::Idle;
                    flush(steps, pending);
                    steps.push(Step::Act(Action::Switch(n)));
                    pending.push(b);
                }
            }
        }
    }

    /// Begin or extend a session number, waiting for another digit only if one
    /// could change which session is meant.
    fn start_number(&mut self, n: u32, steps: &mut Vec<Step>, pending: &mut Vec<u8>) {
        if self.can_extend(n) {
            self.state = State::Number(n);
        } else {
            self.state = State::Idle;
            flush(steps, pending);
            steps.push(Step::Act(Action::Switch(n)));
        }
    }

    /// Call when the requested [`wait`](Self::wait) passes with no input.
    /// Resolves a lone `<prefix>` to a literal one, a half-typed number to its
    /// session, and a cut-off sequence to its bytes (settling those too).
    /// Idempotent, so spurious timer fires are harmless.
    pub fn timeout(&mut self) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut pending = Vec::new();
        if !self.seq.is_empty() {
            // The rest never came, so it was not the prefix.
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

    /// The README's "While attached" table must have each command's name and
    /// default key. Descriptions are not compared: they are worded differently
    /// on purpose.
    #[test]
    fn every_binding_has_a_row_in_the_readme() {
        let readme = include_str!("../README.md");
        for b in BINDINGS {
            let cells = format!("| `{}` | `<prefix>` `{}` |", b.hint, key_label(b.key));
            assert!(
                readme.contains(&cells),
                "README has no row starting {cells:?} — add one to the \
                 \"While attached\" table"
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
            Keymap::default().command(b'x').is_none(),
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

    /// The common case at human typing speed, not an edge case.
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
        // Whether Neovim then suspends is the child TUI's business.
        let mut p = Prefix::new(0);
        let steps = p.feed(&[0x1a]);
        assert_eq!(forwarded(&steps), vec![0x1a]);
        assert!(actions(&steps).is_empty());
    }

    #[test]
    fn ctrl_c_and_ctrl_s_are_not_special() {
        // These arrive as bytes only because raw mode clears ISIG and IXON;
        // the machine must not intercept them.
        let mut p = Prefix::new(0);
        let steps = p.feed(&[0x03, 0x13, 0x1c]);
        assert_eq!(forwarded(&steps), vec![0x03, 0x13, 0x1c]);
    }

    /// Sequences are inspected (to find an encoded prefix), but nothing else is
    /// changed or left waiting once whole.
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

    // The prefix as extended keyboard modes spell it (grammar in `keyseq`).

    /// `Ctrl-Space` under the kitty protocol and `modifyOtherKeys`. Regression:
    /// in Windows Terminal 1.25, `<prefix> d` reached Neovim as the prefix and
    /// a pending delete.
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

    /// The picker key and the chord share key code 32. The chord's release is
    /// held with its press; the bare space after it is the command, and neither
    /// reaches Neovim.
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

    /// Both spellings in one read, with text around them, kept in order.
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

    /// Including right after the `ESC`, which looks like a bare Escape key.
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

    /// Each in the spelling it arrived in.
    #[test]
    fn an_encoded_prefix_then_another_key_replays_both() {
        // A plain letter, and keys that are themselves sequences (e.g. kitty's
        // Escape).
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

    /// A byte prefix then an arrow replays both; `<prefix> <sequence-prefix>`
    /// is still a literal.
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

    /// Release reports (sent for every key) change no state and are forwarded,
    /// except the prefix's own while armed, which is held with its press.
    #[test]
    fn key_release_reports_are_transparent_in_every_state() {
        let release_prefix = b"\x1b[32;5:3u";
        let release_d = b"\x1b[100;1:3u";
        let release_one = b"\x1b[49;1:3u";

        // Idle.
        let mut p = Prefix::new(12);
        assert_eq!(forwarded(&p.feed(release_prefix)), release_prefix);
        assert!(!p.is_armed());

        // Armed: the prefix's own release is not the next key and must not
        // precede its held press; another key's release goes through.
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

    /// The editor queries the terminal at every start and resume, so a reply
    /// can land between a prefix typed at startup and its command. So can a
    /// size report, sent as a held paint turns in-band sizes on.
    #[test]
    fn a_terminal_reply_between_the_prefix_and_its_command_is_not_a_key() {
        for reply in [&b"\x1b[?62;22c"[..], b"\x1b[48;30;120;0;0t"] {
            let mut p = Prefix::new(12);
            p.feed(KITTY);
            assert_eq!(forwarded(&p.feed(reply)), reply, "the answer goes through");
            assert_eq!(p.wait(), Some(Wait::Command), "still armed");
            assert_eq!(actions(&p.feed(b"d")), vec![Action::Detach]);
        }

        let mut p = Prefix::new(12);
        p.feed(&[PREFIX, b'1']);
        assert_eq!(forwarded(&p.feed(b"\x1b[?0u")), b"\x1b[?0u");
        assert_eq!(p.wait(), Some(Wait::Command), "still a number");
        assert_eq!(actions(&p.feed(b"2")), vec![Action::Switch(12)]);
    }

    /// Whether the literal comes from a doubled prefix or the timeout.
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

    /// A repeat is a press: the first makes a literal, the next arms again.
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

    /// It wins over the command wait; settling the sequence settles the
    /// command.
    #[test]
    fn a_cut_off_sequence_asks_for_the_short_wait_in_every_state() {
        // Idle: a bare ESC (the Escape key without the protocol) is held, then
        // forwarded.
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

    /// The next byte may be the prefix.
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

    /// `Ctrl-Shift-Space`, `Ctrl-d` and extra modifiers go through untouched.
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

    /// The default's code is then just another key.
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
        // The non-letter prefix, reported under Space's code.
        assert_eq!(prefix_label(CTRL_SPACE), "Ctrl-Space");
        assert_eq!(prefix_code(CTRL_SPACE), b' ');
    }

    /// It never arms the machine or leaves anything held.
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
        // Literal bytes on purpose, pinning each letter to its action
        // independently of `BINDINGS`.
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

    /// Commands moved onto a capital, punctuation, a key another command left,
    /// and off Enter.
    fn moved() -> Keymap {
        let mut keymap = Keymap::default();
        for (name, key) in [
            ("picker", b'D'),
            ("detach", b' '),
            ("go", b'g'),
            ("help", b'!'),
        ] {
            assert!(keymap.bind(name, key), "no command is called {name}");
        }
        assert_eq!(keymap.clash(), None, "the fixture puts two on one key");
        keymap
    }

    /// Every non-command byte survives a prefix unchanged, with the default and
    /// a moved keymap. With no sessions, digits resolve at once; only `ESC` is
    /// held, as it could start an encoded prefix.
    #[test]
    fn exhaustive_second_byte_table() {
        for keymap in [Keymap::default(), moved()] {
            for b in 0u8..=255 {
                let mut p = Prefix::with_keys(0, PREFIX, keymap);
                let steps = p.feed(&[PREFIX, b]);
                // Same order as `feed`: the literal rule, then digits, then the
                // table.
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
                } else if let Some(action) = keymap.command(b) {
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
    }

    /// And the `[keys.bind]` names are the hints, in table order.
    #[test]
    fn the_default_keymap_is_the_table() {
        let keymap = Keymap::default();
        assert_eq!(keymap.bindings().collect::<Vec<_>>(), BINDINGS);
        let hints: Vec<&str> = BINDINGS.iter().map(|b| b.hint).collect();
        assert_eq!(NAMES.to_vec(), hints);
        assert_eq!(keymap.clash(), None, "the table's own keys are distinct");
    }

    /// The new key runs it, the old key runs nothing, and an unknown name moves
    /// nothing.
    #[test]
    fn binding_a_command_by_name_moves_only_that_command() {
        let mut keymap = Keymap::default();
        assert!(keymap.bind("detach", b'x'));
        assert_eq!(keymap.command(b'x'), Some(Action::Detach));
        assert_eq!(keymap.command(b'd'), None);
        assert_eq!(keymap.key(Action::Detach), Some(b'x'));
        for b in BINDINGS.iter().filter(|b| b.action != Action::Detach) {
            assert_eq!(keymap.key(b.action), Some(b.key), "{} moved too", b.hint);
        }

        let before = keymap;
        assert!(!keymap.bind("detatch", b'y'), "no command is called that");
        assert_eq!(keymap, before, "and nothing moved");
        assert_eq!(keymap.key(Action::Switch(1)), None, "a rule, not a row");
    }

    /// Whether both moved there or one moved onto the other's key.
    #[test]
    fn a_shared_key_is_a_clash_and_a_swap_is_not() {
        let mut keymap = Keymap::default();
        keymap.bind("detach", b'c');
        assert_eq!(keymap.clash(), Some(("detach", "new", b'c')));

        let mut keymap = Keymap::default();
        keymap.bind("next", b'z');
        keymap.bind("help", b'z');
        assert_eq!(keymap.clash(), Some(("next", "help", b'z')));

        let mut keymap = Keymap::default();
        keymap.bind("detach", b'c');
        keymap.bind("new", b'd');
        assert_eq!(keymap.clash(), None);
        assert_eq!(keymap.command(b'c'), Some(Action::Detach));
        assert_eq!(keymap.command(b'd'), Some(Action::Create));
    }

    /// The key it left is replayed with the prefix, in any prefix spelling.
    #[test]
    fn a_moved_command_acts_on_its_new_key_in_every_spelling() {
        let mut keymap = Keymap::default();
        keymap.bind("detach", b'x');
        for spelling in [&[PREFIX][..], KITTY, XTERM] {
            let mut p = Prefix::with_keys(0, PREFIX, keymap);
            assert!(p.feed(spelling).is_empty(), "{spelling:?}");
            let steps = p.feed(b"x");
            assert_eq!(actions(&steps), vec![Action::Detach], "{spelling:?}");
            assert!(forwarded(&steps).is_empty(), "{spelling:?}");

            let mut p = Prefix::with_keys(0, PREFIX, keymap);
            let mut input = spelling.to_vec();
            input.push(b'd');
            let steps = p.feed(&input);
            assert_eq!(forwarded(&steps), input, "{spelling:?}");
            assert!(actions(&steps).is_empty(), "{spelling:?}");
            assert!(!p.is_armed(), "{spelling:?}");
        }
    }

    /// Enter is `go`'s default key and also ends a number; moving `go` removes
    /// only the first.
    #[test]
    fn enter_off_go_is_replayed_and_still_ends_a_number() {
        let mut keymap = Keymap::default();
        keymap.bind("go", b'g');

        let mut p = Prefix::with_keys(12, PREFIX, keymap);
        let steps = p.feed(&[PREFIX, ENTER]);
        assert_eq!(forwarded(&steps), vec![PREFIX, ENTER]);
        assert!(actions(&steps).is_empty());
        assert_eq!(actions(&p.feed(&[PREFIX, b'g'])), vec![Action::Go]);

        let mut p = Prefix::with_keys(12, PREFIX, keymap);
        p.feed(&[PREFIX, b'1']);
        let steps = p.feed(&[ENTER]);
        assert_eq!(actions(&steps), vec![Action::Switch(1)]);
        assert!(
            forwarded(&steps).is_empty(),
            "the terminator must not reach nvim"
        );
    }

    /// As Space is for `Ctrl-Space`: chord and bare key share a key code, and
    /// the chord's release stays with its press.
    #[test]
    fn a_command_on_the_prefix_s_own_letter_is_that_command() {
        let ctrl_a = 0x01;
        let mut keymap = Keymap::default();
        keymap.bind("picker", b'a');
        for spelling in [&[ctrl_a][..], b"\x1b[97;5u", b"\x1b[27;5;97~"] {
            let mut p = Prefix::with_keys(0, ctrl_a, keymap);
            assert!(p.feed(spelling).is_empty(), "{spelling:?}");
            assert!(p.feed(b"\x1b[97;5:3u").is_empty(), "held with its press");
            let steps = p.feed(b"a");
            assert_eq!(actions(&steps), vec![Action::Picker], "{spelling:?}");
            assert!(forwarded(&steps).is_empty(), "{spelling:?}");
        }
    }

    #[test]
    fn a_key_string_parses_to_its_byte() {
        for (s, byte) in [
            ("d", b'd'),
            ("D", b'D'),
            ("?", b'?'),
            ("!", b'!'),
            ("~", b'~'),
            (" ", b' '),
            ("Space", b' '),
            ("space", b' '),
            ("SPACE", b' '),
            ("Enter", ENTER),
            ("enter", ENTER),
        ] {
            assert_eq!(parse_key(s), Ok(byte), "{s:?}");
        }
    }

    /// Keys not sent as a single byte, and digits (session numbers), with a
    /// reason.
    #[test]
    fn what_cannot_be_a_key_is_refused() {
        for s in [
            "", "dd", "Ctrl-d", "C-d", "Esc", "Tab", " d", "d ", "é", "\t", "\x7f", "\r",
        ] {
            assert!(parse_key(s).is_err(), "{s:?} should be refused");
        }
        for d in '0'..='9' {
            let err = parse_key(&d.to_string()).expect_err("a digit");
            assert!(err.contains("session"), "{d:?}: {err}");
        }
    }

    /// [`key_label`] output parses back to the same key, for every possible
    /// key (the first-run file writes the defaults this way).
    #[test]
    fn the_key_string_round_trips() {
        let bindable = (b' '..=b'~').filter(|b| !b.is_ascii_digit()).chain([ENTER]);
        for byte in bindable {
            assert_eq!(parse_key(&key_label(byte)), Ok(byte), "{byte:#04x}");
        }
        for b in BINDINGS {
            assert_eq!(parse_key(&key_label(b.key)), Ok(b.key), "{}", b.hint);
        }
    }

    #[test]
    fn a_key_is_drawn_as_itself_case_and_all() {
        assert_eq!(key_glyph(b'D'), "D");
        assert_eq!(key_glyph(b'd'), "d");
        assert_eq!(key_glyph(b'!'), "!");
        assert_eq!(key_glyph(b' '), SPACE_GLYPH);
        assert_eq!(key_glyph(ENTER), ENTER_GLYPH);
    }

    /// The way to send `<prefix> ?` to Neovim now that `?` is a command.
    #[test]
    fn a_literal_prefix_then_question_mark_still_reaches_neovim() {
        let mut p = Prefix::new(0);
        let steps = p.feed(&[PREFIX, PREFIX, b'?']);
        assert_eq!(forwarded(&steps), vec![PREFIX, b'?']);
        assert!(actions(&steps).is_empty());
    }

    /// The list is kept by hand; the exhaustive `match` makes a new variant
    /// fail to compile until listed, then the count fails until it has a row.
    /// `Switch` is excluded: digits are a rule in `feed`.
    #[test]
    fn the_table_binds_every_action_exactly_once() {
        let all = [
            Action::Picker,
            Action::Detach,
            Action::Create,
            Action::Cycle(Direction::Next),
            Action::Cycle(Direction::Prev),
            Action::Go,
            Action::Help,
            Action::Settings,
        ];
        for action in all {
            match action {
                // `Cycle` is listed twice: each direction has its own key.
                Action::Picker
                | Action::Detach
                | Action::Create
                | Action::Cycle(_)
                | Action::Go
                | Action::Help
                | Action::Settings => {}
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

    /// The digit rule would shadow it, and the help screen would lie.
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
                a.key.is_ascii() && (!a.key.is_ascii_control() || a.key == ENTER),
                "byte {:#04x} cannot be shown on the help screen",
                a.key
            );
            // The literal rule runs first, so a PREFIX row would never fire.
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

    /// The hint bar is one row, lowercase and one word per key, like
    /// `src/ui/draw.rs`.
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

    /// A hint is a bare TOML key in `[keys.bind]`, and the first-run file check
    /// expects lowercase letters and underscores; plain letters satisfy both.
    #[test]
    fn every_hint_is_a_name_a_config_file_can_write() {
        for name in NAMES {
            assert!(
                name.bytes().all(|b| b.is_ascii_lowercase()),
                "{name:?} is not a run of lowercase letters"
            );
        }
    }

    /// No key `parse_key` accepts is a byte `parse_prefix` accepts.
    #[test]
    fn no_key_a_command_can_be_on_is_a_prefix() {
        let prefixes: Vec<u8> = (b'a'..=b'z')
            .filter_map(|c| parse_prefix(&format!("C-{}", c as char)).ok())
            .chain([CTRL_SPACE])
            .collect();
        for byte in 0u8..=255 {
            if parse_key(&key_label(byte)) == Ok(byte) {
                assert!(
                    !prefixes.contains(&byte),
                    "{byte:#04x} is both a key and a prefix"
                );
            }
        }
    }

    #[test]
    fn no_two_bindings_share_a_hint() {
        for (i, a) in BINDINGS.iter().enumerate() {
            for b in &BINDINGS[i + 1..] {
                assert_ne!(a.hint, b.hint, "two commands are labelled {:?}", a.hint);
            }
        }
    }

    /// The hint bar is drawn from `pending`; a bar shown without the prefix or
    /// after the command would cover the editor's row.
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

    /// The bar must not blink off while a next key's sequence arrives over
    /// several reads after the prefix; and a sequence cut short from idle is
    /// just `Esc`, not a pending command.
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

        // The prefix's release, held with its press, does not end the wait.
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
        // Nine sessions: no second digit could change the answer.
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

    #[test]
    fn enter_after_the_prefix_alone_goes_where_the_card_is_about() {
        let mut p = Prefix::new(12);
        let steps = p.feed(&[PREFIX, ENTER]);
        assert_eq!(actions(&steps), vec![Action::Go]);
        assert!(forwarded(&steps).is_empty(), "nothing reaches nvim");
        assert!(!p.is_armed());
        assert_eq!(key_label(ENTER), "Enter");
        assert_eq!(key_glyph(ENTER), "⏎");
    }

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

    /// Split at any point.
    #[test]
    fn a_number_survives_a_chunk_boundary() {
        let mut p = Prefix::new(30);
        assert!(p.feed(&[PREFIX]).is_empty());
        assert!(p.feed(b"1").is_empty(), "1 could still become 12");
        let steps = p.feed(b"2");
        assert_eq!(actions(&steps), vec![Action::Switch(12)]);
    }

    /// The documented way to send a digit to Neovim.
    #[test]
    fn a_literal_prefix_then_a_digit_still_reaches_neovim() {
        let mut p = Prefix::new(9);
        let steps = p.feed(&[PREFIX, PREFIX, b'1']);
        assert_eq!(forwarded(&steps), vec![PREFIX, b'1']);
        assert!(actions(&steps).is_empty());
    }

    #[test]
    fn digits_in_ordinary_input_are_not_commands() {
        let mut p = Prefix::new(30);
        let steps = p.feed(b"buffer 12 line 30");
        assert_eq!(forwarded(&steps), b"buffer 12 line 30");
        assert!(actions(&steps).is_empty());
        assert!(!p.is_armed());
    }

    /// A held digit key must not wrap around into a live session number.
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

    /// Ties [`PREFIX_LABEL`] to [`PREFIX`] in both directions.
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

    /// Surrounding space is ignored too.
    #[test]
    fn a_prefix_string_is_case_insensitive() {
        assert_eq!(parse_prefix("ctrl-T"), Ok(0x14));
        assert_eq!(parse_prefix("c-A"), Ok(0x01));
        assert_eq!(parse_prefix("  Ctrl-B  "), Ok(0x02));
        assert_eq!(parse_prefix("  c-sPaCe  "), Ok(CTRL_SPACE));
    }

    #[test]
    fn the_prefix_string_round_trips() {
        for byte in 1u8..=26 {
            // Enter, newline, Tab and Backspace are refused by design.
            if matches!(byte, 0x08 | 0x09 | 0x0a | 0x0d) {
                continue;
            }
            let label = prefix_label(byte);
            assert_eq!(parse_prefix(&label), Ok(byte), "for {label}");
        }
        // And the one label that is a name rather than a letter.
        assert_eq!(parse_prefix(&prefix_label(CTRL_SPACE)), Ok(CTRL_SPACE));
    }

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

    /// Enter, newline, Tab and Backspace on the wire; refused by name.
    #[test]
    fn prefixes_that_break_the_terminal_are_rejected() {
        for s in ["C-m", "C-j", "C-i", "C-h"] {
            assert!(parse_prefix(s).is_err(), "{s:?} should be rejected");
        }
    }

    /// Allowed, as in tmux: the byte just stops reaching Neovim.
    #[test]
    fn ctrl_c_and_ctrl_z_are_accepted_as_prefixes() {
        assert_eq!(parse_prefix("C-c"), Ok(0x03));
        assert_eq!(parse_prefix("C-z"), Ok(0x1a));
    }

    /// The old default is then an ordinary key.
    #[test]
    fn a_remapped_prefix_arms_on_its_own_byte() {
        let ctrl_a = 0x01;
        let mut p = Prefix::with_prefix(0, ctrl_a);
        let armed = p.feed(&[ctrl_a]);
        assert!(forwarded(&armed).is_empty(), "the new prefix must be eaten");
        assert!(p.is_armed());

        let steps = p.feed(b" ");
        assert_eq!(actions(&steps), vec![Action::Picker]);

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

    #[test]
    fn new_and_default_use_the_standard_prefix() {
        for mut p in [Prefix::new(0), Prefix::default()] {
            assert!(p.feed(&[PREFIX]).is_empty());
            assert!(p.is_armed(), "the standard prefix must arm the machine");
        }
    }
}
