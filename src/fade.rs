//! The dissolve between screens: the outgoing screen fades into the
//! terminal's background colour and the incoming one fades up out of it, each
//! visible cell's colour interpolated over a few frames.
//!
//! nvmux's own ratatui screens are faded by a post-pass over the finished
//! frame ([`apply`]), so the screens never set colours themselves and the last
//! frame of a fade in is byte-identical to a plain draw. Each cell fades from
//! the colour it is drawn in: the terminal's foreground, or under a theme
//! ([`crate::theme`]) the ink on full-strength cells.
//!
//! An attached session is faded from the shadow grid ([`crate::shadow`]) by
//! [`fade_out_session`] and [`fade_in_session`]. For a fade in, the relay
//! holds back the session's first paint until it settles (see `pty::Hold`); a
//! resumed session fades in from the screen the shadow still holds.
//!
//! Both ends must be real colours, so the fade needs the terminal's answer to
//! [`crate::palette::query`]. `NO_COLOR`, `[effects.fade] enabled` and
//! `[effects] enabled` also turn it off; [`enabled`] is the one gate.
//!
//! [`Schedule`] paces fades by the clock: a slow terminal skips frames rather
//! than stretching the fade, and each fade ends exactly on its last value.

use std::io::{self, Write};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::{DefaultTerminal, Frame};

use crate::palette::{self, Palette, Rgb};
use crate::shadow::{Cursor, Kept, Over, Shadow};

/// A synchronized update: the terminal presents nothing between these.
/// Opening one inside another is harmless, which matters where a relay stopped
/// inside Neovim's.
pub const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
pub const SYNC_END: &[u8] = b"\x1b[?2026l";

/// Frame interval, about 60 fps. Also paces [`crate::announce`], whose fade
/// the relay loop drives a frame at a time.
pub const FRAME: Duration = Duration::from_millis(16);

/// Whether config and environment allow a fade, ignoring the terminal's
/// answer (`main` uses this to decide whether to query it).
pub fn configured() -> bool {
    is_configured(
        crate::config::get().effects.fade_enabled(),
        std::env::var_os("NO_COLOR").is_some(),
    )
}

/// `NO_COLOR` wins over the config: the effect paints explicit colours.
fn is_configured(config_enabled: bool, no_color: bool) -> bool {
    config_enabled && !no_color
}

/// Whether transitions animate: configured, and the terminal reported its
/// colours.
pub fn enabled() -> bool {
    active().is_some()
}

/// The palette to fade with, if a fade can run.
fn active() -> Option<&'static Palette> {
    if configured() {
        palette::get()
    } else {
        None
    }
}

/// The terminal's colours, for dimming a session outside a fade (the settings
/// over a session use them even with the fade off). `None` under `NO_COLOR`
/// or if the terminal never said.
pub fn colours() -> Option<&'static Palette> {
    if std::env::var_os("NO_COLOR").is_some() {
        None
    } else {
        palette::get()
    }
}

/// Whether an attached session is watched so it can dissolve out.
pub fn session() -> bool {
    crate::config::get().effects.fade.session
}

/// Half of `effects.fade.duration_ms`, which covers both directions.
fn one_way() -> Duration {
    crate::config::get().effects.fade.one_way()
}

/// What a caller painting its own fade frames needs. [`crate::announce`]
/// drives a [`Schedule`] from the relay's `poll` with this, as it cannot sleep
/// between frames. Plain data copied once, so tests can build every state (no
/// palette is installed under test).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dissolve {
    /// One direction's length: half of `fade.duration_ms`.
    pub one_way: Duration,
    /// The terminal's colours: what a fully dissolved glyph is, and how a
    /// session's default foreground and indexed colours resolve.
    pub palette: Palette,
    /// nvmux's own glyphs at full strength: the terminal's foreground or the
    /// theme's colour (see [`crate::theme`]). Separate from `palette.fg`,
    /// which session cells still resolve through.
    pub ink: Rgb,
}

impl Dissolve {
    /// The colour of nvmux's own glyph `t` into a fade, or `None` at `t == 0`:
    /// as in [`apply`], zero means untouched, so the last frame of a fade in
    /// sets no colour and matches an unfaded draw.
    pub fn colour(self, t: f32) -> Option<Rgb> {
        (t > 0.0).then(|| self.ink.lerp(self.palette.bg, t))
    }
}

/// What to fade with, or `None` exactly when [`enabled`] is false.
pub fn dissolve() -> Option<Dissolve> {
    let palette = *active()?;
    Some(Dissolve {
        one_way: one_way(),
        palette,
        ink: crate::theme::ink().unwrap_or(palette.fg),
    })
}

/// Which way a fade goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// From the screen as drawn to fully dissolved: `t` runs 0 → 1.
    Out,
    /// From dissolved to the screen as drawn: `t` runs 1 → 0.
    In,
}

/// A fade shaped around one row: closing onto it on the way out, opening out
/// of it on the way in. Used by an attach from the picker (see
/// [`crate::handoff`]) and a create (see [`crate::ui::app::App::make_room`]).
///
/// Every row goes the whole way on the same clock; the iris only sets when.
/// Closing, the furthest rows go first; opening, the iris's row returns
/// first. Each row's part takes [`IRIS_SHARE`] of the fade and the starts are
/// spread over the rest, so the edge is a soft band. Both ends match an even
/// fade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Iris {
    /// 0-based.
    row: u16,
    /// Distance to the furthest row; at least 1.
    reach: u16,
}

/// The part of a fade each iris row spends changing; the rest spreads the
/// rows' start times.
const IRIS_SHARE: f32 = 0.4;

impl Iris {
    pub fn new(row: u16, rows: u16) -> Self {
        let reach = row.max(rows.saturating_sub(1).saturating_sub(row)).max(1);
        Self { row, reach }
    }

    /// How far into the background row `y` is at `t` (as [`Schedule::next`]
    /// gives it: 0 drawn, 1 gone).
    pub fn at(&self, direction: Direction, t: f32, y: u16) -> f32 {
        // Exact ends, which the arithmetic below can miss by rounding.
        match direction {
            Direction::Out if t >= 1.0 => return 1.0,
            Direction::In if t <= 0.0 => return 0.0,
            _ => {}
        }
        let far = (f32::from(y.abs_diff(self.row)) / f32::from(self.reach)).min(1.0);
        let lead = 1.0 - IRIS_SHARE;
        match direction {
            // The furthest rows start at once, the iris's own row last.
            Direction::Out => ((t - (1.0 - far) * lead) / IRIS_SHARE).clamp(0.0, 1.0),
            // The iris's own row starts at once, the furthest last.
            Direction::In => 1.0 - ((1.0 - t - far * lead) / IRIS_SHARE).clamp(0.0, 1.0),
        }
    }
}

/// The frames of one fade, paced by the clock. Told the time rather than
/// reading it, so tests reach every value.
#[derive(Debug)]
pub struct Schedule {
    started: Instant,
    duration: Duration,
    direction: Direction,
    finished: bool,
}

impl Schedule {
    pub fn start(duration: Duration, direction: Direction, now: Instant) -> Self {
        Self {
            started: now,
            duration,
            direction,
            finished: false,
        }
    }

    /// How dissolved the next frame should be, or `None` once the last has
    /// been given. Computed for one [`FRAME`] ahead, when it will be seen,
    /// which skips a frame identical to the current screen. The end value is
    /// given exactly once, however late the clock.
    pub fn next(&mut self, now: Instant) -> Option<f32> {
        if self.finished {
            return None;
        }
        let elapsed = now.saturating_duration_since(self.started) + FRAME;
        let progress = if elapsed >= self.duration {
            self.finished = true;
            1.0
        } else {
            elapsed.as_secs_f32() / self.duration.as_secs_f32()
        };
        Some(match self.direction {
            Direction::Out => progress,
            Direction::In => 1.0 - progress,
        })
    }

    /// Whether the last frame has been handed out.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Whether the fade is over by the clock, even if its last frame was never
    /// asked for. [`crate::announce`] only draws in gaps in the session's
    /// output, so it ends fades by this and drains the rest. Same reckoning as
    /// `next`, so the two agree on the ending pass.
    pub fn over(&self, now: Instant) -> bool {
        self.finished || now.saturating_duration_since(self.started) + FRAME >= self.duration
    }
}

/// Move every visible cell of `buf` `t` of the way to the background.
///
/// At `t == 0` nothing is touched, so a faded-in frame ends as a plain draw.
/// Otherwise each cell showing something (a glyph, or a reversed blank) has
/// its foreground moved from [`drawn_in`]. Backgrounds stay: the terminal's
/// own is the target. Modifiers stay: `REVERSED` then paints the faded colour
/// as the background, and `DIM` is kept because terminals dim differently and
/// a guessed dim colour would pop on the first frame.
pub fn apply(buf: &mut Buffer, palette: &Palette, t: f32) {
    if t <= 0.0 {
        return;
    }
    for cell in &mut buf.content {
        sink(cell, palette, t);
    }
}

/// [`apply`] as an [`Iris`] closing onto `keep`'s row, except for the cells
/// of `keep`, which are handed off (see [`crate::handoff`]). A kept cell on
/// the selection bar comes off it, ending as plain text in the colour it was
/// drawn in, as the next screen draws it. A kept cell off the bar is left as
/// drawn.
///
/// The bar sinks to the background; the text takes the background's colour
/// until halfway, then the drawn colour (losing bold and underline). Fading
/// the text up as the bar fades down would make them match halfway and hide
/// the name; this way the contrast never drops below half.
///
/// Colours are set outright (with the reverse off) until the last frame,
/// which restores the terminal's own background so transparency survives.
pub fn apply_keeping(buf: &mut Buffer, palette: &Palette, t: f32, keep: Rect) {
    let area = buf.area;
    let iris = Iris::new(keep.y.saturating_sub(area.y), area.height);
    for (i, cell) in buf.content.iter_mut().enumerate() {
        let i = u16::try_from(i).unwrap_or(u16::MAX);
        let (x, y) = (area.x + i % area.width, area.y + i / area.width);
        let kept = x >= keep.x && x < keep.right() && y >= keep.y && y < keep.bottom();
        if !kept {
            let row = iris.at(Direction::Out, t, y - area.y);
            if row > 0.0 {
                sink(cell, palette, row);
            }
            continue;
        }
        if !cell.modifier.contains(Modifier::REVERSED) {
            continue;
        }
        if t >= 1.0 {
            cell.modifier
                .remove(Modifier::REVERSED | Modifier::BOLD | Modifier::UNDERLINED);
            cell.set_bg(Color::Reset);
            continue;
        }
        let drawn = drawn_in(cell, palette);
        cell.modifier.remove(Modifier::REVERSED);
        let text = if t < 0.5 {
            palette.bg
        } else {
            cell.modifier.remove(Modifier::BOLD | Modifier::UNDERLINED);
            drawn
        };
        let bar = drawn.lerp(palette.bg, t);
        cell.set_fg(Color::Rgb(text.0, text.1, text.2));
        cell.set_bg(Color::Rgb(bar.0, bar.1, bar.2));
    }
}

/// [`apply`] as an [`Iris`] on `row`: closing for [`Direction::Out`],
/// opening for [`Direction::In`]. Both ends match [`apply`].
pub fn apply_iris(buf: &mut Buffer, palette: &Palette, direction: Direction, t: f32, row: u16) {
    let area = buf.area;
    if area.width == 0 {
        return;
    }
    let iris = Iris::new(row.saturating_sub(area.y), area.height);
    for (i, cell) in buf.content.iter_mut().enumerate() {
        let y = u16::try_from(i / usize::from(area.width)).unwrap_or(u16::MAX);
        let k = iris.at(direction, t, y);
        if k > 0.0 {
            sink(cell, palette, k);
        }
    }
}

/// The colour a cell of nvmux's screens is drawn in: what an earlier pass gave
/// it (the theme's ink, see [`crate::theme`]), or the terminal's foreground.
/// Every fade and effect over those screens starts and ends there.
pub(crate) fn drawn_in(cell: &ratatui::buffer::Cell, palette: &Palette) -> Rgb {
    match cell.fg {
        Color::Rgb(r, g, b) => Rgb(r, g, b),
        // Screens only get true colours, from the theme and effects; anything
        // else is the terminal's own.
        _ => palette.fg,
    }
}

/// One cell of [`apply`], if it shows anything (a glyph or a reversed blank).
fn sink(cell: &mut ratatui::buffer::Cell, palette: &Palette, t: f32) {
    let shows = !cell.symbol().trim().is_empty() || cell.modifier.contains(Modifier::REVERSED);
    if shows {
        let fg = drawn_in(cell, palette).lerp(palette.bg, t);
        cell.set_fg(Color::Rgb(fg.0, fg.1, fg.2));
    }
}

/// Dissolve a ratatui screen up out of the background. Ends on the clean
/// content, so the caller's next `draw` repaints nothing.
pub fn fade_in<F>(terminal: &mut DefaultTerminal, draw: F) -> io::Result<()>
where
    F: FnMut(&mut Frame),
{
    run(terminal, draw, Direction::In, Shape::Even)
}

/// Dissolve a ratatui screen out into the background. Ends fully dissolved,
/// so whatever erases the screen next changes nothing visible.
pub fn fade_out<F>(terminal: &mut DefaultTerminal, draw: F) -> io::Result<()>
where
    F: FnMut(&mut Frame),
{
    run(terminal, draw, Direction::Out, Shape::Even)
}

/// [`fade_out`] handing off `keep` and closing onto its row (see
/// [`apply_keeping`]): the picker's, for the name of the session attached.
pub fn fade_out_keeping<F>(terminal: &mut DefaultTerminal, draw: F, keep: Rect) -> io::Result<()>
where
    F: FnMut(&mut Frame),
{
    run(terminal, draw, Direction::Out, Shape::Keeping(keep))
}

/// [`fade_out`] as an [`Iris`] closing onto `row`: the picker's, onto the row
/// a new session will take.
pub fn fade_out_onto<F>(terminal: &mut DefaultTerminal, draw: F, row: u16) -> io::Result<()>
where
    F: FnMut(&mut Frame),
{
    run(terminal, draw, Direction::Out, Shape::Iris(row))
}

/// [`fade_in`] as an [`Iris`] opening out of `row`: the create prompt's, from
/// its name field.
pub fn fade_in_from<F>(terminal: &mut DefaultTerminal, draw: F, row: u16) -> io::Result<()>
where
    F: FnMut(&mut Frame),
{
    run(terminal, draw, Direction::In, Shape::Iris(row))
}

/// How a ratatui screen's fade is laid over it.
#[derive(Debug, Clone, Copy)]
enum Shape {
    /// [`apply`]
    Even,
    /// [`apply_keeping`]
    Keeping(Rect),
    /// [`apply_iris`]
    Iris(u16),
}

fn run<F>(
    terminal: &mut DefaultTerminal,
    mut draw: F,
    direction: Direction,
    shape: Shape,
) -> io::Result<()>
where
    F: FnMut(&mut Frame),
{
    let Some(palette) = active() else {
        return Ok(());
    };
    let mut schedule = Schedule::start(one_way(), direction, Instant::now());
    while let Some(t) = schedule.next(Instant::now()) {
        crate::term::write_stdout(SYNC_BEGIN)?;
        terminal.draw(|f| {
            draw(f);
            // Theme first, so each cell dissolves from its drawn colour.
            crate::theme::paint(f.buffer_mut());
            match shape {
                Shape::Even => apply(f.buffer_mut(), palette, t),
                Shape::Keeping(keep) => apply_keeping(f.buffer_mut(), palette, t, keep),
                Shape::Iris(row) => apply_iris(f.buffer_mut(), palette, direction, t, row),
            }
        })?;
        crate::term::write_stdout(SYNC_END)?;
        if !schedule.finished() {
            thread::sleep(FRAME);
        }
    }
    Ok(())
}

/// Dissolve an attached session's screen into the background, from the
/// shadow. Frames are diffs ([`Shadow::frame`]), so the fade costs about one
/// full repaint plus the cells that change colour. The last frame is all
/// background, matching the hand-off's erase, except the `kept` cells (a card
/// the switch carries over), which stay at full colour. Returns whether it
/// ran, as [`fade_in_session`] does. Calls `between` between frames (see
/// [`Between`]).
pub fn fade_out_session(
    shadow: &mut Shadow,
    kept: &Kept,
    between: Between<'_>,
) -> io::Result<bool> {
    run_session(shadow, Direction::Out, None, kept, between)
}

/// Called between a session fade's frames with the shadow and the next frame's
/// due time: [`wait`], or read the session's output into the shadow so later
/// frames show it (a card in motion on the `kept` cells, see [`crate::pty`]).
pub type Between<'a> = &'a mut dyn FnMut(&mut Shadow, Instant);

/// A [`Between`] that only sleeps until the next frame.
pub fn wait(_: &mut Shadow, until: Instant) {
    thread::sleep(until.saturating_duration_since(Instant::now()));
}

/// The `kept` cells' content redrawn at full colour on the terminal's
/// background, for a switch to leave on screen (see
/// [`crate::handoff::HandOff::card`]). Empty with the fade off.
pub fn standing(shadow: &Shadow, kept: &Kept) -> Vec<u8> {
    active().map_or_else(Vec::new, |palette| shadow.kept_bytes(kept, palette))
}

/// Dissolve an attached session's screen up from the background, from the
/// shadow. Returns `false` when the fade is off or the shadow is unusable, so
/// the caller knows the terminal is still blank.
///
/// The last frame is the shadow's screen at full colour (close to, but not,
/// the client's rendering) with the cursor restored ([`Cursor::Restored`]);
/// the client's own bytes follow and win.
///
/// `over`, the name the picker handed off (see [`crate::handoff`]), fades out
/// as the session fades in; with it, the session opens as an [`Iris`] from
/// the name's line. The `kept` cells (the card the session's client drew)
/// stay at full colour. Calls `between` between frames (see [`Between`]).
pub fn fade_in_session(
    shadow: &mut Shadow,
    over: Option<&Over>,
    kept: &Kept,
    between: Between<'_>,
) -> io::Result<bool> {
    run_session(shadow, Direction::In, over, kept, between)
}

fn run_session(
    shadow: &mut Shadow,
    direction: Direction,
    over: Option<&Over>,
    kept: &Kept,
    between: Between<'_>,
) -> io::Result<bool> {
    let Some(palette) = active() else {
        return Ok(false);
    };
    if !shadow.is_usable() {
        return Ok(false);
    }
    let mut out = io::stdout().lock();
    let mut schedule = Schedule::start(one_way(), direction, Instant::now());
    let (rows, _) = shadow.size();
    let iris = over.map(|o| Iris::new(o.top, rows));
    // Session cells resolve through the palette; the name over them is
    // nvmux's, in its ink.
    let ink = crate::theme::ink().unwrap_or(palette.fg);
    while let Some(t) = schedule.next(Instant::now()) {
        let cursor = cursor_for(direction, schedule.finished());
        let dissolve = |row: u16| iris.map_or(t, |iris| iris.at(direction, t, row));
        let over = over.map(|o| (o, ink, 1.0 - t));
        out.write_all(&shadow.frame_with(dissolve, palette, cursor, over, kept))?;
        out.flush()?;
        if !schedule.finished() {
            between(shadow, Instant::now() + FRAME);
        }
    }
    Ok(true)
}

/// The cursor stays hidden except on the last frame of a fade in, the one
/// frame nothing is certain to follow. After a fade out, whatever takes the
/// screen shows its own cursor.
fn cursor_for(direction: Direction, last: bool) -> Cursor {
    match direction {
        Direction::In if last => Cursor::Restored,
        Direction::In | Direction::Out => Cursor::Hidden,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::palette;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    #[test]
    fn the_config_toggle_and_no_color_both_gate_the_effect() {
        assert!(is_configured(true, false), "on by default");
        assert!(
            !is_configured(true, true),
            "NO_COLOR wins over an enabled config"
        );
        assert!(!is_configured(false, false), "the config can disable it");
        assert!(!is_configured(false, true), "off is off");
    }

    /// Pins the wiring: a driver reading `duration_ms` whole would run twice
    /// as long. Reads the compiled default, as no test installs a config.
    #[test]
    fn a_schedule_is_started_with_half_the_configured_dissolve() {
        let configured = Duration::from_millis(crate::config::get().effects.fade.duration_ms);
        assert_eq!(one_way() * 2, configured);
        assert_eq!(
            one_way(),
            Duration::from_millis(100),
            "the compiled default"
        );
    }

    /// No palette is installed under test, so every driver is a no-op there,
    /// keeping screen tests fast and relay tests unchanged.
    #[test]
    fn without_a_palette_nothing_is_enabled() {
        assert!(!enabled());
        assert!(active().is_none());
    }

    #[test]
    fn a_schedule_on_time_rises_to_one_and_stops() {
        let t0 = Instant::now();
        let mut s = Schedule::start(Duration::from_millis(100), Direction::Out, t0);
        let mut seen = vec![];
        let mut now = t0;
        while let Some(t) = s.next(now) {
            seen.push(t);
            now += FRAME;
        }
        assert!(seen.len() >= 4, "{seen:?}");
        assert!(
            seen[0] > 0.0,
            "the first frame is not the screen as it stands"
        );
        assert!(seen.windows(2).all(|w| w[1] > w[0]), "not rising: {seen:?}");
        assert_eq!(*seen.last().unwrap(), 1.0);
        assert!(s.finished());
        assert_eq!(s.next(now), None, "nothing after the end");
    }

    #[test]
    fn a_fade_in_falls_to_zero() {
        let t0 = Instant::now();
        let mut s = Schedule::start(Duration::from_millis(100), Direction::In, t0);
        let mut seen = vec![];
        let mut now = t0;
        while let Some(t) = s.next(now) {
            seen.push(t);
            now += FRAME;
        }
        assert!(seen[0] < 1.0);
        assert!(
            seen.windows(2).all(|w| w[1] < w[0]),
            "not falling: {seen:?}"
        );
        assert_eq!(*seen.last().unwrap(), 0.0);
    }

    #[test]
    fn a_late_clock_skips_to_the_end() {
        let t0 = Instant::now();
        let mut s = Schedule::start(Duration::from_millis(100), Direction::Out, t0);
        let first = s.next(t0).expect("a first frame");
        let jumped = s
            .next(t0 + Duration::from_millis(60))
            .expect("a second frame");
        assert!(jumped > first + 0.4, "{first} -> {jumped}");
        assert_eq!(s.next(t0 + Duration::from_secs(10)), Some(1.0));
        assert_eq!(s.next(t0 + Duration::from_secs(20)), None);
    }

    /// `over` must agree with `next`, or a fade would end a frame early or
    /// late depending on who asked.
    #[test]
    fn a_schedule_is_over_on_the_same_frame_it_hands_out_its_last() {
        let t0 = Instant::now();
        let mut s = Schedule::start(Duration::from_millis(100), Direction::Out, t0);
        let mut now = t0;
        loop {
            let over = s.over(now);
            let Some(t) = s.next(now) else {
                assert!(over, "a spent schedule is over whatever the clock says");
                break;
            };
            assert_eq!(over, t == 1.0, "disagreed at {:?}", now - t0);
            now += FRAME;
        }

        // And a schedule nobody ever read is over once its time is up.
        let cold = Schedule::start(Duration::from_millis(100), Direction::In, t0);
        assert!(!cold.over(t0));
        assert!(cold.over(t0 + Duration::from_millis(100)));
    }

    #[test]
    fn only_the_last_frame_of_a_fade_in_restores_the_cursor() {
        assert_eq!(cursor_for(Direction::In, true), Cursor::Restored);
        assert_eq!(cursor_for(Direction::In, false), Cursor::Hidden);
        assert_eq!(cursor_for(Direction::Out, false), Cursor::Hidden);
        assert_eq!(cursor_for(Direction::Out, true), Cursor::Hidden);
    }

    fn buffer() -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 3));
        buf.set_string(1, 0, "hello", Style::default());
        buf.set_string(1, 1, "dim", Style::default().add_modifier(Modifier::DIM));
        buf.set_string(
            1,
            2,
            "  ",
            Style::default().add_modifier(Modifier::REVERSED),
        );
        buf
    }

    #[test]
    fn at_zero_nothing_changes() {
        let mut buf = buffer();
        let before = buf.clone();
        apply(&mut buf, &palette(), 0.0);
        assert_eq!(buf, before);
    }

    #[test]
    fn at_one_everything_visible_is_the_background_colour() {
        let mut buf = buffer();
        apply(&mut buf, &palette(), 1.0);
        let bg = Color::Rgb(0, 0, 0);
        for y in 0..3 {
            for x in 0..12 {
                let cell = &buf[(x, y)];
                let shows = cell.symbol() != " " || cell.modifier.contains(Modifier::REVERSED);
                if shows {
                    assert_eq!(cell.fg, bg, "cell ({x},{y}) {:?}", cell.symbol());
                } else {
                    assert_eq!(cell.fg, Color::Reset, "blank ({x},{y}) was coloured");
                }
                assert_eq!(cell.bg, Color::Reset, "cell ({x},{y}) had its bg set");
            }
        }
    }

    #[test]
    fn halfway_is_halfway_and_modifiers_survive() {
        let mut buf = buffer();
        apply(&mut buf, &palette(), 0.5);
        assert_eq!(buf[(1, 0)].fg, Color::Rgb(100, 100, 100));
        assert!(buf[(1, 1)].modifier.contains(Modifier::DIM));
        assert_eq!(buf[(1, 1)].fg, Color::Rgb(100, 100, 100));
        assert!(buf[(1, 2)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buf[(1, 2)].fg, Color::Rgb(100, 100, 100));
    }

    /// `▸ 2  dotfiles` on a reversed bold bar over a plain row, and the name's
    /// cells to keep. The row under it is the furthest from the iris.
    fn a_bar() -> (Buffer, Rect) {
        let mut buf = Buffer::empty(Rect::new(0, 0, 16, 2));
        let bar = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);
        buf.set_string(0, 0, "▸ 2  dotfiles", bar);
        buf.set_string(0, 1, "  3  notes", Style::default());
        (buf, Rect::new(5, 0, 8, 1))
    }

    #[test]
    fn the_kept_name_comes_off_its_bar_while_the_rest_closes_in() {
        let (mut buf, keep) = a_bar();
        apply_keeping(&mut buf, &palette(), 0.25, keep);
        let marker = &buf[(0, 0)];
        assert_eq!(marker.fg, Color::Reset, "its row goes last: {marker:?}");
        assert!(marker.modifier.contains(Modifier::REVERSED));
        let name = &buf[(5, 0)];
        assert!(!name.modifier.contains(Modifier::REVERSED), "{name:?}");
        assert_eq!(
            name.bg,
            Color::Rgb(150, 150, 150),
            "the bar, a quarter down"
        );
        assert_eq!(
            name.fg,
            Color::Rgb(0, 0, 0),
            "the text as it was on the bar"
        );
        assert!(name.modifier.contains(Modifier::BOLD), "still bold");
        assert_eq!(
            buf[(5, 1)].fg,
            Color::Rgb(75, 75, 75),
            "the furthest row, well on"
        );

        let (mut buf, keep) = a_bar();
        apply_keeping(&mut buf, &palette(), 0.8, keep);
        assert_eq!(
            buf[(0, 0)].fg,
            Color::Rgb(100, 100, 100),
            "its own row, going"
        );
        assert_eq!(buf[(5, 1)].fg, Color::Rgb(0, 0, 0), "the furthest, gone");
    }

    /// A row nearer the iris is never further gone closing, or less far back
    /// opening, than one further away.
    #[test]
    fn an_iris_closes_from_the_edges_and_opens_from_its_row() {
        let iris = Iris::new(10, 24);
        for step in 0..=20 {
            let t = step as f32 / 20.0;
            for y in 0..23u16 {
                let (near, far) = if y.abs_diff(10) < (y + 1).abs_diff(10) {
                    (y, y + 1)
                } else {
                    (y + 1, y)
                };
                assert!(
                    iris.at(Direction::Out, t, near) <= iris.at(Direction::Out, t, far),
                    "closing at {t}: row {near} ahead of row {far}"
                );
                assert!(
                    iris.at(Direction::In, t, near) <= iris.at(Direction::In, t, far),
                    "opening at {t}: row {far} back before row {near}"
                );
            }
        }
        assert_eq!(
            iris.at(Direction::Out, 0.3, 23),
            0.75,
            "the furthest, early"
        );
        assert_eq!(iris.at(Direction::Out, 0.3, 10), 0.0, "its own, waiting");
        assert_eq!(iris.at(Direction::In, 0.7, 10), 0.25, "its own, back first");
        assert_eq!(
            iris.at(Direction::In, 0.7, 23),
            1.0,
            "the furthest, waiting"
        );
    }

    #[test]
    fn an_iris_ends_where_an_even_fade_does() {
        let iris = Iris::new(3, 8);
        for y in 0..8 {
            assert_eq!(iris.at(Direction::Out, 0.0, y), 0.0);
            assert_eq!(iris.at(Direction::Out, 1.0, y), 1.0);
            assert_eq!(iris.at(Direction::In, 1.0, y), 1.0);
            assert_eq!(iris.at(Direction::In, 0.0, y), 0.0);
        }
        // A screen one row high has nowhere to close in from.
        assert_eq!(Iris::new(0, 1).at(Direction::Out, 1.0, 0), 1.0);
    }

    /// The text never contrasts with its bar by less than half, and turns to
    /// the foreground once the bar is the darker.
    #[test]
    fn the_name_stays_readable_all_the_way_off_the_bar() {
        let p = palette();
        for step in 0..100 {
            let t = step as f32 / 100.0;
            let (mut buf, keep) = a_bar();
            apply_keeping(&mut buf, &p, t, keep);
            let (Color::Rgb(text, ..), Color::Rgb(bar, ..)) = (buf[(5, 0)].fg, buf[(5, 0)].bg)
            else {
                panic!("set outright before the last frame: {:?}", buf[(5, 0)]);
            };
            assert!(text.abs_diff(bar) >= 100, "{text} on {bar} at {t}");
        }
        let (mut buf, keep) = a_bar();
        apply_keeping(&mut buf, &p, 0.75, keep);
        assert_eq!(buf[(5, 0)].fg, Color::Rgb(200, 200, 200), "the foreground");
        assert!(
            !buf[(5, 0)].modifier.contains(Modifier::BOLD),
            "plain by now"
        );
    }

    #[test]
    fn at_the_end_the_name_is_plain_text_on_nothing() {
        let (mut buf, keep) = a_bar();
        apply_keeping(&mut buf, &palette(), 1.0, keep);
        for x in keep.x..keep.right() {
            let cell = &buf[(x, 0)];
            assert_eq!((cell.fg, cell.bg), (Color::Reset, Color::Reset), "{cell:?}");
            assert!(cell.modifier.is_empty(), "{cell:?}");
        }
        let name: String = (keep.x..keep.right())
            .map(|x| buf[(x, 0)].symbol())
            .collect();
        assert_eq!(name, "dotfiles");
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(0, 0, 0), "the marker is gone");
    }

    fn three_rows() -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 3));
        for y in 0..3 {
            buf.set_string(0, y, "abc", Style::default());
        }
        buf
    }

    #[test]
    fn an_iris_with_nothing_kept_closes_onto_its_row() {
        let p = palette();
        let mut buf = three_rows();
        apply_iris(&mut buf, &p, Direction::Out, 0.5, 2);
        assert_eq!(buf[(0, 2)].fg, Color::Reset, "its own row, still waiting");
        assert!(
            matches!(buf[(0, 0)].fg, Color::Rgb(..)),
            "the furthest, going: {:?}",
            buf[(0, 0)]
        );

        let mut buf = three_rows();
        apply_iris(&mut buf, &p, Direction::Out, 1.0, 2);
        let mut even = three_rows();
        apply(&mut even, &p, 1.0);
        assert_eq!(buf, even, "all the way out, the same as an even fade");
    }

    #[test]
    fn an_iris_with_nothing_kept_opens_out_of_its_row() {
        let p = palette();
        let mut buf = three_rows();
        apply_iris(&mut buf, &p, Direction::In, 0.5, 0);
        assert_eq!(buf[(0, 0)].fg, Color::Reset, "its own row, back already");
        assert_eq!(buf[(0, 2)].fg, Color::Rgb(0, 0, 0), "the furthest, waiting");

        let mut buf = three_rows();
        apply_iris(&mut buf, &p, Direction::In, 0.0, 0);
        assert_eq!(buf, three_rows(), "all the way in, as drawn");
    }

    /// As when a session is picked by typing its number.
    #[test]
    fn a_kept_name_not_on_a_bar_is_left_as_drawn() {
        let (mut buf, _) = a_bar();
        let before = buf[(5, 1)].clone();
        apply_keeping(&mut buf, &palette(), 0.6, Rect::new(5, 1, 5, 1));
        assert_eq!(buf[(5, 1)], before);
    }
}
