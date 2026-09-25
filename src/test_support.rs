//! Helpers shared by the unit tests across modules: scratch paths, a bounded
//! wait, byte-slice searches, and the PTY, palette and shadow fixtures the
//! overlay tests share. The screen tests have their own in
//! [`crate::ui::test_support`].

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::palette::{Palette, Rgb};
use crate::pty::PtySize;
use crate::shadow::Shadow;

/// A scratch path under the temp directory, with anything left there by an
/// earlier run removed. Nothing is created: a test that wants the path absent
/// — so a script can create it, say — takes this as it is.
///
/// Every unit test in the crate shares one pid and they run in parallel, so
/// `tag` must be unique across the crate: callers qualify it with their module
/// (`proc-listing`, `shell-listing`). The prefix is kept short because the
/// socket tests must stay under [`crate::paths::MAX_SOCK_PATH`], and macOS's
/// temp dir is not short.
pub(crate) fn scratch_path(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("nvmux-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    let _ = std::fs::remove_file(&p);
    p
}

/// [`scratch_path`], created as an empty directory.
pub(crate) fn scratch_dir(tag: &str) -> PathBuf {
    let p = scratch_path(tag);
    std::fs::create_dir_all(&p).expect("scratch dir");
    p
}

/// [`scratch_path`] for a socket: `<tag>.sock`, not bound to anything.
pub(crate) fn scratch_sock(tag: &str) -> PathBuf {
    scratch_path(&format!("{tag}.sock"))
}

/// Wait up to `within` for `cond` to hold, polling gently. Returns whether it
/// did, so a caller can assert with its own message.
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

/// Whether `needle` occurs anywhere in `haystack`.
pub(crate) fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Where `needle` starts in `haystack`, which is called `name` in the panic
/// when it is not there at all.
pub(crate) fn position(haystack: &[u8], needle: &[u8], name: &str) -> usize {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap_or_else(|| panic!("{name} dropped {needle:?}"))
}

/// A terminal of this many columns and rows, with no pixel size.
pub(crate) fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// A screen with room for the prefix's rows or the attach notice whole, which
/// most of the overlay tests want.
pub(crate) fn big() -> PtySize {
    size(80, 24)
}

/// A terminal that said its text is light grey on black. No palette is ever
/// installed under test, so this is the only way to reach the restoring and
/// dissolving paths — which is why [`crate::fade::Dissolve`] takes one, and
/// the prefix's bar is handed a `Dissolve` in its tests.
pub(crate) fn palette() -> Palette {
    Palette {
        fg: Rgb(200, 200, 200),
        bg: Rgb(0, 0, 0),
        ansi: [Rgb(0, 0, 0); 16],
    }
}

/// A shadow with something on it to put back or dissolve into: every cell an
/// `x`, which is in none of the bar's words, none of the box's glyphs and no
/// session name the tests use, so a frame shows the screen exactly when it
/// has one in it.
pub(crate) fn screen(rows: u16, cols: u16) -> Shadow {
    let mut shadow = Shadow::new(rows, cols);
    for row in 1..=rows {
        shadow.feed(format!("\x1b[{row};1H{}", "x".repeat(usize::from(cols))).as_bytes());
    }
    shadow
}
