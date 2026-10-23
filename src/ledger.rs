//! Terminal state a client set once and will not set again.
//!
//! Neovim sets mouse reporting, the title, its keyboard protocol and some
//! modes only on change or at startup. nvmux's screens, other sessions'
//! clients ([`crate::config::ClientSettings::per_session`]) and relayed exit
//! sequences overwrite them. So each kept client (see [`crate::pty::Parked`])
//! has a [`Ledger`] decoding its output and recording the last value of
//! each, restored by [`Ledger::put_back`] without asking the server.
//!
//! It also tracks whether output is between frames: a client handed back
//! from its parking thread may be mid-frame, so the relay must not start on
//! the rest (see `pty::Attachment::drain_unrelayed`). It only reads a copy of
//! the bytes.

use std::io::Write;

use crate::boundary::Boundary;
use crate::term::MouseReporting;

/// Longest title or colour kept; anything longer is not a real title.
const MAX_OSC: usize = 1024;

/// OSC payload bytes shown to a watcher's parser; the rest is dropped (see
/// [`OscCap`]).
///
/// vte's OSC buffer grows unbounded and never shrinks, so a 2 MB `OSC 52`
/// would pin 2 MB in both the ledger's and the shadow's parser for the
/// client's life. Neither needs more than [`MAX_OSC`].
pub const OSC_SHOWN: usize = 4 * MAX_OSC;

static OSC_START: std::sync::LazyLock<memchr::memmem::Finder<'static>> =
    std::sync::LazyLock::new(|| memchr::memmem::Finder::new(b"\x1b]"));

/// Truncates each OSC payload to [`OSC_SHOWN`] bytes before a parser sees it,
/// with state carried across reads.
///
/// Matches vte's OSC framing: `ESC ]` from any state, payload up to `BEL`,
/// `CAN`, `SUB` or `ESC`. Enders always pass, so the parser exits the string
/// where it would have. vte also opens an OSC on `ESC`, C0 control, `]`; this
/// does not, so such an OSC passes whole.
#[derive(Debug, Default, Clone)]
pub struct OscCap {
    /// Last byte was `ESC`.
    esc: bool,
    /// Inside an OSC, with this many payload bytes shown.
    osc: Option<usize>,
}

impl OscCap {
    /// Borrowed unless an OSC exceeded the cap.
    pub fn cap<'a>(&mut self, bytes: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        let ender = |b: &u8| matches!(*b, 0x07 | 0x18 | 0x1a | 0x1b);
        let mut out: Option<Vec<u8>> = None;
        let mut i = 0;
        while i < bytes.len() {
            match self.osc {
                None => {
                    // Pass through to the next `ESC ]`; OSCs are rare, so one
                    // vectorised search per read.
                    let from = i;
                    if std::mem::take(&mut self.esc) && bytes[i] == b']' {
                        self.osc = Some(0);
                        i += 1;
                    } else if let Some(k) = OSC_START.find(&bytes[i..]) {
                        self.osc = Some(0);
                        i += k + 2;
                    } else {
                        i = bytes.len();
                        self.esc = bytes.last() == Some(&0x1b);
                    }
                    if let Some(out) = out.as_mut() {
                        out.extend_from_slice(&bytes[from..i]);
                    }
                }
                Some(shown) => {
                    let run = bytes[i..].iter().position(ender).unwrap_or(bytes.len() - i);
                    let keep = run.min(OSC_SHOWN.saturating_sub(shown));
                    if keep < run && out.is_none() {
                        out = Some(bytes[..i + keep].to_vec());
                    } else if let Some(out) = out.as_mut() {
                        out.extend_from_slice(&bytes[i..i + keep]);
                    }
                    self.osc = Some(shown + keep);
                    i += run;
                    if let Some(&end) = bytes.get(i) {
                        // Show the ender; an `ESC` may start the next OSC.
                        self.osc = None;
                        self.esc = end == 0x1b;
                        if let Some(out) = out.as_mut() {
                            out.push(end);
                        }
                        i += 1;
                    }
                }
            }
        }
        match out {
            Some(out) => std::borrow::Cow::Owned(out),
            None => std::borrow::Cow::Borrowed(bytes),
        }
    }
}

/// DEC private modes Neovim sets once and another client's exit resets:
/// application cursor keys, focus reports, bracketed paste, grapheme
/// clusters, colour-scheme reports, in-band size reports. Mouse modes are
/// tracked separately.
const MODES: [u16; 6] = [1, 1004, 2004, 2027, 2031, 2048];

/// vte's `MAX_OSC_PARAMS`: fields past this are dropped.
const MAX_OSC_FIELDS: usize = 16;

/// Sent when the client never set a cursor colour.
const CURSOR_COLOUR_RESET: &[u8] = b"\x1b]112\x07";

pub struct Ledger {
    parser: vte::Parser,
    osc: OscCap,
    told: Told,
    /// Sees every byte, uncapped.
    stream: Boundary,
    /// The parser panicked: feed nothing more and put nothing back.
    broken: bool,
    /// Modes and keypad as the terminal last heard them, before parking.
    relayed: Option<Heard>,
}

/// Modes and keypad state.
type Heard = ([Option<bool>; MODES.len()], Option<bool>);

impl std::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ledger")
            .field("told", &self.told)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Told {
    /// DEC mode 1000: button presses.
    press: bool,
    /// 1002: presses, releases and drags (Neovim's `'mouse'`).
    buttons: bool,
    /// 1003: all motion (`'mousemoveevent'`).
    motion: bool,
    /// Indexed like [`MODES`]; `None` if never set.
    modes: [Option<bool>; MODES.len()],
    /// Application (`ESC =`, true) or numeric (`ESC >`).
    keypad: Option<bool>,
    /// Kitty keyboard flags, tracked through pushes, pops and sets.
    kitty: Option<u16>,
    /// xterm `modifyOtherKeys` level.
    other_keys: Option<u16>,
    /// Last `OSC 0`/`OSC 2`, verbatim.
    title: Option<Vec<u8>>,
    /// Last `OSC 12`/`OSC 112`, verbatim.
    cursor_colour: Option<Vec<u8>>,
    /// Last `DECSCUSR` (`CSI Ps SP q`).
    cursor_shape: Option<u16>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self::new()
    }
}

impl Ledger {
    pub fn new() -> Self {
        Self {
            parser: vte::Parser::new(),
            osc: OscCap::default(),
            told: Told::default(),
            stream: Boundary::new(),
            broken: false,
            relayed: None,
        }
    }

    /// Call when the client stops being relayed; later output reaches only
    /// the ledger until it is back in front.
    pub fn left_the_terminal(&mut self) {
        self.relayed = Some((self.told.modes, self.told.keypad));
    }

    /// Feed client output; OSC payload past [`OSC_SHOWN`] is hidden from the
    /// parser. A parser panic marks the ledger broken instead of taking down
    /// the relay.
    pub fn saw(&mut self, bytes: &[u8]) {
        if self.broken {
            return;
        }
        let (parser, told, stream) = (&mut self.parser, &mut self.told, &mut self.stream);
        let shown = self.osc.cap(bytes);
        let fed = crate::panics::caught(|| {
            parser.advance(told, &shown);
            stream.saw_session(bytes);
        });
        if fed.is_err() {
            tracing::warn!(
                "the ledger's parser panicked; the client's settings will not be put back"
            );
            self.broken = true;
        }
    }

    /// False once the parser has panicked.
    pub fn is_usable(&self) -> bool {
        !self.broken
    }

    /// Whether output is not inside an escape sequence, a UTF-8 character, a
    /// synchronized update or a cursor save. True once broken.
    pub fn between_frames(&self) -> bool {
        self.broken || self.stream.between_sequences()
    }

    pub fn mouse(&self) -> MouseReporting {
        let told = &self.told;
        if told.motion {
            MouseReporting::Motion
        } else if told.buttons || told.press {
            MouseReporting::Buttons
        } else {
            MouseReporting::Off
        }
    }

    /// Whether DEC mode 2048 is on. Such a client ignores `SIGWINCH`, so it
    /// must be sent a report instead (see `pty::Attachment::tell_its_size`).
    pub fn reads_size_in_band(&self) -> bool {
        self.mode(2048) == Some(true)
    }

    /// The last title sequence, verbatim.
    pub fn title(&self) -> Option<&[u8]> {
        self.told.title.as_deref()
    }

    fn mode(&self, mode: u16) -> Option<bool> {
        let at = MODES.iter().position(|&m| m == mode)?;
        self.told.modes[at]
    }

    /// Bytes restoring the client's terminal state, except the mouse (the
    /// caller uses [`crate::term::set_mouse_reporting`] with
    /// [`Ledger::mouse`]).
    ///
    /// Always restores per-session state: title, cursor shape, cursor colour
    /// (or the terminal default) and keyboard protocol. Kitty flags are set
    /// (`CSI = flags ; 1 u`), not pushed, so the stack never grows.
    ///
    /// `MODES` and the keypad are restored only if the terminal may differ:
    /// all of them `after_an_exit` (another client's exit sequence reached the
    /// terminal), otherwise only those changed while parked (Neovim enables
    /// focus reports ~100 ms after startup, which a switch can beat). Others
    /// are untouched and not resent: resending mode 2048 makes the terminal
    /// send a report.
    ///
    /// Resets come before sets, as xterm needs for its mouse modes.
    pub fn put_back(&self, after_an_exit: bool) -> Vec<u8> {
        self.putting_back(after_an_exit).0
    }

    /// Whether [`Ledger::put_back`] enables in-band size reports. The terminal
    /// then sends a report the relay must filter out (see
    /// `pty::Attachment::saw_input`).
    pub fn puts_back_in_band_reports(&self, after_an_exit: bool) -> bool {
        self.putting_back(after_an_exit).1
    }

    fn putting_back(&self, after_an_exit: bool) -> (Vec<u8>, bool) {
        let told = &self.told;
        let mut out = Vec::new();
        if self.broken {
            return (out, false);
        }
        let (heard_modes, heard_keypad) = self.relayed.unwrap_or(([None; MODES.len()], None));
        let news = |now: Option<bool>, heard: Option<bool>| {
            now.is_some() && (after_an_exit || now != heard)
        };
        let said = |word: bool| {
            MODES
                .iter()
                .zip(told.modes.iter().zip(heard_modes))
                .filter(move |(_, (now, heard))| **now == Some(word) && news(**now, *heard))
                .map(|(mode, _)| mode.to_string())
                .collect::<Vec<_>>()
        };
        let mut in_band = false;
        for (word, fin) in [(false, 'l'), (true, 'h')] {
            let modes = said(word);
            in_band |= word && modes.iter().any(|m| m == "2048");
            if !modes.is_empty() {
                let _ = write!(out, "\x1b[?{}{fin}", modes.join(";"));
            }
        }
        if news(told.keypad, heard_keypad) {
            match told.keypad {
                Some(true) => out.extend_from_slice(b"\x1b="),
                Some(false) => out.extend_from_slice(b"\x1b>"),
                None => {}
            }
        }
        if let Some(flags) = told.kitty {
            let _ = write!(out, "\x1b[={flags};1u");
        }
        if let Some(level) = told.other_keys {
            let _ = write!(out, "\x1b[>4;{level}m");
        }
        if let Some(title) = &told.title {
            out.extend_from_slice(title);
        }
        if let Some(shape) = told.cursor_shape {
            let _ = write!(out, "\x1b[{shape} q");
        }
        out.extend_from_slice(told.cursor_colour.as_deref().unwrap_or(CURSOR_COLOUR_RESET));
        (out, in_band)
    }
}

/// First parameter value, or `default` if absent or zero.
fn first(params: &vte::Params, default: u16) -> u16 {
    params
        .iter()
        .next()
        .and_then(|p| p.first().copied())
        .filter(|&n| n != 0 || default == 0)
        .unwrap_or(default)
}

impl vte::Perform for Told {
    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if ignore {
            return;
        }
        match (intermediates, action) {
            // DECSET/DECRST; vte passes the `?` as an intermediate.
            (b"?", 'h' | 'l') => {
                let on = action == 'h';
                for param in params.iter() {
                    match param {
                        [1000] => self.press = on,
                        [1002] => self.buttons = on,
                        [1003] => self.motion = on,
                        [mode] => {
                            if let Some(at) = MODES.iter().position(|m| m == mode) {
                                self.modes[at] = Some(on);
                            }
                        }
                        _ => {}
                    }
                }
            }
            // Kitty keyboard push, pop and set; set mode 1 replaces, 2 adds,
            // 3 removes.
            (b">", 'u') => self.kitty = Some(first(params, 0)),
            (b"<", 'u') => self.kitty = Some(0),
            (b"=", 'u') => {
                let mut values = params.iter().map(|p| p.first().copied().unwrap_or(0));
                let flags = values.next().unwrap_or(0);
                let now = self.kitty.unwrap_or(0);
                self.kitty = Some(match values.next().unwrap_or(1) {
                    2 => now | flags,
                    3 => now & !flags,
                    _ => flags,
                });
            }
            (b" ", 'q') => self.cursor_shape = Some(first(params, 0)),
            // xterm `modifyOtherKeys`: resource 4, then the level.
            (b">", 'm') => {
                let mut values = params.iter().map(|p| p.first().copied().unwrap_or(0));
                if values.next() == Some(4) {
                    self.other_keys = Some(values.next().unwrap_or(0));
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            b'=' => self.keypad = Some(true),
            b'>' => self.keypad = Some(false),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        // `OSC 0` (Neovim's) and `OSC 2` set the title; `OSC 1` (icon name
        // only) is ignored. `OSC 12` sets the cursor colour, `OSC 112` resets.
        let slot = match params.first() {
            Some(&b"0") | Some(&b"2") => &mut self.title,
            Some(&b"12") | Some(&b"112") => &mut self.cursor_colour,
            _ => return,
        };
        // The parser drops fields past 16; keep the previous title rather
        // than a truncated one.
        if params.len() >= MAX_OSC_FIELDS {
            return;
        }
        // Rejoin the fields the parser split at `;`.
        let mut seq = b"\x1b]".to_vec();
        for (i, param) in params.iter().enumerate() {
            if i > 0 {
                seq.push(b';');
            }
            seq.extend_from_slice(param);
        }
        seq.extend_from_slice(if bell_terminated { b"\x07" } else { b"\x1b\\" });
        if seq.len() <= MAX_OSC {
            *slot = Some(seq);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(bytes: &[u8]) -> Ledger {
        let mut l = Ledger::new();
        l.saw(bytes);
        l
    }

    #[test]
    fn a_client_that_said_nothing_has_the_mouse_off() {
        assert_eq!(Ledger::new().mouse(), MouseReporting::Off);
        assert!(!Ledger::new().reads_size_in_band());
        assert_eq!(Ledger::new().title(), None);
    }

    /// As a Neovim 0.12 client writes them.
    #[test]
    fn neovims_mouse_sequences_map_to_what_it_asked_for() {
        assert_eq!(
            fed(b"\x1b[?1002h\x1b[?1006h").mouse(),
            MouseReporting::Buttons
        );
        assert_eq!(
            fed(b"\x1b[?1002h\x1b[?1006h\x1b[?1003h").mouse(),
            MouseReporting::Motion
        );
        assert_eq!(
            fed(b"\x1b[?1002h\x1b[?1006h\x1b[?1002l\x1b[?1006l").mouse(),
            MouseReporting::Off
        );
        assert_eq!(fed(b"\x1b[?1002;1006h").mouse(), MouseReporting::Buttons);
    }

    #[test]
    fn the_last_word_on_the_mouse_is_the_one_kept() {
        let mut l = fed(b"\x1b[?1002h\x1b[?1003h");
        l.saw(b"\x1b[?1003l");
        assert_eq!(l.mouse(), MouseReporting::Buttons);
        l.saw(b"\x1b[?1002l");
        assert_eq!(l.mouse(), MouseReporting::Off);
    }

    #[test]
    fn a_sequence_cut_by_a_read_still_counts() {
        let mut l = Ledger::new();
        for byte in b"\x1b[?2048h\x1b]0;notes - NVIM\x07" {
            l.saw(std::slice::from_ref(byte));
        }
        assert!(l.reads_size_in_band());
        assert_eq!(l.title(), Some(&b"\x1b]0;notes - NVIM\x07"[..]));
    }

    /// `CSI 1002 h` without `?` is an ANSI mode.
    #[test]
    fn an_ansi_mode_is_not_mistaken_for_a_private_one() {
        assert_eq!(fed(b"\x1b[1002h").mouse(), MouseReporting::Off);
        assert!(!fed(b"\x1b[2048h").reads_size_in_band());
    }

    /// Terminator and any `;` included.
    #[test]
    fn a_title_is_kept_as_it_was_written() {
        assert_eq!(
            fed(b"\x1b]2;a;b;c\x1b\\").title(),
            Some(&b"\x1b]2;a;b;c\x1b\\"[..])
        );
        let mut l = fed(b"\x1b]0;first\x07");
        l.saw(b"\x1b]0;second\x07");
        assert_eq!(l.title(), Some(&b"\x1b]0;second\x07"[..]));
    }

    /// Neovim wraps each frame in a synchronized update, so the frame is whole
    /// only when the update closes.
    #[test]
    fn a_frame_is_whole_only_once_its_update_has_closed() {
        let mut l = Ledger::new();
        assert!(l.between_frames(), "a client that has said nothing");
        l.saw(b"\x1b[?2026h\x1b[1;1Hhello");
        assert!(!l.between_frames(), "inside the client's own update");
        l.saw(b"\x1b[38;2;1");
        assert!(!l.between_frames(), "inside a sequence");
        l.saw(b";2;3mworld\x1b[?2026l");
        assert!(l.between_frames(), "the update has closed");
        l.saw(b"\xe2\x96");
        assert!(!l.between_frames(), "inside a character");
    }

    #[test]
    fn other_operating_system_commands_are_not_titles() {
        for seq in [
            &b"\x1b]1;icon\x07"[..],
            b"\x1b]11;?\x07",
            b"\x1b]52;c;aGVsbG8=\x07",
            b"\x1b]112\x07",
        ] {
            assert_eq!(fed(seq).title(), None, "{seq:?}");
        }
    }

    #[test]
    fn an_overlong_title_is_not_kept() {
        let mut l = fed(b"\x1b]0;short\x07");
        let mut long = b"\x1b]0;".to_vec();
        long.extend(std::iter::repeat_n(b'x', MAX_OSC));
        long.push(0x07);
        l.saw(&long);
        assert_eq!(l.title(), Some(&b"\x1b]0;short\x07"[..]));
    }

    /// Resending mode 2048 makes the terminal send a report, so modes are
    /// restored only after another client's exit.
    #[test]
    fn modes_every_client_sets_come_back_only_after_an_exit() {
        let mut l = fed(b"\x1b[?1h\x1b=\x1b[?2004h\x1b[?1004h\x1b[?2048h\x1b[?2027l");
        l.left_the_terminal();
        let quiet = l.put_back(false);
        assert!(!quiet.windows(3).any(|w| w == b"\x1b[?"), "{quiet:?}");
        let after = String::from_utf8(l.put_back(true)).expect("text");
        assert!(after.contains("\x1b[?2027l"), "{after:?}");
        assert!(after.contains("\x1b[?1;1004;2004;2048h"), "{after:?}");
        assert!(after.contains("\x1b="), "{after:?}");
        assert!(
            after.find("\x1b[?2027l") < after.find("\x1b[?1;"),
            "resets before sets: {after:?}"
        );
    }

    #[test]
    fn putting_in_band_reports_back_is_said_to_be_so() {
        let mut l = fed(b"\x1b[?2004h\x1b[?2048h");
        l.left_the_terminal();
        assert!(!l.puts_back_in_band_reports(false));
        assert!(l.puts_back_in_band_reports(true));
        let mut other = fed(b"\x1b[?2004h");
        other.left_the_terminal();
        assert!(!other.puts_back_in_band_reports(true));
    }

    /// Modes set while parked were never heard by the terminal.
    #[test]
    fn modes_set_while_away_come_back() {
        let mut l = fed(b"\x1b[?2004h\x1b[?2048h");
        l.left_the_terminal();
        l.saw(b"\x1b[?1004h");
        let back = String::from_utf8(l.put_back(false)).expect("text");
        assert!(back.contains("\x1b[?1004h"), "{back:?}");
        assert!(!back.contains("2048"), "{back:?}");
        assert!(!back.contains("2004"), "{back:?}");
    }

    #[test]
    fn a_title_the_parser_cut_short_is_not_kept() {
        let mut l = fed(b"\x1b]0;whole\x07");
        l.saw(b"\x1b]0;a;b;c;d;e;f;g;h;i;j;k;l;m;n;o;p;q;r\x07");
        assert_eq!(l.title(), Some(&b"\x1b]0;whole\x07"[..]));
    }

    /// A push would grow the stack on every return.
    #[test]
    fn the_keyboard_protocol_is_set_back_not_pushed() {
        let pushed = String::from_utf8(fed(b"\x1b[>3u").put_back(false)).expect("text");
        assert!(pushed.contains("\x1b[=3;1u"), "{pushed:?}");
        assert!(!pushed.contains("\x1b[>"), "{pushed:?}");
        let adjusted = String::from_utf8(fed(b"\x1b[>1u\x1b[=2;2u").put_back(false)).expect("text");
        assert!(adjusted.contains("\x1b[=3;1u"), "{adjusted:?}");
        let other = String::from_utf8(fed(b"\x1b[>4;2m").put_back(false)).expect("text");
        assert!(other.contains("\x1b[>4;2m"), "{other:?}");
    }

    /// A client that never set one gets the terminal default, not the last
    /// session's colour.
    #[test]
    fn the_cursor_colour_is_the_clients_or_the_terminals() {
        let set = fed(b"\x1b]12;#ff0000\x07").put_back(false);
        assert!(set.ends_with(b"\x1b]12;#ff0000\x07"), "{set:?}");
        let never = Ledger::new().put_back(false);
        assert!(never.ends_with(CURSOR_COLOUR_RESET), "{never:?}");
    }

    /// Otherwise the local paint would show the last session's shape until
    /// the server repaints.
    #[test]
    fn the_cursor_shape_comes_back() {
        let shaped = String::from_utf8(fed(b"\x1b[2 q\x1b[6 q").put_back(false)).expect("text");
        assert!(shaped.contains("\x1b[6 q"), "{shaped:?}");
        assert!(!shaped.contains("\x1b[2 q"), "{shaped:?}");
    }

    #[test]
    fn a_broken_ledger_puts_nothing_back() {
        let mut l = fed(b"\x1b]0;title\x07\x1b[>3u");
        l.broken = true;
        assert!(l.put_back(true).is_empty());
    }

    /// `stream` through one cap in reads of `piece` bytes.
    fn capped(stream: &[u8], piece: usize) -> Vec<u8> {
        let mut cap = OscCap::default();
        let mut out = Vec::new();
        for chunk in stream.chunks(piece) {
            out.extend_from_slice(&cap.cap(chunk));
        }
        out
    }

    /// Output with no over-long OSC passes unchanged and uncopied.
    #[test]
    fn the_osc_cap_leaves_ordinary_output_alone() {
        let stream: &[u8] = b"text\x1b[1;2H\x1b]0;title\x07\x1b]12;#ff0000\x1b\\\x1b\x1b]2;t\x07\xe2\x94\x80\x1b[?2026l";
        for piece in [1, 2, 3, 7, stream.len()] {
            assert_eq!(capped(stream, piece), stream, "in pieces of {piece}");
        }
        assert!(matches!(
            OscCap::default().cap(stream),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    /// Every ender and read size; what follows the OSC is untouched.
    #[test]
    fn the_osc_cap_cuts_a_long_osc_down_to_the_cap() {
        let payload: Vec<u8> = (0..OSC_SHOWN * 3).map(|i| b'A' + (i % 26) as u8).collect();
        for end in [&b"\x07"[..], b"\x1b\\", b"\x18", b"\x1a"] {
            let stream = [b"a\x1b]52;c;", &payload[..], end, b"\x1b[1mb"].concat();
            let want = [b"a\x1b]52;c;", &payload[..OSC_SHOWN - 5], end, b"\x1b[1mb"].concat();
            for piece in [1, 5, 4095, stream.len()] {
                assert_eq!(capped(&stream, piece), want, "in pieces of {piece}");
            }
        }
    }

    /// Over random streams and read splits, the cap matches a byte-at-a-time
    /// model, and a capped ledger matches an uncapped parser.
    #[test]
    fn the_osc_cap_drops_only_what_nothing_keeps() {
        for seed in 0..200 {
            let (stream, reads) = crate::test_support::osc_noise(seed);
            // Byte-at-a-time model of the cap.
            let mut want = Vec::new();
            let (mut esc, mut osc) = (false, None);
            for &b in &stream {
                match osc {
                    Some(_) if matches!(b, 0x07 | 0x18 | 0x1a | 0x1b) => {
                        (esc, osc) = (b == 0x1b, None);
                        want.push(b);
                    }
                    Some(shown) => {
                        if shown < OSC_SHOWN {
                            want.push(b);
                        }
                        osc = Some(shown + 1);
                    }
                    None => {
                        if esc && b == b']' {
                            osc = Some(0);
                        }
                        esc = b == 0x1b;
                        want.push(b);
                    }
                }
            }
            let mut cap = OscCap::default();
            let mut got = Vec::new();
            let mut ledger = Ledger::new();
            let (mut parser, mut told) = (vte::Parser::new(), Told::default());
            let mut at = 0;
            for read in reads {
                let chunk = &stream[at..at + read];
                at += read;
                got.extend_from_slice(&cap.cap(chunk));
                ledger.saw(chunk);
                parser.advance(&mut told, chunk);
            }
            assert!(
                got == want,
                "seed {seed}: the cap is not the byte-at-a-time one"
            );
            assert_eq!(ledger.told, told, "seed {seed}");
            assert!(ledger.is_usable(), "seed {seed}");
        }
    }

    #[test]
    fn a_long_clipboard_write_changes_nothing_the_ledger_keeps() {
        let mut l = Ledger::new();
        l.saw(b"\x1b]0;before\x07");
        let mut osc = b"\x1b]52;c;".to_vec();
        osc.extend(std::iter::repeat_n(b'Q', 100_000));
        osc.push(0x07);
        for chunk in osc.chunks(4095) {
            l.saw(chunk);
        }
        assert_eq!(l.title(), Some(&b"\x1b]0;before\x07"[..]));
        assert!(l.between_frames());
        l.saw(b"\x1b]2;after\x1b\\\x1b[?1000h");
        assert_eq!(l.title(), Some(&b"\x1b]2;after\x1b\\"[..]));
        assert_eq!(l.mouse(), MouseReporting::Buttons);
    }
}
