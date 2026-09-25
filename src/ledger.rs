//! What a client has told its terminal and will not tell it again.
//!
//! A Neovim client sets some of the terminal's state once and then trusts it
//! to stay set: mouse reporting when `'mouse'` changes, the window title when
//! `'titlestring'` does, its keyboard protocol and a handful of modes once at
//! startup. That trust is sound while the client has the terminal to itself.
//! It stops being sound the moment something else writes the same state —
//! nvmux's own screens turn the mouse on and off, and with
//! [`crate::config::ClientSettings::per_session`] so does every *other*
//! session's client, each with its own `'mouse'`, its own title and its own
//! cursor colour, and a client whose session ends has its exit sequence
//! relayed, which turns off the modes every other client set once.
//!
//! So a client that is kept (see [`crate::pty::Parked`]) keeps a [`Ledger`] as
//! well: a decoder fed every byte the client writes, recording the last word
//! it said on each of those things. When the client comes back to the front
//! the ledger is what puts them back (see [`Ledger::put_back`]) — without
//! asking its server, and without the guesswork of asking at all: what the
//! client wrote is exactly what it believes the terminal holds.
//!
//! It also keeps one thing that is not state the client set but a place in
//! what it is saying: whether its output stands between two of its frames.
//! A kept client handed from its parking thread back to the relay may be half
//! way through writing one, and the half the thread read went to the shadow
//! rather than the terminal — so the relay must not start on the other half
//! (see `pty::Attachment::drain_unrelayed`).
//!
//! **It only watches.** Like the shadow, it is shown a copy of the bytes and
//! can neither change nor delay them.

use std::io::Write;

use crate::boundary::Boundary;
use crate::term::MouseReporting;

/// The longest title or colour the ledger will keep. A title is one line of a
/// tab bar; past this it is not a title but somebody's clipboard in the wrong
/// OSC, and not worth holding on to for every parked session.
const MAX_OSC: usize = 1024;

/// The DEC private modes a Neovim client sets once, for the whole terminal,
/// and that another client's exit turns off: application cursor keys,
/// focus reports, bracketed paste, grapheme clusters, colour-scheme reports
/// and in-band size reports. The mouse's own modes are kept apart (see
/// [`Ledger::mouse`]).
const MODES: [u16; 6] = [1, 1004, 2004, 2027, 2031, 2048];

/// How many `;`-separated fields of an OSC the parser keeps (vte's
/// `MAX_OSC_PARAMS`); anything past them it drops.
const MAX_OSC_FIELDS: usize = 16;

/// Resetting a cursor colour the client never set: `OSC 112`.
const CURSOR_COLOUR_RESET: &[u8] = b"\x1b]112\x07";

/// Every byte a client wrote, reduced to the state it set and would not set
/// again. See the module docs.
pub struct Ledger {
    parser: vte::Parser,
    told: Told,
    /// Where the client's output stands, as the terminal's parser would see
    /// it had it been shown every byte — which this has.
    stream: Boundary,
    /// Something in here panicked on what the client wrote, so nothing it
    /// says is to be trusted: it is fed no more, and puts nothing back (see
    /// [`Ledger::is_usable`]).
    broken: bool,
    /// The modes and the keypad as they stood when the client last stopped
    /// being relayed: what the terminal had heard from it. Anything said on
    /// them since — while parked, where nothing reaches the terminal — is
    /// news to the terminal (see [`Ledger::put_back`]).
    relayed: Option<Heard>,
}

/// What the terminal had heard of the modes and the keypad.
type Heard = ([Option<bool>; MODES.len()], Option<bool>);

impl std::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ledger")
            .field("told", &self.told)
            .finish_non_exhaustive()
    }
}

/// The state itself, apart from the parser that reads it out of the bytes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Told {
    /// DEC private mode 1000: button presses.
    press: bool,
    /// 1002: presses, releases and drags — what Neovim turns on for `'mouse'`.
    buttons: bool,
    /// 1003: every motion — what `'mousemoveevent'` adds.
    motion: bool,
    /// The last word on each of [`MODES`], in that order, or `None` for one
    /// never mentioned.
    modes: [Option<bool>; MODES.len()],
    /// The keypad: application (`ESC =`) or numeric (`ESC >`).
    keypad: Option<bool>,
    /// The kitty keyboard flags the client believes are in force, from its
    /// pushes, pops and sets.
    kitty: Option<u16>,
    /// xterm's `modifyOtherKeys` level, for a client that fell back to it.
    other_keys: Option<u16>,
    /// The last `OSC 0` or `OSC 2` the client wrote, as it wrote it.
    title: Option<Vec<u8>>,
    /// The last `OSC 12` or `OSC 112` the client wrote, as it wrote it.
    cursor_colour: Option<Vec<u8>>,
    /// The cursor's shape, as the client last set it with `DECSCUSR`
    /// (`CSI Ps SP q`): per mode, so per session.
    cursor_shape: Option<u16>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self::new()
    }
}

impl Ledger {
    /// A client that has said nothing yet — and so believes the terminal is
    /// as a terminal starts: no mouse reporting, no modes, no title of its
    /// own.
    pub fn new() -> Self {
        Self {
            parser: vte::Parser::new(),
            told: Told::default(),
            stream: Boundary::new(),
            broken: false,
            relayed: None,
        }
    }

    /// The client has stopped being relayed: whatever it says from here on
    /// reaches the ledger and not the terminal, until it is back in front.
    pub fn left_the_terminal(&mut self) {
        self.relayed = Some((self.told.modes, self.told.keypad));
    }

    /// Watch these bytes go by. A sequence cut in half by a read is carried
    /// over to the next call by the parser itself.
    ///
    /// Like the shadow's, never allowed to take the relay down: a panic in
    /// here — somebody else's parser, fed whatever an editor writes — is
    /// caught, and retires the ledger instead.
    pub fn saw(&mut self, bytes: &[u8]) {
        if self.broken {
            return;
        }
        let (parser, told, stream) = (&mut self.parser, &mut self.told, &mut self.stream);
        let fed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            parser.advance(told, bytes);
            stream.saw_session(bytes);
        }));
        if fed.is_err() {
            tracing::warn!(
                "the ledger's parser panicked; the client's settings will not be put back"
            );
            self.broken = true;
        }
    }

    /// Whether what the ledger holds can be trusted: false once its parser
    /// has panicked, after which nothing is put back from it.
    pub fn is_usable(&self) -> bool {
        !self.broken
    }

    /// Whether the client's output stands between its frames: not inside an
    /// escape sequence or a character, and not inside a synchronized update
    /// or a cursor save of its own. Where whatever it writes next begins
    /// something whole.
    pub fn between_frames(&self) -> bool {
        self.broken || self.stream.between_sequences()
    }

    /// The mouse reporting the client last asked for, in the three shapes
    /// Neovim uses: motion on top of buttons for `'mousemoveevent'`, buttons
    /// for any other non-empty `'mouse'`, nothing otherwise.
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

    /// Whether the client has turned on in-band resize reports (DEC mode
    /// 2048). A client that has ignores `SIGWINCH` altogether — it takes its
    /// size from the reports instead — so the way to tell it a new size is
    /// the report, not the signal (see `pty::Attachment::tell_its_size`).
    pub fn reads_size_in_band(&self) -> bool {
        self.mode(2048) == Some(true)
    }

    /// The window title the client last set, as the bytes that set it, if it
    /// has set one.
    pub fn title(&self) -> Option<&[u8]> {
        self.told.title.as_deref()
    }

    /// The last word the client said on one of [`MODES`].
    fn mode(&self, mode: u16) -> Option<bool> {
        let at = MODES.iter().position(|&m| m == mode)?;
        self.told.modes[at]
    }

    /// The bytes that put back, on a terminal something else has had, what
    /// the client told it — all but the mouse, which the caller puts back
    /// through [`crate::term::set_mouse_reporting`] from [`Ledger::mouse`].
    ///
    /// Always what is the client's own and differs from session to session —
    /// its title, its cursor's shape, and its cursor colour, or the terminal's
    /// own if it never set one — and its keyboard protocol, which terminals
    /// keep one of per
    /// screen: set rather than pushed (`CSI = flags ; 1 u`), so however many
    /// times this runs the stack does not grow.
    ///
    /// The modes every client sets alike, and the keypad, only where the
    /// terminal may not have them as the client does: all of them
    /// `after_an_exit` — when some client's exit sequence has reached the
    /// terminal since this one last had it, and may have turned them off —
    /// and otherwise only those the client set while it was not being
    /// relayed: Neovim turns focus reports on a tenth of a second after it
    /// starts, which a switch can beat. The rest nothing has touched, and one
    /// of them, in-band size reports, is not idle to set again: a terminal
    /// answers it with a report of its own.
    ///
    /// Resets before sets, as xterm needs of its mouse modes and nothing here
    /// minds.
    pub fn put_back(&self, after_an_exit: bool) -> Vec<u8> {
        self.putting_back(after_an_exit).0
    }

    /// Whether [`Ledger::put_back`] turns in-band size reports on — which a
    /// terminal that has them answers with a report of its own, and so with a
    /// resize the client asks its server for (see `pty::relay`).
    pub fn puts_back_in_band_reports(&self, after_an_exit: bool) -> bool {
        self.putting_back(after_an_exit).1
    }

    /// The bytes [`Ledger::put_back`] writes, and whether they turn in-band
    /// size reports on.
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

/// A parameter's first value, or `default` for one left out.
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
            // `CSI ? Pm h` and `CSI ? Pm l`: DECSET and DECRST. The `?`
            // reaches here as an intermediate.
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
            // The kitty keyboard protocol: push, pop, and set with a mode —
            // 1 replaces the flags, 2 adds to them, 3 takes them away.
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
            // xterm's `modifyOtherKeys`: resource 4, and its level.
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
        // `OSC 0` sets the title and the icon name, `OSC 2` the title alone;
        // Neovim's own is `OSC 0`. `OSC 1`, the icon name alone, is left out:
        // it is not the title, and nothing shows it. `OSC 12` sets the
        // cursor's colour and `OSC 112` puts the terminal's own back.
        let slot = match params.first() {
            Some(&b"0") | Some(&b"2") => &mut self.title,
            Some(&b"12") | Some(&b"112") => &mut self.cursor_colour,
            _ => return,
        };
        // The parser keeps sixteen fields and drops the rest, so a title with
        // more `;` in it than that has not come through whole; better the
        // last one than a truncated one the client never wrote.
        if params.len() >= MAX_OSC_FIELDS {
            return;
        }
        // Put back together as it arrived: the parser split it at every `;`,
        // and a title may have had any number of them.
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

    /// A client that has never mentioned the mouse believes it is off, which
    /// is what the ledger then puts back — rather than leaving it however the
    /// last screen or the last session's client left it.
    #[test]
    fn a_client_that_said_nothing_has_the_mouse_off() {
        assert_eq!(Ledger::new().mouse(), MouseReporting::Off);
        assert!(!Ledger::new().reads_size_in_band());
        assert_eq!(Ledger::new().title(), None);
    }

    /// Neovim's own sequences, as a 0.12 client writes them.
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
        // Several modes in one sequence count one by one.
        assert_eq!(fed(b"\x1b[?1002;1006h").mouse(), MouseReporting::Buttons);
    }

    /// The last word wins, however many were said.
    #[test]
    fn the_last_word_on_the_mouse_is_the_one_kept() {
        let mut l = fed(b"\x1b[?1002h\x1b[?1003h");
        l.saw(b"\x1b[?1003l");
        assert_eq!(l.mouse(), MouseReporting::Buttons);
        l.saw(b"\x1b[?1002l");
        assert_eq!(l.mouse(), MouseReporting::Off);
    }

    /// A read can cut a sequence anywhere; the parser carries the rest over.
    #[test]
    fn a_sequence_cut_by_a_read_still_counts() {
        let mut l = Ledger::new();
        for byte in b"\x1b[?2048h\x1b]0;notes - NVIM\x07" {
            l.saw(std::slice::from_ref(byte));
        }
        assert!(l.reads_size_in_band());
        assert_eq!(l.title(), Some(&b"\x1b]0;notes - NVIM\x07"[..]));
    }

    /// Only private modes: `CSI 1002 h` without the `?` is an ANSI mode, and
    /// not the mouse.
    #[test]
    fn an_ansi_mode_is_not_mistaken_for_a_private_one() {
        assert_eq!(fed(b"\x1b[1002h").mouse(), MouseReporting::Off);
        assert!(!fed(b"\x1b[2048h").reads_size_in_band());
    }

    /// A title is kept as it was written — its terminator, and any `;` in
    /// it, included — so putting it back is writing it again.
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

    /// Half a frame is not a place to hand a client over, and Neovim brackets
    /// each of its frames in a synchronized update: the end of the update, not
    /// the end of any one sequence inside it, is where the frame is whole.
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

    /// The icon name, a colour, a clipboard: none of them is the title.
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

    /// A title too long to be one is not kept, and does not displace the
    /// last one that was.
    #[test]
    fn an_overlong_title_is_not_kept() {
        let mut l = fed(b"\x1b]0;short\x07");
        let mut long = b"\x1b]0;".to_vec();
        long.extend(std::iter::repeat_n(b'x', MAX_OSC));
        long.push(0x07);
        l.saw(&long);
        assert_eq!(l.title(), Some(&b"\x1b]0;short\x07"[..]));
    }

    /// What every client sets for the whole terminal, and another's exit
    /// turns off, comes back — but only after such an exit: in-band size
    /// reports are not idle to set again.
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

    /// Turning in-band size reports back on is said to be so, for the relay
    /// that has to expect the report a terminal answers it with.
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

    /// And what the client set while it was not being relayed — which the
    /// terminal never heard — comes back without any exit, while what the
    /// terminal did hear is left alone.
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

    /// A title of more fields than the parser keeps is not kept truncated.
    #[test]
    fn a_title_the_parser_cut_short_is_not_kept() {
        let mut l = fed(b"\x1b]0;whole\x07");
        l.saw(b"\x1b]0;a;b;c;d;e;f;g;h;i;j;k;l;m;n;o;p;q;r\x07");
        assert_eq!(l.title(), Some(&b"\x1b]0;whole\x07"[..]));
    }

    /// The keyboard protocol comes back every time, and as a set, never a
    /// push: the stack must not grow however often a client returns.
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

    /// A cursor colour comes back as it was set — and a client that never set
    /// one gets the terminal's own back, not the last session's.
    #[test]
    fn the_cursor_colour_is_the_clients_or_the_terminals() {
        let set = fed(b"\x1b]12;#ff0000\x07").put_back(false);
        assert!(set.ends_with(b"\x1b]12;#ff0000\x07"), "{set:?}");
        let never = Ledger::new().put_back(false);
        assert!(never.ends_with(CURSOR_COLOUR_RESET), "{never:?}");
    }

    /// The cursor's shape comes back as the client last set it: the local
    /// paint would otherwise show the last session's until the server's
    /// repaint arrived.
    #[test]
    fn the_cursor_shape_comes_back() {
        let shaped = String::from_utf8(fed(b"\x1b[2 q\x1b[6 q").put_back(false)).expect("text");
        assert!(shaped.contains("\x1b[6 q"), "{shaped:?}");
        assert!(!shaped.contains("\x1b[2 q"), "{shaped:?}");
    }

    /// A ledger that has panicked puts nothing back.
    #[test]
    fn a_broken_ledger_puts_nothing_back() {
        let mut l = fed(b"\x1b]0;title\x07\x1b[>3u");
        l.broken = true;
        assert!(l.put_back(true).is_empty());
    }
}
