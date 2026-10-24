//! Parses terminal input into Neovim key notation and events.
//!
//! Does what libtermkey does for `nvim --remote-ui`, on the same encodings,
//! since nvmux passes bytes on as the terminal sent them (see
//! [`crate::keyseq`]):
//!
//! - plain text, and C0 bytes as `Ctrl` chords (`0x08` is `<C-h>`, `0x7f` is
//!   `<BS>`, as termkey makes them for Neovim);
//! - `ESC` and a key as that key with `Alt`;
//! - `CSI` and `SS3` keys with xterm's modifier parameter;
//! - kitty's `CSI … u` where the terminal has it (see `super::term`), else
//!   xterm's `modifyOtherKeys` (`CSI 27 ; mods ; key ~`);
//! - SGR mouse reports, bracketed paste, focus reports;
//! - query replies: the client's own are kept; others go to the server as a
//!   `termresponse` without the string terminator, as its handlers expect
//!   (`vim.tty.query`, background detection in `_core/defaults`).
//!
//! A trailing `ESC` may be the Escape key or a sequence start, so it is held
//! ([`Parser::waiting`]) until more arrives or `'ttimeoutlen'` passes, as in
//! Neovim's TUI, but no longer than the relay's own wait, which it was already
//! held for (see `super::RELAYED_ESCAPE`). A sequence cut by a read is held
//! the same way. A known reply (an OSC, DCS or APC whose first bytes no key
//! makes, [`Parser::in_reply`]) is held longer, since an OSC 52 paste can be
//! hundreds of KB; one longer than the client keeps is dropped through its end.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Keys in Neovim's notation, for `nvim_input`; runs of plain keys merged.
    Keys(String),
    Mouse(Mouse),
    /// Part of a bracketed paste, for `nvim_paste`: `phase` 1 first, 2 middle,
    /// 3 last, -1 for a paste that arrived all at once.
    Paste {
        phase: i64,
        data: Vec<u8>,
    },
    /// `true` when the terminal gained focus.
    Focus(bool),
    Reply(Reply),
    /// The body of nvmux's own card sequence (see [`crate::card`]), without
    /// its opener and terminator.
    Card(Vec<u8>),
}

/// A terminal's reply to a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Primary device attributes: `CSI ? … c`.
    DeviceAttributes(Vec<u8>),
    /// The kitty keyboard protocol's flags: `CSI ? flags u`.
    KittyFlags,
    /// Anything else, as it is to be handed to the server.
    Other(Vec<u8>),
}

/// A mouse report in `nvim_input_mouse`'s terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mouse {
    pub button: &'static str,
    pub action: &'static str,
    /// `S`, `A`, `C` in any combination.
    pub mods: String,
    pub row: usize,
    pub col: usize,
}

/// Result of parsing from the start of the pending bytes.
enum Step {
    /// The first `n` bytes made this.
    Took(usize, Option<Input>),
    /// Needs more bytes.
    More,
    /// The first `n` bytes start a reply too long to keep, not yet ended:
    /// they are dropped, and so is the rest of it as it arrives.
    TooLong(usize),
}

/// Longest a sequence may grow before it is given up on: past this it is no
/// key, and holding it would hold every later key. String replies (e.g. an
/// OSC 52 paste) may reach `MAX_STRING`.
const MAX_SEQUENCE: usize = 256;
const MAX_STRING: usize = 1 << 20;

/// What may follow `ESC ]`, `ESC P` and `ESC _` in a reply but in no Alt
/// chord: an OSC number, a DCS reply's first characters, kitty's `G` for
/// graphics, and nvmux's `~` for cards ([`crate::card::OPENER`]).
pub(crate) const OSC_OPENS: &[u8] = b"0123456789";
pub(crate) const DCS_OPENS: &[u8] = b"0123456789>|+$=";
pub(crate) const APC_OPENS: &[u8] = &[b'G', crate::card::OPENER];

#[derive(Debug, Default)]
pub struct Parser {
    pending: Vec<u8>,
    /// Inside a bracketed paste; `true` once its first part has gone.
    paste: Option<bool>,
    /// Introducer of a reply too long to keep (OSC, DCS or APC), whose rest
    /// is dropped through its end.
    skipping: Option<u8>,
}

pub(crate) const PASTE_START: &[u8] = b"\x1b[200~";
pub(crate) const PASTE_END: &[u8] = b"\x1b[201~";

impl Parser {
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Input>) {
        self.pending.extend_from_slice(bytes);
        self.run(out, false);
    }

    /// Whether bytes are held pending what comes next.
    pub fn waiting(&self) -> bool {
        !self.pending.is_empty() && self.paste.is_none()
    }

    /// Whether what is held is a partial reply (an OSC, DCS or APC string known
    /// not to be a key) or the rest of one being dropped.
    pub fn in_reply(&self) -> bool {
        self.paste.is_none() && (self.skipping.is_some() || string_opener(&self.pending).is_some())
    }

    /// Nothing more arrived: resolve what is held as it stands.
    pub fn timeout(&mut self, out: &mut Vec<Input>) {
        // A reply being dropped has stalled: whatever comes next is new.
        self.skipping = None;
        self.run(out, true);
    }

    fn run(&mut self, out: &mut Vec<Input>, final_: bool) {
        let mut keys = String::new();
        let mut at = 0;
        while at < self.pending.len() {
            if let Some(kind) = self.skipping {
                let rest = &self.pending[at..];
                match string_end(rest, kind) {
                    Some(end) => {
                        self.skipping = None;
                        at += end;
                        continue;
                    }
                    None => {
                        // Drop all but a trailing `ESC`, which may start the
                        // terminating `ESC \`.
                        let keep = usize::from(rest.last() == Some(&0x1b));
                        flush_keys(&mut keys, out);
                        let len = self.pending.len();
                        self.pending.drain(..len - keep);
                        return;
                    }
                }
            }
            if self.paste.is_some() {
                flush_keys(&mut keys, out);
                match self.paste_part(at, out) {
                    Some(n) => {
                        at += n;
                        continue;
                    }
                    None => {
                        self.pending.drain(..at);
                        return;
                    }
                }
            }
            let rest = &self.pending[at..];
            if rest.starts_with(PASTE_START) {
                flush_keys(&mut keys, out);
                self.paste = Some(false);
                at += PASTE_START.len();
                continue;
            }
            match parse(rest, final_) {
                Step::TooLong(n) => {
                    self.skipping = string_opener(rest);
                    at += n;
                }
                Step::Took(n, input) => {
                    at += n;
                    match input {
                        Some(Input::Keys(k)) => keys.push_str(&k),
                        Some(other) => {
                            flush_keys(&mut keys, out);
                            out.push(other);
                        }
                        None => {}
                    }
                }
                Step::More => break,
            }
        }
        flush_keys(&mut keys, out);
        self.pending.drain(..at);
    }

    /// The paste from `at`, through its end marker or to the end of what has
    /// arrived, holding back a possible partial marker or character. `None`
    /// when there is nothing to pass on yet.
    fn paste_part(&mut self, at: usize, out: &mut Vec<Input>) -> Option<usize> {
        let rest = &self.pending[at..];
        let first = !self.paste.unwrap_or(false);
        if let Some(end) = find(rest, PASTE_END) {
            let data = rest[..end].to_vec();
            // A whole paste in one read is one call (-1); otherwise this is
            // the last part.
            let phase = if first { -1 } else { 3 };
            out.push(Input::Paste { phase, data });
            self.paste = None;
            return Some(end + PASTE_END.len());
        }
        let keep = partial_suffix(rest, PASTE_END).max(partial_char(rest));
        let take = rest.len() - keep;
        if take == 0 {
            return None;
        }
        out.push(Input::Paste {
            phase: if first { 1 } else { 2 },
            data: rest[..take].to_vec(),
        });
        self.paste = Some(true);
        Some(take)
    }
}

fn flush_keys(keys: &mut String, out: &mut Vec<Input>) {
    if !keys.is_empty() {
        out.push(Input::Keys(std::mem::take(keys)));
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// How many bytes at the end of `hay` could be the start of `needle`.
fn partial_suffix(hay: &[u8], needle: &[u8]) -> usize {
    (1..needle.len().min(hay.len() + 1))
        .rev()
        .find(|&n| hay.ends_with(&needle[..n]))
        .unwrap_or(0)
}

/// How many bytes at the end of `bytes` are an incomplete character.
fn partial_char(bytes: &[u8]) -> usize {
    for back in 1..=3.min(bytes.len()) {
        let b = bytes[bytes.len() - back];
        if b & 0xc0 == 0x80 {
            continue;
        }
        let need = utf8_len(b);
        return if need > back { back } else { 0 };
    }
    0
}

/// Length of a UTF-8 sequence starting with `b`; 1 for a byte that starts
/// none, which is then taken alone.
fn utf8_len(b: u8) -> usize {
    match b {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    }
}

/// Parse one input from the start of non-empty `bytes`.
fn parse(bytes: &[u8], final_: bool) -> Step {
    match bytes[0] {
        0x1b => escape(bytes, final_),
        b @ (0x00..=0x1f | 0x7f) => Step::Took(1, Some(Input::Keys(control(b, "")))),
        b => {
            let need = utf8_len(b);
            if bytes.len() < need {
                return if final_ {
                    Step::Took(bytes.len(), None)
                } else {
                    Step::More
                };
            }
            match std::str::from_utf8(&bytes[..need]) {
                Ok(s) => {
                    let c = s.chars().next().expect("one character");
                    Step::Took(need, Some(Input::Keys(plain(c))))
                }
                // Invalid UTF-8: dropped a byte at a time.
                Err(_) => Step::Took(1, None),
            }
        }
    }
}

/// A plain key; `<` is spelt `<lt>`.
fn plain(c: char) -> String {
    if c == '<' {
        "<lt>".into()
    } else {
        c.to_string()
    }
}

/// A C0 byte or DEL as its chord, prefixed with `mods`.
fn control(b: u8, mods: &str) -> String {
    let name: String = match b {
        0x00 => "C-Space".into(),
        0x09 => "Tab".into(),
        0x0d => "CR".into(),
        0x1b => "Esc".into(),
        0x7f => "BS".into(),
        0x01..=0x1a => format!("C-{}", (b + 0x60) as char),
        0x1c => "C-\\".into(),
        0x1d => "C-]".into(),
        0x1e => "C-^".into(),
        _ => "C-_".into(),
    };
    format!("<{mods}{name}>")
}

fn escape(bytes: &[u8], final_: bool) -> Step {
    let Some(&next) = bytes.get(1) else {
        return if final_ {
            Step::Took(1, Some(Input::Keys("<Esc>".into())))
        } else {
            Step::More
        };
    };
    // OSC, DCS and APC replies open with bytes no Alt chord makes (see
    // `OSC_OPENS`). Anything else after `ESC ]`, `ESC P` or `ESC _` is Alt and
    // a key, as is a lone one the wait gave up on.
    let opens = |starts: &[u8]| match bytes.get(2) {
        Some(b) => starts.contains(b),
        None => !final_,
    };
    let step = match next {
        b'[' => csi(bytes, final_),
        b'O' => ss3(bytes, final_),
        b']' if opens(OSC_OPENS) => string(bytes, b']', final_),
        b'P' if opens(DCS_OPENS) => string(bytes, b'P', final_),
        b'_' if opens(APC_OPENS) => match string(bytes, next, final_) {
            // nvmux's card: for the client only, never typed or sent on.
            Step::Took(n, Some(Input::Reply(Reply::Other(s))))
                if s.get(2) == Some(&crate::card::OPENER) =>
            {
                Step::Took(n, Some(Input::Card(s[3..].to_vec())))
            }
            // Graphics replies answer nothing the server asked: dropped.
            Step::Took(n, _) => Step::Took(n, None),
            more => more,
        },
        // `ESC ESC [ A`: Alt and an arrow, as some terminals spell it. Two
        // Escapes alone are two Escapes.
        0x1b => match escape(&bytes[1..], final_) {
            Step::Took(n, Some(Input::Keys(k))) if is_key_sequence(&bytes[1..=n]) => {
                Step::Took(n + 1, Some(Input::Keys(with_alt(&k))))
            }
            Step::More => Step::More,
            _ => Step::Took(1, Some(Input::Keys("<Esc>".into()))),
        },
        _ => return alt(bytes, final_),
    };
    match step {
        // Given up on: the ESC was the Escape key, and what followed is typed.
        Step::More
            if final_ || bytes.len() > MAX_SEQUENCE && !matches!(next, b']' | b'P' | b'_') =>
        {
            Step::Took(1, Some(Input::Keys("<Esc>".into())))
        }
        step => step,
    }
}

/// Whether `seq` is a key's escape sequence, not a reply or a lone escape
/// (for the `ESC ESC` form of Alt).
fn is_key_sequence(seq: &[u8]) -> bool {
    seq.len() > 2 && matches!(seq[1], b'[' | b'O')
}

fn with_alt(keys: &str) -> String {
    match keys.strip_prefix('<') {
        Some(rest) => format!("<M-{rest}"),
        None => match keys {
            "<lt>" => "<M-lt>".into(),
            k => format!("<M-{}>", name_in_brackets(k.chars().next().unwrap_or(' '))),
        },
    }
}

/// `ESC` and one key: that key with Alt.
fn alt(bytes: &[u8], final_: bool) -> Step {
    match parse(&bytes[1..], final_) {
        Step::Took(n, Some(Input::Keys(k))) => Step::Took(n + 1, Some(Input::Keys(with_alt(&k)))),
        Step::Took(n, other) => Step::Took(n + 1, other),
        Step::TooLong(n) => Step::TooLong(n + 1),
        Step::More => Step::More,
    }
}

/// A character as written inside `<…>`.
fn name_in_brackets(c: char) -> String {
    match c {
        ' ' => "Space".into(),
        '<' => "lt".into(),
        '\\' => "Bslash".into(),
        '|' => "Bar".into(),
        c => c.to_string(),
    }
}

/// The introducer (`]` OSC, `P` DCS, `_` APC) if `bytes` open a reply no key
/// could: [`escape`]'s test, once a byte follows the introducer.
fn string_opener(bytes: &[u8]) -> Option<u8> {
    match bytes {
        [0x1b, b']', b, ..] if OSC_OPENS.contains(b) => Some(b']'),
        [0x1b, b'P', b, ..] if DCS_OPENS.contains(b) => Some(b'P'),
        [0x1b, b'_', b, ..] if APC_OPENS.contains(b) => Some(b'_'),
        _ => None,
    }
}

/// Offset just past the terminator of the `kind` reply `bytes` are inside:
/// BEL for an OSC, or `ESC \` for any.
fn string_end(bytes: &[u8], kind: u8) -> Option<usize> {
    bytes.iter().enumerate().find_map(|(i, b)| match b {
        0x07 if kind == b']' => Some(i + 1),
        0x1b if bytes.get(i + 1) == Some(&b'\\') => Some(i + 2),
        _ => None,
    })
}

/// An OSC, DCS or APC reply up to its terminator (BEL for an OSC, or
/// `ESC \`), passed on without it.
fn string(bytes: &[u8], kind: u8, final_: bool) -> Step {
    let mut i = 2;
    while i < bytes.len() {
        match bytes[i] {
            0x07 if kind == b']' => {
                return Step::Took(i + 1, Some(Input::Reply(Reply::Other(bytes[..i].to_vec()))));
            }
            0x1b => {
                return match bytes.get(i + 1) {
                    Some(b'\\') => {
                        Step::Took(i + 2, Some(Input::Reply(Reply::Other(bytes[..i].to_vec()))))
                    }
                    // Another sequence started before this one ended: drop
                    // this one.
                    Some(_) => Step::Took(i, None),
                    None if final_ => Step::Took(i, None),
                    None => Step::More,
                };
            }
            _ => i += 1,
        }
    }
    if final_ {
        Step::Took(bytes.len(), None)
    } else if bytes.len() > MAX_STRING {
        Step::TooLong(bytes.len())
    } else {
        Step::More
    }
}

/// `ESC O`, optional xterm modifier, final byte: application-mode cursor
/// keys, F1 to F4, the keypad.
fn ss3(bytes: &[u8], final_: bool) -> Step {
    let mut i = 2;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let Some(&fin) = bytes.get(i) else {
        return if final_ {
            Step::Took(bytes.len(), Some(Input::Keys("<M-O>".into())))
        } else {
            Step::More
        };
    };
    let mods = std::str::from_utf8(&bytes[2..i])
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .map_or(String::new(), modifiers);
    let name = match fin {
        b'A' => "Up",
        b'B' => "Down",
        b'C' => "Right",
        b'D' => "Left",
        b'H' => "Home",
        b'F' => "End",
        b'E' => "kOrigin",
        b'P' => "F1",
        b'Q' => "F2",
        b'R' => "F3",
        b'S' => "F4",
        b'M' => "kEnter",
        b'X' => "kEqual",
        b'j' => "kMultiply",
        b'k' => "kPlus",
        b'l' => "kComma",
        b'm' => "kMinus",
        b'n' => "kPoint",
        b'o' => "kDivide",
        b'p'..=b'y' => {
            return Step::Took(i + 1, Some(Input::Keys(format!("<{mods}k{}>", fin - b'p'))))
        }
        // Not a key: Alt-O, and what follows stands on its own.
        _ => return Step::Took(2, Some(Input::Keys("<M-O>".into()))),
    };
    Step::Took(i + 1, Some(Input::Keys(format!("<{mods}{name}>"))))
}

/// A `CSI` sequence: a key, a mouse report, a focus report, or a reply.
fn csi(bytes: &[u8], final_: bool) -> Step {
    // The legacy X10 mouse report: `CSI M` and three bytes, any bytes.
    if bytes.get(2) == Some(&b'M') {
        if bytes.len() < 6 {
            return if final_ {
                Step::Took(bytes.len(), None)
            } else {
                Step::More
            };
        }
        let at = |i: usize| u32::from(bytes[i].wrapping_sub(32));
        let report = mouse(
            at(3),
            at(4).saturating_sub(1),
            at(5).saturating_sub(1),
            false,
        );
        return Step::Took(6, report.map(Input::Mouse));
    }
    let mut i = 2;
    let private = match bytes.get(2) {
        Some(&b @ (b'<' | b'=' | b'>' | b'?')) => {
            i += 1;
            Some(b)
        }
        _ => None,
    };
    let params_start = i;
    while bytes.get(i).is_some_and(|b| (0x30..=0x3f).contains(b)) {
        i += 1;
    }
    let params_end = i;
    while bytes.get(i).is_some_and(|b| (0x20..=0x2f).contains(b)) {
        i += 1;
    }
    let Some(&fin) = bytes.get(i) else {
        return if final_ {
            Step::Took(bytes.len(), None)
        } else {
            Step::More
        };
    };
    if !(0x40..=0x7e).contains(&fin) {
        // Not a valid byte here: the sequence is broken and ends before it.
        return Step::Took(i, None);
    }
    let n = i + 1;
    let seq = &bytes[..n];
    let intermediates = &bytes[params_end..i];
    let params = Params::read(&bytes[params_start..params_end]);
    let input = match (private, fin) {
        (Some(b'<'), b'M' | b'm') => {
            let (b, x, y) = (params.get(0), params.get(1), params.get(2));
            mouse(b, x.saturating_sub(1), y.saturating_sub(1), fin == b'm').map(Input::Mouse)
        }
        (Some(b'?'), b'c') => Some(Input::Reply(Reply::DeviceAttributes(seq.to_vec()))),
        (Some(b'?'), b'u') => Some(Input::Reply(Reply::KittyFlags)),
        (Some(_), _) => Some(Input::Reply(Reply::Other(seq.to_vec()))),
        (None, b'I') if params.is_empty() => Some(Input::Focus(true)),
        (None, b'O') if params.is_empty() => Some(Input::Focus(false)),
        // Device status, window reports: answers, not keys.
        (None, b'n' | b't') => Some(Input::Reply(Reply::Other(seq.to_vec()))),
        // A cursor position report — unless it is F3 with a modifier, which
        // xterm spells `CSI 1 ; mods R`.
        (None, b'R') if params.len() == 2 && params.get(0) != 1 => {
            Some(Input::Reply(Reply::Other(seq.to_vec())))
        }
        (None, _) if !intermediates.is_empty() => None,
        (None, b'u') => kitty(&params),
        (None, b'~') => tilde(&params),
        (None, _) => letter(fin, &params),
    };
    Step::Took(n, input)
}

/// The parameters of a `CSI`, each with its `:` sub-parameters.
struct Params(Vec<Vec<u32>>);

impl Params {
    fn read(bytes: &[u8]) -> Self {
        let text = String::from_utf8_lossy(bytes);
        if text.is_empty() {
            return Params(Vec::new());
        }
        Params(
            text.split(';')
                .map(|p| p.split(':').map(|n| n.parse().unwrap_or(0)).collect())
                .collect(),
        )
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    /// Parameter `i`'s main value, 0 if absent.
    fn get(&self, i: usize) -> u32 {
        self.0.get(i).and_then(|p| p.first()).copied().unwrap_or(0)
    }

    fn sub(&self, i: usize, j: usize) -> Option<u32> {
        self.0.get(i).and_then(|p| p.get(j)).copied()
    }
}

/// A modifier parameter as Neovim's prefix. The parameter is 1 + a mask:
/// shift 1, alt 2, ctrl 4, super 8, hyper 16, meta 32, plus lock bits. Hyper
/// and the locks are ignored; meta is written as alt.
fn modifiers(param: u32) -> String {
    let m = param.saturating_sub(1);
    let mut s = String::new();
    for (bit, name) in [(4, "C-"), (1, "S-"), (2, "M-"), (32, "M-"), (8, "D-")] {
        if m & bit != 0 && !s.contains(name) {
            s.push_str(name);
        }
    }
    s
}

/// A key with a letter final byte: cursor keys, Home, End, F1 to F4,
/// Shift-Tab, the keypad's middle.
fn letter(fin: u8, params: &Params) -> Option<Input> {
    let mods = if params.len() >= 2 {
        modifiers(params.get(1))
    } else {
        String::new()
    };
    let name = match fin {
        b'A' => "Up",
        b'B' => "Down",
        b'C' => "Right",
        b'D' => "Left",
        b'H' => "Home",
        b'F' => "End",
        b'E' => "kOrigin",
        b'P' => "F1",
        b'Q' => "F2",
        b'R' => "F3",
        b'S' => "F4",
        b'Z' => return Some(Input::Keys(format!("<{mods}S-Tab>").replace("S-S-", "S-"))),
        _ => return None,
    };
    Some(Input::Keys(format!("<{mods}{name}>")))
}

/// `CSI number ; mods ~`: editing keys and F5 up, and xterm's
/// `modifyOtherKeys` (`CSI 27 ; mods ; key ~`).
fn tilde(params: &Params) -> Option<Input> {
    let n = params.get(0);
    if n == 27 && params.len() >= 3 {
        return key_code(params.get(2), params.get(1));
    }
    let mods = if params.len() >= 2 {
        modifiers(params.get(1))
    } else {
        String::new()
    };
    let name: String = match n {
        1 | 7 => "Home".into(),
        2 => "Insert".into(),
        3 => "Del".into(),
        4 | 8 => "End".into(),
        5 => "PageUp".into(),
        6 => "PageDown".into(),
        11..=15 => format!("F{}", n - 10),
        17..=21 => format!("F{}", n - 11),
        23..=26 => format!("F{}", n - 12),
        28 | 29 => format!("F{}", n - 13),
        31..=34 => format!("F{}", n - 14),
        _ => return None,
    };
    Some(Input::Keys(format!("<{mods}{name}>")))
}

/// The kitty keyboard protocol's `CSI key[:shifted[:base]] ; mods[:event] u`.
/// Releases are dropped: the client asks for none, but a terminal another
/// client set up may send them.
fn kitty(params: &Params) -> Option<Input> {
    if params.sub(1, 1) == Some(3) {
        return None;
    }
    let mods = if params.len() >= 2 { params.get(1) } else { 1 };
    key_code(params.get(0), mods)
}

/// A key by code (a codepoint, or kitty's code for a key with none) with a
/// modifier parameter.
fn key_code(code: u32, mods_param: u32) -> Option<Input> {
    let m = mods_param.saturating_sub(1);
    let named = |name: &str| Some(Input::Keys(format!("<{}{name}>", modifiers(mods_param))));
    match code {
        9 => return named("Tab"),
        13 => return named("CR"),
        27 => return named("Esc"),
        8 | 127 => return named("BS"),
        57376..=57398 => return named(&format!("F{}", code - 57376 + 13)),
        57399..=57408 => return named(&format!("k{}", code - 57399)),
        57409 => return named("kPoint"),
        57410 => return named("kDivide"),
        57411 => return named("kMultiply"),
        57412 => return named("kMinus"),
        57413 => return named("kPlus"),
        57414 => return named("kEnter"),
        57415 => return named("kEqual"),
        57416 => return named("kComma"),
        57417 => return named("kLeft"),
        57418 => return named("kRight"),
        57419 => return named("kUp"),
        57420 => return named("kDown"),
        57421 => return named("kPageUp"),
        57422 => return named("kPageDown"),
        57423 => return named("kHome"),
        57424 => return named("kEnd"),
        57425 => return named("kInsert"),
        57426 => return named("kDel"),
        57427 => return named("kOrigin"),
        // Media keys and modifiers on their own: nothing Neovim has a name for.
        57344..=63743 => return None,
        _ => {}
    }
    let c = char::from_u32(code)?;
    // Shift alone with a character is that character shifted: the terminal
    // sends it as text unless asked for more, and Neovim reads `<S-a>` as `A`.
    if m & !(64 | 128) == 1 {
        let shifted: String = c.to_uppercase().collect();
        if shifted != c.to_string() {
            return Some(Input::Keys(shifted));
        }
    }
    if m & !(64 | 128) == 0 {
        return Some(Input::Keys(plain(c)));
    }
    let mut out = String::new();
    let _ = write!(out, "<{}{}>", modifiers(mods_param), name_in_brackets(c));
    Some(Input::Keys(out))
}

/// An SGR (or X10) mouse report for `nvim_input_mouse`: `b` is the button
/// byte; `col` and `row` are 0-based.
fn mouse(b: u32, col: u32, row: u32, release: bool) -> Option<Mouse> {
    let mut mods = String::new();
    if b & 4 != 0 {
        mods.push('S');
    }
    if b & 8 != 0 {
        mods.push('A');
    }
    if b & 16 != 0 {
        mods.push('C');
    }
    let (button, action) = if b & 64 != 0 {
        (
            "wheel",
            match b & 3 {
                0 => "up",
                1 => "down",
                2 => "left",
                _ => "right",
            },
        )
    } else if b & 128 != 0 {
        let button = match b & 3 {
            0 => "x1",
            1 => "x2",
            _ => return None,
        };
        (button, if release { "release" } else { "press" })
    } else {
        let button = match b & 3 {
            0 => "left",
            1 => "middle",
            2 => "right",
            // No button: a plain move, or a legacy release that does not
            // name the button.
            _ if b & 32 != 0 => "move",
            _ => "left",
        };
        let action = if button == "move" {
            ""
        } else if release || b & 3 == 3 {
            "release"
        } else if b & 32 != 0 {
            "drag"
        } else {
            "press"
        };
        (button, action)
    };
    Some(Mouse {
        button,
        action,
        mods,
        row: row as usize,
        col: col as usize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(bytes: &[u8]) -> Vec<Input> {
        let mut p = Parser::default();
        let mut out = Vec::new();
        p.feed(bytes, &mut out);
        p.timeout(&mut out);
        out
    }

    /// nvmux's card sequence becomes an `Input::Card` in place among the
    /// keys, never typed or sent to the server, however the reads split it.
    /// Graphics replies are still dropped, and `ESC _` and a key is still Alt.
    #[test]
    fn what_nvmux_says_to_show_is_never_typed() {
        let card = crate::card::encode(&crate::card::Command::Clear);
        let body = card[crate::card::OPEN.len()..card.len() - crate::card::CLOSE.len()].to_vec();
        assert_eq!(read(&card), [Input::Card(body.clone())]);

        let mut between = b"a".to_vec();
        between.extend_from_slice(&card);
        between.extend_from_slice(b"b");
        assert_eq!(
            read(&between),
            [
                Input::Keys("a".into()),
                Input::Card(body.clone()),
                Input::Keys("b".into())
            ]
        );

        // Cut anywhere, it is held whole as a reply, not as an escape the wait
        // could give up on as a key.
        for cut in 1..card.len() {
            let mut p = Parser::default();
            let mut out = Vec::new();
            p.feed(&card[..cut], &mut out);
            assert!(out.is_empty(), "cut at {cut}: {out:?}");
            if cut >= 3 {
                assert!(p.in_reply(), "cut at {cut}: not known to be a reply");
            }
            p.feed(&card[cut..], &mut out);
            assert_eq!(out, [Input::Card(body.clone())], "cut at {cut}");
        }

        // Still a card after a held lone Escape.
        let mut escaped = b"\x1b".to_vec();
        escaped.extend_from_slice(&card);
        assert_eq!(
            read(&escaped),
            [Input::Keys("<Esc>".into()), Input::Card(body)]
        );

        assert_eq!(read(b"\x1b_Gi=1;OK\x1b\\"), []);
        assert_eq!(keys(b"\x1b_x"), "<M-_>x");
    }

    fn keys(bytes: &[u8]) -> String {
        read(bytes)
            .into_iter()
            .map(|i| match i {
                Input::Keys(k) => k,
                other => panic!("{other:?} from {bytes:?}"),
            })
            .collect()
    }

    #[test]
    fn text_is_typed_with_the_one_reserved_character_spelt_out() {
        assert_eq!(keys(b"abc"), "abc");
        assert_eq!(keys("é中".as_bytes()), "é中");
        assert_eq!(keys(b"a<b"), "a<lt>b");
    }

    /// As termkey makes them for Neovim.
    #[test]
    fn control_bytes_are_their_chords() {
        assert_eq!(keys(b"\x00"), "<C-Space>");
        assert_eq!(keys(b"\x01"), "<C-a>");
        assert_eq!(keys(b"\x08"), "<C-h>");
        assert_eq!(keys(b"\x09"), "<Tab>");
        assert_eq!(keys(b"\x0a"), "<C-j>");
        assert_eq!(keys(b"\x0d"), "<CR>");
        assert_eq!(keys(b"\x1c"), "<C-\\>");
        assert_eq!(keys(b"\x1f"), "<C-_>");
        assert_eq!(keys(b"\x7f"), "<BS>");
    }

    #[test]
    fn two_escapes_are_two_escapes() {
        assert_eq!(keys(b"\x1b\x1b"), "<Esc><Esc>");
        let mut p = Parser::default();
        let mut out = Vec::new();
        p.feed(b"\x1b\x1b[", &mut out);
        assert!(out.is_empty(), "the rest of an Alt-arrow may be coming");
        p.feed(b"A", &mut out);
        assert_eq!(out, vec![Input::Keys("<M-Up>".into())]);
    }

    #[test]
    fn escape_and_a_key_is_that_key_with_alt() {
        assert_eq!(keys(b"\x1bx"), "<M-x>");
        assert_eq!(keys(b"\x1bX"), "<M-X>");
        assert_eq!(keys(b"\x1bP"), "<M-P>");
        assert_eq!(keys(b"\x1b]"), "<M-]>");
        assert_eq!(keys(b"\x1b_"), "<M-_>");
        assert_eq!(keys(b"\x1b^"), "<M-^>");
        assert_eq!(keys(b"\x1bPx"), "<M-P>x");
        assert_eq!(keys(b"\x1b<"), "<M-lt>");
        assert_eq!(keys(b"\x1b "), "<M-Space>");
        assert_eq!(keys(b"\x1b\x01"), "<M-C-a>");
        assert_eq!(keys(b"\x1b\x7f"), "<M-BS>");
        assert_eq!(keys(b"\x1b\x1b[A"), "<M-Up>");
    }

    /// Both cursor-key modes, with xterm's modifiers.
    #[test]
    fn cursor_and_editing_keys() {
        assert_eq!(keys(b"\x1b[A"), "<Up>");
        assert_eq!(keys(b"\x1bOB"), "<Down>");
        assert_eq!(keys(b"\x1b[1;5C"), "<C-Right>");
        assert_eq!(keys(b"\x1b[1;2D"), "<S-Left>");
        assert_eq!(keys(b"\x1b[1;7H"), "<C-M-Home>");
        assert_eq!(keys(b"\x1b[3~"), "<Del>");
        assert_eq!(keys(b"\x1b[5;5~"), "<C-PageUp>");
        assert_eq!(keys(b"\x1b[Z"), "<S-Tab>");
        assert_eq!(keys(b"\x1b[2~"), "<Insert>");
    }

    #[test]
    fn function_keys_by_every_spelling() {
        assert_eq!(keys(b"\x1bOP"), "<F1>");
        assert_eq!(keys(b"\x1b[1;2Q"), "<S-F2>");
        assert_eq!(keys(b"\x1b[13~"), "<F3>");
        assert_eq!(keys(b"\x1b[15~"), "<F5>");
        assert_eq!(keys(b"\x1b[24;5~"), "<C-F12>");
        assert_eq!(keys(b"\x1b[57376u"), "<F13>");
    }

    #[test]
    fn the_keypad_by_its_application_keys() {
        assert_eq!(keys(b"\x1bOM"), "<kEnter>");
        assert_eq!(keys(b"\x1bOp\x1bOy"), "<k0><k9>");
        assert_eq!(keys(b"\x1bOk"), "<kPlus>");
        assert_eq!(keys(b"\x1b[57414u"), "<kEnter>");
    }

    /// Kitty codes for keys without a character, codepoints otherwise; Shift
    /// alone is folded into the character.
    #[test]
    fn the_kitty_keyboard_protocol() {
        assert_eq!(keys(b"\x1b[27u"), "<Esc>");
        assert_eq!(keys(b"\x1b[97;5u"), "<C-a>");
        assert_eq!(keys(b"\x1b[97;6u"), "<C-S-a>");
        assert_eq!(keys(b"\x1b[97;2u"), "A");
        assert_eq!(keys(b"\x1b[32;5u"), "<C-Space>");
        assert_eq!(keys(b"\x1b[13;2u"), "<S-CR>");
        assert_eq!(keys(b"\x1b[9;5u"), "<C-Tab>");
        assert_eq!(keys(b"\x1b[127;3u"), "<M-BS>");
        assert_eq!(keys(b"\x1b[60;3u"), "<M-lt>");
        assert_eq!(keys(b"\x1b[92;5u"), "<C-Bslash>");
        // Caps Lock and Num Lock say nothing about the chord.
        assert_eq!(keys(b"\x1b[97;69u"), "<C-a>");
        // A release is no keystroke.
        assert!(read(b"\x1b[97;5:3u").is_empty());
    }

    #[test]
    fn modify_other_keys() {
        assert_eq!(keys(b"\x1b[27;5;105~"), "<C-i>");
        assert_eq!(keys(b"\x1b[27;6;65~"), "<C-S-A>");
    }

    #[test]
    fn a_lone_escape_waits_for_the_timeout() {
        let mut p = Parser::default();
        let mut out = Vec::new();
        p.feed(b"\x1b", &mut out);
        assert!(out.is_empty());
        assert!(p.waiting());
        p.timeout(&mut out);
        assert_eq!(out, vec![Input::Keys("<Esc>".into())]);
        assert!(!p.waiting());
    }

    #[test]
    fn a_split_sequence_is_joined() {
        let mut p = Parser::default();
        let mut out = Vec::new();
        p.feed(b"a\x1b[1;", &mut out);
        assert_eq!(out, vec![Input::Keys("a".into())]);
        p.feed(b"5A", &mut out);
        assert_eq!(out[1], Input::Keys("<C-Up>".into()));
        // A character split the same way.
        let bytes = "中".as_bytes();
        let mut out = Vec::new();
        p.feed(&bytes[..1], &mut out);
        assert!(out.is_empty());
        p.feed(&bytes[1..], &mut out);
        assert_eq!(out, vec![Input::Keys("中".into())]);
    }

    /// Clicks, drags, releases, wheel and moves, with modifiers; 0-based.
    #[test]
    fn sgr_mouse_reports() {
        let m = |bytes: &[u8]| match read(bytes).pop() {
            Some(Input::Mouse(m)) => (m.button, m.action, m.mods, m.row, m.col),
            other => panic!("{other:?}"),
        };
        assert_eq!(m(b"\x1b[<0;5;3M"), ("left", "press", String::new(), 2, 4));
        assert_eq!(m(b"\x1b[<0;5;3m"), ("left", "release", String::new(), 2, 4));
        assert_eq!(m(b"\x1b[<32;6;3M"), ("left", "drag", String::new(), 2, 5));
        assert_eq!(m(b"\x1b[<2;1;1M"), ("right", "press", String::new(), 0, 0));
        assert_eq!(m(b"\x1b[<64;1;1M"), ("wheel", "up", String::new(), 0, 0));
        assert_eq!(m(b"\x1b[<65;1;1M"), ("wheel", "down", String::new(), 0, 0));
        assert_eq!(m(b"\x1b[<35;9;9M"), ("move", "", String::new(), 8, 8));
        assert_eq!(m(b"\x1b[<20;1;1M"), ("left", "press", "SC".into(), 0, 0));
        assert_eq!(m(b"\x1b[<128;1;1M"), ("x1", "press", String::new(), 0, 0));
    }

    #[test]
    fn focus_reports() {
        assert_eq!(read(b"\x1b[I"), vec![Input::Focus(true)]);
        assert_eq!(read(b"\x1b[O"), vec![Input::Focus(false)]);
    }

    /// Replies are passed on without their terminator.
    #[test]
    fn replies_are_not_keys() {
        assert_eq!(
            read(b"\x1b[?62;22c"),
            vec![Input::Reply(Reply::DeviceAttributes(
                b"\x1b[?62;22c".to_vec()
            ))]
        );
        assert_eq!(read(b"\x1b[?1u"), vec![Input::Reply(Reply::KittyFlags)]);
        assert_eq!(
            read(b"\x1b]11;rgb:1414/1616/1b1b\x1b\\"),
            vec![Input::Reply(Reply::Other(
                b"\x1b]11;rgb:1414/1616/1b1b".to_vec()
            ))]
        );
        assert_eq!(
            read(b"\x1b]11;rgb:0/0/0\x07x"),
            vec![
                Input::Reply(Reply::Other(b"\x1b]11;rgb:0/0/0".to_vec())),
                Input::Keys("x".into())
            ]
        );
        assert_eq!(
            read(b"\x1bP1+r5463\x1b\\"),
            vec![Input::Reply(Reply::Other(b"\x1bP1+r5463".to_vec()))]
        );
        assert_eq!(
            read(b"\x1b[0n"),
            vec![Input::Reply(Reply::Other(b"\x1b[0n".to_vec()))]
        );
        assert_eq!(
            read(b"\x1b[?2026;2$y"),
            vec![Input::Reply(Reply::Other(b"\x1b[?2026;2$y".to_vec()))]
        );
    }

    /// Known as soon as its first bytes say so, not before: a lone `ESC ]` may
    /// be Alt and `]`.
    #[test]
    fn a_reply_under_way_is_known_for_one() {
        let held = |bytes: &[u8]| {
            let mut p = Parser::default();
            let mut out = Vec::new();
            p.feed(bytes, &mut out);
            assert!(out.is_empty(), "{out:?}");
            p.in_reply()
        };
        assert!(held(b"\x1b]52;c;aGVsbG8"));
        assert!(held(b"\x1bP1+r4d73"));
        assert!(!held(b"\x1b"));
        assert!(!held(b"\x1b]"));
        assert!(!held(b"\x1b[1;"));
    }

    /// Dropped through its end over any number of reads; the keys after it
    /// are still keys.
    #[test]
    fn a_reply_too_long_to_keep_is_dropped_to_its_end() {
        let mut p = Parser::default();
        let mut out = Vec::new();
        p.feed(b"\x1b]52;c;", &mut out);
        let chunk = vec![b'A'; 64 * 1024];
        for _ in 0..(MAX_STRING / chunk.len() + 2) {
            p.feed(&chunk, &mut out);
        }
        assert!(p.in_reply());
        p.feed(b"AAAA", &mut out);
        p.feed(b"AA\x1b", &mut out);
        p.feed(b"\\x", &mut out);
        assert_eq!(out, vec![Input::Keys("x".into())]);
        assert!(!p.in_reply());
        // And one ended by BEL.
        let mut p = Parser::default();
        let mut out = Vec::new();
        p.feed(b"\x1b]52;c;", &mut out);
        for _ in 0..(MAX_STRING / chunk.len() + 2) {
            p.feed(&chunk, &mut out);
        }
        p.feed(b"AA\x07y", &mut out);
        assert_eq!(out, vec![Input::Keys("y".into())]);
    }

    /// Split anywhere, even inside the end marker or a character, the same
    /// bytes come out, in parts numbered as `nvim_paste` counts them.
    #[test]
    fn a_bracketed_paste_comes_out_whole_however_it_was_read() {
        let whole = "\x1b[200~line one\r\nl<ine 中\x1b[201~x".as_bytes();
        for split in 0..whole.len() {
            let mut p = Parser::default();
            let mut out = Vec::new();
            p.feed(&whole[..split], &mut out);
            p.feed(&whole[split..], &mut out);
            let mut data = Vec::new();
            let mut phases = Vec::new();
            let mut after = String::new();
            for i in out {
                match i {
                    Input::Paste { phase, data: d } => {
                        assert!(std::str::from_utf8(&d).is_ok(), "split at {split}");
                        data.extend_from_slice(&d);
                        phases.push(phase);
                    }
                    Input::Keys(k) => after.push_str(&k),
                    other => panic!("{other:?}"),
                }
            }
            assert_eq!(data, "line one\r\nl<ine 中".as_bytes(), "split at {split}");
            if phases != [-1] {
                assert_eq!(phases.first(), Some(&1), "split at {split}");
                assert_eq!(phases.last(), Some(&3), "split at {split}");
                assert!(phases[1..phases.len() - 1].iter().all(|p| *p == 2));
            }
            assert_eq!(after, "x", "split at {split}");
        }
    }

    /// Consecutive keys go as one call; anything else between splits them.
    #[test]
    fn plain_keys_are_batched() {
        assert_eq!(
            read(b"ab\x1b[Ic"),
            vec![
                Input::Keys("ab".into()),
                Input::Focus(true),
                Input::Keys("c".into())
            ]
        );
    }

    #[test]
    fn invalid_bytes_are_dropped() {
        assert_eq!(keys(b"a\xffb"), "ab");
    }
}
