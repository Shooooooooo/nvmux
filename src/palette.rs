//! The terminal's own colours, asked for once at startup.
//!
//! nvmux's screens set no colours of their own (see [`crate::ui`]), so it does
//! not know what its text looks like on the terminal, and it never sees the
//! cells of an attached session at all. The fade ([`crate::fade`]) needs both
//! ends of every interpolation to be real colours: what a cell is now, and the
//! terminal background it is dissolving into. So this module asks the terminal
//! what those are — the default foreground and background (OSC 10 and 11) and
//! the sixteen ANSI colours (OSC 4) — and keeps the answer for the run.
//!
//! # The query, and its one risk
//!
//! The answers arrive on stdin, as if typed, and nothing but this module ever
//! parses an OSC reply: crossterm has no parser for one, so a reply that
//! reached the picker would be read as `Alt-]`, `1`, `1`, `;`, `r`, … — a
//! session number, `r`ename, `/`filter. Two things keep the replies here.
//! The request ends with a DSR (`ESC [ 5 n`), which every terminal answers,
//! after the OSC replies it is going to send, so the wait ends after one round
//! trip whether the terminal knows OSC or not — the same terminator Neovim's
//! own TUI uses for its background query. And the cap on that wait is
//! generous, [`QUERY_CAP`], because a late reply is worse than a slow start.
//!
//! What the query cannot survive is type-ahead. The read that collects the
//! replies collects whatever else is in the input queue, and a keystroke
//! taken that way cannot be given back (there is no `TIOCSTI` on a modern
//! kernel). So if anything is already waiting on stdin the query is skipped,
//! and the run does without a fade rather than eat the user's first key. The
//! look has to be taken from raw mode: until then a key typed at the shell's
//! prompt is held in the canonical line buffer, invisible to `poll`.
//!
//! # What is not asked
//!
//! Only the terminals that answer OSC 4 give up their ANSI sixteen — tmux,
//! Terminal.app and Windows Terminal do not. For those the xterm defaults
//! stand in, so a cell painted in an ANSI colour starts its fade one shade
//! off; the default foreground and background, which are most of any screen,
//! are always the terminal's own or the fade does not run.

use std::io::IsTerminal;
use std::os::fd::AsRawFd;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// A colour, as the terminal draws it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// The colour `t` of the way from `self` to `to`, per channel. `t` is
    /// clamped to `0..=1`: 0 is `self`, 1 is `to`.
    pub fn lerp(self, to: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let mix = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
        Rgb(mix(self.0, to.0), mix(self.1, to.1), mix(self.2, to.2))
    }
}

/// What the terminal said its colours are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// The default foreground: what text with no colour set is drawn in.
    pub fg: Rgb,
    /// The default background: what the fade dissolves into.
    pub bg: Rgb,
    /// The sixteen ANSI colours, indices 0–15 of the 256-colour table.
    pub ansi: [Rgb; 16],
}

impl Palette {
    /// The colour behind an indexed SGR (`38;5;n` / `48;5;n`): the terminal's
    /// own for the first sixteen, and xterm's fixed 6×6×6 cube and grey ramp
    /// for the rest, which every terminal shares.
    pub fn index(&self, n: u8) -> Rgb {
        match n {
            0..=15 => self.ansi[usize::from(n)],
            16..=231 => {
                let n = n - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
                Rgb(level(n / 36), level((n / 6) % 6), level(n % 6))
            }
            232..=255 => {
                let v = 8 + 10 * (n - 232);
                Rgb(v, v, v)
            }
        }
    }
}

/// xterm's default sixteen, for a terminal that does not answer OSC 4.
const XTERM_ANSI: [Rgb; 16] = [
    Rgb(0x00, 0x00, 0x00),
    Rgb(0xcd, 0x00, 0x00),
    Rgb(0x00, 0xcd, 0x00),
    Rgb(0xcd, 0xcd, 0x00),
    Rgb(0x00, 0x00, 0xee),
    Rgb(0xcd, 0x00, 0xcd),
    Rgb(0x00, 0xcd, 0xcd),
    Rgb(0xe5, 0xe5, 0xe5),
    Rgb(0x7f, 0x7f, 0x7f),
    Rgb(0xff, 0x00, 0x00),
    Rgb(0x00, 0xff, 0x00),
    Rgb(0xff, 0xff, 0x00),
    Rgb(0x5c, 0x5c, 0xff),
    Rgb(0xff, 0x00, 0xff),
    Rgb(0x00, 0xff, 0xff),
    Rgb(0xff, 0xff, 0xff),
];

/// What the terminal answered, as far as it did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Replies {
    fg: Option<Rgb>,
    bg: Option<Rgb>,
    ansi: [Option<Rgb>; 16],
    /// The DSR reply arrived: the terminal has answered everything it is
    /// going to.
    dsr: bool,
}

impl Replies {
    /// Whether there is any point waiting longer: the terminator has arrived,
    /// or every answer already has.
    fn complete(&self) -> bool {
        self.dsr
            || (self.fg.is_some() && self.bg.is_some() && self.ansi.iter().all(Option::is_some))
    }

    /// The palette these answers make, if the two that matter are in.
    fn palette(&self) -> Option<Palette> {
        let mut ansi = XTERM_ANSI;
        for (slot, answer) in ansi.iter_mut().zip(&self.ansi) {
            if let Some(colour) = answer {
                *slot = *colour;
            }
        }
        Some(Palette {
            fg: self.fg?,
            bg: self.bg?,
            ansi,
        })
    }
}

/// The DSR reply, `ESC [ 0 n`: the terminal is fine, and has worked through
/// everything before it.
const DSR_REPLY: &[u8] = b"\x1b[0n";

/// Read every reply out of `bytes`, ignoring whatever else is in there.
///
/// Pure, so the shapes a terminal can answer in are testable without one. An
/// OSC reply is `ESC ] <payload> BEL` or `ESC ] <payload> ESC \`; the payload
/// is `10;<colour>`, `11;<colour>` or `4;<n>;<colour>`. A reply cut short by
/// the end of `bytes` is not yet a reply, and the caller comes back with more.
fn parse_replies(bytes: &[u8]) -> Replies {
    let mut replies = Replies::default();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(DSR_REPLY) {
            replies.dsr = true;
            i += DSR_REPLY.len();
            continue;
        }
        if !bytes[i..].starts_with(b"\x1b]") {
            i += 1;
            continue;
        }
        let start = i + 2;
        let Some((payload, end)) = osc_payload(&bytes[start..]) else {
            // Unterminated: wait for the rest.
            break;
        };
        i = start + end;
        let Ok(payload) = std::str::from_utf8(payload) else {
            continue;
        };
        let mut parts = payload.splitn(3, ';');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("10"), Some(colour), None) => replies.fg = parse_colour(colour),
            (Some("11"), Some(colour), None) => replies.bg = parse_colour(colour),
            (Some("4"), Some(n), Some(colour)) => {
                if let Some(slot) = n.parse::<usize>().ok().filter(|n| *n < 16) {
                    replies.ansi[slot] = parse_colour(colour);
                }
            }
            _ => {}
        }
    }
    replies
}

/// The payload of an OSC that starts at `bytes[0]` (just past `ESC ]`), and
/// the index just past its terminator — or `None` if it has no terminator yet.
fn osc_payload(bytes: &[u8]) -> Option<(&[u8], usize)> {
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            0x07 => return Some((&bytes[..i], i + 1)),
            0x1b if bytes.get(i + 1) == Some(&b'\\') => return Some((&bytes[..i], i + 2)),
            // An ESC with nothing after it yet may be the first half of ST:
            // wait for the rest. One followed by anything else is a stray,
            // and this OSC was never finished — treat it as ending here so
            // the bytes after it are still looked at.
            0x1b if i + 1 == bytes.len() => return None,
            0x1b => return Some((&bytes[..i], i)),
            _ => {}
        }
    }
    None
}

/// A colour as a terminal spells one back: `rgb:rr/gg/bb` with one to four
/// hex digits per channel (`rgba:` with a fourth channel, ignored), or
/// `#rgb` / `#rrggbb` / `#rrrgggbbb` / `#rrrrggggbbbb`. Anything else is not
/// a colour.
fn parse_colour(spec: &str) -> Option<Rgb> {
    let spec = spec.trim();
    if let Some(hex) = spec.strip_prefix('#') {
        if hex.is_empty() || hex.len() % 3 != 0 || hex.len() > 12 {
            return None;
        }
        let width = hex.len() / 3;
        let mut channels = (0..3).map(|c| scale(&hex[c * width..(c + 1) * width]));
        return Some(Rgb(channels.next()??, channels.next()??, channels.next()??));
    }
    let body = spec
        .strip_prefix("rgb:")
        .or_else(|| spec.strip_prefix("rgba:"))?;
    let mut channels = body.split('/');
    let r = scale(channels.next()?)?;
    let g = scale(channels.next()?)?;
    let b = scale(channels.next()?)?;
    Some(Rgb(r, g, b))
}

/// One to four hex digits, scaled so that all-ones is 255: `f` and `ffff`
/// both mean full, `1e1e` means `1e`.
fn scale(digits: &str) -> Option<u8> {
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    let max = (1u32 << (4 * digits.len())) - 1;
    Some(((value * 255 + max / 2) / max) as u8)
}

/// The request, in the order the answers are wanted: fg, bg, the sixteen, and
/// the DSR that says the terminal is done answering.
fn request() -> Vec<u8> {
    let mut out = Vec::from(&b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\"[..]);
    for n in 0..16 {
        out.extend_from_slice(format!("\x1b]4;{n};?\x1b\\").as_bytes());
    }
    out.extend_from_slice(b"\x1b[5n");
    out
}

/// The most a run will wait for its terminal. Long, on purpose: the DSR reply
/// ends the wait after one round trip on any terminal that answers DSR at all,
/// so this is only ever reached by one that answers nothing — and a reply that
/// arrived *after* the wait would be typed into the picker.
const QUERY_CAP: Duration = Duration::from_millis(1500);

/// Ask the terminal for its colours. `None` when it cannot be asked (no
/// terminal, a key already waiting) or did not say.
///
/// Raw mode for the duration, so the replies are neither echoed nor held back
/// for a newline; restored on the way out, whichever way that is.
pub fn query() -> Option<Palette> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() || !std::io::stdout().is_terminal() {
        return None;
    }
    let fd = stdin.as_raw_fd();
    let _raw = match crate::term::RawMode::enter() {
        Ok(raw) => raw,
        Err(e) => {
            tracing::debug!(error = %e, "palette: could not enter raw mode to ask");
            return None;
        }
    };
    // Looked at from raw mode, not before it: a key typed before nvmux started
    // is sitting in the line discipline's canonical buffer, where `poll` does
    // not report it until the mode changes. Restoring the modes on the way out
    // does not flush input, so the key is still there for the picker.
    if readable_now(fd) {
        tracing::debug!("palette: a key is already waiting; not asking the terminal");
        return None;
    }
    if crate::term::write_stdout(&request()).is_err() {
        return None;
    }

    let started = Instant::now();
    let deadline = started + QUERY_CAP;
    let mut bytes = Vec::new();
    let mut buf = [0u8; 4096];
    let mut replies = Replies::default();
    while !replies.complete() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut p = [crate::pty::pollfd(fd)];
        let wait = left.as_millis().min(i32::MAX as u128) as libc::c_int;
        let n = unsafe { libc::poll(p.as_mut_ptr(), 1, wait) };
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if n <= 0 {
            break;
        }
        match crate::pty::read_fd(fd, &mut buf) {
            Ok(len) if len > 0 => bytes.extend_from_slice(&buf[..len]),
            _ => break,
        }
        replies = parse_replies(&bytes);
    }

    let palette = replies.palette();
    tracing::debug!(
        ms = started.elapsed().as_secs_f64() * 1000.0,
        answered = palette.is_some(),
        dsr = replies.dsr,
        "palette: asked the terminal for its colours"
    );
    palette
}

/// Whether a read on `fd` would return at once.
fn readable_now(fd: std::os::fd::RawFd) -> bool {
    let mut p = [crate::pty::pollfd(fd)];
    let n = unsafe { libc::poll(p.as_mut_ptr(), 1, 0) };
    n > 0 && crate::pty::ready(&p[0])
}

static PALETTE: OnceLock<Option<Palette>> = OnceLock::new();

/// Keep the terminal's answer, once. `main` calls this at startup, and the
/// relay tests' child process does the same to switch the fade on; a second
/// call in one process is a bug, so it warns and keeps the first.
pub fn init(palette: Option<Palette>) {
    if PALETTE.set(palette).is_err() {
        tracing::warn!("palette initialised more than once; keeping the first");
    }
}

/// The terminal's colours, if it was asked and answered. `None` before
/// [`init`] — which is what every unit test sees, so nothing animates under
/// `cargo test`.
pub fn get() -> Option<&'static Palette> {
    PALETTE.get().and_then(Option::as_ref)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lerp_runs_from_one_end_to_the_other() {
        let a = Rgb(0, 100, 200);
        let b = Rgb(200, 100, 0);
        assert_eq!(a.lerp(b, 0.0), a);
        assert_eq!(a.lerp(b, 1.0), b);
        assert_eq!(a.lerp(b, 0.5), Rgb(100, 100, 100));
        // Clamped, so a schedule that overshoots cannot leave the range.
        assert_eq!(a.lerp(b, -1.0), a);
        assert_eq!(a.lerp(b, 2.0), b);
    }

    fn palette() -> Palette {
        Palette {
            fg: Rgb(0xee, 0xee, 0xee),
            bg: Rgb(0x10, 0x10, 0x10),
            ansi: XTERM_ANSI,
        }
    }

    /// The corners of the 256-colour table, as xterm defines them.
    #[test]
    fn the_indexed_table_has_xterms_corners() {
        let p = palette();
        assert_eq!(p.index(0), Rgb(0, 0, 0));
        assert_eq!(p.index(15), Rgb(0xff, 0xff, 0xff));
        assert_eq!(p.index(16), Rgb(0, 0, 0), "the cube starts at black");
        assert_eq!(p.index(21), Rgb(0, 0, 0xff), "the cube's blue corner");
        assert_eq!(p.index(196), Rgb(0xff, 0, 0), "the cube's red corner");
        assert_eq!(
            p.index(231),
            Rgb(0xff, 0xff, 0xff),
            "the cube ends at white"
        );
        assert_eq!(p.index(232), Rgb(8, 8, 8), "the ramp starts near black");
        assert_eq!(p.index(255), Rgb(238, 238, 238), "and ends near white");
    }

    /// The sixteen come from the terminal when it answered and from xterm's
    /// defaults when it did not, entry by entry.
    #[test]
    fn unanswered_ansi_entries_fall_back_to_xterms() {
        let mut replies = Replies {
            fg: Some(Rgb(1, 2, 3)),
            bg: Some(Rgb(4, 5, 6)),
            ..Replies::default()
        };
        replies.ansi[1] = Some(Rgb(0xaa, 0, 0));
        let p = replies.palette().expect("fg and bg are in");
        assert_eq!(p.ansi[1], Rgb(0xaa, 0, 0));
        assert_eq!(p.ansi[2], XTERM_ANSI[2]);
        assert_eq!(p.fg, Rgb(1, 2, 3));
        assert_eq!(p.bg, Rgb(4, 5, 6));
    }

    /// Without both defaults there is nothing to fade to, whatever else came.
    #[test]
    fn a_palette_needs_both_defaults() {
        let only_bg = Replies {
            bg: Some(Rgb(0, 0, 0)),
            ..Replies::default()
        };
        assert_eq!(only_bg.palette(), None);
        let only_fg = Replies {
            fg: Some(Rgb(0, 0, 0)),
            ..Replies::default()
        };
        assert_eq!(only_fg.palette(), None);
    }

    /// xterm's answer, four hex digits per channel and ST-terminated, which is
    /// the common case.
    #[test]
    fn xterms_replies_are_read_top_eight_bits() {
        let bytes = b"\x1b]10;rgb:eeee/eeee/eeee\x1b\\\x1b]11;rgb:1e1e/1f1f/2020\x1b\\\x1b[0n";
        let r = parse_replies(bytes);
        assert_eq!(r.fg, Some(Rgb(0xee, 0xee, 0xee)));
        assert_eq!(r.bg, Some(Rgb(0x1e, 0x1f, 0x20)));
        assert!(r.dsr, "the terminator was seen");
        assert!(r.complete());
    }

    /// BEL-terminated, two-digit and `#`-spelled answers all mean the same.
    #[test]
    fn every_spelling_of_a_colour_is_accepted() {
        assert_eq!(parse_colour("rgb:ff/80/00"), Some(Rgb(0xff, 0x80, 0x00)));
        assert_eq!(parse_colour("rgb:f/8/0"), Some(Rgb(0xff, 0x88, 0x00)));
        assert_eq!(parse_colour("rgb:fff/888/000"), Some(Rgb(0xff, 0x88, 0x00)));
        assert_eq!(
            parse_colour("rgba:ffff/8080/0000/ffff"),
            Some(Rgb(0xff, 0x80, 0x00))
        );
        assert_eq!(parse_colour("#ff8000"), Some(Rgb(0xff, 0x80, 0x00)));
        assert_eq!(parse_colour("#f80"), Some(Rgb(0xff, 0x88, 0x00)));
        assert_eq!(parse_colour("#ffff80800000"), Some(Rgb(0xff, 0x80, 0x00)));
        for bad in [
            "",
            "rgb:",
            "rgb:gg/00/00",
            "rgb:ff/00",
            "#ff",
            "#fffff",
            "blue",
        ] {
            assert_eq!(parse_colour(bad), None, "{bad:?}");
        }
        let r = parse_replies(b"\x1b]11;rgb:00/00/00\x07");
        assert_eq!(r.bg, Some(Rgb(0, 0, 0)));
        assert!(!r.dsr);
    }

    #[test]
    fn the_sixteen_are_filed_by_their_index() {
        let bytes = b"\x1b]4;0;rgb:0000/0000/0000\x1b\\\x1b]4;15;rgb:ffff/ffff/ffff\x1b\\\x1b]4;16;rgb:1111/1111/1111\x1b\\";
        let r = parse_replies(bytes);
        assert_eq!(r.ansi[0], Some(Rgb(0, 0, 0)));
        assert_eq!(r.ansi[15], Some(Rgb(0xff, 0xff, 0xff)));
        assert!(r.ansi[1..15].iter().all(Option::is_none));
    }

    /// Whatever else is in the queue — a key, a DA1 reply, an OSC nvmux never
    /// asked for — is not a colour, and a DSR reply is not one either.
    #[test]
    fn junk_and_the_terminator_are_never_mistaken_for_colours() {
        let bytes = b"j\x1b[?62;22c\x1b]52;c;aGk=\x1b\\\x1b[0n\x1b]11;rgb:00/00/00\x07";
        let r = parse_replies(bytes);
        assert!(r.dsr);
        assert_eq!(r.fg, None);
        assert_eq!(
            r.bg,
            Some(Rgb(0, 0, 0)),
            "a reply after the junk still counts"
        );
    }

    /// A reply split across two reads is not a reply until the second read.
    #[test]
    fn an_unterminated_reply_waits_for_the_rest() {
        let whole = b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\";
        for cut in 1..whole.len() {
            let r = parse_replies(&whole[..cut]);
            assert_eq!(r.bg, None, "cut at {cut}");
            assert!(!r.complete());
        }
        assert_eq!(parse_replies(whole).bg, Some(Rgb(0x1e, 0x1e, 0x1e)));
    }

    /// Every answer in and no terminator yet is still done: nothing is left to
    /// wait for.
    #[test]
    fn every_answer_in_is_as_good_as_the_terminator() {
        let mut bytes = Vec::from(&b"\x1b]10;rgb:ff/ff/ff\x07\x1b]11;rgb:00/00/00\x07"[..]);
        for n in 0..16 {
            bytes.extend_from_slice(format!("\x1b]4;{n};rgb:11/11/11\x07").as_bytes());
        }
        let r = parse_replies(&bytes);
        assert!(!r.dsr);
        assert!(r.complete());
    }

    /// The request asks for exactly what the parser files, and ends on the
    /// terminator, so the wait is bounded by one round trip.
    #[test]
    fn the_request_ends_on_the_terminator_it_waits_for() {
        let req = request();
        assert!(req.ends_with(b"\x1b[5n"));
        let text = String::from_utf8(req).expect("ascii");
        assert!(text.contains("\x1b]10;?\x1b\\"));
        assert!(text.contains("\x1b]11;?\x1b\\"));
        for n in 0..16 {
            assert!(text.contains(&format!("\x1b]4;{n};?\x1b\\")), "entry {n}");
        }
        assert!(
            !text.contains("\x1b]4;16;"),
            "only the sixteen are asked for"
        );
    }

    /// Before `init`, there is no palette: the tests must never animate.
    #[test]
    fn nothing_is_known_until_the_terminal_is_asked() {
        // `init` is never called from a test, so this holds for the whole run.
        assert_eq!(get(), None);
    }
}
