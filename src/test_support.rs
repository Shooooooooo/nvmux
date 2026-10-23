//! Helpers shared by unit tests across modules. The screen tests have their
//! own in [`crate::ui::test_support`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::palette::{Palette, Rgb};
use crate::pty::PtySize;
use crate::shadow::Shadow;

/// A scratch path under the temp dir, cleared but not created. Unit tests
/// share one pid and run in parallel, so `tag` must be unique across the
/// crate. The prefix is short so sockets stay under
/// [`crate::paths::MAX_SOCK_PATH`] in macOS's long temp dir.
pub(crate) fn scratch_path(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("nvmux-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    let _ = std::fs::remove_file(&p);
    p
}

pub(crate) fn scratch_dir(tag: &str) -> PathBuf {
    let p = scratch_path(tag);
    std::fs::create_dir_all(&p).expect("scratch dir");
    p
}

pub(crate) fn scratch_sock(tag: &str) -> PathBuf {
    scratch_path(&format!("{tag}.sock"))
}

/// Run `f`, which panics on purpose, with the panic hook silenced. Callers
/// are serialized, as two tests swapping the global hook could leave it
/// silenced for good.
pub(crate) fn quietly<T>(f: impl FnOnce() -> T) -> T {
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _turn = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    // Restore the hook before re-raising, so a real failure is reported.
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    std::panic::set_hook(hook);
    out.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Leave a socket at `path` that refuses every connection, like a dead
/// server's. Not a bound-and-dropped listener: a child forked by a parallel
/// test could inherit it (on macOS for its whole life, as std cannot create
/// it close-on-exec atomically) and accept connects. A datagram socket never
/// listens.
pub(crate) fn dead_socket(path: &Path) {
    drop(std::os::unix::net::UnixDatagram::bind(path).expect("bind"));
}

/// Poll `cond` for up to `within`; returns whether it held.
pub(crate) fn wait_until(within: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

pub(crate) fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Where `needle` starts in `haystack`; panics naming `name` if absent.
pub(crate) fn position(haystack: &[u8], needle: &[u8], name: &str) -> usize {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap_or_else(|| panic!("{name} dropped {needle:?}"))
}

pub(crate) fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Big enough for a whole hint bar or attach notice.
pub(crate) fn big() -> PtySize {
    size(80, 24)
}

/// Light grey on black. No palette is installed under test, hence
/// `hint::Bar::with_palette` and [`crate::fade::Dissolve`] take one.
pub(crate) fn palette() -> Palette {
    Palette {
        fg: Rgb(200, 200, 200),
        bg: Rgb(0, 0, 0),
        ansi: [Rgb(0, 0, 0); 16],
    }
}

/// A shadow with every cell `x`, a letter in no bar word, box glyph or test
/// session name, so a frame shows the screen exactly when it contains one.
pub(crate) fn screen(rows: u16, cols: u16) -> Shadow {
    let mut shadow = Shadow::new(rows, cols);
    for row in 1..=rows {
        shadow.feed(format!("\x1b[{row};1H{}", "x".repeat(usize::from(cols))).as_bytes());
    }
    shadow
}

/// Seeded client output aimed at [`crate::ledger::OscCap`], with read sizes:
/// text, escapes, DCS/APC, stray `ESC`s, `ESC` + C0 + `]` (vte still opens
/// an OSC), and OSCs around [`crate::ledger::OSC_SHOWN`] in length, ended
/// every way vte accepts or left open.
pub(crate) fn osc_noise(seed: u64) -> (Vec<u8>, Vec<usize>) {
    // xorshift64*, as in `ui::starfield`.
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut below = move |n: usize| {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        (state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 32) as usize % n
    };
    let shown = crate::ledger::OSC_SHOWN;
    let mut out = Vec::new();
    for _ in 0..1 + below(12) {
        let pick =
            |below: &mut dyn FnMut(usize) -> usize, of: &[&'static [u8]]| of[below(of.len())];
        match below(10) {
            0 | 1 => out.extend_from_slice(pick(
                &mut below,
                &[
                    b"some text ",
                    "é─漢😀".as_bytes(),
                    b"\r\n",
                    b"\xe2\x94",
                    b"\xc3",
                    b"\x9c\x9d",
                ],
            )),
            2 => out.extend_from_slice(pick(
                &mut below,
                &[
                    b"\x1b[1;31m",
                    b"\x1b[3;5H",
                    b"\x1b[?1000h",
                    b"\x1b[?1002l",
                    b"\x1b[?2004h",
                    b"\x1b[?1049h",
                    b"\x1b[5 q",
                    b"\x1b[>3u",
                    b"\x1b[<u",
                    b"\x1b[>4;2m",
                ],
            )),
            3 => out.extend_from_slice(pick(
                &mut below,
                &[
                    b"\x1bP1$r0m\x1b\\",
                    b"\x1b_Gf=24;AAAA\x1b\\",
                    b"\x1bPq#0",
                    b"\x1b_x",
                ],
            )),
            4 => out.extend_from_slice(pick(
                &mut below,
                &[b"\x1b", b"\x1b\x1b", b"\x1b=", b"\x1b>", b"\x1b\n]0;t\x07"],
            )),
            _ => {
                out.extend_from_slice(pick(
                    &mut below,
                    &[
                        b"\x1b]0;",
                        b"\x1b]2;",
                        b"\x1b]1;",
                        b"\x1b]12;",
                        b"\x1b]112",
                        b"\x1b]52;c;",
                        b"\x1b]8;;",
                        b"\x1b]",
                    ],
                ));
                let len = match below(4) {
                    0 => below(40),
                    1 => shown / 4 - 8 + below(16),
                    2 => shown - 8 + below(16),
                    _ => shown + below(4 * shown),
                };
                for _ in 0..len {
                    out.push(match below(64) {
                        0 => b';',
                        1 => 0x01,
                        2 => b'\n',
                        3 => 0x9c,
                        4 => 0xc3,
                        5 => 0xa9,
                        6 => 0x7f,
                        n => b'A' + (n % 26) as u8,
                    });
                }
                out.extend_from_slice(pick(
                    &mut below,
                    &[b"\x07", b"\x1b\\", b"\x18", b"\x1a", b"\x1b", b"\x1b]", b""],
                ));
            }
        }
    }
    let mut reads = Vec::new();
    let mut left = out.len();
    while left > 0 {
        let read = match below(4) {
            0 => 1 + below(3),
            1 => 1 + below(64),
            2 => 4095,
            _ => 1 + below(3 * shown),
        };
        reads.push(read.min(left));
        left -= read.min(left);
    }
    (out, reads)
}
