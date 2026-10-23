//! Where nvmux may write over a session.
//!
//! [`crate::pty`] copies the client's bytes to the terminal unparsed, and
//! [`crate::announce`] draws a box over that stream. That is only safe between
//! the client's escape sequences. A quiet-period heuristic (25 ms) failed: a
//! session repainting at 60 fps is never quiet, so the box blinked or never
//! appeared. A [`Boundary`] instead tracks the terminal's parser state over
//! the bytes written to it (a decoder, not an emulator: no grid or cursor)
//! and reports [`Boundary::between_sequences`].
//!
//! # The tail
//!
//! The relay reads 8 KiB at a time, and only about one read in six ends on a
//! sequence boundary. So [`Boundary::relay`] holds back any incomplete tail
//! until the rest arrives (usually microseconds later), leaving the terminal
//! always between sequences. Kept bytes are never dropped, reordered or
//! changed, and are held for at most [`KEPT_FOR`] or [`KEPT_MAX`] bytes,
//! since a client stopped mid-sequence may be waiting for the terminal's
//! answer.
//!
//! `pty` keeps a `Boundary` only while something is drawn: a notice (about a
//! second at the start of a relay) or the prefix's hint bar
//! ([`crate::hint`]).
//!
//! # Non-sequences that also block a write
//!
//! A **synchronized update** (`CSI ? 2026 h` … `CSI ? 2026 l`): nvmux's box
//! opens its own span and terminals do not nest them, so writing inside the
//! client's would present a half-drawn frame. No boundary is reported inside
//! one, except at its close, which is the best moment (see
//! [`Boundary::holding_the_sessions_close`]).
//!
//! A **`DECSC`** (`ESC 7`): the notice uses the terminal's single cursor save
//! slot too, so it must not write between the client's `DECSC` and `DECRC`.
//!
//! Both block only nvmux's writes; the session's own bytes pass through.
//! Both are common: `nvim --remote-ui` queries `CSI ? 2026 $ p` and brackets
//! every frame once a terminal answers, which nearly all do. nvmux's own
//! client brackets every frame unless `'termsync'` is off.

use std::time::{Duration, Instant};

/// How long a tail is held before it goes out anyway: one 60 Hz frame. A
/// client waiting for the terminal's answer to an unfinished sequence pays
/// this much.
pub const KEPT_FOR: Duration = Duration::from_millis(16);

/// The most ever held back. Ordinary output never comes near it; it bounds
/// unlimited sequences such as an OSC 52 clipboard write (up to megabytes).
const KEPT_MAX: usize = 64 * 1024;

/// The terminal's parser state over what nvmux has written, plus the held-back
/// tail of the session's output.
///
/// Every byte written to the terminal while one exists must be fed exactly
/// once, attributed correctly: [`Boundary::relay`] for relayed session bytes,
/// [`Boundary::saw_session`] for session bytes sent another way, and
/// [`Boundary::saw_own`] for nvmux's.
///
/// Exception: the synchronized update nvmux opens around a relayed write (see
/// `Attachment::write_session_frame` in [`crate::pty`]) is not fed. It is
/// balanced, and counting it would look like the session holding the
/// terminal, shutting the notice out of the frame it was opened for.
#[derive(Debug, Default)]
pub struct Boundary {
    parser: Parser,
    /// The incomplete tail.
    kept: Vec<u8>,
    /// When the oldest kept byte was kept.
    since: Option<Instant>,
    /// Length of the session's frame close held at the front of `kept` so
    /// the notice can be written before it.
    closing: Option<usize>,
}

impl Boundary {
    /// Starts between sequences: everything before a relay is nvmux's own and
    /// complete.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed `chunk` read from the client and return what may go to the
    /// terminal now: everything up to the last sequence boundary. The rest is
    /// kept and prepended next call.
    ///
    /// A tail older than [`KEPT_FOR`] or longer than [`KEPT_MAX`] goes out
    /// unfinished: the session's output matters more than the notice.
    ///
    /// **A frame close is held ahead of that timeout check.** The tail's age
    /// counts from the first kept byte, so on a busy session the deadline has
    /// nearly always passed, and the relay flushes before offering the notice
    /// its turn. Without this, the close (the notice's only chance on a
    /// bracketing session) was flushed and the box missing in 3 runs of 8.
    ///
    /// Reads without a close still flush unfinished every [`KEPT_FOR`], as on
    /// terminals without `?2026`. Counting age from the last hand-out instead
    /// was tried and reverted: it still let a flush take the close.
    pub fn relay(&mut self, chunk: &[u8], now: Instant) -> Vec<u8> {
        self.kept.extend_from_slice(chunk);
        self.closing = None;
        let scan = self.parser.scan(&self.kept);
        let cut = match scan.close.filter(|_| scan.brackets) {
            // Stop before the session's frame close and hold it, so the
            // notice lands inside that frame. Checked before patience; the
            // buffer shrinks either way.
            Some(close) => {
                self.parser
                    .take(close.starts, close.counters, scan.brackets);
                self.closing = Some(close.ends - close.starts);
                close.starts
            }
            None if self.out_of_patience(now) => {
                self.parser.feed(&self.kept);
                self.kept.len()
            }
            None => {
                self.parser.take(scan.rest, scan.counters, scan.brackets);
                scan.rest
            }
        };
        let ready = self.kept.drain(..cut).collect();
        self.since = if self.kept.is_empty() {
            None
        } else {
            self.since.or(Some(now))
        };
        ready
    }

    /// Like [`Boundary::relay`] but with no timeout and no held frame close:
    /// an incomplete tail is kept however long it takes, up to [`KEPT_MAX`].
    ///
    /// For the settings panel over a session (see `crate::pty::panel`), which
    /// writes session output only in front of its own frame in one
    /// synchronized update. An unfinished tail would leave nowhere to put that
    /// frame and the session would draw over the panel. Waiting costs nothing:
    /// the bytes go out with the next frame anyway.
    pub fn relay_whole(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.kept.extend_from_slice(chunk);
        self.closing = None;
        self.since = None;
        let scan = self.parser.scan(&self.kept);
        let cut = if self.kept.len() - scan.rest >= KEPT_MAX {
            // An unbounded string such as OSC 52: flush unfinished.
            self.parser.feed(&self.kept);
            self.kept.len()
        } else {
            self.parser.take(scan.rest, scan.counters, scan.brackets);
            scan.rest
        };
        self.kept.drain(..cut).collect()
    }

    /// Whether session bytes are held back (after [`Boundary::relay_whole`],
    /// an incomplete tail).
    pub fn keeping(&self) -> bool {
        !self.kept.is_empty()
    }

    /// Whether the held bytes start with the session's frame close, where
    /// nvmux's box belongs.
    ///
    /// A bracketing session's frame is presented only at `CSI ? 2026 l`, so a
    /// box written after it misses every frame (measured: visible 15 ms of
    /// its second). The close is held for just the box's write, then sent.
    fn holding_the_sessions_close(&self) -> bool {
        self.closing.is_some()
    }

    /// The held frame close, to write after the notice.
    pub fn release_the_sessions_close(&mut self) -> Vec<u8> {
        let Some(len) = self.closing.take() else {
            return Vec::new();
        };
        let close: Vec<u8> = self.kept.drain(..len.min(self.kept.len())).collect();
        self.parser.feed(&close);
        if self.kept.is_empty() {
            self.since = None;
        }
        close
    }

    /// Session bytes sent other than through [`Boundary::relay`] (the held
    /// first paint). Unlike [`Boundary::saw_own`], these count towards
    /// [`Boundary::session_brackets_its_own_frames`].
    pub fn saw_session(&mut self, bytes: &[u8]) {
        self.parser.feed(bytes);
    }

    /// Bytes nvmux wrote over the session (notice frames, hint bar). They are
    /// balanced, but still fed, as they move the terminal's parser.
    ///
    /// They must not set [`Boundary::session_brackets_its_own_frames`]: each
    /// is wrapped in its own synchronized update, which would otherwise
    /// disable the mechanism after the first box (once measured: 13% instead
    /// of 100%).
    pub fn saw_own(&mut self, bytes: &[u8]) {
        let session_syncs = self.parser.seen_sync;
        self.parser.feed(bytes);
        self.parser.seen_sync = session_syncs;
    }

    /// Whether the terminal is mid-character or mid-sequence.
    ///
    /// Weaker than [`Boundary::between_sequences`], which also refuses inside
    /// a session span or saved cursor. This is the check for the sequences
    /// nvmux wraps a relayed write in: they overwrite nothing, but must not
    /// split a UTF-8 character or sequence.
    pub fn mid_sequence(&self) -> bool {
        !self.parser.at_sequence_boundary()
    }

    /// Whether nvmux may write now without landing inside a session sequence,
    /// span or saved cursor. Also true at a held frame close, the best moment
    /// (see [`Boundary::holding_the_sessions_close`]).
    pub fn between_sequences(&self) -> bool {
        self.parser.at_rest() || self.holding_the_sessions_close()
    }

    /// Whether the session has ever opened a synchronized update.
    ///
    /// If so, nvmux must not wrap relayed writes in its own (see
    /// `Attachment::write_session_frame` in [`crate::pty`]): terminals treat
    /// `?2026` as a mode, not a count, so the session's reset would end
    /// nvmux's span early and tear the frame. A client that brackets once
    /// brackets every frame.
    pub fn session_brackets_its_own_frames(&self) -> bool {
        self.parser.seen_sync
    }

    /// When the kept tail is flushed regardless, or `None` if nothing is kept.
    ///
    /// Also `None` while a frame close is held: otherwise a past deadline
    /// flushes the close before the notice gets its turn (see
    /// [`Boundary::relay`]). The close goes out on the same pass anyway.
    pub fn due(&self) -> Option<Instant> {
        if self.holding_the_sessions_close() {
            return None;
        }
        self.since.map(|since| since + KEPT_FOR)
    }

    /// Return everything kept, complete or not, when the wait is over or on
    /// exit, so no client byte is lost.
    ///
    /// Clears any held close too. Leaving it set made
    /// [`Boundary::between_sequences`] report a boundary that had passed, and
    /// the box was written mid-character (seen inside a `\xe2\x94\x8c`).
    pub fn give_up(&mut self) -> Vec<u8> {
        self.parser.feed(&self.kept);
        self.since = None;
        self.closing = None;
        std::mem::take(&mut self.kept)
    }

    fn out_of_patience(&self, now: Instant) -> bool {
        self.kept.len() >= KEPT_MAX
            || self
                .since
                .is_some_and(|since| now.saturating_duration_since(since) >= KEPT_FOR)
    }
}

/// How many CSI parameter and intermediate bytes are kept.
///
/// Only `? 2026` is recognised by its parameters; 32 fits combined forms like
/// `? 1049 ; 1002 ; 1006 ; 2026 h`. Longer parameters are discarded and the
/// CSI is treated as ordinary.
const PARAMS: usize = 32;

/// The terminal's parser state. `Copy`, so [`Parser::scan`] can scan ahead on
/// a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Parser {
    state: State,
    /// Continuation bytes the current UTF-8 character still needs.
    owed: u8,
    params: [u8; PARAMS],
    /// Bytes used in `params`, or `PARAMS + 1` once overflowed.
    used: usize,
    /// Nesting depth of the session's synchronized updates.
    sync: u32,
    /// Whether the session has ever opened or closed one.
    seen_sync: bool,
    /// `DECSC`s not yet matched by a `DECRC`.
    saved: u32,
}

/// Result of [`Parser::scan`].
#[derive(Debug, Clone, Copy)]
struct Scan {
    /// Index just past the last byte that ends a sequence.
    rest: usize,
    /// `(sync, saved)` at `rest`.
    counters: (u32, u32),
    /// The first frame close in the scan, if any.
    close: Option<Close>,
    /// Whether the session has ever bracketed a frame, this scan included.
    brackets: bool,
}

/// The session's closing `CSI ? 2026 l`.
#[derive(Debug, Clone, Copy)]
struct Close {
    starts: usize,
    /// Index just past its last byte.
    ends: usize,
    /// `(sync, saved)` just before it, still inside the frame.
    counters: (u32, u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Between sequences.
    Ground,
    Utf8,
    /// An `ESC` with nothing after it yet.
    Escape,
    /// `ESC` plus intermediate bytes awaiting a final byte, as in `ESC ( B`.
    Intermediate,
    Csi,
    /// Until `BEL` or `ST`.
    Osc,
    /// `ESC P`, `ESC X`, `ESC ^`, `ESC _`: until `ST`.
    String,
    /// `ESC` inside a string: half of `ST`, or a new sequence that ends the
    /// string.
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
    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.byte(byte);
        }
    }

    /// Scan `bytes` without advancing `self`.
    fn scan(&self, bytes: &[u8]) -> Scan {
        let mut scan = *self;
        let mut found = Scan {
            rest: 0,
            counters: (self.sync, self.saved),
            close: None,
            brackets: self.seen_sync,
        };
        // Start of the current sequence, so a close can be cut before it.
        let mut started = 0;
        for (i, &byte) in bytes.iter().enumerate() {
            let was_ground = scan.at_sequence_boundary();
            let inside_a_frame = scan.sync;
            scan.byte(byte);
            if was_ground {
                started = i;
            }
            if scan.at_sequence_boundary() {
                found.rest = i + 1;
                found.counters = (scan.sync, scan.saved);
                // A frame close, where the box can go into the frame. Record
                // the first, not the last: any close handed out unheld is a
                // frame presented without the box. Later ones get their turn
                // on later relay passes.
                if inside_a_frame == 1 && scan.sync == 0 && found.close.is_none() {
                    found.close = Some(Close {
                        starts: started,
                        ends: i + 1,
                        counters: (inside_a_frame, scan.saved),
                    });
                }
            }
        }
        found.brackets = self.seen_sync || scan.seen_sync;
        found
    }

    /// Advance to a ground position `at` from a scan, with its counters.
    fn take(&mut self, at: usize, counters: (u32, u32), brackets: bool) {
        if at == 0 {
            return;
        }
        *self = Parser {
            sync: counters.0,
            saved: counters.1,
            // Never unlearned.
            seen_sync: brackets,
            ..Parser::default()
        };
    }

    /// Nothing is half-read; the stream may be cut here.
    fn at_sequence_boundary(&self) -> bool {
        self.state == State::Ground
    }

    /// Where nvmux may write: at a boundary with no session span or saved
    /// cursor open.
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
                    // Malformed character; this byte starts afresh.
                    self.state = State::Ground;
                    self.ground(byte);
                }
            }
            State::Escape => self.escape(byte),
            State::Intermediate => match byte {
                0x20..=0x2f => {}
                0x1b => self.state = State::Escape,
                // Final byte.
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
                // A C0 control inside a CSI is executed and the CSI goes on.
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
                    // `ESC \` (ST).
                    self.state = State::Ground;
                } else {
                    // The `ESC` began a new sequence, ending the string.
                    self.escape(byte);
                }
            }
        }
    }

    fn ground(&mut self, byte: u8) {
        match byte {
            0x1b => self.state = State::Escape,
            // UTF-8 lead byte. `0xf8` and above are not UTF-8 and pass as
            // single stray bytes.
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
            // DCS, SOS, PM, APC: all run to ST.
            b'P' | b'X' | b'^' | b'_' => self.state = State::String,
            0x1b => self.state = State::Escape,
            0x20..=0x2f => self.state = State::Intermediate,
            _ => {
                // `ESC 7` / `ESC 8` (DECSC/DECRC). Saturating: a restore with
                // nothing saved is not a negative depth.
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
            // Overflowed: cannot be `? 2026`. Marked distinct from a full
            // buffer.
            self.used = PARAMS + 1;
        }
    }

    /// A CSI ended; only synchronized update set/reset is tracked.
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

    /// A screenful like `nvim --remote-ui` writes for a starfield: a
    /// truecolour pair and a half block per cell.
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

    /// A boundary that has been shown `bytes` as the session's.
    fn shown(bytes: &[u8]) -> Boundary {
        let mut boundary = Boundary::new();
        boundary.saw_session(bytes);
        boundary
    }

    /// `true` at index `i` means the parser rests after `bytes[..=i]`.
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

    /// Everything handed out for `chunks`, plus what is kept at the end.
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

    /// Why the tail is kept: few reads end on a sequence boundary (about one
    /// in six in real output).
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

    #[test]
    fn everything_kept_back_comes_out_in_the_order_it_arrived() {
        let frame = frame();
        for read in [1, 3, 64, 1000, 8192] {
            let chunks: Vec<&[u8]> = frame.chunks(read).collect();
            assert_eq!(through(&chunks), frame, "reassembly failed at {read}");
        }
    }

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

    /// Terminals do not nest synchronized updates.
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

    /// As terminfo entries spell it.
    #[test]
    fn a_synchronized_update_is_recognised_among_other_modes() {
        let mut boundary = shown(b"\x1b[?1049;2026h");
        assert!(!boundary.between_sequences());
        boundary.saw_session(b"\x1b[?2026;1049l");
        assert!(boundary.between_sequences());
    }

    /// `20268`, the public `CSI 2026 h`, and the DECRQM query.
    #[test]
    fn a_mode_that_is_not_the_synchronized_update_is_left_alone() {
        for set in [&b"\x1b[?20268h"[..], b"\x1b[2026h", b"\x1b[?2026$p"] {
            assert!(
                shown(set).between_sequences(),
                "{set:?} was read as a synchronized update"
            );
        }
    }

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

    /// Regression: learning from the notice's own spans disabled the
    /// mechanism after the first box (13% duty cycle instead of 100%).
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

    /// The common case: a bracketing session's close is held so the box goes
    /// inside its frame. Without this the notice was visible 15 ms of its
    /// second.
    #[test]
    fn the_notice_goes_inside_the_frame_the_session_is_about_to_present() {
        let mut boundary = Boundary::new();
        let now = Instant::now();
        // One frame of a bracketing client, arriving in two reads.
        boundary.relay(b"\x1b[?2026h\x1b[1;1Hdrawing", now);
        assert!(
            !boundary.holding_the_sessions_close(),
            "there is no close in sight yet"
        );
        let ready = boundary.relay(b" some more\x1b[?2026l", now);
        assert_eq!(
            ready, b" some more",
            "the frame's close went out ahead of the notice"
        );
        assert!(
            boundary.holding_the_sessions_close(),
            "the close was not held for the notice"
        );
        assert!(
            boundary.between_sequences(),
            "the one moment the box belongs in is refused"
        );

        boundary.saw_own(b"\x1b[?2026h\x1b7box\x1b8\x1b[?2026l");
        assert_eq!(boundary.release_the_sessions_close(), b"\x1b[?2026l");
        assert!(!boundary.holding_the_sessions_close());
    }

    /// A close is held even past the deadline, and nothing is due while it
    /// is; otherwise the relay flushes it before the notice gets a turn.
    #[test]
    fn a_frame_close_is_worth_more_than_the_clock_that_would_flush_it() {
        let mut boundary = Boundary::new();
        let t0 = Instant::now();
        boundary.relay(b"\x1b[?2026h\x1b[1;1Hdrawing\x1b[", t0);
        assert_eq!(
            boundary.due(),
            Some(t0 + KEPT_FOR),
            "an ordinary tail is on the clock"
        );

        // Past the deadline, with a frame close in this read.
        let late = t0 + KEPT_FOR + KEPT_FOR;
        let ready = boundary.relay(b"2;1Hmore\x1b[?2026l\x1b[?2026h\x1b[3;1H", late);
        assert_eq!(
            ready, b"\x1b[2;1Hmore",
            "the close went out with the impatient tail"
        );
        assert!(
            boundary.holding_the_sessions_close(),
            "the clock took away the one moment the notice has"
        );
        assert_eq!(
            boundary.due(),
            None,
            "the relay would flush the close before the notice was offered it"
        );
        assert_eq!(boundary.release_the_sessions_close(), b"\x1b[?2026l");
    }

    /// Regression: a held close flushed with the tail (as `release_kept` does
    /// in the normal relay order) must stop counting as a boundary, or the box
    /// lands wherever the flush ended, e.g. mid-character.
    #[test]
    fn a_close_that_goes_out_with_the_tail_stops_being_a_moment_to_write_in() {
        let mut boundary = Boundary::new();
        let now = Instant::now();
        // A frame close, then the next frame stopping mid-character.
        boundary.relay(b"\x1b[?2026h\x1b[1;1Hdrawing", now);
        let held = boundary.relay(b"\x1b[?2026l\x1b[?2026h\x1b[2;3H\xe2\x94", now);
        assert!(held.is_empty(), "the close went out ahead of the notice");
        assert!(boundary.holding_the_sessions_close());

        // The tail times out before the notice gets its turn.
        let flushed = boundary.give_up();
        assert_eq!(
            flushed, b"\x1b[?2026l\x1b[?2026h\x1b[2;3H\xe2\x94",
            "the held close was not written with the tail it was sitting on"
        );
        assert!(
            !boundary.holding_the_sessions_close(),
            "a close already at the terminal is still being offered to write in front of"
        );
        assert!(
            !boundary.between_sequences(),
            "the box would land in the middle of the character that flush ended on"
        );
    }

    #[test]
    fn a_held_frame_close_is_the_next_thing_out_after_the_notice() {
        let frame = b"\x1b[?2026h\x1b[1;1Hhello\x1b[?2026l\x1b[?2026h\x1b[2;1Hagain\x1b[?2026l";
        for read in [1, 5, 64, 8192] {
            let mut boundary = Boundary::new();
            let now = Instant::now();
            let mut out = Vec::new();
            for chunk in frame.chunks(read) {
                out.extend_from_slice(&boundary.relay(chunk, now));
                out.extend_from_slice(&boundary.release_the_sessions_close());
            }
            out.extend_from_slice(&boundary.give_up());
            assert_eq!(out, frame, "the stream came apart at a read of {read}");
        }
    }

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

    /// The notice also uses the terminal's single cursor save slot.
    #[test]
    fn nothing_is_written_between_the_sessions_save_and_its_restore() {
        let mut boundary = shown(b"\x1b7");
        assert!(!boundary.between_sequences(), "the slot is the client's");
        boundary.saw_session(b"\x1b[9;9Hmark");
        assert!(!boundary.between_sequences());
        boundary.saw_session(b"\x1b8");
        assert!(boundary.between_sequences(), "the client took it back");
    }

    /// A client stopped mid-sequence may be waiting for the terminal's answer.
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

    #[test]
    fn a_whole_relay_keeps_an_unfinished_tail_for_as_long_as_it_takes() {
        let mut boundary = Boundary::new();
        assert_eq!(boundary.relay_whole(b"text\x1b[38;2"), b"text");
        assert!(!boundary.mid_sequence(), "nothing unfinished has gone out");
        assert!(boundary.keeping(), "it is kept here instead");
        assert_eq!(boundary.due(), None, "and no clock is running on it");
        assert!(boundary.relay_whole(b"").is_empty());
        assert_eq!(boundary.relay_whole(b";1;2;3mX"), b"\x1b[38;2;1;2;3mX");
        assert!(!boundary.mid_sequence());
        assert!(!boundary.keeping());
        let whole = frame();
        let mut out = Vec::new();
        for piece in whole.chunks(7) {
            out.extend_from_slice(&boundary.relay_whole(piece));
            assert!(!boundary.mid_sequence(), "handed out mid-sequence");
        }
        assert_eq!(out, whole, "all of it, in order, once the last piece came");
    }

    /// Its caller writes its own frame after everything handed out.
    #[test]
    fn a_whole_relay_hands_out_a_frame_close_with_the_rest() {
        let mut boundary = Boundary::new();
        let framed = b"\x1b[?2026hpaint\x1b[?2026lmore";
        assert_eq!(boundary.relay_whole(framed), framed);
        assert!(boundary.release_the_sessions_close().is_empty());
    }

    /// An unterminated string goes out unfinished at [`KEPT_MAX`]; a long
    /// read of complete sequences is not mistaken for one.
    #[test]
    fn a_whole_relay_lets_a_string_with_no_end_go() {
        let mut boundary = Boundary::new();
        let mut out = boundary.relay_whole(b"before\x1b]52;c;");
        assert_eq!(out, b"before");
        let mut written = 0usize;
        while written < 4 * KEPT_MAX {
            let chunk = vec![b'A'; 8192];
            written += chunk.len();
            out.extend_from_slice(&boundary.relay_whole(&chunk));
        }
        assert!(boundary.mid_sequence(), "gone out inside the string");
        assert!(
            out.len() > written - KEPT_MAX,
            "{} of {written} bytes are still being kept",
            written - out.len()
        );
        let mut whole = [frame(), frame(), frame()].concat();
        assert!(whole.len() > KEPT_MAX);
        whole.extend_from_slice(b"\x1b[38;2");
        let mut boundary = Boundary::new();
        assert_eq!(
            boundary.relay_whole(&whole),
            &whole[..whole.len() - 6],
            "a long read is no unfinished string"
        );
        assert!(!boundary.mid_sequence());
    }

    /// An OSC 52 clipboard write has no length limit.
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

    /// The invariant [`Parser::take`] relies on: at a boundary, only the
    /// counters and `seen_sync` differ from a default parser.
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

    /// Otherwise a stray `ESC` inside an OSC would block nvmux forever.
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

    #[test]
    fn a_run_of_plain_text_is_a_boundary_at_every_byte() {
        assert!(rests(b"the quick brown fox").iter().all(|r| *r));
    }

    #[test]
    fn a_new_boundary_is_between_sequences() {
        let boundary = Boundary::new();
        assert!(boundary.between_sequences());
        assert_eq!(boundary.due(), None);
    }
}
