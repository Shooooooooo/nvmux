//! The picker's passing effects: the afterglow the cursor leaves behind, the
//! glint it lands with, the rows a filter keystroke drops fading out where
//! they stood, the line struck through a name a `[y/N]` is asking about, and
//! the rings that go out from the session you came back to the picker from.
//!
//! All five are a post-pass over a frame [`super::draw`] has already drawn,
//! the way [`crate::fade::apply`] is, and for the same reason: the screen's own
//! drawing stays colourless, and its tests keep saying so. What is passing —
//! which rows, and how far through — is [`App`]'s, moved on by the caller's
//! clock like the trail is; where those rows are on screen is the renderer's,
//! asked through [`draw::row_rect`] so the pass lands on the row the eye sees.
//!
//! # Colour where the terminal said, modifiers where it did not
//!
//! A real fade needs real colours at both ends, so with the terminal's answer
//! to [`crate::palette::query`] and no `NO_COLOR`, all five paint in colour:
//!
//! - the afterglow runs from the selection's look — the foreground as the
//!   background, the background as the foreground — back to the plain row;
//! - the glint is a band a few cells wide that crosses the new selection's
//!   bar from left to right, brightening it towards [`SHINE`] at its middle;
//! - a dropped row's text runs from most of the way to all of the way into
//!   the background, so it is plainly leaving from its first frame;
//! - the bar of a row a `[y/N]` is asking about warms towards the terminal's
//!   own red as the line goes through its name;
//! - each ring fades into the background as it spreads.
//!
//! Without them, each falls back to a modifier, which sets no colour: the
//! afterglow holds the bar, reversed and dim, for the first third of its time
//! and then lets it go; the glint is an underline running under the bar where
//! the band would be; a dropped row is dim until it goes; the struck row is the
//! line alone, which is a modifier to begin with; a ring is dim once it is
//! halfway out. Coarser, and correct under `NO_COLOR` by construction, as
//! everything else here is.
//!
//! The underline on a filter's matched letters is not here, and is not an
//! effect: it is still, a modifier, and says why a row is still on screen, so
//! [`super::draw`] draws it whatever `[effects]` says.

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Frame;

use super::app::App;
use super::draw;
use super::sonar::{self, Sonar};
use crate::palette::{Palette, Rgb};

/// How long a row a filter keystroke dropped takes to fade before the list
/// closes up. Short: the row has already been ruled out, and every
/// millisecond of this is a millisecond the list is longer than the query
/// says. Long enough to see which rows went, which is all it is for.
pub const SIFT: Duration = Duration::from_millis(120);

/// How long the row the cursor leaves glows. Long enough that a quick `j j j`
/// leaves a visible tail, short enough that one key's glow is gone before the
/// next is pressed at an ordinary pace. Fixed, where the fade's length is
/// configurable: the fade is waited on at every switch, and this holds nothing
/// up.
pub const AFTERGLOW: Duration = Duration::from_millis(180);

/// How long the glint takes to cross the row the cursor lands on. A touch
/// longer than the afterglow, so the two read as one movement: the old row
/// letting go while the light runs across the new one. Like the afterglow it
/// holds nothing up, and the next move simply takes it to the next row.
pub const GLINT: Duration = Duration::from_millis(260);

/// What the glint brightens the bar towards: white, the one colour these
/// effects use that the terminal did not report. Its own bright white (ANSI
/// 15) was the obvious alternative and is no good: terminals commonly make it
/// the default foreground itself — the demo's theme does — and a glint towards
/// the bar's own colour shows nothing.
const SHINE: Rgb = Rgb(255, 255, 255);

/// How much of the way to [`SHINE`] the middle of the band goes. Not all of
/// it: the glint is a sheen over the bar, not a hole in it.
const GLINT_PEAK: f32 = 0.9;

/// Half the width of the band, in cells: how far from its middle a cell is
/// still lit at all, the light falling off evenly to nothing there.
const GLINT_REACH: f32 = 2.6;

/// How far off each end of the bar the middle of the band starts and stops,
/// in cells, so it is seen to come on at the left and go off at the right
/// rather than appear and vanish at full strength.
const GLINT_LEAD: f32 = 2.0;

/// How lit a cell has to be for the modifier fallback to underline it: the
/// band's brighter middle only, a few cells wide.
const GLINT_UNDERLINE: f32 = 0.3;

/// How long the line takes to go all the way through a name a `[y/N]` is
/// asking about, and to come back off it when the answer is no. Quick: the
/// question is already on the hint row, and this only says which row it
/// means.
pub const STRIKE: Duration = Duration::from_millis(180);

/// How much of the way towards the terminal's red the struck row's bar goes
/// once the line is all the way across. Most of the way to the bar's own
/// colour is kept, so the row still reads as the selection, warned about.
const STRIKE_WARMTH: f32 = 0.55;

/// The terminal's own red, among the sixteen [`Palette::ansi`].
const RED: usize = 1;

/// How far into the background a ring's colour has gone by the end of its
/// life. Not all the way: by then it is a single dot, and fading that to
/// nothing as well reads as it vanishing early.
const RING_FADE: f32 = 0.85;

/// How much further into the background a ring's arcs are than its edge, so
/// the ring is heaviest level with the row it leaves.
const ARC_FAINTER: f32 = 0.25;

/// Past this part of its life a ring is dim, without a palette.
const RING_FAR: f32 = 0.45;

/// The part of the afterglow the modifier fallback holds the bar for.
const FALLBACK_HOLD: f32 = 1.0 / 3.0;

/// How far into the background a dropped row starts. Not zero: the row should
/// read as leaving on the very frame the keystroke lands.
const SIFT_FROM: f32 = 0.3;

/// The palette the effects paint with, or `None` to fall back to modifiers:
/// none when `NO_COLOR` is set or the terminal did not say what its colours
/// are.
pub fn palette() -> Option<&'static Palette> {
    if std::env::var_os("NO_COLOR").is_some() {
        return None;
    }
    crate::palette::get()
}

/// Whether the config wants an effect that paints in colour, so the terminal
/// should be asked what its colours are even with the fade switched off.
/// `NO_COLOR` answers no for all of them, as it does for the fade.
pub fn want_palette() -> bool {
    let effects = &crate::config::get().effects;
    let wanted = effects.cursor_enabled()
        || effects.filter_enabled()
        || effects.kill_enabled()
        || effects.back_enabled();
    wanted && std::env::var_os("NO_COLOR").is_none()
}

/// Paint whatever is passing over the frame `draw::draw` has just drawn.
pub fn paint(frame: &mut Frame, app: &App, palette: Option<&Palette>) {
    let area = frame.area();
    let buf = frame.buffer_mut();
    let selected = app.selected_row_id();

    for (id, progress) in app.glows() {
        if selected == Some(id) {
            continue;
        }
        if let Some(rect) = draw::row_rect(app, area, id) {
            glow(buf, rect, palette, progress);
        }
    }

    if let Some((id, progress)) = app.glint() {
        // The bar is the selection's: a glint left on a row the cursor has
        // since left would be lighting a plain row.
        if selected == Some(id) {
            if let Some(rect) = draw::row_rect(app, area, id) {
                glint(buf, rect, palette, progress);
            }
        }
    }

    if let Some(progress) = app.leaving() {
        for row in app.rows().iter().filter(|r| r.leaving) {
            if let Some(rect) = draw::row_rect(app, area, &row.session.id) {
                leave(buf, rect, palette, progress);
            }
        }
    }

    if let Some((id, drawn)) = app.strike() {
        if let (Some(bar), Some(name)) = (
            draw::row_rect(app, area, id),
            draw::name_rect(app, area, id),
        ) {
            // The warmth is the bar's, so only while the row is the selection:
            // a line still being drawn back off a row the cursor has since
            // left goes off a plain row.
            let warm = palette.filter(|_| selected == Some(id));
            strike(buf, bar, name, warm, drawn);
        }
    }

    // Last, so the rings find the screen as everything else left it, and only
    // ever take what is still the terminal's own background.
    if let Some((id, sonar)) = app.sonar() {
        if let Some(rect) = draw::row_rect(app, area, id) {
            rings(buf, app, area, rect, sonar, palette);
        }
    }
}

/// The row the cursor left, `progress` of the way back to plain.
fn glow(buf: &mut Buffer, rect: Rect, palette: Option<&Palette>, progress: f32) {
    match palette {
        Some(p) => {
            let t = ease_out(progress);
            let bg = rgb(p.fg.lerp(p.bg, t));
            let fg = rgb(p.bg.lerp(p.fg, t));
            each_cell(buf, rect, |cell| {
                cell.set_bg(bg);
                cell.set_fg(fg);
            });
        }
        None if progress < FALLBACK_HOLD => {
            each_cell(buf, rect, |cell| {
                cell.modifier.insert(Modifier::REVERSED | Modifier::DIM);
            });
        }
        None => {}
    }
}

/// The bar the cursor has just landed on, with the glint `progress` of the way
/// across it. `rect` is the bar: [`draw::row_rect`] measures a row as far as
/// the pad past its name, which is as far as the selection's reversed bar
/// runs.
fn glint(buf: &mut Buffer, rect: Rect, palette: Option<&Palette>, progress: f32) {
    let middle = -GLINT_LEAD + ease_in_out(progress) * (f32::from(rect.width) + 2.0 * GLINT_LEAD);
    let on_screen = rect.intersection(buf.area);
    for x in on_screen.x..on_screen.x + on_screen.width {
        // From the bar's own left edge, not the screen's, and to the middle of
        // the cell.
        let along = f32::from(x - rect.x) + 0.5;
        let lit = 1.0 - (along - middle).abs() / GLINT_REACH;
        if lit <= 0.0 {
            continue;
        }
        let cell = &mut buf[(x, rect.y)];
        match palette {
            // The bar is the reversed default colours. Painting it means
            // setting both ends outright, the background now the brightened
            // foreground, so the reverse comes off first.
            Some(p) => {
                cell.modifier.remove(Modifier::REVERSED);
                cell.set_bg(rgb(p.fg.lerp(SHINE, GLINT_PEAK * lit)));
                cell.set_fg(rgb(p.bg));
            }
            None if lit > GLINT_UNDERLINE => {
                cell.modifier.insert(Modifier::UNDERLINED);
            }
            None => {}
        }
    }
}

/// The row a `[y/N]` is asking about: a line through its name, `drawn` of
/// the way across, and — given a palette — its bar warmed towards the
/// terminal's red by as much. The line is the crossed-out modifier, drawn in
/// the text's own colour, so it is the same with a palette or without one.
fn strike(buf: &mut Buffer, bar: Rect, name: Rect, palette: Option<&Palette>, drawn: f32) {
    let k = ease_out(drawn);
    let across = ((k * f32::from(name.width)).ceil() as u16).min(name.width);
    let line = Rect {
        width: across,
        ..name
    };
    each_cell(buf, line, |cell| {
        cell.modifier.insert(Modifier::CROSSED_OUT)
    });
    if let Some(p) = palette {
        let warm = rgb(p.fg.lerp(p.ansi[RED], STRIKE_WARMTH * k));
        let text = rgb(p.bg);
        each_cell(buf, bar, |cell| {
            cell.modifier.remove(Modifier::REVERSED);
            cell.set_bg(warm);
            cell.set_fg(text);
        });
    }
}

/// The rings going out from the row at `rect` (see [`super::sonar`]): each
/// ring's edge on the row's own line past both its ends, and once it is clear
/// of the row, its arcs on the lines above and below. Laid only on the
/// terminal's own background — never on a row, the hint row, or a glyph
/// something else has put there — and not past the edge of the screen.
fn rings(
    buf: &mut Buffer,
    app: &App,
    area: Rect,
    rect: Rect,
    sonar: &Sonar,
    palette: Option<&Palette>,
) {
    let left = |out: u16| rect.x.checked_sub(1 + out);
    let right = |out: u16| rect.x.checked_add(rect.width + out);
    // Put `glyph` at a cell, if it is on the screen and is the terminal's own
    // background.
    let mut lay = |x: Option<u16>, y: Option<u16>, glyph: char, style: Style| {
        if let (Some(x), Some(y)) = (x, y) {
            if ground(buf, app, area, x, y) {
                buf[(x, y)].set_char(glyph).set_style(style);
            }
        }
    };
    for ring in sonar.rings() {
        let edge = ring_style(palette, RING_FADE * ring.life, ring.life > RING_FAR);
        lay(left(ring.out), Some(rect.y), ring.left, edge);
        lay(right(ring.out), Some(rect.y), ring.right, edge);
        let Some(arc) = ring.arc() else {
            continue;
        };
        let faint = ring_style(palette, RING_FADE * ring.life + ARC_FAINTER, true);
        for (y, (l, r)) in [
            (rect.y.checked_sub(1), sonar::ABOVE),
            (rect.y.checked_add(1), sonar::BELOW),
        ] {
            lay(left(arc), y, l, faint);
            lay(right(arc), y, r, faint);
        }
    }
}

/// How a ring's glyph is drawn: `fade` of the way into the background with a
/// palette, and without one, dim if `dim` says so.
fn ring_style(palette: Option<&Palette>, fade: f32, dim: bool) -> Style {
    match palette {
        Some(p) => Style::default().fg(rgb(p.fg.lerp(p.bg, fade))),
        None if dim => Style::default().add_modifier(Modifier::DIM),
        None => Style::default(),
    }
}

/// Whether (`x`, `y`) is the terminal's own background around the list: on
/// the screen above the hint row, blank, painted by nothing, and on no row as
/// drawn — a blank inside a name like `session 3` is the row's, not the
/// background's.
fn ground(buf: &Buffer, app: &App, area: Rect, x: u16, y: u16) -> bool {
    let screen = buf.area.intersection(area);
    let above_hint = screen.y + screen.height.saturating_sub(1);
    if x < screen.x || x >= screen.x + screen.width || y < screen.y || y >= above_hint {
        return false;
    }
    let cell = &buf[(x, y)];
    cell.symbol() == " "
        && cell.bg == Color::Reset
        && !cell.modifier.contains(Modifier::REVERSED)
        && !draw::on_a_row(app, area, x, y)
}

/// A row a filter keystroke dropped, `progress` of the way to gone.
fn leave(buf: &mut Buffer, rect: Rect, palette: Option<&Palette>, progress: f32) {
    match palette {
        Some(p) => {
            let t = SIFT_FROM + (1.0 - SIFT_FROM) * ease_out(progress);
            let fg = rgb(p.fg.lerp(p.bg, t));
            each_cell(buf, rect, |cell| {
                cell.set_fg(fg);
            });
        }
        None => {
            each_cell(buf, rect, |cell| {
                cell.modifier.insert(Modifier::DIM);
            });
        }
    }
}

/// Every cell of `rect` that is on the buffer.
fn each_cell(buf: &mut Buffer, rect: Rect, mut f: impl FnMut(&mut ratatui::buffer::Cell)) {
    let rect = rect.intersection(buf.area);
    for y in rect.y..rect.y + rect.height {
        for x in rect.x..rect.x + rect.width {
            f(&mut buf[(x, y)]);
        }
    }
}

fn rgb(c: Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

/// Quick at first and settling, so the change is seen on the first frames and
/// the end is soft.
fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t)
}

/// Slow off the mark, quick through the middle and slow to a stop, so the
/// glint is seen arriving and leaving and sweeps the name in between.
fn ease_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        2.0 * t * t
    } else {
        1.0 - 2.0 * (1.0 - t) * (1.0 - t)
    }
}

#[cfg(test)]
mod tests {
    use super::super::app::Key;
    use super::super::test_support::{self, picker};
    use super::*;
    use crate::test_support::palette as test_palette;

    const W: u16 = 40;
    const H: u16 = 8;

    /// Draw `app` and paint its effects, the way the picker does.
    fn frame(app: &App, palette: Option<&Palette>) -> Buffer {
        test_support::buffer(W, H, |f| {
            draw::draw(f, app);
            paint(f, app, palette);
        })
    }

    fn row_of(buf: &Buffer, name: &str) -> u16 {
        (0..buf.area.height)
            .find(|y| {
                let line: String = (0..buf.area.width).map(|x| buf[(x, *y)].symbol()).collect();
                line.contains(name)
            })
            .unwrap_or_else(|| panic!("{name} not drawn"))
    }

    fn first_glyph(buf: &Buffer, y: u16) -> &ratatui::buffer::Cell {
        (0..buf.area.width)
            .map(|x| &buf[(x, y)])
            .find(|c| c.symbol().trim() != "")
            .expect("something on the row")
    }

    /// A move leaves the row it came from looking selected, in colour, and the
    /// glow is gone once its time is up.
    #[test]
    fn the_row_the_cursor_left_glows_and_then_settles() {
        let p = test_palette();
        let mut a = picker(&["one", "two", "three"]);
        a.on_key(Key::Char('j'));

        let buf = frame(&a, Some(&p));
        let y = row_of(&buf, "one");
        let cell = first_glyph(&buf, y);
        assert_eq!(cell.bg, rgb(p.fg), "starts as the selection's bar");
        assert_eq!(cell.fg, rgb(p.bg));

        a.tick(Duration::from_millis(90));
        let buf = frame(&a, Some(&p));
        let cell = first_glyph(&buf, row_of(&buf, "one"));
        let Color::Rgb(r, ..) = cell.bg else {
            panic!("halfway, the bar is still painted: {cell:?}")
        };
        assert!(r > p.bg.0 && r < p.fg.0, "halfway between: {r}");

        a.tick(Duration::from_secs(1));
        let buf = frame(&a, Some(&p));
        let cell = first_glyph(&buf, row_of(&buf, "one"));
        assert_eq!(cell.bg, Color::Reset, "settled: {cell:?}");
        assert_eq!(cell.fg, Color::Reset);
        assert!(!a.animating(), "nothing left to animate");
    }

    /// The row now selected never glows, even if it was glowing a moment ago:
    /// moving back onto a row takes its glow away.
    #[test]
    fn moving_back_onto_a_glowing_row_ends_its_glow() {
        let mut a = picker(&["one", "two"]);
        a.on_key(Key::Char('j'));
        a.on_key(Key::Char('k'));
        let ids: Vec<&str> = a.glows().map(|(id, _)| id).collect();
        assert_eq!(ids.len(), 1, "{ids:?}");
        assert_ne!(Some(ids[0]), a.selected_row_id());
    }

    /// Without a palette the glow is a modifier: reversed and dim for its first
    /// third, and nothing after — and no colour at any point.
    #[test]
    fn without_a_palette_the_glow_is_a_held_bar() {
        let mut a = picker(&["one", "two", "three"]);
        a.on_key(Key::Char('j'));
        let buf = frame(&a, None);
        let cell = first_glyph(&buf, row_of(&buf, "one"));
        assert!(cell.modifier.contains(Modifier::REVERSED | Modifier::DIM));
        test_support::assert_no_colour(W, H, |f| {
            draw::draw(f, &a);
            paint(f, &a, None);
        });

        a.tick(Duration::from_millis(100));
        let buf = frame(&a, None);
        let cell = first_glyph(&buf, row_of(&buf, "one"));
        assert!(!cell.modifier.contains(Modifier::REVERSED), "let go");
    }

    /// The column of the brightest painted cell on row `y`: the middle of the
    /// glint's band, if it is on the row.
    fn brightest(buf: &Buffer, y: u16) -> Option<u16> {
        (0..buf.area.width)
            .filter_map(|x| match buf[(x, y)].bg {
                Color::Rgb(r, ..) => Some((x, r)),
                _ => None,
            })
            .max_by_key(|(_, r)| *r)
            .map(|(x, _)| x)
    }

    /// The row the cursor lands on has a band of light crossing its bar,
    /// brighter than the bar, over the bar's own reversed look elsewhere; and
    /// once it has crossed, the bar is plain again.
    #[test]
    fn the_row_the_cursor_lands_on_is_crossed_by_a_glint() {
        let p = test_palette();
        let mut a = picker(&["api-server", "dotfiles", "notes"]);
        a.on_key(Key::Char('j'));
        a.tick(GLINT / 2);

        let buf = frame(&a, Some(&p));
        let y = row_of(&buf, "dotfiles");
        let x = brightest(&buf, y).expect("the band is on the bar");
        let cell = &buf[(x, y)];
        let Color::Rgb(r, ..) = cell.bg else {
            unreachable!("found by its colour")
        };
        assert!(r > p.fg.0, "brighter than the bar: {cell:?}");
        assert_eq!(cell.fg, rgb(p.bg), "the name keeps the bar's text colour");
        assert!(!cell.modifier.contains(Modifier::REVERSED), "{cell:?}");
        assert!(
            (0..W).any(|x| buf[(x, y)].modifier.contains(Modifier::REVERSED)),
            "away from the band the bar is untouched"
        );

        a.tick(GLINT);
        let buf = frame(&a, Some(&p));
        let y = row_of(&buf, "dotfiles");
        assert!(
            (0..W).all(|x| buf[(x, y)].bg == Color::Reset),
            "crossed and gone"
        );
        assert!(a.glint().is_none());
        assert!(!a.animating(), "nothing left to animate");
    }

    /// The band runs left to right.
    #[test]
    fn the_glint_runs_left_to_right() {
        let p = test_palette();
        let mut a = picker(&["api-server", "dotfiles", "notes"]);
        a.on_key(Key::Char('j'));
        a.tick(GLINT / 4);
        let buf = frame(&a, Some(&p));
        let early = brightest(&buf, row_of(&buf, "dotfiles")).expect("lit early on");
        a.tick(GLINT / 2);
        let buf = frame(&a, Some(&p));
        let late = brightest(&buf, row_of(&buf, "dotfiles")).expect("lit later on");
        assert!(late > early, "{early} and then {late}");
    }

    /// Without a palette the glint is a short underline running under the bar,
    /// and no colour at any point.
    #[test]
    fn without_a_palette_the_glint_is_an_underline() {
        let mut a = picker(&["api-server", "dotfiles", "notes"]);
        a.on_key(Key::Char('j'));
        a.tick(GLINT / 2);
        let buf = frame(&a, None);
        let y = row_of(&buf, "dotfiles");
        let under = (0..W)
            .filter(|x| buf[(*x, y)].modifier.contains(Modifier::UNDERLINED))
            .count();
        assert!(under > 0 && under < 6, "a short run: {under} cells");
        test_support::assert_no_colour(W, H, |f| {
            draw::draw(f, &a);
            paint(f, &a, None);
        });

        a.tick(GLINT);
        let buf = frame(&a, None);
        let y = row_of(&buf, "dotfiles");
        assert!(
            !(0..W).any(|x| buf[(x, y)].modifier.contains(Modifier::UNDERLINED)),
            "gone once it has crossed"
        );
    }

    /// One glint at a time, on the selection: the next move takes it to the
    /// next row and starts it over there.
    #[test]
    fn the_next_move_takes_the_glint_with_it() {
        let mut a = picker(&["one", "two", "three"]);
        a.on_key(Key::Char('j'));
        a.tick(GLINT / 2);
        a.on_key(Key::Char('j'));
        let (id, progress) = a.glint().expect("a glint");
        assert_eq!(Some(id), a.selected_row_id());
        assert_eq!(progress, 0.0, "starting over on the new row");
    }

    /// A `[y/N]` or a session picked up takes the bar over, and the glint goes
    /// with it.
    #[test]
    fn a_question_or_a_pick_up_ends_the_glint() {
        let mut a = picker(&["one", "two", "three"]);
        a.on_key(Key::Char('j'));
        a.on_key(Key::Char('x'));
        assert!(a.glint().is_none(), "under a [y/N]");
        a.on_key(Key::Char('n'));
        assert!(a.glint().is_none(), "not back once it is answered");
        a.on_key(Key::Char('j'));
        assert!(a.glint().is_some());
        a.on_key(Key::Char(' '));
        assert!(a.glint().is_none(), "in flight");
    }

    /// A keystroke that drops rows leaves them on screen, fading, until their
    /// time is up; then the list closes up.
    #[test]
    fn dropped_rows_fade_where_they_stood_and_then_go() {
        let p = test_palette();
        let mut a = picker(&["api-server", "dotfiles", "notes"]);
        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('n'));

        let buf = frame(&a, Some(&p));
        let y = row_of(&buf, "dotfiles");
        let fg = first_glyph(&buf, y).fg;
        assert_eq!(fg, rgb(p.fg.lerp(p.bg, SIFT_FROM)), "fading from the start");
        let kept = first_glyph(&buf, row_of(&buf, "notes"));
        assert_eq!(kept.fg, Color::Reset, "a kept row is untouched");

        a.tick(SIFT);
        let lines = test_support::render(W, H, |f| draw::draw(f, &a));
        assert!(
            !lines.iter().any(|l| l.contains("dotfiles")),
            "gone once its time is up: {lines:#?}"
        );
    }

    /// Without a palette, a dropped row is dim instead.
    #[test]
    fn without_a_palette_a_dropped_row_is_dim() {
        let mut a = picker(&["api-server", "dotfiles", "notes"]);
        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('n'));
        let buf = frame(&a, None);
        let cell = first_glyph(&buf, row_of(&buf, "dotfiles"));
        assert!(cell.modifier.contains(Modifier::DIM));
        assert_eq!(cell.fg, Color::Reset);
    }

    /// The next key ends the last key's fade at once: a fast typist never
    /// waits on rows the query ruled out two letters ago.
    #[test]
    fn the_next_key_ends_the_last_keys_fade() {
        let mut a = picker(&["api-server", "dotfiles", "notes", "nvmux"]);
        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('n'));
        assert!(a.leaving().is_some());
        a.on_key(Key::Char('v'));
        let leaving: Vec<&str> = a
            .rows()
            .iter()
            .filter(|r| r.leaving)
            .map(|r| r.session.name.as_str())
            .collect();
        assert_eq!(leaving, ["notes"], "only what this key dropped");
    }

    /// A session in flight carries the cursor with it, and that is the trail's
    /// to show: moving it leaves no glow behind.
    #[test]
    fn moving_a_session_in_flight_leaves_no_glow() {
        let mut a = picker(&["one", "two", "three"]);
        a.on_key(Key::Char(' '));
        a.on_key(Key::Char('j'));
        a.on_key(Key::Char('j'));
        assert_eq!(a.glows().count(), 0);
        assert!(a.glint().is_none());
    }

    /// With the picker's effects off, a move and a filter keystroke leave
    /// nothing behind, and the picker never asks for a frame's pace.
    #[test]
    fn with_the_effects_off_nothing_passes() {
        let mut a = picker(&["api-server", "dotfiles", "notes"]);
        a.set_effects(false);
        a.on_key(Key::Char('j'));
        assert_eq!(a.glows().count(), 0);
        assert!(a.glint().is_none());
        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('n'));
        assert!(a.leaving().is_none());
        assert!(a.rows().iter().all(|r| !r.leaving));
        a.on_key(Key::Esc);
        a.on_key(Key::Char('x'));
        assert!(a.strike().is_none(), "no line through the name");
        a.on_key(Key::Char('n'));
        a.set_came_from("id000000");
        assert!(a.sonar().is_none(), "no rings");
        assert!(!a.animating());
    }

    // --- the strike ---------------------------------------------------------

    /// [`picker`] with the second row selected and `x` pressed on it, the line
    /// `ms` milliseconds along.
    fn asking(ms: u64) -> App {
        let mut a = picker(&["api-server", "dotfiles", "notes"]);
        a.on_key(Key::Char('j'));
        a.on_key(Key::Char('x'));
        a.tick(Duration::from_millis(ms));
        a
    }

    /// The column `needle` starts at on row `y`, in cells rather than bytes:
    /// the marker before a name is three bytes and one cell.
    fn column_of(buf: &Buffer, y: u16, needle: &str) -> u16 {
        let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        let at = line.find(needle).expect("drawn");
        line[..at].chars().count() as u16
    }

    /// The cells of row `y` that are crossed out.
    fn crossed(buf: &Buffer, y: u16) -> Vec<u16> {
        (0..buf.area.width)
            .filter(|x| buf[(*x, y)].modifier.contains(Modifier::CROSSED_OUT))
            .collect()
    }

    /// While the `[y/N]` is up a line goes through the name — the name only,
    /// not the marker or the number — and, in colour, the bar warms towards
    /// the terminal's red.
    #[test]
    fn a_name_asked_about_is_struck_through() {
        let p = test_palette();
        let a = asking(STRIKE.as_millis() as u64);
        let buf = frame(&a, Some(&p));
        let y = row_of(&buf, "dotfiles");
        let name = column_of(&buf, y, "dotfiles");
        assert_eq!(crossed(&buf, y), (name..name + 8).collect::<Vec<_>>());

        let warm = rgb(p.fg.lerp(p.ansi[RED], STRIKE_WARMTH));
        let marker = first_glyph(&buf, y);
        assert_eq!(marker.bg, warm, "{marker:?}");
        assert_eq!(marker.fg, rgb(p.bg));
        assert!(!marker.modifier.contains(Modifier::REVERSED));
    }

    /// The line is drawn across from the left: partway, only the start of the
    /// name is crossed out.
    #[test]
    fn the_line_goes_through_from_the_left() {
        let a = asking(STRIKE.as_millis() as u64 / 4);
        let buf = frame(&a, None);
        let y = row_of(&buf, "dotfiles");
        let first = crossed(&buf, y);
        assert!(!first.is_empty() && first.len() < 8, "{first:?}");
        assert_eq!(first[0], column_of(&buf, y, "dotfiles"));
    }

    /// Without a palette the line is all there is: the bar stays reversed,
    /// and no colour is set.
    #[test]
    fn without_a_palette_the_strike_is_the_line_alone() {
        let a = asking(STRIKE.as_millis() as u64);
        let buf = frame(&a, None);
        let y = row_of(&buf, "dotfiles");
        assert_eq!(crossed(&buf, y).len(), 8);
        assert!(first_glyph(&buf, y).modifier.contains(Modifier::REVERSED));
        test_support::assert_no_colour(W, H, |f| {
            draw::draw(f, &a);
            paint(f, &a, None);
        });
    }

    /// Once the line is all the way across, the question waits without
    /// asking for frames.
    #[test]
    fn a_line_all_the_way_across_is_still() {
        let a = asking(STRIKE.as_millis() as u64);
        assert!(!a.animating());
        assert!(asking(10).animating());
    }

    /// Any answer but `y` draws the line back off, from wherever it had got
    /// to, and then it is gone.
    #[test]
    fn declining_draws_the_line_back_off() {
        let mut a = asking(STRIKE.as_millis() as u64);
        a.on_key(Key::Char('n'));
        assert!(a.animating(), "on its way back");
        a.tick(STRIKE / 2);
        let buf = frame(&a, None);
        let partway = crossed(&buf, row_of(&buf, "dotfiles")).len();
        assert!(partway > 0 && partway < 8, "{partway}");
        a.tick(STRIKE);
        assert!(a.strike().is_none());
        let buf = frame(&a, None);
        assert!(crossed(&buf, row_of(&buf, "dotfiles")).is_empty());
        assert!(!a.animating());
    }

    /// `y` hands the row over to the backspace: the line does not linger to
    /// be drawn back.
    #[test]
    fn confirming_ends_the_line_at_once() {
        let mut a = asking(STRIKE.as_millis() as u64);
        a.on_key(Key::Char('y'));
        assert!(a.strike().is_none());
    }

    // --- the sonar ----------------------------------------------------------

    fn is_braille(symbol: &str) -> bool {
        symbol
            .chars()
            .next()
            .is_some_and(|c| (0x2801..=0x28ff).contains(&(c as u32)))
    }

    /// `names` in the picker, opened back over the session at `index` the way
    /// `<prefix> Space` opens it, `ms` milliseconds ago.
    fn back_over(names: &[&str], index: usize, ms: u64) -> App {
        let mut a = picker(names);
        let id = format!("id{index:06}");
        a.select_session(&id);
        a.set_came_from(&id);
        a.tick(Duration::from_millis(ms));
        a
    }

    /// Rings go out past both ends of the row you came back from, and once
    /// they are clear of it, over the lines above and below as well.
    #[test]
    fn rings_go_out_from_the_session_came_back_from() {
        let p = test_palette();
        let a = back_over(&["api-server", "dotfiles", "notes"], 1, 200);
        let buf = frame(&a, Some(&p));
        let y = row_of(&buf, "dotfiles");
        let start = column_of(&buf, y, "▸");
        let end = start + "▸ 2  dotfiles".chars().count() as u16;
        let braille_at = |y: u16| -> Vec<u16> {
            (0..W)
                .filter(|x| is_braille(buf[(*x, y)].symbol()))
                .collect()
        };
        let on_row = braille_at(y);
        assert!(
            on_row.iter().any(|x| *x < start),
            "left of the row: {on_row:?}"
        );
        assert!(
            on_row.iter().any(|x| *x >= end),
            "right of the row: {on_row:?}"
        );
        assert!(!braille_at(y - 1).is_empty(), "an arc above");
        assert!(!braille_at(y + 1).is_empty(), "an arc below");
        let ring = &buf[(on_row[0], y)];
        assert!(
            matches!(ring.fg, Color::Rgb(..)),
            "faded in colour: {ring:?}"
        );
    }

    /// A ring never lands on a row — not even in the blank inside a name — and
    /// never on the hint row.
    #[test]
    fn rings_never_land_on_a_row() {
        let names = ["session 3 is long", "x", "y"];
        for ms in (0..sonar::LENGTH.as_millis() as u64).step_by(5) {
            let a = back_over(&names, 1, ms);
            let lines = test_support::render(W, H, |f| {
                draw::draw(f, &a);
                paint(f, &a, None);
            });
            assert!(
                lines.iter().any(|l| l.contains("session 3 is long")),
                "a name broken into at {ms} ms: {lines:#?}"
            );
            let hint = lines.last().expect("a hint row");
            assert!(
                !hint.chars().any(|c| is_braille(&c.to_string())),
                "on the hint row at {ms} ms"
            );
        }
    }

    /// Without a palette the rings are braille and the dim modifier, and no
    /// colour is set at any point.
    #[test]
    fn without_a_palette_rings_are_dim_braille() {
        for ms in (0..sonar::LENGTH.as_millis() as u64).step_by(20) {
            let a = back_over(&["api-server", "dotfiles", "notes"], 1, ms);
            test_support::assert_no_colour(W, H, |f| {
                draw::draw(f, &a);
                paint(f, &a, None);
            });
        }
        let late = back_over(&["api-server", "dotfiles", "notes"], 1, 280);
        let buf = frame(&late, None);
        assert!(
            buf.content
                .iter()
                .any(|c| is_braille(c.symbol()) && c.modifier.contains(Modifier::DIM)),
            "dim far out"
        );
    }

    /// The rings run their course and stop asking for frames — or stop at
    /// once on the first key, which has seen them or moved on.
    #[test]
    fn rings_end_on_time_or_on_a_key() {
        let done = back_over(&["one", "two"], 1, sonar::LENGTH.as_millis() as u64);
        assert!(done.sonar().is_none());
        assert!(!done.animating());

        let mut a = back_over(&["one", "two"], 1, 50);
        assert!(a.sonar().is_some());
        a.on_key(Key::Char('j'));
        assert!(a.sonar().is_none(), "a key ends them");
    }

    /// A session that has gone from the list sends out no rings: there is no
    /// row to send them from.
    #[test]
    fn no_rings_from_a_session_not_listed() {
        let mut a = picker(&["one", "two"]);
        a.set_came_from("gone");
        assert!(a.sonar().is_none());
    }
}
