//! What the terminal sends, as what Neovim takes.
//!
//! `nvim --remote-ui` reads its terminal with libtermkey and hands the server
//! keys in Neovim's own notation; this is the same job for this client, and
//! it reads the same encodings, since nvmux passes every byte on as the
//! terminal spelt it (see [`crate::keyseq`]):
//!
//! - plain text, and the C0 bytes as `Ctrl` chords — `0x08` is `<C-h>` and
//!   `0x7f` is `<BS>`, as termkey makes them for Neovim;
//! - `ESC` and a key as that key with `Alt`;
//! - `CSI` and `SS3` keys, with xterm's modifier parameter — the arrows,
//!   Home and End, the function keys, the keypad;
//! - the kitty keyboard protocol's `CSI … u`, which the client asks for where
//!   the terminal has it (see `super::term`), and xterm's `modifyOtherKeys`
//!   (`CSI 27 ; mods ; key ~`) where it does not;
//! - SGR mouse reports, bracketed paste, and focus reports;
//! - and the terminal's answers to questions — the client's own, which it
//!   keeps, and anybody else's, which go to the server as a `termresponse`
//!   without the string terminator, which is how the server's handlers expect
//!   them (`vim.tty.query` and the background detection in `_core/defaults`).
//!
//! # A lone `ESC`
//!
//! The Escape key and the first byte of every sequence are the same byte, so
//! an `ESC` at the end of what has arrived could be either. It is held, and
//! [`Parser::waiting`] says so, until the rest comes or `'ttimeoutlen'`
//! passes — Neovim's own TUI's rule, and the reason it has that option. A
//! sequence cut short by the end of a read is held the same way.
//!
//! An answer already known to be one — an OSC or a DCS string whose first
//! bytes no key makes ([`Parser::in_reply`]) — is no key to be let go of
//! quickly, and is given longer: an OSC 52 paste can be hundreds of kilobytes
//! long. One longer still than the client keeps is dropped up to its end,
//! never typed.

use std::fmt::Write as _;

/// One thing the terminal said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Keys in Neovim's notation, for `nvim_input`. Runs of plain keys come as
    /// one.
    Keys(String),
    Mouse(Mouse),
    /// Part of a bracketed paste, for `nvim_paste`: `phase` 1 for the first
    /// part, 2 for one in the middle, 3 for the last, and -1 for a paste that
    /// arrived all at once.
    Paste {
        phase: i64,
        data: Vec<u8>,
    },
    /// The terminal gained or lost focus.
    Focus(bool),
    /// An answer to a question.
    Reply(Reply),
}

/// A terminal's answer to a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Primary device attributes: `CSI ? … c`.
    DeviceAttributes(Vec<u8>),
    /// The kitty keyboard protocol's flags: `CSI ? flags u`.
    KittyFlags,
    /// Anything else, as the server is to be handed it.
    Other(Vec<u8>),
}

/// A mouse report, in `nvim_input_mouse`'s terms, at a cell of the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mouse {
    pub button: &'static str,
    pub action: &'static str,
    /// `S`, `A`, `C` in any combination.
    pub mods: String,
    pub row: usize,
    pub col: usize,
}

/// Where a parse of what has arrived got to.
enum Step {
    /// The first `n` bytes made this.
    Took(usize, Option<Input>),
    /// Not enough has arrived to say.
    More,
    /// The first `n` bytes are a reply too long to keep, and not yet ended:
    /// they go, and so does the rest of it as it comes.
    TooLong(usize),
}

/// The longest a sequence is let grow before it is given up on: past this it
/// is no key, and holding it would hold every key after it. An OSC reply —
/// an OSC 52 paste — is allowed far longer.
const MAX_SEQUENCE: usize = 256;
const MAX_STRING: usize = 1 << 20;

/// What may follow `ESC ]`, `ESC P` and `ESC _` in a reply, and in no Alt
/// chord: an OSC's number, the few characters a DCS reply starts with,
/// kitty's `G` for a graphics reply.
const OSC_OPENS: &[u8] = b"0123456789";
const DCS_OPENS: &[u8] = b"0123456789>|+$=";
const APC_OPENS: &[u8] = b"G";

/// The parser, and what it is holding.
#[derive(Debug, Default)]
pub struct Parser {
    pending: Vec<u8>,
    /// Inside a bracketed paste, and whether its first part has gone.
    paste: Option<bool>,
    /// Inside a reply too long to keep — an OSC, a DCS or an APC, by its
    /// introducer — whose rest is dropped up to its end.
    skipping: Option<u8>,
}

/// The bracketed paste's markers.
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

impl Parser {
    /// Read `bytes`, appending what they make to `out`.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Input>) {
        self.pending.extend_from_slice(bytes);
        self.run(out, false);
    }

    /// Whether something is held to see what comes next.
    pub fn waiting(&self) -> bool {
        !self.pending.is_empty() && self.paste.is_none()
    }

    /// Whether what is held is a reply part way through — an OSC or a DCS
    /// string already known to be no key — or the rest of one being dropped:
    /// see the module docs.
    pub fn in_reply(&self) -> bool {
        self.paste.is_none() && (self.skipping.is_some() || string_opener(&self.pending).is_some())
    }

    /// Nothing more came: what is held is what it is.
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
                        // All of it, but an `ESC` that may begin the `ESC \`
                        // that ends it.
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

    /// The part of a paste from `at`: up to its end marker, or up to the end
    /// of what has arrived, keeping back what could be the start of the
    /// marker or of a character. `None` when there is nothing to hand on yet.
    fn paste_part(&mut self, at: usize, out: &mut Vec<Input>) -> Option<usize> {
        let rest = &self.pending[at..];
        let first = !self.paste.unwrap_or(false);
        if let Some(end) = find(rest, PASTE_END) {
            let data = rest[..end].to_vec();
            // The whole paste in one read is one call (-1); otherwise this is
            // the last of several.
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

/// How many bytes at the end of `bytes` are a character not yet complete.
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

/// How long a UTF-8 sequence starting with `b` is; 1 for a byte that starts
/// none, which is then taken alone.
fn utf8_len(b: u8) -> usize {
    match b {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    }
}

/// One thing from the start of `bytes`, which is not empty.
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
                // Not a character: dropped, a byte at a time.
                Err(_) => Step::Took(1, None),
            }
        }
    }
}

/// A key typed on its own: itself, or `<lt>` for the one character the
/// notation reserves.
fn plain(c: char) -> String {
    if c == '<' {
        "<lt>".into()
    } else {
        c.to_string()
    }
}

/// A C0 byte or DEL as the chord it is, with `mods` in front.
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

/// What follows an `ESC`.
fn escape(bytes: &[u8], final_: bool) -> Step {
    let Some(&next) = bytes.get(1) else {
        return if final_ {
            Step::Took(1, Some(Input::Keys("<Esc>".into())))
        } else {
            Step::More
        };
    };
    // An OSC, a DCS or an APC reply opens with what no Alt chord does (see
    // `OSC_OPENS`). Anything else after `ESC ]`, `ESC P` or `ESC _` is Alt and
    // a key — and so is a lone one the wait gave up on.
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
            // Graphics replies answer nothing the server asked, and are
            // dropped.
            Step::Took(n, _) => Step::Took(n, None),
            more => more,
        },
        // `ESC ESC [ A`: Alt and an arrow, as some terminals spell it. Two
        // Escapes and nothing after them are two Escapes.
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

/// Whether these bytes are a key's escape sequence rather than a reply or a
/// lone escape, for the `ESC ESC` form of Alt.
fn is_key_sequence(seq: &[u8]) -> bool {
    seq.len() > 2 && matches!(seq[1], b'[' | b'O')
}

/// A key given Alt.
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

/// A character as it is written inside `<…>`.
fn name_in_brackets(c: char) -> String {
    match c {
        ' ' => "Space".into(),
        '<' => "lt".into(),
        '\\' => "Bslash".into(),
        '|' => "Bar".into(),
        c => c.to_string(),
    }
}

/// The introducer of the reply `bytes` open — `]` for an OSC, `P` for a DCS,
/// `_` for an APC — if they open one no key could: the test [`escape`] makes,
/// once there is a byte after the introducer to make it on.
fn string_opener(bytes: &[u8]) -> Option<u8> {
    match bytes {
        [0x1b, b']', b, ..] if OSC_OPENS.contains(b) => Some(b']'),
        [0x1b, b'P', b, ..] if DCS_OPENS.contains(b) => Some(b'P'),
        [0x1b, b'_', b, ..] if APC_OPENS.contains(b) => Some(b'_'),
        _ => None,
    }
}

/// Where a reply of this kind that `bytes` are the middle of ends: just past
/// its terminator, BEL for an OSC or `ESC \` for any.
fn string_end(bytes: &[u8], kind: u8) -> Option<usize> {
    bytes.iter().enumerate().find_map(|(i, b)| match b {
        0x07 if kind == b']' => Some(i + 1),
        0x1b if bytes.get(i + 1) == Some(&b'\\') => Some(i + 2),
        _ => None,
    })
}

/// An OSC or DCS reply, up to its terminator — BEL, or `ESC \` — handed on
/// without it.
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
                    // Another sequence starting before this one ended: this
                    // one is abandoned where it stands.
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

/// `ESC O` and a final byte, maybe with xterm's modifier between: the cursor
/// keys in application mode, F1 to F4, the keypad.
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
        // Not a key: Alt-O, and what came after it stands on its own.
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
        // A byte no sequence holds: the sequence ended before it, broken.
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

    /// Parameter `i`'s main value, 0 for one absent.
    fn get(&self, i: usize) -> u32 {
        self.0.get(i).and_then(|p| p.first()).copied().unwrap_or(0)
    }

    /// Sub-parameter `j` of parameter `i`.
    fn sub(&self, i: usize, j: usize) -> Option<u32> {
        self.0.get(i).and_then(|p| p.get(j)).copied()
    }
}

/// A modifier parameter — one plus a mask: shift 1, alt 2, ctrl 4, super 8,
/// hyper 16, meta 32, and the locks, which say nothing about the chord — as
/// the prefix Neovim writes for it.
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

/// A key with a letter for its final byte: the cursor keys, Home and End,
/// F1 to F4, Shift-Tab, the keypad's middle.
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

/// `CSI number ; mods ~`: the editing keys and F5 on, by number — and
/// xterm's `modifyOtherKeys`, which is `CSI 27 ; mods ; key ~`.
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
/// A release is no keystroke and is dropped; the client asks for no
/// releases, but a terminal another client set up may send them.
fn kitty(params: &Params) -> Option<Input> {
    if params.sub(1, 1) == Some(3) {
        return None;
    }
    let mods = if params.len() >= 2 { params.get(1) } else { 1 };
    key_code(params.get(0), mods)
}

/// A key by its code — a codepoint, or one of kitty's for a key that has
/// none — with a modifier parameter.
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
    // Shift on its own with a character is that character shifted: the
    // terminal sends it as text unless it was asked for more, and Neovim
    // reads `<S-a>` as `A` anyway.
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

/// An SGR (or X10) mouse report as `nvim_input_mouse` takes it: `b` the
/// report's button byte, `col` and `row` from zero.
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
            // No button: a plain move, or a legacy release that does not say
            // which button went up.
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

    /// The C0 bytes as termkey makes them for Neovim.
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

    /// The cursor keys in both modes, with xterm's modifiers.
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

    /// kitty's form: codes for keys with no character, codepoints for the
    /// rest, and the shift folded into the character when it is the only
    /// modifier.
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

    /// A lone Escape is held until it is plain that nothing follows it.
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

    /// A sequence split by a read is put back together.
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

    /// Clicks, drags, releases, the wheel and plain moves, with their
    /// modifiers, from zero.
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

    /// Replies are told from keys, and handed on without their terminator.
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

    /// A reply under way is known for one as soon as its first bytes say so,
    /// and not before: a lone `ESC ]` may be Alt and `]`.
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

    /// A reply longer than the client keeps is dropped, all of it, up to its
    /// end however many reads it takes — and the keys after it are keys.
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

    /// A paste in one read, split across reads, and split in the middle of
    /// its end marker or of a character: the same bytes come out, in parts
    /// numbered as `nvim_paste` counts them.
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

    /// Keys in a row go to Neovim as one call; anything else in between
    /// splits them.
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

    /// Bytes that are not a character are dropped rather than typed.
    #[test]
    fn invalid_bytes_are_dropped() {
        assert_eq!(keys(b"a\xffb"), "ab");
    }
}
