//! The dissolve between screens.
//!
//! nvmux hard-cut between its screens. This module smooths each cut into a
//! short fade: the outgoing screen dissolves into the terminal's own background
//! colour, and the incoming one dissolves up out of it — a real colour fade,
//! every visible cell interpolated toward the background over a few frames,
//! not a dither.
//!
//! # Two kinds of screen, one effect
//!
//! nvmux's own screens — the picker, the prompt, the help — are ratatui
//! buffers it draws itself, so the fade is a post-pass over a finished frame
//! ([`apply`]): the screen is drawn as usual, then every visible cell's
//! foreground is moved toward the background before the frame is presented.
//! `draw::draw` and its siblings never set a colour and keep their tests
//! saying so; the colour is added afterwards, and at the end of a fade-in the
//! frame is byte-identical to a plain draw. The one colour a screen can carry
//! into a fade is the picker's selection bar under a `[theme] highlight` (see
//! [`crate::theme`]), laid as a post-pass of its own; a cell drawn in colours
//! of its own dissolves from them rather than from the terminal's.
//!
//! An attached session is raw Neovim that nvmux only proxies bytes for. Its
//! cells come from the shadow grid ([`crate::shadow`]) that watched those
//! bytes go by, and [`fade_out_session`] and [`fade_in_session`] paint the
//! frames from that. A fade *in* needs the finished screen before any of it
//! is shown, so the relay holds a session's first paint back from the
//! terminal until it has settled, dissolves the shadow in, and only then lets
//! the bytes through (see `pty::Hold`); a resumed session dissolves in from
//! the screen the shadow still holds, ahead of its repaint.
//!
//! # Colour, and what turns the effect off
//!
//! Both ends of the interpolation have to be real colours, so the fade rests
//! on the terminal's answer to [`crate::palette::query`]: no answer, no fade,
//! and the transitions are exactly what they were before. `NO_COLOR` turns it
//! off too — the effect paints explicit colours — and so does switching it off
//! in the config, as `[effects.fade] enabled` or with every other effect under
//! `[effects] enabled`. [`enabled`] is the one gate every entry point checks.
//!
//! # Time
//!
//! A fade is paced by the clock, not by counting frames: [`Schedule`] reads
//! how far into the fade it is at every frame, so a slow terminal skips frames
//! rather than stretching the effect, and every fade ends on exactly its last
//! value — fully dissolved, or fully drawn — whatever the clock did.

use std::io::{self, Write};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::{DefaultTerminal, Frame};

use crate::palette::{self, Palette, Rgb};
use crate::shadow::{Cursor, Over, Shadow};

/// A synchronized update: the terminal presents nothing between these, so a
/// frame is never seen half-composited. Opening one inside another is
/// harmless, which matters where a relay stopped inside Neovim's own.
pub const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
pub const SYNC_END: &[u8] = b"\x1b[?2026l";

/// How often a frame goes out. Roughly sixty a second; more is not visible.
///
/// Shared with [`crate::announce`], whose fade is driven a frame at a time by
/// the relay loop rather than by a loop of its own: the same pace, from the one
/// place it is decided.
pub const FRAME: Duration = Duration::from_millis(16);

/// Whether the config and the environment allow a fade — everything but the
/// terminal's answer, which is what `main` decides whether to ask for on the
/// strength of this.
pub fn configured() -> bool {
    is_configured(
        crate::config::get().effects.fade_enabled(),
        std::env::var_os("NO_COLOR").is_some(),
    )
}

/// The gate itself, factored out so the precedence is testable without the
/// process globals [`configured`] reads. `NO_COLOR` wins over the config
/// because the effect paints explicit colours, and honouring the request
/// means never running it whatever the config says.
fn is_configured(config_enabled: bool, no_color: bool) -> bool {
    config_enabled && !no_color
}

/// Whether transitions animate at all: configured, and the terminal said what
/// its colours are. `false` restores the pre-fade behaviour exactly.
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

/// Whether an attached session is watched so it can dissolve out.
pub fn session() -> bool {
    crate::config::get().effects.fade.session
}

/// How long one direction takes: half of `effects.fade.duration_ms`, which measures
/// a dissolve both ways (see [`crate::config::FadeSettings::one_way`]).
fn one_way() -> Duration {
    crate::config::get().effects.fade.one_way()
}

/// Everything a caller needs to paint its own fade frames: what to interpolate
/// between, and how long one direction takes.
///
/// One direction, not the configured length of both: a caller here is driving
/// a single [`Schedule`], and the halving happens once, on the way out of the
/// config.
///
/// The drivers below sleep between frames, which the relay loop cannot do — it
/// has a child to read and keys to pass on. So the attach notice
/// ([`crate::announce`]) drives a [`Schedule`] from its own `poll` and is
/// handed one of these to do it with. Plain data, copied out of the config and
/// the palette once: a caller that read process globals mid-fade would have
/// states no test could reach, and no palette is ever installed under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dissolve {
    /// One direction's length — half of `fade.duration_ms`. Named for what it
    /// is rather than for the key it comes from, since that key measures both
    /// directions and these two numbers are no longer the same one.
    pub one_way: Duration,
    /// The terminal's own colours: what a glyph fully drawn is, what a glyph
    /// fully dissolved is, and — for a caller painting the session's own cells
    /// rather than its own text — what every indexed colour in between means.
    pub palette: Palette,
}

impl Dissolve {
    /// The colour a glyph is drawn in `t` of the way through a fade, or `None`
    /// for one not being faded at all.
    ///
    /// Zero is "do not touch it" rather than "the foreground at no distance" —
    /// the same rule [`apply`] keeps for a ratatui frame. It is what lets the
    /// last frame of a fade in set no colour whatever, so the screen it leaves
    /// behind is byte for byte the one drawn without any fade at all.
    pub fn colour(self, t: f32) -> Option<Rgb> {
        (t > 0.0).then(|| self.palette.fg.lerp(self.palette.bg, t))
    }
}

/// What to fade with, or `None` when there is no fade — no palette, `NO_COLOR`,
/// or the fade switched off in `[effects]`. Exactly the gate [`enabled`] reports, in the
/// one form a caller that paints its own frames can use.
pub fn dissolve() -> Option<Dissolve> {
    Some(Dissolve {
        one_way: one_way(),
        palette: *active()?,
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

/// A fade shaped around one row: an iris, closing onto that row on the way
/// out and opening out of it on the way in. An attach from the picker's (see
/// [`crate::handoff`]): the picker closes onto the row of the session chosen,
/// and the session opens out of the line its name stands on. And a create's
/// (see [`crate::ui::app::App::make_room`]): the picker closes onto the row
/// the new session will take, and the prompt opens out of its name field.
///
/// Every row still goes the whole way, and on the same clock: an iris only
/// says *when* in the fade each row does its part. Closing, the rows furthest
/// from the iris's go first and its own last; opening, its own comes back
/// first and the furthest last. Each row's part takes [`IRIS_SHARE`] of the
/// fade's time, and the rows' parts start spread across the rest of it, so
/// neighbours overlap and the edge moves as a soft band rather than a line. Both directions end exactly where an even
/// fade does — everything gone, or everything back as drawn — so the frames
/// either side of one are the ones they always were.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Iris {
    /// The row it closes onto and opens out of, 0-based.
    row: u16,
    /// How many rows away the furthest is: never less than one.
    reach: u16,
}

/// The part of a fade each row of an iris spends going, or coming back. The
/// rest is how far apart in time the nearest and furthest rows start.
const IRIS_SHARE: f32 = 0.4;

impl Iris {
    /// An iris on `row` of a screen `rows` high.
    pub fn new(row: u16, rows: u16) -> Self {
        let reach = row.max(rows.saturating_sub(1).saturating_sub(row)).max(1);
        Self { row, reach }
    }

    /// How far into the background row `y` is when a fade going `direction`
    /// is `t` into it — `t` as [`Schedule::next`] hands it out, 0 the screen as
    /// drawn and 1 gone.
    pub fn at(&self, direction: Direction, t: f32, y: u16) -> f32 {
        // Exactly an even fade's ends, which the arithmetic below would miss
        // by a rounding error: the last frame out must be all background, and
        // the last frame in the screen exactly as drawn.
        match direction {
            Direction::Out if t >= 1.0 => return 1.0,
            Direction::In if t <= 0.0 => return 0.0,
            _ => {}
        }
        let far = (f32::from(y.abs_diff(self.row)) / f32::from(self.reach)).min(1.0);
        let lead = 1.0 - IRIS_SHARE;
        match direction {
            // The furthest rows start at once; the iris's own row waits until
            // the last of the time.
            Direction::Out => ((t - (1.0 - far) * lead) / IRIS_SHARE).clamp(0.0, 1.0),
            // Back out of the background: the iris's own row starts at once,
            // the furthest waits until the last of the time.
            Direction::In => 1.0 - ((1.0 - t - far * lead) / IRIS_SHARE).clamp(0.0, 1.0),
        }
    }
}

/// The frames of one fade, paced by the clock.
///
/// Told the time rather than reading it, like [`crate::announce::Popup`], so
/// every value it can produce is reachable from a test.
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

    /// How far dissolved the next frame should be, or `None` once the last
    /// frame has been handed out.
    ///
    /// The frame is drawn for the moment it will be seen, a frame from now,
    /// which is what skips the frame identical to the screen as it stands. The
    /// end value is always produced exactly once, however late the clock is.
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

    /// Whether the last frame has been handed out — and so whether there is
    /// anything to wait for before the next.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Whether the fade is over by the clock, whether or not its last frame was
    /// ever asked for.
    ///
    /// [`Schedule::next`] is what ends a fade being drawn frame by frame, and
    /// it can only end one that is asked for. [`crate::announce`] cannot
    /// promise that: its frames go out only where the session's own output
    /// leaves a gap between sequences, and a session that never left one would
    /// otherwise hold a fade open for ever. So it asks the clock instead, and
    /// drains whatever is left of the schedule when the answer is yes — which
    /// is how a fade nobody could draw still ends on exactly its last value.
    ///
    /// The same reckoning as `next`'s, down to the frame it looks ahead by, so
    /// the two can never disagree about which pass a fade ends on.
    pub fn over(&self, now: Instant) -> bool {
        self.finished || now.saturating_duration_since(self.started) + FRAME >= self.duration
    }
}

/// Move every visible cell of `buf` `t` of the way to the background.
///
/// A post-pass over a finished frame, so the screens themselves stay
/// colourless. At `t == 0` nothing is touched at all — a faded-in frame's
/// last state is exactly a plain draw. Otherwise every cell that shows
/// something — a glyph, or a reversed blank, which shows its background —
/// has its foreground set to the interpolated colour: from its own, if it was
/// drawn in one, and otherwise from the terminal's. Backgrounds are left
/// alone: the terminal's own, transparent or not, is what everything is
/// dissolving into — except one a cell was drawn with, which goes the same
/// way and is the terminal's own again at the end. Modifiers are left alone
/// too. `REVERSED` then paints the interpolated colour as the cell's
/// background, which is what a selected row dissolving should look like;
/// `DIM` keeps the hint row starting exactly as it was drawn, since terminals
/// dim differently and a guess at the dimmed colour would pop on the first
/// frame.
pub fn apply(buf: &mut Buffer, palette: &Palette, t: f32) {
    if t <= 0.0 {
        return;
    }
    for cell in &mut buf.content {
        sink(cell, palette, t);
    }
}

/// [`apply`], except for the cells of `keep`, which are handed off rather
/// than dissolved (see [`crate::handoff`]), and with everything else going as
/// an [`Iris`] closing onto `keep`'s row: the rows furthest from it first, its
/// own last. A kept cell on the selection's bar comes off it as `t` goes, and
/// on the last frame is plain text in the terminal's own colours — the very
/// cell the screen the picker leaves behind draws it as. A kept cell that was
/// not on a bar is left as it was drawn.
///
/// The bar under the text sinks into the background, and the text is
/// whichever end of the two colours stands out from it: the background's, as
/// on the bar, until the bar is halfway down, and the foreground's after, when
/// its bold and underline go with it. Fading the text up while the bar fades
/// down would be the obvious move and is the wrong one — the two cross
/// halfway, and on that frame the name is the bar's own colour and gone. This
/// way it never stands out from the bar by less than half.
///
/// The colours are set outright until the last frame: a bar is drawn
/// reversed, and its text can only be given a colour of its own once the
/// reverse is off. A bar drawn in colours of its own — a `[theme] highlight`
/// — sinks from them: the bar from its own, the text from its own until it
/// turns; and a kept name in a colour of its own off any bar comes round to
/// the terminal's foreground. The last frame puts the terminal's own back, so
/// a transparent background ends transparent.
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
            // Not on a bar, and drawn in a colour of its own — a name under a
            // `[theme] highlight` — it comes round to the terminal's own on
            // the same clock, so it ends as the plain text the hand-off
            // leaves. In the terminal's colours already, it is left as drawn.
            if let Some(fg) = own(cell.fg) {
                if t >= 1.0 {
                    cell.set_fg(Color::Reset);
                } else {
                    let fg = fg.lerp(palette.fg, t);
                    cell.set_fg(Color::Rgb(fg.0, fg.1, fg.2));
                }
            }
            continue;
        }
        if t >= 1.0 {
            cell.modifier
                .remove(Modifier::REVERSED | Modifier::BOLD | Modifier::UNDERLINED);
            cell.set_fg(Color::Reset);
            cell.set_bg(Color::Reset);
            continue;
        }
        // Reversed, the cell's foreground is the bar and its background the
        // text.
        let bar = own(cell.fg).unwrap_or(palette.fg).lerp(palette.bg, t);
        let on_bar = own(cell.bg).unwrap_or(palette.bg);
        cell.modifier.remove(Modifier::REVERSED);
        let text = if t < 0.5 {
            on_bar
        } else {
            cell.modifier.remove(Modifier::BOLD | Modifier::UNDERLINED);
            palette.fg
        };
        cell.set_fg(Color::Rgb(text.0, text.1, text.2));
        cell.set_bg(Color::Rgb(bar.0, bar.1, bar.2));
    }
}

/// [`apply`], shaped as an [`Iris`] on `row` of the buffer: closing onto it
/// going [`Direction::Out`], opening out of it going [`Direction::In`]. Nothing
/// is kept back, so both ends are exactly [`apply`]'s: every visible cell gone
/// at the end of a fade out, the screen untouched at the end of a fade in.
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

/// One cell of [`apply`], `t` of the way into the background: its foreground,
/// if it shows anything — a glyph, or a reversed blank, which shows its
/// background — from its own colour or the terminal's; and a background it
/// was drawn with, which at the end is the terminal's own outright.
fn sink(cell: &mut ratatui::buffer::Cell, palette: &Palette, t: f32) {
    let shows = !cell.symbol().trim().is_empty() || cell.modifier.contains(Modifier::REVERSED);
    if shows {
        let fg = own(cell.fg).unwrap_or(palette.fg).lerp(palette.bg, t);
        cell.set_fg(Color::Rgb(fg.0, fg.1, fg.2));
    }
    if let Some(bg) = own(cell.bg) {
        if t >= 1.0 {
            cell.set_bg(Color::Reset);
        } else {
            let bg = bg.lerp(palette.bg, t);
            cell.set_bg(Color::Rgb(bg.0, bg.1, bg.2));
        }
    }
}

/// The colour a cell was drawn in, if it was drawn in one of its own rather
/// than the terminal's.
fn own(colour: Color) -> Option<Rgb> {
    match colour {
        Color::Rgb(r, g, b) => Some(Rgb(r, g, b)),
        _ => None,
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
/// so whatever erases the screen next — a hand-off, a restore — changes
/// nothing visible.
pub fn fade_out<F>(terminal: &mut DefaultTerminal, draw: F) -> io::Result<()>
where
    F: FnMut(&mut Frame),
{
    run(terminal, draw, Direction::Out, Shape::Even)
}

/// [`fade_out`], handing the cells of `keep` off rather than dissolving them,
/// and closing in onto their row (see [`apply_keeping`]): the picker's, for
/// the name of the session it is attaching to. Ends with everything else
/// dissolved and the name standing as plain text.
pub fn fade_out_keeping<F>(terminal: &mut DefaultTerminal, draw: F, keep: Rect) -> io::Result<()>
where
    F: FnMut(&mut Frame),
{
    run(terminal, draw, Direction::Out, Shape::Keeping(keep))
}

/// [`fade_out`], closing in onto `row` as an [`Iris`], that row last: the
/// picker's, onto the row a session about to be created will take.
pub fn fade_out_onto<F>(terminal: &mut DefaultTerminal, draw: F, row: u16) -> io::Result<()>
where
    F: FnMut(&mut Frame),
{
    run(terminal, draw, Direction::Out, Shape::Iris(row))
}

/// [`fade_in`], opening out of `row` as an [`Iris`], that row first: the
/// create prompt's, out of its name field.
pub fn fade_in_from<F>(terminal: &mut DefaultTerminal, draw: F, row: u16) -> io::Result<()>
where
    F: FnMut(&mut Frame),
{
    run(terminal, draw, Direction::In, Shape::Iris(row))
}

/// How a ratatui screen's fade is laid over it.
#[derive(Debug, Clone, Copy)]
enum Shape {
    /// Every cell together ([`apply`]).
    Even,
    /// An iris onto the row of these cells, which are handed off rather than
    /// dissolved ([`apply_keeping`]).
    Keeping(Rect),
    /// An iris on this row, nothing kept ([`apply_iris`]).
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

/// Dissolve an attached session's screen out into the background, from the
/// shadow that watched it being drawn.
///
/// Every frame is a diff against the one before (see [`Shadow::frame`]), so
/// the whole fade costs about one full repaint plus the cells that change
/// colour — which nvmux already emits on every resize and resume. The last
/// frame has every visible cell at the background colour, which is what the
/// hand-off's erase paints next, so the seam is flat.
pub fn fade_out_session(shadow: &mut Shadow) -> io::Result<()> {
    run_session(shadow, Direction::Out, None).map(|_| ())
}

/// Dissolve an attached session's screen up out of the background, from the
/// shadow. Says whether it did: `false` when the fade is off or the shadow
/// cannot be trusted, so the caller knows the terminal is still blank.
///
/// The last frame is the shadow's screen at full colour — nvmux's rendering
/// of it, which is close but not the client's own — with the cursor back on
/// its cell (see [`Cursor::Restored`]). What the client wrote is written
/// after it, so its rendering is what stays.
///
/// `over` is drawn over every frame, dissolving out as the session dissolves
/// in: the name the picker handed off (see [`crate::handoff`]), fully drawn on
/// the first frame and gone on the last. With it, the session comes back as an
/// [`Iris`] opening out of the line the name stands on: that line first, the
/// rows furthest from it last.
pub fn fade_in_session(shadow: &mut Shadow, over: Option<&Over>) -> io::Result<bool> {
    run_session(shadow, Direction::In, over)
}

fn run_session(shadow: &mut Shadow, direction: Direction, over: Option<&Over>) -> io::Result<bool> {
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
    while let Some(t) = schedule.next(Instant::now()) {
        let cursor = cursor_for(direction, schedule.finished());
        let dissolve = |row: u16| iris.map_or(t, |iris| iris.at(direction, t, row));
        out.write_all(&shadow.frame_with(dissolve, palette, cursor, over.map(|o| (o, 1.0 - t))))?;
        out.flush()?;
        if !schedule.finished() {
            thread::sleep(FRAME);
        }
    }
    Ok(true)
}

/// Where a session frame leaves the cursor: hidden throughout, except on the
/// last frame of a fade in, which is the one frame nothing is certain to
/// follow. A fade out ends hidden — whatever takes the screen next shows the
/// cursor for itself, and until then there is nothing for it to sit on.
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

    /// The gate's precedence, without touching any global: the config can
    /// turn the effect off, and `NO_COLOR` turns it off even when the config
    /// wants it.
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

    /// What the schedules are actually started with: one direction, half the
    /// configured dissolve. The arithmetic is pinned where it happens
    /// (`config::FadeSettings::one_way`); this pins the wiring, so a driver
    /// that went back to reading `duration_ms` whole fails here rather than
    /// quietly running twice as long.
    ///
    /// Reads the compiled default, since no test installs a config (see
    /// [`crate::config::get`]).
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

    /// No palette is ever installed under test, so nothing can animate: every
    /// driver is a no-op here, which is what keeps the screen tests fast and
    /// the relay tests unchanged.
    #[test]
    fn without_a_palette_nothing_is_enabled() {
        assert!(!enabled());
        assert!(active().is_none());
    }

    /// The values a fade-out hands out, frame by frame on a well-behaved clock:
    /// never the untouched screen, rising, and ending on exactly 1 once.
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

    /// A fade-in runs the same clock backwards and ends on exactly 0 — which is
    /// the plain draw.
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

    /// A terminal that took its time drawing skips ahead rather than
    /// stretching the fade, and a clock that is very late still gets the end
    /// value, once.
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

    /// The clock's own answer, for a caller that cannot draw every frame. It
    /// must agree with `next` exactly — the pass on which one says the fade is
    /// over is the pass on which the other hands out its end value — or a fade
    /// would end a frame early or a frame late depending on who asked.
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

    /// Exactly one frame puts the cursor back: the last of a fade in. Every
    /// frame of a fade out hides it, including the last — the screen it would
    /// sit on is gone, and the next owner shows it.
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

    /// The post-pass is exactly that: at zero it must not touch a single cell,
    /// so a faded-in frame's final state is byte-identical to a plain draw.
    #[test]
    fn at_zero_nothing_changes() {
        let mut buf = buffer();
        let before = buf.clone();
        apply(&mut buf, &palette(), 0.0);
        assert_eq!(buf, before);
    }

    /// Fully dissolved, every visible cell's foreground is the background
    /// colour: a glyph in it is invisible, a reversed blank is background on
    /// background.
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

    /// Halfway, the foreground is halfway — and the modifiers are exactly as
    /// drawn, so the hint row is still dim and the selection still reversed.
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

    /// A selected row, `▸ 2  dotfiles` on a reversed bold bar, with a plain
    /// row under it; and the name's cells, to keep. The iris closes onto the
    /// selected row, so the row under it is the furthest there is.
    fn a_bar() -> (Buffer, Rect) {
        let mut buf = Buffer::empty(Rect::new(0, 0, 16, 2));
        let bar = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);
        buf.set_string(0, 0, "▸ 2  dotfiles", bar);
        buf.set_string(0, 1, "  3  notes", Style::default());
        (buf, Rect::new(5, 0, 8, 1))
    }

    /// Everything outside the name closes in onto its row, the furthest row
    /// first; the name's cells come off the bar instead, the bar under them
    /// sinking.
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

    /// Closing, rows go from the furthest in; opening, from the iris's own row
    /// out. A row nearer the iris is never further gone closing, or less far
    /// back opening, than one further away.
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

    /// Whatever the shape, both ends are an even fade's: nothing gone at the
    /// start of a fade-out, everything back at the end of a fade-in, and the
    /// reverse — so the frames either side of an iris are untouched.
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

    /// The name never stands out from its bar by less than half: the text is
    /// the bar's own colour on no frame, and turns to the foreground the
    /// moment the bar is the darker of the two.
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

    /// On the last frame the name is plain text in the terminal's own colours,
    /// and everything else has gone into the background.
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

    /// Three plain rows, `abc` on each, for the iris without a hand-off.
    fn three_rows() -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 3));
        for y in 0..3 {
            buf.set_string(0, y, "abc", Style::default());
        }
        buf
    }

    /// Closing onto a row with nothing kept: the rows away from it go first
    /// and it goes last, and at the end every row is gone, as an even fade
    /// leaves them.
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

    /// Opening out of a row: it comes back first and the rest after, and at
    /// the end the screen is exactly as drawn.
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

    /// A name typed by number is not on the bar: it is left exactly as drawn.
    #[test]
    fn a_kept_name_not_on_a_bar_is_left_as_drawn() {
        let (mut buf, _) = a_bar();
        let before = buf[(5, 1)].clone();
        apply_keeping(&mut buf, &palette(), 0.6, Rect::new(5, 1, 5, 1));
        assert_eq!(buf[(5, 1)], before);
    }

    /// A `[theme] highlight`, and the text the theme puts on it.
    const BAR: Rgb = Rgb(100, 0, 200);
    const ON_BAR: Rgb = Rgb(200, 200, 200);

    /// [`a_bar`], coloured the way [`crate::theme`] colours it: still
    /// reversed, the bar its foreground and the text its background.
    fn a_themed_bar() -> (Buffer, Rect) {
        let (mut buf, keep) = a_bar();
        for cell in &mut buf.content {
            if cell.modifier.contains(Modifier::REVERSED) {
                cell.set_fg(Color::Rgb(BAR.0, BAR.1, BAR.2));
                cell.set_bg(Color::Rgb(ON_BAR.0, ON_BAR.1, ON_BAR.2));
            }
        }
        (buf, keep)
    }

    /// A bar in colours of its own sinks from them, not from the terminal's:
    /// untouched at zero, halfway at half, and all the way out it is exactly
    /// what a plain bar leaves.
    #[test]
    fn a_themed_bar_dissolves_from_its_own_colours() {
        let (mut buf, _) = a_themed_bar();
        let before = buf.clone();
        apply(&mut buf, &palette(), 0.0);
        assert_eq!(buf, before);

        let (mut buf, _) = a_themed_bar();
        apply(&mut buf, &palette(), 0.5);
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(50, 0, 100), "the bar, halfway");
        assert_eq!(
            buf[(0, 0)].bg,
            Color::Rgb(100, 100, 100),
            "its text, halfway"
        );
        assert_eq!(buf[(5, 1)].fg, Color::Rgb(100, 100, 100), "a plain row");
        assert_eq!(buf[(5, 1)].bg, Color::Reset);

        let (mut buf, _) = a_themed_bar();
        apply(&mut buf, &palette(), 1.0);
        let (mut plain, _) = a_bar();
        apply(&mut plain, &palette(), 1.0);
        assert_eq!(buf, plain, "all the way out, as a plain bar goes");
    }

    /// Handed off a themed bar, the name comes off the bar's own colour, in
    /// its own text colour, and ends as plain text on nothing.
    #[test]
    fn a_name_handed_off_a_themed_bar_ends_on_nothing() {
        let (mut buf, keep) = a_themed_bar();
        apply_keeping(&mut buf, &palette(), 0.25, keep);
        let name = &buf[(5, 0)];
        assert_eq!(name.bg, Color::Rgb(75, 0, 150), "the bar, a quarter down");
        assert_eq!(
            name.fg,
            Color::Rgb(200, 200, 200),
            "the text it was drawn in"
        );
        assert!(!name.modifier.contains(Modifier::REVERSED));

        let (mut buf, keep) = a_themed_bar();
        apply_keeping(&mut buf, &palette(), 1.0, keep);
        for x in keep.x..keep.right() {
            let cell = &buf[(x, 0)];
            assert_eq!((cell.fg, cell.bg), (Color::Reset, Color::Reset), "{cell:?}");
            assert!(cell.modifier.is_empty(), "{cell:?}");
        }
        for cell in &buf.content {
            assert_eq!(cell.bg, Color::Reset, "nothing left of the bar: {cell:?}");
        }
    }

    /// A name typed by number is kept off any bar; in the highlight a theme
    /// draws the other names in, it comes round to the terminal's foreground
    /// and ends as plain text, the hand-off's.
    #[test]
    fn a_themed_name_kept_off_a_bar_comes_round_to_the_terminals() {
        let named = || {
            let (mut buf, _) = a_bar();
            for x in 5..10 {
                buf[(x, 1)].set_fg(Color::Rgb(BAR.0, BAR.1, BAR.2));
            }
            buf
        };
        let keep = Rect::new(5, 1, 5, 1);
        let mut buf = named();
        apply_keeping(&mut buf, &palette(), 0.5, keep);
        assert_eq!(buf[(5, 1)].fg, Color::Rgb(150, 100, 200), "halfway round");
        let mut buf = named();
        apply_keeping(&mut buf, &palette(), 1.0, keep);
        for x in 5..10 {
            assert_eq!(buf[(x, 1)].fg, Color::Reset, "plain text at {x}");
        }
    }
}
