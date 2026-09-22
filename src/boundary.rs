//! Where nvmux may write over a session.
//!
//! [`crate::pty`] copies the client's bytes to the terminal without looking at
//! them, and [`crate::announce`] has to put a box on top of that stream.
//! Between two of the client's escape sequences that is safe; inside one it is
//! corruption — a `CUP` cut in half by a colour of nvmux's own paints neither
//! thing. The old answer was a clock: 25 ms with the child quiet, on the
//! reasoning that nothing can be half-written when nothing has been written at
//! all. It is a true test and the wrong one. A session with a window
//! repainting at sixty frames a second never goes quiet for 25 ms, so the box
//! either blinked — up at each lull, wiped by the next frame — or, at full
//! rate, never appeared at all.
//!
//! So this module answers the question directly. A [`Boundary`] is the
//! terminal's own parser as far as the bytes nvmux has written to it: enough
//! state to say whether the next byte starts something new. It is a decoder
//! rather than an emulator — it has no grid, no cursor and no idea what any
//! sequence *means* — and the one thing it reports is
//! [`Boundary::between_sequences`].
//!
//! # And the tail
//!
//! Knowing is not quite enough. The relay reads the master in 8 KiB bites, and
//! a bite of a screenful of output almost never ends where a sequence does —
//! measured on a real flight, about one in six. A notice repainted only on
//! those would still blink.
//!
//! So a [`Boundary`] also holds the last few bytes back. [`Boundary::relay`]
//! hands out everything up to the last point that leaves the parser between
//! sequences and keeps the rest until the bytes that finish it arrive — which
//! for a client mid-frame is microseconds later, on the next pass of the same
//! loop. The terminal is then *always* left between sequences, so the notice
//! can go back up straight after every write rather than once in six.
//!
//! A kept byte is never dropped, reordered or changed: it goes out at the
//! front of the next call, ahead of everything that came after it. And it is
//! never kept long — [`KEPT_FOR`], or [`KEPT_MAX`] bytes, whichever comes
//! first — because a client that has stopped in the middle of a sequence may
//! be one waiting for the terminal's answer to it, and no picture is worth a
//! session that cannot ask its terminal a question.
//!
//! None of this is on while there is nothing to draw: `pty` keeps a
//! `Boundary` only for the life of a notice, which is a second and a bit at
//! the start of a relay.
//!
//! # Two things that are not sequences and still say "not here"
//!
//! A **synchronized update** (`CSI ? 2026 h` … `CSI ? 2026 l`) is the client
//! asking the terminal to present nothing until the frame is whole. nvmux's
//! own box opens a span of its own, and terminals do not nest them: dropped in
//! the middle of the client's, it would end the span early and present a frame
//! drawn half way. So a `Boundary` counts the client's spans and reports no
//! boundary inside one.
//!
//! A **`DECSC`** (`ESC 7`) is the client putting the cursor and its attributes
//! in the terminal's one save slot, to take them back later with `DECRC`. The
//! notice uses that slot too. Between the two the slot is the client's, and
//! writing there would hand it back somebody else's cursor.
//!
//! Both refuse *nvmux* and neither holds the session up: its own bytes pass
//! through either, cut where its sequences end, because it is only a write of
//! nvmux's that must not land there.
//!
//! Neither is hypothetical, and which of them shows up is a fact about
//! somebody else's terminal rather than about nvmux. `nvim --remote-ui`
//! brackets every frame in a synchronized update when its `TERM` has a `Sync`
//! capability to do it with; on the `xterm-256color` this was measured
//! against it has none and sent not one, nor a single `ESC 7`. A guard is
//! cheap and the alternative is a bug nobody here would ever see.

use std::time::{Duration, Instant};

/// How long a tail is kept back before it goes to the terminal anyway.
///
/// One frame of a sixty-a-second terminal. A client that has stopped in the
/// middle of a sequence is either about to write the rest of it — in which
/// case this is never reached, the rest having turned up microseconds later —
/// or it is waiting for the terminal to answer the sequence it has not
/// finished, in which case this is how long that costs it.
pub const KEPT_FOR: Duration = Duration::from_millis(16);

/// The most that is ever kept back, after which it goes out unfinished.
///
/// A ceiling rather than a budget: a sequence is a few dozen bytes and a
/// screenful of them is a few tens of KiB, so nothing ordinary comes near it.
/// What it bounds is the one sequence that has no length limit — an OSC 52
/// clipboard write can be a megabyte — which must not be accumulated here
/// while the session waits for it.
pub const KEPT_MAX: usize = 64 * 1024;

/// The terminal's own parser as far as nvmux has written to it, and the tail
/// of the session's output kept back so that it is never left mid-sequence.
///
/// Every byte written to the terminal while one of these exists must be shown
/// to it exactly once, and said whose it is: [`Boundary::relay`] hands back
/// the session's after taking account of them, [`Boundary::saw_session`]
/// takes the session's that went out by another route, and
/// [`Boundary::saw_own`] takes nvmux's. A byte shown twice moves the parser
/// twice, one not shown at all leaves it describing a terminal that no longer
/// exists, and one shown under the wrong name teaches it something about the
/// session that is really about nvmux.
///
/// The one exception is the synchronized update nvmux opens around a relayed
/// write (see `Attachment::write_session_frame` in [`crate::pty`]). Its two
/// sequences are balanced across a pass of the relay and are nvmux's own, and
/// a `sync` depth raised by them would read as *the session* holding the
/// terminal — shutting the notice out of the very frame the span was opened
/// to put it in.
#[derive(Debug, Default)]
pub struct Boundary {
    parser: Parser,
    /// The tail, waiting for the bytes that finish it.
    kept: Vec<u8>,
    /// When the oldest kept byte was kept, or `None` when nothing is.
    since: Option<Instant>,
}

impl Boundary {
    /// A terminal between sequences, which is where every relay starts: what
    /// came before it — the hand-off, the clear, a dissolve — is nvmux's own
    /// and complete.
    pub fn new() -> Self {
        Self::default()
    }

    /// What may go to the terminal now, given that `chunk` has just been read
    /// from the client.
    ///
    /// Everything up to the last point that leaves the parser between
    /// sequences; the rest is kept for the next call, which puts it back at
    /// the front. Nothing is dropped, reordered or changed.
    ///
    /// Once the tail is older than [`KEPT_FOR`] or longer than [`KEPT_MAX`] it
    /// all goes out regardless, unfinished — the session's output matters more
    /// than the notice does, and this is the only way that trade is ever made.
    ///
    /// **The tail is dated from the first time anything was kept, not from
    /// the last, and that is a known cost rather than a considered choice.**
    /// On a busy session something is kept back after nearly every read, so
    /// the clock rarely restarts and a tail goes out unfinished every
    /// [`KEPT_FOR`]; each one leaves the parser mid-sequence for a pass, and
    /// the notice cannot be put back into the frame that just wiped it. It
    /// is what is left of the flicker — about one presented frame in thirty.
    ///
    /// Dating it from the last hand-out instead is the obvious repair, and it
    /// was tried and backed out. Instrumented, it is plainly better — every
    /// one of 188 relayed chunks left the terminal where nvmux could write,
    /// against about half of them here, and the tail went out unfinished not
    /// once. But on two runs of the same flight the notice then did not
    /// appear *at all*, with the parser ending its life inside a CSI, and the
    /// two observations have not been reconciled. A mechanism that measures
    /// better and behaves worse is not understood, and shipping it would have
    /// traded a flicker for a silence. Whatever replaces this has to explain
    /// that first.
    pub fn relay(&mut self, chunk: &[u8], now: Instant) -> Vec<u8> {
        self.kept.extend_from_slice(chunk);
        let cut = if self.out_of_patience(now) {
            self.parser.feed(&self.kept);
            self.kept.len()
        } else {
            self.parser.advance_to_last_rest(&self.kept)
        };
        let ready = self.kept.drain(..cut).collect();
        self.since = if self.kept.is_empty() {
            None
        } else {
            self.since.or(Some(now))
        };
        ready
    }

    /// Bytes of the *session's* that reached the terminal by some route other
    /// than [`Boundary::relay`] — the first paint, let out of its hold.
    ///
    /// Shown for the same reason the relayed ones are: the parser is the
    /// terminal's, and these moved it. And read as the session's, which is
    /// the difference from [`Boundary::saw_own`]: a client that brackets its
    /// first paint is a client that brackets, and it says so here.
    pub fn saw_session(&mut self, bytes: &[u8]) {
        self.parser.feed(bytes);
    }

    /// Bytes nvmux wrote over the session itself: the notice's own frames.
    ///
    /// Shown for the same reason again — the parser is the terminal's, not
    /// the client's, and what nvmux writes moves it too. Everything nvmux
    /// writes over a session is balanced and ends between sequences, so in
    /// practice this leaves the parser where it found it, which is exactly
    /// why it must be said rather than assumed.
    ///
    /// What it must *not* do is answer
    /// [`Boundary::session_brackets_its_own_frames`]. Every frame of the
    /// notice is bracketed in a synchronized update of its own, so a parser
    /// that learned from these would conclude after the first box that the
    /// session brackets its frames — and stand the whole mechanism down on
    /// the strength of nvmux's own handwriting. That is not a hypothetical:
    /// it is what this did for its first measurement, which came back at 13%
    /// instead of 100% and was the only reason anybody looked.
    pub fn saw_own(&mut self, bytes: &[u8]) {
        let session_syncs = self.parser.seen_sync;
        self.parser.feed(bytes);
        self.parser.seen_sync = session_syncs;
    }

    /// Whether the terminal is between sequences, so a write of nvmux's own
    /// would land beside one of the session's rather than inside it.
    pub fn between_sequences(&self) -> bool {
        self.parser.at_rest()
    }

    /// Whether the session has ever bracketed anything in a synchronized
    /// update of its own.
    ///
    /// What it decides is whether nvmux may open one *around* a relayed
    /// write, which is how the notice is kept on the screen rather than
    /// merely put back on it (see `Attachment::write_session_frame` in
    /// [`crate::pty`]). Terminals hold `?2026` as a mode rather than as a
    /// count, so a span of nvmux's around a span of the session's would end
    /// at the session's reset and present the frame early — exactly the tear
    /// the span was opened to prevent. One sighting is enough to stand down
    /// for the rest of the relay: a client that brackets one frame brackets
    /// them all, and its frames are already whole without help.
    pub fn session_brackets_its_own_frames(&self) -> bool {
        self.parser.seen_sync
    }

    /// When the kept tail goes out whether or not the rest of it has arrived,
    /// or `None` while nothing is kept.
    pub fn due(&self) -> Option<Instant> {
        self.since.map(|since| since + KEPT_FOR)
    }

    /// Everything kept back, complete or not: what the relay writes when the
    /// wait is over, and what it writes on the way out so that no byte the
    /// client produced is lost with the notice that caused it to be held.
    pub fn give_up(&mut self) -> Vec<u8> {
        self.parser.feed(&self.kept);
        self.since = None;
        std::mem::take(&mut self.kept)
    }

    /// Whether the tail has waited long enough, or grown large enough, to go
    /// out unfinished.
    fn out_of_patience(&self, now: Instant) -> bool {
        self.kept.len() >= KEPT_MAX
            || self
                .since
                .is_some_and(|since| now.saturating_duration_since(since) >= KEPT_FOR)
    }
}

/// How many of a CSI's parameter and intermediate bytes are remembered.
///
/// Only one sequence is ever recognised by its parameters, and it is
/// `? 2026`. Generous for that: a terminfo entry that sets the synchronized
/// update alongside every other mode it wants — `? 1049 ; 1002 ; 1006 ; 2026
/// h` — still fits. A CSI longer than this is read as an ordinary sequence
/// and the rest of its parameters are thrown away rather than grown a buffer
/// for, which is the safe direction for everything but that one mode.
const PARAMS: usize = 32;

/// Where the terminal's parser stands.
///
/// Plain data and `Copy`, which is what lets [`Parser::advance_to_last_rest`]
/// run a scan ahead of itself and keep the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Parser {
    state: State,
    /// Continuation bytes a UTF-8 character still owes.
    owed: u8,
    params: [u8; PARAMS],
    /// How much of `params` is filled, or `PARAMS + 1` once it has overflowed
    /// and the sequence can no longer be the one that matters.
    used: usize,
    /// How deep the session is inside its own synchronized updates.
    sync: u32,
    /// Whether the session has ever opened or closed one at all.
    seen_sync: bool,
    /// How many `DECSC`s the session has not taken back with a `DECRC`.
    saved: u32,
}

/// What the parser is in the middle of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Between sequences: the next byte starts something new.
    Ground,
    /// Inside a UTF-8 character.
    Utf8,
    /// An `ESC` with nothing after it yet.
    Escape,
    /// An `ESC` and its intermediate bytes, waiting for the final one:
    /// `ESC ( B` and its kind.
    Intermediate,
    /// `ESC [`: parameters and intermediates until a final byte.
    Csi,
    /// `ESC ]`: an operating-system command, until `BEL` or `ST`.
    Osc,
    /// `ESC P`, `ESC X`, `ESC ^`, `ESC _`: a control string, until `ST`.
    String,
    /// An `ESC` inside a string: the first half of `ST`, or the start of
    /// something new that abandons the string where it stands.
    StringEscape,
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            state: State::Ground,
            owed: 0,
            params: [0; PARAMS],
            used: 0,
            sync: 0,
            seen_sync: false,
            saved: 0,
        }
    }
}

impl Parser {
    /// Watch these bytes go to the terminal.
    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.byte(byte);
        }
    }

    /// The index just past the last byte of `bytes` that ends a sequence,
    /// with the parser advanced to exactly there.
    ///
    /// Zero when there is no such byte, and then the parser is left untouched
    /// — the same bytes come back next time with more behind them, and the
    /// scan starts again from where it really is.
    ///
    /// It can afford to throw the scan away and keep an index because a parser
    /// at a sequence boundary is a *new* one but for its two counters: no
    /// half-read character, no half-read escape, and parameters that the next
    /// sequence will overwrite before it reads them. That is what
    /// `a_parser_between_sequences_carries_only_its_counters` holds.
    ///
    /// The cut asks only about the sequence, not about the span or the save
    /// slot: the session's own bytes may be passed on in the middle of either,
    /// and it is only a write of *nvmux's* that must not land there.
    fn advance_to_last_rest(&mut self, bytes: &[u8]) -> usize {
        let mut scan = *self;
        let mut at = 0;
        let mut counters = (self.sync, self.saved);
        for (i, &byte) in bytes.iter().enumerate() {
            scan.byte(byte);
            if scan.at_sequence_boundary() {
                at = i + 1;
                counters = (scan.sync, scan.saved);
            }
        }
        if at > 0 {
            *self = Parser {
                sync: counters.0,
                saved: counters.1,
                // Not a counter and never unlearned: a session that brackets
                // its frames once brackets them always.
                seen_sync: self.seen_sync || scan.seen_sync,
                ..Parser::default()
            };
        }
        at
    }

    /// Whether the next byte would start something new: nothing is half-read.
    /// Where the stream may be cut.
    fn at_sequence_boundary(&self) -> bool {
        self.state == State::Ground
    }

    /// Where nvmux may write: between sequences, and with nothing the session
    /// opened still open.
    fn at_rest(&self) -> bool {
        self.at_sequence_boundary() && self.sync == 0 && self.saved == 0
    }

    fn byte(&mut self, byte: u8) {
        match self.state {
            State::Ground => self.ground(byte),
            State::Utf8 => {
                if (0x80..=0xbf).contains(&byte) {
                    self.owed = self.owed.saturating_sub(1);
                    if self.owed == 0 {
                        self.state = State::Ground;
                    }
                } else {
                    // Not the continuation it was promised: the character is
                    // malformed and this byte starts something of its own.
                    self.state = State::Ground;
                    self.ground(byte);
                }
            }
            State::Escape => self.escape(byte),
            State::Intermediate => match byte {
                0x20..=0x2f => {}
                0x1b => self.state = State::Escape,
                // The final byte, and the end of the sequence.
                _ => self.state = State::Ground,
            },
            State::Csi => match byte {
                0x40..=0x7e => {
                    self.csi_final(byte);
                    self.state = State::Ground;
                }
                // `ESC`, `CAN` and `SUB` abandon a sequence wherever it is.
                0x1b => self.state = State::Escape,
                0x18 | 0x1a => self.state = State::Ground,
                0x20..=0x3f => self.parameter(byte),
                // A C0 control inside a CSI is executed and the sequence goes
                // on; anything else here cannot happen in a byte's range.
                _ => {}
            },
            State::Osc | State::String => match byte {
                0x07 if self.state == State::Osc => self.state = State::Ground,
                0x1b => self.state = State::StringEscape,
                0x18 | 0x1a => self.state = State::Ground,
                _ => {}
            },
            State::StringEscape => {
                if byte == b'\\' {
                    // `ESC \`, the string terminator.
                    self.state = State::Ground;
                } else {
                    // Not a terminator, so the `ESC` was the start of
                    // something new and the string ended where it stood.
                    self.escape(byte);
                }
            }
        }
    }

    fn ground(&mut self, byte: u8) {
        match byte {
            0x1b => self.state = State::Escape,
            // A UTF-8 lead byte, and how many continuations it owes. `0xf8`
            // and above lead nothing: they are not UTF-8 at all, and are left
            // to pass as the single stray byte they are.
            0xc0..=0xf7 => {
                self.owed = if byte < 0xe0 {
                    1
                } else if byte < 0xf0 {
                    2
                } else {
                    3
                };
                self.state = State::Utf8;
            }
            _ => {}
        }
    }

    fn escape(&mut self, byte: u8) {
        match byte {
            b'[' => {
                self.used = 0;
                self.state = State::Csi;
            }
            b']' => self.state = State::Osc,
            // DCS, SOS, PM, APC: all run to a string terminator.
            b'P' | b'X' | b'^' | b'_' => self.state = State::String,
            0x1b => self.state = State::Escape,
            0x20..=0x2f => self.state = State::Intermediate,
            _ => {
                // `ESC 7` and `ESC 8`: the terminal's one cursor save slot,
                // which the notice writes to as well. Saturating rather than
                // wrapping, and a restore with nothing saved is the terminal's
                // own business and not a negative depth here.
                if byte == b'7' {
                    self.saved = self.saved.saturating_add(1);
                } else if byte == b'8' {
                    self.saved = self.saved.saturating_sub(1);
                }
                self.state = State::Ground;
            }
        }
    }

    fn parameter(&mut self, byte: u8) {
        if self.used < PARAMS {
            self.params[self.used] = byte;
            self.used += 1;
        } else {
            // Past what is remembered: this is not `? 2026` and cannot become
            // it. Marked so, rather than left looking like a full buffer.
            self.used = PARAMS + 1;
        }
    }

    /// A CSI has ended. The only one worth recognising is the synchronized
    /// update, whose span nvmux must not write inside.
    fn csi_final(&mut self, byte: u8) {
        if byte != b'h' && byte != b'l' {
            return;
        }
        let Some(params) = self.params.get(..self.used) else {
            return;
        };
        let Some(private) = params.strip_prefix(b"?") else {
            return;
        };
        if private.split(|b| *b == b';').any(|p| p == b"2026") {
            self.seen_sync = true;
            self.sync = if byte == b'h' {
                self.sync.saturating_add(1)
            } else {
                self.sync.saturating_sub(1)
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A screenful as `nvim --remote-ui` writes one when the session is a
    /// starfield: a truecolour pair per cell and a half block to paint it in.
    /// What every test here is cut up and fed.
    fn frame() -> Vec<u8> {
        let mut out = Vec::new();
        for row in 1..=24u32 {
            out.extend_from_slice(format!("\x1b[{row};1H").as_bytes());
            for col in 0..40u32 {
                let v = (row * 7 + col * 3) % 256;
                out.extend_from_slice(format!("\x1b[38;2;{v};{v};{v};48;2;0;0;0m").as_bytes());
                out.extend_from_slice("▀".as_bytes());
            }
        }
        out.extend_from_slice(b"\x1b[?25h");
        out
    }

    /// Where the terminal has been left, having been shown `bytes` — which is
    /// what [`Boundary::saw`] answers and what a tail given up on produces.
    fn shown(bytes: &[u8]) -> Boundary {
        let mut boundary = Boundary::new();
        boundary.saw_session(bytes);
        boundary
    }

    /// Where a stream is between sequences, byte by byte: `true` at index `i`
    /// means the parser rests after `bytes[..=i]`.
    fn rests(bytes: &[u8]) -> Vec<bool> {
        let mut parser = Parser::default();
        bytes
            .iter()
            .map(|b| {
                parser.byte(*b);
                parser.at_rest()
            })
            .collect()
    }

    /// The whole of what a boundary hands out for `chunks`, plus whatever it
    /// was still keeping at the end.
    fn through(chunks: &[&[u8]]) -> Vec<u8> {
        let mut boundary = Boundary::new();
        let now = Instant::now();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend_from_slice(&boundary.relay(chunk, now));
        }
        out.extend_from_slice(&boundary.give_up());
        out
    }

    /// Nothing of an escape sequence is a place to write except its end.
    #[test]
    fn a_cut_escape_sequence_is_not_a_place_to_write() {
        for seq in [
            &b"\x1b[1;1H"[..],
            b"\x1b[38;2;1;2;3m",
            b"\x1b[?25l",
            b"\x1b]0;a title\x07",
            b"\x1b]11;rgb:00/00/00\x1b\\",
            b"\x1bP+q544e\x1b\\",
            b"\x1b(B",
            b"\x1bM",
        ] {
            let rests = rests(seq);
            let (last, rest) = rests.split_last().expect("a sequence has bytes");
            assert!(*last, "{seq:?} does not end between sequences");
            assert!(
                !rest.iter().any(|r| *r),
                "{seq:?} reported a boundary inside itself"
            );
        }
    }

    /// The bytes of one character are one thing, and a box dropped between
    /// them paints neither.
    #[test]
    fn a_character_is_never_split_down_the_middle() {
        let text = "aé▀😀z";
        let rests = rests(text.as_bytes());
        let ends: Vec<usize> = text.char_indices().map(|(i, c)| i + c.len_utf8()).collect();
        for (i, at_rest) in rests.iter().enumerate() {
            assert_eq!(
                *at_rest,
                ends.contains(&(i + 1)),
                "byte {i} of {text:?} answered {at_rest}"
            );
        }
    }

    /// The whole reason for the tail: a read of a screenful almost never ends
    /// where a sequence does, so a notice repainted only on the reads that do
    /// would still blink. Measured on a real flight at about one in six; a
    /// synthetic frame is tighter, and either way it is nowhere near enough.
    #[test]
    fn a_read_of_a_frame_rarely_ends_between_sequences() {
        let frame = frame();
        let mut parser = Parser::default();
        let mut reads = 0;
        let mut landed = 0;
        for chunk in frame.chunks(8192) {
            parser.feed(chunk);
            reads += 1;
            landed += usize::from(parser.at_rest());
        }
        assert!(
            reads >= 4,
            "a frame that does not fill a read proves nothing"
        );
        assert!(
            landed * 2 < reads,
            "{landed} of {reads} reads ended between sequences; the tail buys nothing"
        );
    }

    /// And what the tail buys: whatever the read was cut at, what reaches the
    /// terminal ends between sequences.
    #[test]
    fn what_is_handed_out_always_ends_between_sequences() {
        let frame = frame();
        for read in [1, 7, 64, 8192] {
            let mut boundary = Boundary::new();
            let now = Instant::now();
            for chunk in frame.chunks(read) {
                boundary.relay(chunk, now);
                assert!(
                    boundary.between_sequences(),
                    "a read of {read} left the terminal mid-sequence"
                );
            }
        }
    }

    /// A kept byte is never dropped, reordered or changed, however the reads
    /// fell.
    #[test]
    fn everything_kept_back_comes_out_in_the_order_it_arrived() {
        let frame = frame();
        for read in [1, 3, 64, 1000, 8192] {
            let chunks: Vec<&[u8]> = frame.chunks(read).collect();
            assert_eq!(through(&chunks), frame, "reassembly failed at {read}");
        }
    }

    /// The parser is the terminal's, so nvmux's own writes move it too — and
    /// what nvmux writes leaves it where it found it.
    #[test]
    fn what_nvmux_writes_over_a_session_leaves_the_parser_where_it_was() {
        let mut boundary = Boundary::new();
        boundary.relay(b"hello", Instant::now());
        assert!(boundary.between_sequences());
        boundary.saw_own(b"\x1b[?2026h\x1b7\x1b[5;5H\x1b[0mbox\x1b8\x1b[?2026l");
        assert!(
            boundary.between_sequences(),
            "the notice left the parser somewhere"
        );
    }

    /// Terminals do not nest synchronized updates, so a box dropped inside the
    /// client's would end it early and present a frame drawn half way.
    #[test]
    fn nothing_is_written_inside_the_sessions_own_synchronized_update() {
        let mut boundary = shown(b"\x1b[?2026h");
        assert!(
            !boundary.between_sequences(),
            "inside a synchronized update"
        );
        boundary.saw_session(b"\x1b[1;1Hstill drawing");
        assert!(!boundary.between_sequences(), "still inside it");
        boundary.saw_session(b"\x1b[?2026l");
        assert!(boundary.between_sequences(), "the span closed");
    }

    /// The same mode set with its neighbours, which is how a terminfo entry
    /// spells one.
    #[test]
    fn a_synchronized_update_is_recognised_among_other_modes() {
        let mut boundary = shown(b"\x1b[?1049;2026h");
        assert!(!boundary.between_sequences());
        boundary.saw_session(b"\x1b[?2026;1049l");
        assert!(boundary.between_sequences());
    }

    /// Modes that merely look like it are not it. `20268` is not `2026`, and
    /// the public `CSI 2026 h` is a different sequence from the private one.
    #[test]
    fn a_mode_that_is_not_the_synchronized_update_is_left_alone() {
        for set in [&b"\x1b[?20268h"[..], b"\x1b[2026h", b"\x1b[?2026$p"] {
            assert!(
                shown(set).between_sequences(),
                "{set:?} was read as a synchronized update"
            );
        }
    }

    /// One frame of the session's own is enough to stand down for the rest of
    /// the relay: what nvmux does with the answer is stop wrapping its own
    /// span around a write, and a client that brackets one frame brackets
    /// them all.
    #[test]
    fn a_session_that_brackets_a_frame_is_remembered_for_having_done_it() {
        let mut boundary = Boundary::new();
        let now = Instant::now();
        boundary.relay(b"\x1b[1;1Hplain text", now);
        assert!(!boundary.session_brackets_its_own_frames());
        boundary.relay(b"\x1b[?2026h\x1b[1;1Hdrawing\x1b[?2026l", now);
        assert!(boundary.session_brackets_its_own_frames());
        boundary.relay(b"\x1b[2;1Hand more plain text", now);
        assert!(
            boundary.session_brackets_its_own_frames(),
            "the span closing unlearned it"
        );
    }

    /// Every frame of the notice is bracketed in a synchronized update of its
    /// own, so a parser that learned from those would decide, on the strength
    /// of nvmux's own handwriting, that the session brackets its frames — and
    /// stand the whole mechanism down after the first box.
    ///
    /// Not hypothetical: that is what it did, and the duty cycle came back at
    /// 13% instead of 100%, which is the only reason anybody looked.
    #[test]
    fn the_notices_own_brackets_are_not_the_sessions() {
        let mut boundary = Boundary::new();
        boundary.relay(b"\x1b[1;1Hplain", Instant::now());
        boundary.saw_own(b"\x1b[?2026h\x1b7\x1b[5;5H\x1b[0mbox\x1b8\x1b[?2026l");
        assert!(
            !boundary.session_brackets_its_own_frames(),
            "nvmux read its own span as the session's"
        );
        assert!(boundary.between_sequences());
    }

    /// A span the session has opened closes the terminal to nvmux and not to
    /// the session: its own bytes go on through, cut where its sequences end.
    #[test]
    fn a_synchronized_span_holds_back_nothing_of_the_sessions_own() {
        let mut boundary = Boundary::new();
        let now = Instant::now();
        let mut out = Vec::new();
        out.extend_from_slice(&boundary.relay(b"\x1b[?2026h\x1b[1;1Hdrawing", now));
        assert!(!boundary.between_sequences(), "nvmux may not write here");
        assert_eq!(out, b"\x1b[?2026h\x1b[1;1Hdrawing", "the frame was held up");
        out.extend_from_slice(&boundary.relay(b"\x1b[?2026l", now));
        assert!(boundary.between_sequences());
    }

    /// The terminal has one cursor save slot and the notice writes to it.
    /// Between a client's `DECSC` and its `DECRC` the slot is the client's.
    #[test]
    fn nothing_is_written_between_the_sessions_save_and_its_restore() {
        let mut boundary = shown(b"\x1b7");
        assert!(!boundary.between_sequences(), "the slot is the client's");
        boundary.saw_session(b"\x1b[9;9Hmark");
        assert!(!boundary.between_sequences());
        boundary.saw_session(b"\x1b8");
        assert!(boundary.between_sequences(), "the client took it back");
    }

    /// A client that has stopped in the middle of a sequence may be waiting
    /// for the terminal's answer to it, so the wait has an end.
    #[test]
    fn a_tail_that_never_finishes_goes_out_anyway() {
        let mut boundary = Boundary::new();
        let t0 = Instant::now();
        assert!(boundary.relay(b"text\x1b[38;2", t0).ends_with(b"text"));
        assert_eq!(boundary.due(), Some(t0 + KEPT_FOR));
        assert!(boundary.relay(b"", t0 + KEPT_FOR / 2).is_empty());
        assert_eq!(
            boundary.relay(b"", t0 + KEPT_FOR),
            b"\x1b[38;2",
            "the tail was kept past its welcome"
        );
        assert_eq!(boundary.due(), None);
        assert!(
            !boundary.between_sequences(),
            "an unfinished sequence is still unfinished once it is written"
        );
    }

    /// An OSC 52 clipboard write has no length limit, and accumulating one
    /// here would hold the session's own output while it ran.
    #[test]
    fn a_string_with_no_end_to_it_is_not_kept_for_ever() {
        let mut boundary = Boundary::new();
        let now = Instant::now();
        let mut out = Vec::new();
        let mut written = 0usize;
        out.extend_from_slice(&boundary.relay(b"\x1b]52;c;", now));
        while written < 4 * KEPT_MAX {
            let chunk = vec![b'A'; 8192];
            written += chunk.len();
            out.extend_from_slice(&boundary.relay(&chunk, now));
        }
        assert!(
            out.len() > written - KEPT_MAX,
            "{} of {written} bytes are still being kept",
            written - out.len()
        );
    }

    /// The invariant [`Parser::advance_to_last_rest`] throws its scan away on:
    /// at a sequence boundary the parser is the one it started as but for its
    /// two counters, so the scan has nothing to carry back but an index and
    /// those.
    #[test]
    fn a_parser_between_sequences_carries_only_its_counters() {
        let mut parser = Parser::default();
        let mut stream = frame();
        stream.extend_from_slice(b"\x1b[?2026h\x1b7mid\x1b8\x1b[?2026l");
        for byte in stream {
            parser.byte(byte);
            if parser.at_sequence_boundary() {
                assert_eq!(
                    (parser.state, parser.owed),
                    (State::Ground, 0),
                    "a parser at a boundary carried something"
                );
            }
        }
    }

    /// A stray `ESC` abandons whatever was being read rather than being
    /// swallowed by it, or a session that wrote one inside an OSC would never
    /// be written over again.
    #[test]
    fn an_escape_abandons_the_string_it_lands_in() {
        let mut boundary = shown(b"\x1b]0;a title");
        assert!(!boundary.between_sequences());
        boundary.saw_session(b"\x1b[1;1H");
        assert!(
            boundary.between_sequences(),
            "the CSI inside the OSC was swallowed"
        );
    }

    /// Plain text is all boundary, which is the common case and the cheap one.
    #[test]
    fn a_run_of_plain_text_is_a_boundary_at_every_byte() {
        assert!(rests(b"the quick brown fox").iter().all(|r| *r));
    }

    /// A boundary starts where a relay does: the terminal has been handed over
    /// with complete sequences and is waiting for the session's first.
    #[test]
    fn a_new_boundary_is_between_sequences() {
        let boundary = Boundary::new();
        assert!(boundary.between_sequences());
        assert_eq!(boundary.due(), None);
    }
}
