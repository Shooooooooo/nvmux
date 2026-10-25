//! The terminal's colours (OSC 10, 11 and 4), queried once at startup for the
//! fade ([`crate::fade`]), the picker's effects and nvmux's own client.
//!
//! Replies arrive on stdin, and one reaching the picker would be read as keys.
//! So the request ends with a DSR (`ESC [ 5 n`), which every terminal answers
//! after its OSC replies, ending the wait after one round trip, and the cap
//! [`QUERY_CAP`] is generous. The read would swallow type-ahead (no
//! `TIOCSTI` to give it back), so the query is skipped if input is waiting.
//!
//! Terminals that do not answer OSC 4 (tmux, Terminal.app, Windows Terminal)
//! get xterm's defaults for the 16.

use std::io::IsTerminal;
use std::os::fd::AsRawFd;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Per channel; `t` is clamped to `0..=1`, where 0 is `self`.
    pub fn lerp(self, to: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let mix = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
        Rgb(mix(self.0, to.0), mix(self.1, to.1), mix(self.2, to.2))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub fg: Rgb,
    pub bg: Rgb,
    pub ansi: [Rgb; 16],
}

impl Palette {
    /// For a child's environment (see [`crate::client::PALETTE_ENV`]): fg, bg
    /// and the 16, each `rrggbb`, colon-separated.
    pub fn to_env(&self) -> String {
        std::iter::once(&self.fg)
            .chain(std::iter::once(&self.bg))
            .chain(self.ansi.iter())
            .map(|Rgb(r, g, b)| format!("{r:02x}{g:02x}{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Inverse of [`Palette::to_env`].
    pub fn from_env(text: &str) -> Option<Palette> {
        let colours: Vec<Rgb> = text
            .split(':')
            .map(|hex| {
                if hex.len() != 6 {
                    return None;
                }
                let n = u32::from_str_radix(hex, 16).ok()?;
                Some(Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
            })
            .collect::<Option<_>>()?;
        let [fg, bg, ansi @ ..] = colours.as_slice() else {
            return None;
        };
        Some(Palette {
            fg: *fg,
            bg: *bg,
            ansi: ansi.try_into().ok()?,
        })
    }

    /// The terminal's own for 0–15, else xterm's cube and grey ramp, which all
    /// terminals share.
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

pub(crate) const XTERM_ANSI: [Rgb; 16] = [
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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Replies {
    fg: Option<Rgb>,
    bg: Option<Rgb>,
    ansi: [Option<Rgb>; 16],
    /// No more replies are coming.
    dsr: bool,
}

impl Replies {
    fn complete(&self) -> bool {
        self.dsr
            || (self.fg.is_some() && self.bg.is_some() && self.ansi.iter().all(Option::is_some))
    }

    /// `None` unless fg and bg were answered.
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

const DSR_REPLY: &[u8] = b"\x1b[0n";

/// Skips anything that is not a reply. A reply cut off at the end is ignored;
/// the caller retries with more bytes.
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

/// `bytes` starts just past `ESC ]`. Returns the payload and the index past
/// its terminator; `None` if unterminated.
fn osc_payload(bytes: &[u8]) -> Option<(&[u8], usize)> {
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            0x07 => return Some((&bytes[..i], i + 1)),
            0x1b if bytes.get(i + 1) == Some(&b'\\') => return Some((&bytes[..i], i + 2)),
            // A trailing ESC may start an ST: wait. Any other ESC is stray:
            // end the OSC there so what follows is still parsed.
            0x1b if i + 1 == bytes.len() => return None,
            0x1b => return Some((&bytes[..i], i)),
            _ => {}
        }
    }
    None
}

/// `rgb:` or `rgba:` with 1–4 hex digits per channel, or `#` forms.
pub(crate) fn parse_colour(spec: &str) -> Option<Rgb> {
    let spec = spec.trim();
    if let Some(hex) = spec.strip_prefix('#') {
        // Before slicing by byte: a multibyte char would be split.
        if hex.is_empty()
            || hex.len() % 3 != 0
            || hex.len() > 12
            || !hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
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

/// Scaled so all-ones is 255: `f` and `ffff` are full, `1e1e` is `1e`.
fn scale(digits: &str) -> Option<u8> {
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    let max = (1u32 << (4 * digits.len())) - 1;
    Some(((value * 255 + max / 2) / max) as u8)
}

fn request() -> Vec<u8> {
    let mut out = Vec::from(&b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\"[..]);
    for n in 0..16 {
        out.extend_from_slice(format!("\x1b]4;{n};?\x1b\\").as_bytes());
    }
    out.extend_from_slice(b"\x1b[5n");
    out
}

/// Long on purpose: only a terminal answering nothing hits it, and a late
/// reply would be typed into the picker.
const QUERY_CAP: Duration = Duration::from_millis(1500);

/// `None` with no terminal, a key already waiting, or no answer.
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
    // Checked in raw mode: before it, typed keys sit in the canonical buffer,
    // invisible to `poll`. Leaving raw mode does not flush them.
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

fn readable_now(fd: std::os::fd::RawFd) -> bool {
    let mut p = [crate::pty::pollfd(fd)];
    let n = unsafe { libc::poll(p.as_mut_ptr(), 1, 0) };
    n > 0 && crate::pty::ready(&p[0])
}

static PALETTE: OnceLock<Option<Palette>> = OnceLock::new();

/// A second call warns and keeps the first.
pub fn init(palette: Option<Palette>) {
    if PALETTE.set(palette).is_err() {
        tracing::warn!("palette initialised more than once; keeping the first");
    }
}

/// `None` before [`init`], so nothing animates under `cargo test`.
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

    #[test]
    fn a_palette_survives_the_environment() {
        let mut p = palette();
        p.ansi[3] = Rgb(1, 2, 3);
        let text = p.to_env();
        assert_eq!(text.split(':').count(), 18);
        assert_eq!(Palette::from_env(&text), Some(p));
        assert_eq!(Palette::from_env(""), None);
        assert_eq!(Palette::from_env("eeeeee:101010"), None, "no sixteen");
        assert_eq!(Palette::from_env(&text.replace('e', "x")), None);
        assert_eq!(Palette::from_env(&format!("{text}:000000")), None);
    }

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

    #[test]
    fn xterms_replies_are_read_top_eight_bits() {
        let bytes = b"\x1b]10;rgb:eeee/eeee/eeee\x1b\\\x1b]11;rgb:1e1e/1f1f/2020\x1b\\\x1b[0n";
        let r = parse_replies(bytes);
        assert_eq!(r.fg, Some(Rgb(0xee, 0xee, 0xee)));
        assert_eq!(r.bg, Some(Rgb(0x1e, 0x1f, 0x20)));
        assert!(r.dsr, "the terminator was seen");
        assert!(r.complete());
    }

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
            // Byte lengths a multiple of 3, so a slice would split a char.
            "#èa",
            "#€abc",
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

    #[test]
    fn nothing_is_known_until_the_terminal_is_asked() {
        // `init` is never called from a test, so this holds for the whole run.
        assert_eq!(get(), None);
    }
}
