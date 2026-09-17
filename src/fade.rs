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
//! frame is byte-identical to a plain draw.
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
//! off too — the effect paints explicit colours — and so does `[fade]
//! enabled = false`. [`enabled`] is the one gate every entry point checks.
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
use ratatui::style::{Color, Modifier};
use ratatui::{DefaultTerminal, Frame};

use crate::palette::{self, Palette, Rgb};
use crate::shadow::{Cursor, Shadow};

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
        crate::config::get().fade.enabled,
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

/// Whether the quick excursions — the help and the create prompt, entered
/// from a session — fade too.
pub fn excursions() -> bool {
    crate::config::get().fade.excursions
}

/// Whether an attached session is watched so it can dissolve out.
pub fn session() -> bool {
    crate::config::get().fade.session
}

/// One direction's length, from the config.
fn duration() -> Duration {
    Duration::from_millis(crate::config::get().fade.duration_ms)
}

/// Everything a caller needs to paint its own fade frames: what to interpolate
/// between, and how long one direction takes.
///
/// The drivers below sleep between frames, which the relay loop cannot do — it
/// has a child to read and keys to pass on. So the attach notice
/// ([`crate::announce`]) drives a [`Schedule`] from its own `poll` and is
/// handed one of these to do it with. Plain data, copied out of the config and
/// the palette once: a caller that read process globals mid-fade would have
/// states no test could reach, and no palette is ever installed under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dissolve {
    /// One direction's length.
    pub duration: Duration,
    /// The terminal's own foreground: a glyph fully drawn.
    pub fg: Rgb,
    /// The terminal's own background: a glyph fully dissolved.
    pub bg: Rgb,
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
        (t > 0.0).then(|| self.fg.lerp(self.bg, t))
    }
}

/// What to fade with, or `None` when there is no fade — no palette, `NO_COLOR`,
/// or `[fade] enabled = false`. Exactly the gate [`enabled`] reports, in the
/// one form a caller that paints its own frames can use.
pub fn dissolve() -> Option<Dissolve> {
    let palette = active()?;
    Some(Dissolve {
        duration: duration(),
        fg: palette.fg,
        bg: palette.bg,
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
    /// promise that: its frames go out only at a lull in the session's own
    /// output, and a session that never goes quiet would otherwise hold a fade
    /// open for ever. So it asks the clock instead, and drains whatever is left
    /// of the schedule when the answer is yes — which is how a fade nobody
    /// could draw still ends on exactly its last value.
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
/// has its foreground set to the interpolated colour. Backgrounds are left
/// alone: the terminal's own, transparent or not, is what everything is
/// dissolving into. Modifiers are left alone too. `REVERSED` then paints the
/// interpolated colour as the cell's background, which is what a selected
/// row dissolving should look like; `DIM` keeps the hint row starting
/// exactly as it was drawn, since terminals dim differently and a guess at
/// the dimmed colour would pop on the first frame.
pub fn apply(buf: &mut Buffer, palette: &Palette, t: f32) {
    if t <= 0.0 {
        return;
    }
    let fg = palette.fg.lerp(palette.bg, t);
    let colour = Color::Rgb(fg.0, fg.1, fg.2);
    for cell in &mut buf.content {
        let shows = !cell.symbol().trim().is_empty() || cell.modifier.contains(Modifier::REVERSED);
        if shows {
            cell.set_fg(colour);
        }
    }
}

/// Dissolve a ratatui screen up out of the background. Ends on the clean
/// content, so the caller's next `draw` repaints nothing.
pub fn fade_in<F>(terminal: &mut DefaultTerminal, draw: F) -> io::Result<()>
where
    F: FnMut(&mut Frame),
{
    run(terminal, draw, Direction::In)
}

/// Dissolve a ratatui screen out into the background. Ends fully dissolved,
/// so whatever erases the screen next — a hand-off, a restore — changes
/// nothing visible.
pub fn fade_out<F>(terminal: &mut DefaultTerminal, draw: F) -> io::Result<()>
where
    F: FnMut(&mut Frame),
{
    run(terminal, draw, Direction::Out)
}

fn run<F>(terminal: &mut DefaultTerminal, mut draw: F, direction: Direction) -> io::Result<()>
where
    F: FnMut(&mut Frame),
{
    let Some(palette) = active() else {
        return Ok(());
    };
    let mut schedule = Schedule::start(duration(), direction, Instant::now());
    while let Some(t) = schedule.next(Instant::now()) {
        write_all(SYNC_BEGIN)?;
        terminal.draw(|f| {
            draw(f);
            apply(f.buffer_mut(), palette, t);
        })?;
        write_all(SYNC_END)?;
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
    run_session(shadow, Direction::Out).map(|_| ())
}

/// Dissolve an attached session's screen up out of the background, from the
/// shadow. Says whether it did: `false` when the fade is off or the shadow
/// cannot be trusted, so the caller knows the terminal is still blank.
///
/// The last frame is the shadow's screen at full colour — nvmux's rendering
/// of it, which is close but not the client's own — with the cursor back on
/// its cell (see [`Cursor::Restored`]). What the client wrote is written
/// after it, so its rendering is what stays.
pub fn fade_in_session(shadow: &mut Shadow) -> io::Result<bool> {
    run_session(shadow, Direction::In)
}

fn run_session(shadow: &mut Shadow, direction: Direction) -> io::Result<bool> {
    let Some(palette) = active() else {
        return Ok(false);
    };
    if !shadow.is_usable() {
        return Ok(false);
    }
    let mut out = io::stdout().lock();
    let mut schedule = Schedule::start(duration(), direction, Instant::now());
    while let Some(t) = schedule.next(Instant::now()) {
        let cursor = cursor_for(direction, schedule.finished());
        out.write_all(&shadow.frame(t, palette, cursor))?;
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

/// Write to stdout and flush, so an escape sequence lands before the next.
fn write_all(bytes: &[u8]) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(bytes)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::Rgb;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    fn palette() -> Palette {
        Palette {
            fg: Rgb(200, 200, 200),
            bg: Rgb(0, 0, 0),
            ansi: [Rgb(0, 0, 0); 16],
        }
    }

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
}
