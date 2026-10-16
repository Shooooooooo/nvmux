//! The terminal's input read as keys and mouse reports, for the settings
//! when they float over a session (see `crate::pty::panel`).
//!
//! nvmux's own screens read the terminal through crossterm, which has it to
//! itself while they are up. Over a session the relay has it instead, a read
//! at a time (see [`crate::pty`]), and the terminal goes on spelling keys the
//! way the session's client asked it to: as legacy control bytes, in the
//! kitty keyboard protocol, or in xterm's `modifyOtherKeys` (see
//! [`crate::keyseq`]). So this reads all three into crossterm's own events,
//! which the settings take as they take them from crossterm on their own
//! screen — and the mouse reports nvmux asks the terminal for while the
//! settings are up, in SGR's spelling or X10's.
//!
//! Not everything that arrives is for the settings. The terminal answers the
//! questions the session's client asks it — its attributes, a colour, the
//! cursor's position, a mode — and tells the client its size and when it
//! gains or loses focus; and the client goes on asking while the settings are
//! up, since it goes on drawing behind them. All of that goes on to the
//! client untouched ([`Input::Client`]). A paste goes nowhere: typed into the
//! settings, a `w` in it would save them and a `q` take them down.
//!
//! nvmux's own client reads the same spellings (see
//! [`crate::client::input`]), into Neovim's notation for its server, with the
//! answers reshaped as the server's handlers take them. This is the other
//! direction's need: crossterm's events for a screen of nvmux's own, and the
//! answers passed on as they came, for a client that asked for them.

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Longer than this and a sequence is not one anything sends a screen: what
/// is kept of it so far is dropped rather than waited on.
const MAX_SEQUENCE: usize = 4096;

/// What a piece of the terminal's input turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Key(KeyEvent),
    Mouse(MouseEvent),
    /// For the session's client, as it came: the terminal answering one of
    /// its questions, or telling it its size or its focus.
    Client(Vec<u8>),
}

/// The terminal's input as it arrives, a read at a time: a sequence cut off
/// by the end of one read waits here for the rest of it.
#[derive(Debug, Default)]
pub struct Decoder {
    pending: Vec<u8>,
    /// Inside a bracketed paste, which goes nowhere.
    pasting: bool,
}

/// One piece of input, read or not yet.
enum Parsed {
    /// More bytes are needed to tell what this is.
    Need,
    /// The first `len` bytes are one piece.
    Done(usize, Piece),
}

enum Piece {
    Input(Input),
    PasteStart,
    PasteEnd,
    /// Read, and for nobody: a key the settings have no use for, a paste, or
    /// something no terminal should have sent.
    Nothing,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// What `bytes`, read after whatever came before, make: every piece that
    /// is whole, in order. A piece cut off at the end waits for the rest.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Input> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut at = 0;
        while at < self.pending.len() {
            let rest = &self.pending[at..];
            let parsed = if self.pasting {
                pasted(rest)
            } else {
                parse(rest)
            };
            match parsed {
                Parsed::Need if rest.len() > MAX_SEQUENCE => at = self.pending.len(),
                Parsed::Need => break,
                Parsed::Done(len, piece) => {
                    at += len.max(1);
                    match piece {
                        Piece::Input(input) => out.push(input),
                        Piece::PasteStart => self.pasting = true,
                        Piece::PasteEnd => self.pasting = false,
                        Piece::Nothing => {}
                    }
                }
            }
        }
        self.pending.drain(..at);
        out
    }

    /// Whether a piece is waiting for the rest of itself.
    pub fn waiting(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The rest of what is waiting never came: a lone `ESC` is the Escape
    /// key, and anything else cut off is dropped.
    pub fn flush(&mut self) -> Vec<Input> {
        let lone_esc = self.pending == [ESC] && !self.pasting;
        self.pending.clear();
        if lone_esc {
            vec![key(KeyCode::Esc, KeyModifiers::NONE)]
        } else {
            Vec::new()
        }
    }
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Input {
    key_as(code, modifiers, KeyEventKind::Press)
}

fn key_as(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> Input {
    Input::Key(KeyEvent {
        code,
        modifiers,
        kind,
        state: KeyEventState::NONE,
    })
}

fn done(len: usize, input: Input) -> Parsed {
    Parsed::Done(len, Piece::Input(input))
}

/// Inside a paste: everything up to its end, which is dropped.
fn pasted(bytes: &[u8]) -> Parsed {
    const END: &[u8] = b"\x1b[201~";
    if let Some(at) = bytes.windows(END.len()).position(|w| w == END) {
        return Parsed::Done(at + END.len(), Piece::PasteEnd);
    }
    // All but what could be the start of the end.
    let keep = (1..END.len())
        .rev()
        .find(|&n| bytes.len() >= n && bytes.ends_with(&END[..n]))
        .unwrap_or(0);
    match bytes.len() - keep {
        0 => Parsed::Need,
        n => Parsed::Done(n, Piece::Nothing),
    }
}

fn parse(bytes: &[u8]) -> Parsed {
    let b = bytes[0];
    let ctrl = KeyModifiers::CONTROL;
    match b {
        ESC => escape(bytes),
        0x00 => done(1, key(KeyCode::Char(' '), ctrl)),
        b'\t' => done(1, key(KeyCode::Tab, KeyModifiers::NONE)),
        b'\r' | b'\n' => done(1, key(KeyCode::Enter, KeyModifiers::NONE)),
        0x7f | 0x08 => done(1, key(KeyCode::Backspace, KeyModifiers::NONE)),
        0x01..=0x1a => done(1, key(KeyCode::Char(char::from(b - 1 + b'a')), ctrl)),
        0x1c..=0x1f => done(1, key(KeyCode::Char(char::from(b - 0x1c + b'4')), ctrl)),
        _ => text(bytes),
    }
}

/// A character typed, in UTF-8: an upper-case letter is shifted, as
/// crossterm has it.
fn text(bytes: &[u8]) -> Parsed {
    let len = match bytes[0] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return Parsed::Done(1, Piece::Nothing),
    };
    if bytes.len() < len {
        return Parsed::Need;
    }
    let Some(c) = std::str::from_utf8(&bytes[..len])
        .ok()
        .and_then(|s| s.chars().next())
    else {
        return Parsed::Done(1, Piece::Nothing);
    };
    let modifiers = if c.is_uppercase() {
        KeyModifiers::SHIFT
    } else {
        KeyModifiers::NONE
    };
    done(len, key(KeyCode::Char(c), modifiers))
}

fn escape(bytes: &[u8]) -> Parsed {
    let Some(&second) = bytes.get(1) else {
        return Parsed::Need;
    };
    match second {
        b'[' => csi(bytes),
        b'O' => ss3(bytes),
        // OSC, ended by `BEL` or `ST`; and the strings ended by `ST` alone:
        // DCS, APC, PM and SOS. All of them replies, for the client.
        b']' => string(bytes, true),
        b'P' | b'_' | b'^' | b'X' => string(bytes, false),
        // The Escape key, and the start of whatever follows it.
        ESC => done(1, key(KeyCode::Esc, KeyModifiers::NONE)),
        // `Alt` and a key, which the settings have no use for.
        _ => match text(&bytes[1..]) {
            Parsed::Need => Parsed::Need,
            Parsed::Done(len, _) => Parsed::Done(1 + len, Piece::Nothing),
        },
    }
}

/// A string sequence, up to its terminator: `ST`, or for an OSC `BEL` too.
fn string(bytes: &[u8], bel: bool) -> Parsed {
    for (i, &b) in bytes.iter().enumerate().skip(2) {
        if bel && b == BEL {
            return done(i + 1, Input::Client(bytes[..=i].to_vec()));
        }
        if b == b'\\' && bytes[i - 1] == ESC {
            return done(i + 1, Input::Client(bytes[..=i].to_vec()));
        }
    }
    Parsed::Need
}

/// `ESC O` and one byte: the arrows and the rest of the cursor keys in
/// application mode, and F1 to F4.
fn ss3(bytes: &[u8]) -> Parsed {
    let Some(&fin) = bytes.get(2) else {
        return Parsed::Need;
    };
    let code = match fin {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'M' => KeyCode::Enter,
        b'P'..=b'S' => KeyCode::F(fin - b'P' + 1),
        _ => return Parsed::Done(3, Piece::Nothing),
    };
    done(3, key(code, KeyModifiers::NONE))
}

/// A `CSI` sequence: a key, a mouse report, or a reply for the client.
fn csi(bytes: &[u8]) -> Parsed {
    // X10's mouse report: `CSI M` and three bytes, each a value plus 32.
    if bytes.get(2) == Some(&b'M') {
        let Some(&[cb, cx, cy]) = bytes.get(3..6).and_then(|b| <&[u8; 3]>::try_from(b).ok()) else {
            return Parsed::Need;
        };
        let column = u16::from(cx.saturating_sub(33));
        let row = u16::from(cy.saturating_sub(33));
        return match mouse(u16::from(cb.saturating_sub(32)), true, column, row) {
            Some(m) => done(6, Input::Mouse(m)),
            None => Parsed::Done(6, Piece::Nothing),
        };
    }
    let mut end = None;
    for (i, &b) in bytes.iter().enumerate().skip(2) {
        match b {
            0x20..=0x3f => {}
            0x40..=0x7e => {
                end = Some(i);
                break;
            }
            // Not a sequence after all: what came of it so far is dropped.
            _ => return Parsed::Done(i, Piece::Nothing),
        }
    }
    let Some(end) = end else {
        return Parsed::Need;
    };
    let len = end + 1;
    let params = &bytes[2..end];
    let fin = bytes[end];
    let whole = || Input::Client(bytes[..len].to_vec());
    match (params.first(), fin) {
        (Some(b'<'), b'M' | b'm') => match sgr_mouse(&params[1..], fin == b'M') {
            Some(m) => done(len, Input::Mouse(m)),
            None => Parsed::Done(len, Piece::Nothing),
        },
        // A private marker first: a reply, which no key ever has.
        (Some(b'?' | b'>' | b'='), _) => done(len, whole()),
        // The cursor's position; focus in and out; the size, in band or
        // asked for; a status report; the attributes; a mode.
        (_, b'R' | b'I' | b'O' | b't' | b'n' | b'c' | b'y') => done(len, whole()),
        (_, b'u') => match kitty(params) {
            Some(input) => done(len, input),
            None => Parsed::Done(len, Piece::Nothing),
        },
        (_, b'~') => tilde(params, len),
        (_, b'A' | b'B' | b'C' | b'D' | b'H' | b'F' | b'Z' | b'P' | b'Q' | b'S') => {
            let fields = fields(params);
            let (modifiers, kind) = modifiers(fields.get(1));
            let code = match fin {
                b'A' => KeyCode::Up,
                b'B' => KeyCode::Down,
                b'C' => KeyCode::Right,
                b'D' => KeyCode::Left,
                b'H' => KeyCode::Home,
                b'F' => KeyCode::End,
                b'Z' => KeyCode::BackTab,
                b'P' => KeyCode::F(1),
                b'Q' => KeyCode::F(2),
                _ => KeyCode::F(4),
            };
            done(len, key_as(code, modifiers, kind))
        }
        _ => Parsed::Done(len, Piece::Nothing),
    }
}

/// A sequence's parameters, each its sub-parameters: `1;5:3` is `[[1], [5, 3]]`.
fn fields(params: &[u8]) -> Vec<Vec<u32>> {
    let text = std::str::from_utf8(params).unwrap_or_default();
    text.split(';')
        .map(|field| field.split(':').map(|n| n.parse().unwrap_or(0)).collect())
        .collect()
}

/// A modifier field — one plus a mask, and after a colon the event — as
/// crossterm's modifiers and the kind of key event.
fn modifiers(field: Option<&Vec<u32>>) -> (KeyModifiers, KeyEventKind) {
    let mask = field
        .and_then(|f| f.first())
        .copied()
        .unwrap_or(1)
        .saturating_sub(1);
    let mut m = KeyModifiers::NONE;
    for (bit, modifier) in [
        (1, KeyModifiers::SHIFT),
        (2, KeyModifiers::ALT),
        (4, KeyModifiers::CONTROL),
        (8, KeyModifiers::SUPER),
        (16, KeyModifiers::HYPER),
        (32, KeyModifiers::META),
    ] {
        if mask & bit != 0 {
            m |= modifier;
        }
    }
    let kind = match field.and_then(|f| f.get(1)) {
        Some(2) => KeyEventKind::Repeat,
        Some(3) => KeyEventKind::Release,
        _ => KeyEventKind::Press,
    };
    (m, kind)
}

/// The kitty protocol's spelling: `CSI key[:shifted[:base]] ; modifiers[:event] u`.
fn kitty(params: &[u8]) -> Option<Input> {
    let fields = fields(params);
    let code = *fields.first()?.first()?;
    let (modifiers, kind) = modifiers(fields.get(1));
    let code = match code {
        9 if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
        9 => KeyCode::Tab,
        13 | 57414 => KeyCode::Enter,
        27 => KeyCode::Esc,
        8 | 127 => KeyCode::Backspace,
        c => {
            let c = char::from_u32(c).filter(|c| !c.is_control())?;
            KeyCode::Char(if modifiers.contains(KeyModifiers::SHIFT) {
                c.to_ascii_uppercase()
            } else {
                c
            })
        }
    };
    Some(key_as(code, modifiers, kind))
}

/// `CSI number [; modifiers] ~`: the editing keys and the function keys, a
/// paste's two ends, and xterm's `modifyOtherKeys` (`CSI 27 ; modifiers ;
/// key ~`).
fn tilde(params: &[u8], len: usize) -> Parsed {
    let fields = fields(params);
    let number = fields.first().and_then(|f| f.first()).copied().unwrap_or(0);
    let (modifiers, kind) = modifiers(fields.get(1));
    let code = match number {
        200 => return Parsed::Done(len, Piece::PasteStart),
        201 => return Parsed::Done(len, Piece::PasteEnd),
        27 => {
            let other = fields.get(2).and_then(|f| f.first()).copied();
            return match other.and_then(|c| kitty(format!("{c};{}", fields[1][0]).as_bytes())) {
                Some(input) => done(len, input),
                None => Parsed::Done(len, Piece::Nothing),
            };
        }
        1 | 7 => KeyCode::Home,
        4 | 8 => KeyCode::End,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        n @ 11..=15 => KeyCode::F((n - 10) as u8),
        n @ 17..=21 => KeyCode::F((n - 11) as u8),
        n @ 23..=24 => KeyCode::F((n - 12) as u8),
        _ => return Parsed::Done(len, Piece::Nothing),
    };
    done(len, key_as(code, modifiers, kind))
}

/// SGR's mouse report, after its `<`: `button ; column ; row`, 1-based,
/// pressed (`M`) or let go (`m`).
fn sgr_mouse(params: &[u8], pressed: bool) -> Option<MouseEvent> {
    let fields = fields(params);
    let value = |i: usize| fields.get(i).and_then(|f| f.first()).copied();
    let cb = u16::try_from(value(0)?).ok()?;
    let column = u16::try_from(value(1)?).ok()?.saturating_sub(1);
    let row = u16::try_from(value(2)?).ok()?.saturating_sub(1);
    mouse(cb, pressed, column, row)
}

/// A report's button byte, read as crossterm reads it.
fn mouse(cb: u16, pressed: bool, column: u16, row: u16) -> Option<MouseEvent> {
    let button = (cb & 0b11) | ((cb & 0b1100_0000) >> 4);
    let dragging = cb & 0b10_0000 != 0;
    let kind = match (button, dragging) {
        (0, false) => MouseEventKind::Down(MouseButton::Left),
        (1, false) => MouseEventKind::Down(MouseButton::Middle),
        (2, false) => MouseEventKind::Down(MouseButton::Right),
        (0, true) => MouseEventKind::Drag(MouseButton::Left),
        (1, true) => MouseEventKind::Drag(MouseButton::Middle),
        (2, true) => MouseEventKind::Drag(MouseButton::Right),
        (3, false) => MouseEventKind::Up(MouseButton::Left),
        (3..=5, true) => MouseEventKind::Moved,
        (4, false) => MouseEventKind::ScrollUp,
        (5, false) => MouseEventKind::ScrollDown,
        (6, false) => MouseEventKind::ScrollLeft,
        (7, false) => MouseEventKind::ScrollRight,
        _ => return None,
    };
    let kind = match kind {
        MouseEventKind::Down(b) if !pressed => MouseEventKind::Up(b),
        other => other,
    };
    let mut modifiers = KeyModifiers::NONE;
    for (bit, modifier) in [
        (4, KeyModifiers::SHIFT),
        (8, KeyModifiers::ALT),
        (16, KeyModifiers::CONTROL),
    ] {
        if cb & bit != 0 {
            modifiers |= modifier;
        }
    }
    Some(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(bytes: &[u8]) -> Vec<Input> {
        let mut d = Decoder::new();
        let out = d.feed(bytes);
        assert!(!d.waiting(), "nothing left waiting after {bytes:?}");
        out
    }

    fn pressed(code: KeyCode, modifiers: KeyModifiers) -> Input {
        key(code, modifiers)
    }

    const NONE: KeyModifiers = KeyModifiers::NONE;
    const CTRL: KeyModifiers = KeyModifiers::CONTROL;

    /// The plain spelling: text, and the control bytes the screens' keys are.
    #[test]
    fn plain_bytes_are_the_keys_they_spell() {
        assert_eq!(
            read(b"aG\x00\x03\t\r\x7f"),
            vec![
                pressed(KeyCode::Char('a'), NONE),
                pressed(KeyCode::Char('G'), KeyModifiers::SHIFT),
                pressed(KeyCode::Char(' '), CTRL),
                pressed(KeyCode::Char('c'), CTRL),
                pressed(KeyCode::Tab, NONE),
                pressed(KeyCode::Enter, NONE),
                pressed(KeyCode::Backspace, NONE),
            ]
        );
        assert_eq!(
            read("é".as_bytes()),
            vec![pressed(KeyCode::Char('é'), NONE)]
        );
    }

    /// The arrows in both their spellings, with a modifier, and Shift-Tab.
    #[test]
    fn the_cursor_keys_in_either_mode() {
        assert_eq!(
            read(b"\x1b[A\x1bOB\x1b[1;5C\x1b[Z\x1b[H\x1b[4~"),
            vec![
                pressed(KeyCode::Up, NONE),
                pressed(KeyCode::Down, NONE),
                pressed(KeyCode::Right, CTRL),
                pressed(KeyCode::BackTab, NONE),
                pressed(KeyCode::Home, NONE),
                pressed(KeyCode::End, NONE),
            ]
        );
    }

    /// The kitty protocol's keys, a release and a repeat among them, and
    /// `modifyOtherKeys`'s prefix chord.
    #[test]
    fn the_kitty_protocol_and_modify_other_keys() {
        assert_eq!(
            read(b"\x1b[27u\x1b[13u\x1b[32;5u\x1b[9;2u\x1b[27;5;32~"),
            vec![
                pressed(KeyCode::Esc, NONE),
                pressed(KeyCode::Enter, NONE),
                pressed(KeyCode::Char(' '), CTRL),
                pressed(KeyCode::BackTab, KeyModifiers::SHIFT),
                pressed(KeyCode::Char(' '), CTRL),
            ]
        );
        assert_eq!(
            read(b"\x1b[106;1:3u\x1b[106;1:2u\x1b[1;1:3B"),
            vec![
                key_as(KeyCode::Char('j'), NONE, KeyEventKind::Release),
                key_as(KeyCode::Char('j'), NONE, KeyEventKind::Repeat),
                key_as(KeyCode::Down, NONE, KeyEventKind::Release),
            ]
        );
    }

    /// SGR's mouse reports, 0-based where they land: a press, its release,
    /// plain motion, and the wheel; and X10's.
    #[test]
    fn the_mouse_in_sgr_and_x10() {
        let at = |kind, column, row| {
            Input::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: NONE,
            })
        };
        assert_eq!(
            read(b"\x1b[<0;10;5M\x1b[<0;10;5m\x1b[<35;3;3M\x1b[<64;1;1M\x1b[M !!"),
            vec![
                at(MouseEventKind::Down(MouseButton::Left), 9, 4),
                at(MouseEventKind::Up(MouseButton::Left), 9, 4),
                at(MouseEventKind::Moved, 2, 2),
                at(MouseEventKind::ScrollUp, 0, 0),
                at(MouseEventKind::Down(MouseButton::Left), 0, 0),
            ]
        );
    }

    /// What the terminal tells the client goes to the client, as it came.
    #[test]
    fn replies_and_reports_are_the_clients() {
        let replies: [&[u8]; 7] = [
            b"\x1b[?1;2c",
            b"\x1b]11;rgb:0000/0000/0000\x07",
            b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\",
            b"\x1bP>|kitty(0.31)\x1b\\",
            b"\x1b[I",
            b"\x1b[48;24;80;0;0t",
            b"\x1b[12;40R",
        ];
        for reply in replies {
            assert_eq!(
                read(reply),
                vec![Input::Client(reply.to_vec())],
                "{reply:?}"
            );
        }
    }

    /// A paste goes nowhere, however it is cut up, and what follows it is
    /// read as ever.
    #[test]
    fn a_paste_goes_nowhere() {
        assert_eq!(
            read(b"\x1b[200~wq\x1b[201~j"),
            vec![pressed(KeyCode::Char('j'), NONE)]
        );
        let mut d = Decoder::new();
        assert!(d.feed(b"\x1b[200~w").is_empty());
        assert!(d.feed(b"q\x1b[20").is_empty());
        assert_eq!(d.feed(b"1~k"), vec![pressed(KeyCode::Char('k'), NONE)]);
    }

    /// A piece cut off by the end of a read waits for the rest of itself; a
    /// lone `ESC` that nothing follows is the Escape key.
    #[test]
    fn a_piece_cut_off_waits_for_the_rest() {
        let mut d = Decoder::new();
        assert!(d.feed(b"\x1b[").is_empty());
        assert!(d.waiting());
        assert_eq!(d.feed(b"A"), vec![pressed(KeyCode::Up, NONE)]);
        assert!(d.feed(&"é".as_bytes()[..1]).is_empty());
        assert_eq!(
            d.feed(&"é".as_bytes()[1..]),
            vec![pressed(KeyCode::Char('é'), NONE)]
        );
        assert!(d.feed(b"\x1b").is_empty());
        assert_eq!(d.flush(), vec![pressed(KeyCode::Esc, NONE)]);
        assert!(!d.waiting());
        assert!(d.feed(b"\x1b]11;rgb:00").is_empty());
        assert!(d.flush().is_empty(), "an unfinished reply is dropped");
    }

    /// Two escapes in a row are the Escape key and then whatever the second
    /// one starts; Alt and a key is nobody's.
    #[test]
    fn escape_then_a_sequence_and_alt_keys() {
        assert_eq!(
            read(b"\x1b\x1b[B\x1bx"),
            vec![pressed(KeyCode::Esc, NONE), pressed(KeyCode::Down, NONE)]
        );
    }
}
