//! Dip-to-black transitions between screens.
//!
//! nvmux hard-cuts between its screens. This module smooths those cuts with a
//! short dip to black: the outgoing screen dissolves to solid black, the seam is
//! held black across the hand-off, and the incoming screen dissolves back up.
//!
//! # Why a dither dissolve rather than a brightness fade
//!
//! Two of the three screen kinds are ratatui buffers ([`crate::ui`]), but an
//! attached session is raw Neovim: nvmux is a byte proxy and never owns those
//! cells (see [`crate::pty`]), so there is nothing to dim. The one effect that
//! works on *both* worlds without a cell model is spatial: an ordered-dither
//! dissolve that turns an increasing fraction of cells black. A per-cell Bayer
//! threshold makes the pattern deterministic (no RNG, so a resize mid-fade just
//! redraws at the new size) and *monotonic* in coverage — a cell only ever flips
//! from visible to black — which is what lets the raw writer emit each newly
//! black cell exactly once instead of repainting the whole screen every frame.
//!
//! # Colour, and `NO_COLOR`
//!
//! The picker and its siblings deliberately set no colour (see
//! [`crate::ui::draw`]); this module is the one place that paints an explicit
//! black, and only transiently, on top of a finished frame — never inside
//! `draw::draw`, so that invariant and its tests stand. The whole effect is
//! gated on [`enabled`]: with `NO_COLOR` set every entry point is a no-op and the
//! transitions are exactly what they were before. That gate is also where the
//! config file hooks in to tune or disable the fade (see [`crate::settings`]);
//! the constants below are its defaults.

use std::io::{self, Write};
use std::thread;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::{DefaultTerminal, Frame};

use crate::term;

/// Steps per direction. Eight is enough to read as motion without dragging.
pub const FRAMES: usize = 8;

/// Delay between frames — `FRAMES * FRAME_DELAY` is one direction's duration.
pub const FRAME_DELAY: Duration = Duration::from_millis(12);

/// How long the screen is held fully black across a hand-off, covering the
/// client swap or the alt-screen crossing so neither shows through.
pub const HOLD: Duration = Duration::from_millis(30);

/// Whether the quick `<prefix> ?` / `<prefix> c` / picker-peek excursions fade too.
/// Off makes those snappier at the cost of consistency.
pub const EXCURSIONS: bool = true;

/// Dissolve the raw session out to black cell by cell. Off makes the raw path an
/// instant blackout instead — cheaper over a slow link, but a hard cut.
pub const RAW_FADE_DISSOLVE: bool = true;

/// Pure black, written the same way on both paths (truecolor `48;2;0;0;0`) so the
/// picker's dip and a session's dip are the same shade.
const BLACK: Color = Color::Rgb(0, 0, 0);

/// Set the raw background to [`BLACK`].
const SET_BLACK_BG: &[u8] = b"\x1b[48;2;0;0;0m";
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";
/// Clear to the current background (black, once [`SET_BLACK_BG`] is in effect)
/// and home the cursor.
const CLEAR_HOME: &[u8] = b"\x1b[2J\x1b[H";

/// A 4x4 ordered-dither matrix, values `0..16`. Sixteen thresholds is finer than
/// the eight frames need, so the dissolve never looks banded.
const BAYER_N: usize = 4;
const BAYER: [[u8; BAYER_N]; BAYER_N] =
    [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

/// The coverage at which a cell turns black, in `(0, 1)`. The `+ 0.5` centres
/// each level in its band so `coverage == 0.0` blackens nothing and
/// `coverage == 1.0` blackens everything.
fn threshold(x: u16, y: u16) -> f32 {
    let level = BAYER[y as usize % BAYER_N][x as usize % BAYER_N];
    (level as f32 + 0.5) / (BAYER_N * BAYER_N) as f32
}

/// Whether the cell at `(x, y)` is black at this `coverage`. Monotonic in
/// `coverage` for any fixed cell.
pub fn cell_is_black(x: u16, y: u16, coverage: f32) -> bool {
    coverage >= threshold(x, y)
}

/// Whether transitions are animated at all. `false` restores the pre-fade
/// behaviour exactly. Two things can turn it off: `fade.enabled = false` in the
/// config file, or `NO_COLOR`.
///
/// `NO_COLOR` wins over the config because the effect paints an explicit colour
/// and the ratatui screens force colour output on (see [`crate::ui`]), so
/// honouring the request means never running the effect, whatever the config says.
pub fn enabled() -> bool {
    is_enabled(
        crate::settings::get().fade.enabled,
        std::env::var_os("NO_COLOR").is_some(),
    )
}

/// The gate itself, factored out so the precedence rule is testable without the
/// process globals [`enabled`] reads.
fn is_enabled(config_enabled: bool, no_color: bool) -> bool {
    config_enabled && !no_color
}

/// Whether the quick excursions — [`crate::ui::help`], the create prompt, a
/// picker peek — fade too. Config-driven; the [`EXCURSIONS`] constant is its
/// default.
pub fn excursions() -> bool {
    crate::settings::get().fade.excursions
}

/// Blacken every cell of `area` that is black at `coverage`, in place. Applied to
/// a finished frame, so `draw::draw` itself stays colourless.
pub fn overlay(buf: &mut Buffer, area: Rect, coverage: f32) {
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if cell_is_black(x, y, coverage) {
                let cell = &mut buf[(x, y)];
                cell.reset();
                cell.set_bg(BLACK);
                cell.set_fg(BLACK);
            }
        }
    }
}

/// Overlay a fully black frame.
pub fn paint_black(buf: &mut Buffer, area: Rect) {
    overlay(buf, area, 1.0);
}

// --- ratatui-side drivers (picker / help / prompt) -------------------------

/// Draw one full-black frame, so a ratatui screen that has just entered the
/// alternate screen does not show its blank buffer through the slow work (a
/// session listing) that precedes its first real draw.
pub fn prime_black(terminal: &mut DefaultTerminal) -> io::Result<()> {
    if !enabled() {
        return Ok(());
    }
    write_all(SYNC_BEGIN)?;
    terminal.draw(|f| {
        let area = f.area();
        paint_black(f.buffer_mut(), area);
    })?;
    write_all(SYNC_END)
}

/// Dissolve a ratatui screen up from black. Starts on a full-black frame (so it
/// is self-contained even without a preceding [`prime_black`]) and ends on the
/// clean content.
pub fn fade_in_ratatui<F>(terminal: &mut DefaultTerminal, mut draw: F) -> io::Result<()>
where
    F: FnMut(&mut Frame),
{
    if !enabled() {
        return Ok(());
    }
    let cfg = crate::settings::get().fade;
    let delay = Duration::from_millis(cfg.frame_delay_ms);
    for step in (0..=cfg.frames).rev() {
        let coverage = step as f32 / cfg.frames as f32;
        draw_overlaid(terminal, &mut draw, coverage)?;
        if step != 0 {
            thread::sleep(delay);
        }
    }
    Ok(())
}

/// Dissolve a ratatui screen out to black, ending fully black and held for
/// [`HOLD`] so the hand-off that follows never flashes the old content.
pub fn fade_out_ratatui<F>(terminal: &mut DefaultTerminal, mut draw: F) -> io::Result<()>
where
    F: FnMut(&mut Frame),
{
    if !enabled() {
        return Ok(());
    }
    let cfg = crate::settings::get().fade;
    let delay = Duration::from_millis(cfg.frame_delay_ms);
    for step in 1..=cfg.frames {
        let coverage = step as f32 / cfg.frames as f32;
        draw_overlaid(terminal, &mut draw, coverage)?;
        if step != cfg.frames {
            thread::sleep(delay);
        }
    }
    thread::sleep(Duration::from_millis(cfg.hold_ms));
    Ok(())
}

/// One frame: the caller's content, then the black overlay on top, wrapped in a
/// synchronized update so the terminal never presents a half-composited frame.
fn draw_overlaid<F>(terminal: &mut DefaultTerminal, draw: &mut F, coverage: f32) -> io::Result<()>
where
    F: FnMut(&mut Frame),
{
    write_all(SYNC_BEGIN)?;
    terminal.draw(|f| {
        draw(&mut *f);
        let area = f.area();
        overlay(f.buffer_mut(), area, coverage);
    })?;
    write_all(SYNC_END)
}

/// Leave a ratatui screen and hand a **black** primary screen to the session
/// that follows. Replaces a bare `ratatui::try_restore()` on the picker's attach
/// path: the alt-screen leave and the primary black-fill go out in one
/// synchronized batch, so the stale primary (a shell prompt, the previous
/// session) is never presented during the client spawn that comes next.
pub fn leave_ratatui_to_black() -> io::Result<()> {
    if !enabled() {
        ratatui::try_restore()?;
        return Ok(());
    }
    write_all(SYNC_BEGIN)?;
    // Errors still propagate; the sync span is closed on the way out either way.
    let restored = ratatui::try_restore();
    let mut out = io::stdout().lock();
    out.write_all(HIDE_CURSOR)?;
    out.write_all(SET_BLACK_BG)?;
    out.write_all(CLEAR_HOME)?;
    out.write_all(SYNC_END)?;
    out.flush()?;
    restored
}

// --- raw-side drivers (attached session) -----------------------------------

/// Leave the alternate screen into a **black** primary screen for a session.
///
/// The fade-enabled counterpart of [`crate::term::leave_alt_screen_and_clear`],
/// which stays the `NO_COLOR` fallback. Painted before raw mode and before any
/// forced repaint, so Neovim redraws its grid on top of the black.
pub fn enter_session_black() {
    if !enabled() {
        term::leave_alt_screen_and_clear();
        return;
    }
    let mut out = io::stdout().lock();
    let _ = out.write_all(SYNC_BEGIN);
    // ?1049l leaves the alternate screen; then hide the cursor and clear to
    // black so the session starts from a known, dark state.
    let _ = out.write_all(b"\x1b[?1049l");
    let _ = out.write_all(HIDE_CURSOR);
    let _ = out.write_all(SET_BLACK_BG);
    let _ = out.write_all(CLEAR_HOME);
    let _ = out.write_all(SYNC_END);
    let _ = out.flush();
}

/// Instantly black the current (primary) screen. Used when a child has already
/// exited and emitted its own restore — there is no live frame worth dissolving,
/// and a competing dissolve could race the child's teardown.
pub fn black_now() {
    if !enabled() {
        return;
    }
    let mut out = io::stdout().lock();
    let _ = out.write_all(SYNC_BEGIN);
    let _ = out.write_all(HIDE_CURSOR);
    let _ = out.write_all(SET_BLACK_BG);
    let _ = out.write_all(CLEAR_HOME);
    let _ = out.write_all(SYNC_END);
    let _ = out.flush();
}

/// Dissolve the raw session screen out to black, then hold [`HOLD`].
///
/// Writes only the cells that turn black on each frame (a delta — the threshold
/// is monotonic, so each cell is written once across the whole dissolve),
/// batching contiguous cells per row into one cursor move and a run of spaces.
/// The total is on the order of a single full-screen repaint, which nvmux
/// already emits on every resize and resume.
pub fn fade_out_raw() -> io::Result<()> {
    if !enabled() {
        return Ok(());
    }
    let cfg = crate::settings::get().fade;
    let delay = Duration::from_millis(cfg.frame_delay_ms);
    let mut out = io::stdout().lock();
    out.write_all(HIDE_CURSOR)?;
    out.write_all(SET_BLACK_BG)?;

    if cfg.raw_dissolve {
        let mut prev = 0.0f32;
        for step in 1..=cfg.frames {
            let coverage = step as f32 / cfg.frames as f32;
            let size = term::terminal_size();
            out.write_all(&delta_frame(size.cols, size.rows, prev, coverage))?;
            out.flush()?;
            prev = coverage;
            if step != cfg.frames {
                thread::sleep(delay);
            }
        }
    }

    // A guaranteed full black, regardless of any dither leftover or a resize
    // during the dissolve.
    out.write_all(SYNC_BEGIN)?;
    out.write_all(CLEAR_HOME)?;
    out.write_all(SYNC_END)?;
    out.flush()?;
    thread::sleep(Duration::from_millis(cfg.hold_ms));
    Ok(())
}

/// The escape sequence for one dissolve frame: every cell whose threshold falls
/// in `(prev, cur]`, positioned and filled with a space (black, from the bg set
/// by the caller), inside a synchronized update.
fn delta_frame(cols: u16, rows: u16, prev: f32, cur: f32) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(SYNC_BEGIN);
    for y in 0..rows {
        let mut x = 0;
        while x < cols {
            let t = threshold(x, y);
            if t > prev && t <= cur {
                let start = x;
                while x < cols {
                    let tt = threshold(x, y);
                    if tt > prev && tt <= cur {
                        x += 1;
                    } else {
                        break;
                    }
                }
                // 1-based cursor position, then a run of spaces for the run.
                let _ = write!(buf, "\x1b[{};{}H", y + 1, start + 1);
                buf.extend(std::iter::repeat_n(b' ', (x - start) as usize));
            } else {
                x += 1;
            }
        }
    }
    buf.extend_from_slice(SYNC_END);
    buf
}

// --- small shared helpers --------------------------------------------------

/// Re-show the cursor if fades are on. The dissolve and black-bridge hide it;
/// paths that do not hand off to an owner that re-shows it (Neovim on paint,
/// ratatui on its next restore) call this so the shell never inherits a hidden
/// cursor.
pub fn show_cursor_if_enabled() {
    if !enabled() {
        return;
    }
    let mut out = io::stdout().lock();
    let _ = out.write_all(SHOW_CURSOR);
    let _ = out.flush();
}

/// Write to stdout and flush, so an escape sequence lands before the next.
fn write_all(bytes: &[u8]) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(bytes)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the raw delta writer relies on: a cell that is black at some
    /// coverage stays black at every higher coverage, so it is written once.
    #[test]
    fn blackness_is_monotonic_in_coverage() {
        for y in 0..8u16 {
            for x in 0..8u16 {
                let mut was_black = false;
                let mut c = 0.0;
                while c <= 1.0 {
                    let black = cell_is_black(x, y, c);
                    assert!(
                        !was_black || black,
                        "cell ({x},{y}) un-blacked from coverage below {c}"
                    );
                    was_black |= black;
                    c += 0.05;
                }
            }
        }
    }

    #[test]
    fn coverage_zero_blackens_nothing_and_one_blackens_everything() {
        for y in 0..16u16 {
            for x in 0..16u16 {
                assert!(!cell_is_black(x, y, 0.0), "cell ({x},{y}) black at 0.0");
                assert!(cell_is_black(x, y, 1.0), "cell ({x},{y}) not black at 1.0");
            }
        }
    }

    /// The overlay is a post-pass: at zero coverage it must not touch a single
    /// cell, so a faded-in frame's final state is byte-identical to a plain draw.
    #[test]
    fn overlay_at_zero_changes_nothing() {
        let area = Rect::new(0, 0, 20, 6);
        let mut buf = Buffer::empty(area);
        buf.set_string(1, 1, "hello", ratatui::style::Style::default());
        let before = buf.clone();
        overlay(&mut buf, area, 0.0);
        assert_eq!(buf, before);
    }

    #[test]
    fn overlay_at_one_is_all_black_spaces() {
        let area = Rect::new(0, 0, 20, 6);
        let mut buf = Buffer::empty(area);
        buf.set_string(1, 1, "hello", ratatui::style::Style::default());
        overlay(&mut buf, area, 1.0);
        for y in 0..area.height {
            for x in 0..area.width {
                let cell = &buf[(x, y)];
                assert_eq!(cell.symbol(), " ", "cell ({x},{y}) not blanked");
                assert_eq!(cell.bg, BLACK);
                assert_eq!(cell.fg, BLACK);
                assert!(cell.modifier.is_empty());
            }
        }
    }

    /// The black fraction tracks coverage, which pins the schedule: at half
    /// coverage about half the cells are black.
    #[test]
    fn black_fraction_tracks_coverage() {
        let (w, h) = (64u16, 64u16);
        for &(coverage, want) in &[(0.25, 0.25), (0.5, 0.5), (0.75, 0.75)] {
            let mut black = 0;
            for y in 0..h {
                for x in 0..w {
                    if cell_is_black(x, y, coverage) {
                        black += 1;
                    }
                }
            }
            let frac = black as f32 / (w * h) as f32;
            assert!(
                (frac - want).abs() < 0.05,
                "coverage {coverage}: {frac} black, wanted ~{want}"
            );
        }
    }

    /// The dissolve frames partition the screen: every cell is blacked exactly
    /// once across the run, and the whole screen is black by the last frame.
    #[test]
    fn dissolve_frames_black_each_cell_exactly_once() {
        let (w, h) = (40u16, 12u16);
        let mut counts = vec![0u32; (w as usize) * (h as usize)];
        let mut prev = 0.0f32;
        for step in 1..=FRAMES {
            let cur = step as f32 / FRAMES as f32;
            for y in 0..h {
                for x in 0..w {
                    let t = threshold(x, y);
                    if t > prev && t <= cur {
                        counts[(y as usize) * (w as usize) + x as usize] += 1;
                    }
                }
            }
            prev = cur;
        }
        assert!(
            counts.iter().all(|&c| c == 1),
            "some cell was blacked zero or multiple times"
        );
    }

    /// A frame only ever describes newly-black cells, so a delta never repaints a
    /// cell an earlier frame already blacked.
    #[test]
    fn delta_frame_is_empty_when_nothing_changes() {
        // Between two coverages with no Bayer level in the gap, nothing new turns
        // black, so the frame carries only its sync markers.
        let f = delta_frame(40, 12, 0.99, 1.0);
        let overhead = SYNC_BEGIN.len() + SYNC_END.len();
        assert_eq!(f.len(), overhead, "expected no cell writes, got {f:?}");
    }

    #[test]
    fn no_color_disables_the_effect() {
        use std::sync::Mutex;
        // Env is process-global; this is the only test that touches NO_COLOR.
        static GUARD: Mutex<()> = Mutex::new(());
        let _lock = GUARD.lock().unwrap();

        let had = std::env::var_os("NO_COLOR");
        std::env::remove_var("NO_COLOR");
        assert!(enabled(), "should be enabled without NO_COLOR");
        std::env::set_var("NO_COLOR", "1");
        assert!(!enabled(), "NO_COLOR must disable the effect");
        match had {
            Some(v) => std::env::set_var("NO_COLOR", v),
            None => std::env::remove_var("NO_COLOR"),
        }
    }

    /// The gate's precedence, without touching any global: the config can turn
    /// the effect off, and `NO_COLOR` turns it off even when the config wants it.
    #[test]
    fn the_config_toggle_and_no_color_both_gate_the_effect() {
        assert!(is_enabled(true, false), "on by default");
        assert!(
            !is_enabled(true, true),
            "NO_COLOR wins over an enabled config"
        );
        assert!(!is_enabled(false, false), "config can disable it");
        assert!(!is_enabled(false, true), "off is off");
    }
}
