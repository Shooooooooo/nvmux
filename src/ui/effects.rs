//! The picker's transient effects, painted over a frame [`super::draw`] has
//! drawn (like [`crate::fade::apply`], so drawing itself stays colourless):
//! the afterglow on the row the cursor left (also on rows a session in flight
//! crosses, [`super::swap`], and in [`super::settings`]), rows a filter drops
//! fading in place, a line struck through the name a `[y/N]` asks about, and
//! the row in the gap `c` opens rising out of the background. Progress lives
//! in [`App`]; positions come from [`draw::row_rect`].
//!
//! With the terminal's palette ([`crate::palette::query`]) and no `NO_COLOR`
//! they fade between real colours, the strike warming the bar toward the
//! terminal's red. Otherwise they fall back to modifiers: the glow is reversed
//! and dim for its first third, a dropped row and the gap row are dim, and the
//! strike is the crossed-out line alone.
//!
//! Under a theme ([`crate::theme`]) the frame arrives coloured; effects fade
//! each cell to the colour it is drawn in ([`crate::fade::drawn_in`]) and draw
//! their own marks in the ink.
//!
//! The filter's match underline is static and drawn by [`super::draw`] even
//! with `[effects]` off. [`rings`] ([`super::sonar`]) is unused but kept,
//! tested, for later.

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Frame;

use super::app::{App, GHOST_ID};
use super::draw;
use super::sonar::{self, Sonar};
use crate::fade::drawn_in;
use crate::palette::{Palette, Rgb};

/// Fade time for rows a filter keystroke drops, before the list closes up.
/// Short: meanwhile the list is longer than the query says.
pub const SIFT: Duration = Duration::from_millis(120);

/// Time for the gap row `c` opens to rise from the background (see
/// [`App::make_room`]). Waited on before every create prompt, so kept short.
pub const ROOM: Duration = Duration::from_millis(130);

/// How long the gap is shown before the prompt when fades are off (the row is
/// dim at once); a little longer than [`ROOM`].
pub const ROOM_HELD: Duration = Duration::from_millis(200);

/// Glow time for the row the cursor left. Only that row glows, so a held key
/// moves the glow instead of leaving a tail. Fixed, unlike the fade's length,
/// since nothing waits on it.
pub const AFTERGLOW: Duration = Duration::from_millis(100);

/// Time for the strike line to cross a name, or to retract on a no.
pub const STRIKE: Duration = Duration::from_millis(180);

/// Fraction of the way to red the struck bar goes, so the row still reads as
/// the selection.
const STRIKE_WARMTH: f32 = 0.55;

/// Red's index in [`Palette::ansi`].
const RED: usize = 1;

/// A ring's fade toward the background at the end of its life. Not 1: its
/// final dot would seem to vanish early.
const RING_FADE: f32 = 0.85;

/// Extra fade of a ring's arcs over its edge, so it is heaviest level with the
/// row.
const ARC_FAINTER: f32 = 0.25;

/// Life fraction past which a ring is dim, without a palette.
const RING_FAR: f32 = 0.45;

/// Fraction of the afterglow the modifier fallback holds the bar for.
const FALLBACK_HOLD: f32 = 1.0 / 3.0;

/// A dropped row's starting fade; nonzero so it reads as leaving on the first
/// frame.
const SIFT_FROM: f32 = 0.3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colours {
    /// The terminal's: the background effects fade into, and the foreground of
    /// uncoloured cells (all cells with no theme, dim ones with one).
    pub palette: Palette,
    /// The theme's ink, or `None` for no theme (see [`crate::theme`]).
    pub theme: Option<Rgb>,
}

impl Colours {
    /// Colour of full-strength text, the selection bar and effects' own marks:
    /// the theme's ink, else the terminal's foreground.
    fn ink(&self) -> Rgb {
        self.theme.unwrap_or(self.palette.fg)
    }
}

/// `None` (use modifiers) under `NO_COLOR` or when the terminal did not report
/// its colours.
pub fn colours() -> Option<Colours> {
    if std::env::var_os("NO_COLOR").is_some() {
        return None;
    }
    crate::palette::get().map(|palette| Colours {
        palette: *palette,
        theme: crate::theme::ink(),
    })
}

/// Whether an enabled effect paints in colour, so the palette must be queried
/// even with the fade off. False under `NO_COLOR`.
pub fn want_palette() -> bool {
    let effects = crate::config::get().effects;
    let wanted = effects.cursor_enabled()
        || effects.move_enabled()
        || effects.filter_enabled()
        || effects.kill_enabled()
        || effects.create_enabled();
    wanted && std::env::var_os("NO_COLOR").is_none()
}

/// Paint current effects over a frame from `draw::draw` (and, under a theme,
/// [`crate::theme::paint`]).
pub fn paint(frame: &mut Frame, app: &App, colours: Option<&Colours>) {
    let area = frame.area();
    let buf = frame.buffer_mut();
    let selected = app.selected_row_id();

    if let Some((id, progress)) = app.glow() {
        if selected != Some(id) {
            if let Some(rect) = draw::row_rect(app, area, id) {
                glow(buf, rect, colours, progress);
            }
        }
    }

    // Rows a session in flight crossed get the same glow.
    for (id, progress) in app.swap().echoes() {
        if selected != Some(id) {
            if let Some(rect) = draw::row_rect(app, area, id) {
                glow(buf, rect, colours, progress);
            }
        }
    }

    if let Some(progress) = app.leaving() {
        for row in app.rows().iter().filter(|r| r.leaving) {
            if let Some(rect) = draw::row_rect(app, area, &row.session.id) {
                leave(buf, rect, colours, progress);
            }
        }
    }

    if let Some((id, drawn)) = app.strike() {
        if let (Some(bar), Some(name)) = (
            draw::row_rect(app, area, id),
            draw::name_rect(app, area, id),
        ) {
            // Warm only while the row is selected: a line retracting from a
            // row the cursor left is drawn on a plain row.
            let warm = colours.filter(|_| selected == Some(id));
            strike(buf, bar, name, warm, drawn);
        }
    }

    if let (Some(progress), Some(c)) = (app.room(), colours) {
        if let Some(rect) = draw::row_rect(app, area, GHOST_ID) {
            arrive(buf, rect, c, progress);
        }
    }
}

/// The row the cursor left, `progress` of the way back to plain: the bar fades
/// into the background and text rises to its plain colour. Also used by
/// [`super::settings`].
pub(super) fn glow(buf: &mut Buffer, rect: Rect, colours: Option<&Colours>, progress: f32) {
    match colours {
        Some(c) => {
            let t = ease_out(progress);
            let p = &c.palette;
            let bar = rgb(c.ink().lerp(p.bg, t));
            each_cell(buf, rect, |cell| {
                let text = rgb(p.bg.lerp(drawn_in(cell, p), t));
                cell.set_bg(bar);
                cell.set_fg(text);
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

/// Cross out the name `drawn` of the way across (the same with or without a
/// palette) and, with colours, warm the bar from the ink toward red by as much.
fn strike(buf: &mut Buffer, bar: Rect, name: Rect, colours: Option<&Colours>, drawn: f32) {
    let k = ease_out(drawn);
    let across = ((k * f32::from(name.width)).ceil() as u16).min(name.width);
    let line = Rect {
        width: across,
        ..name
    };
    each_cell(buf, line, |cell| {
        cell.modifier.insert(Modifier::CROSSED_OUT)
    });
    if let Some(c) = colours {
        let warm = rgb(c.ink().lerp(c.palette.ansi[RED], STRIKE_WARMTH * k));
        let text = rgb(c.palette.bg);
        each_cell(buf, bar, |cell| {
            cell.modifier.remove(Modifier::REVERSED);
            cell.set_bg(warm);
            cell.set_fg(text);
        });
    }
}

/// Draw [`super::sonar`] rings from the row at `rect`: edges on the row's line
/// past both ends, arcs above and below once clear of the row, only on bare
/// background ([`ground`]). Call it last. Currently unused.
#[allow(dead_code)]
fn rings(
    buf: &mut Buffer,
    app: &App,
    area: Rect,
    rect: Rect,
    sonar: &Sonar,
    colours: Option<&Colours>,
) {
    let left = |out: u16| rect.x.checked_sub(1 + out);
    let right = |out: u16| rect.x.checked_add(rect.width + out);
    // Only on bare background within the screen.
    let mut lay = |x: Option<u16>, y: Option<u16>, glyph: char, style: Style| {
        if let (Some(x), Some(y)) = (x, y) {
            if ground(buf, app, area, x, y) {
                buf[(x, y)].set_char(glyph).set_style(style);
            }
        }
    };
    for ring in sonar.rings() {
        let edge = ring_style(colours, RING_FADE * ring.life, ring.life > RING_FAR);
        lay(left(ring.out), Some(rect.y), ring.left, edge);
        lay(right(ring.out), Some(rect.y), ring.right, edge);
        let Some(arc) = ring.arc() else {
            continue;
        };
        let faint = ring_style(colours, RING_FADE * ring.life + ARC_FAINTER, true);
        for (y, (l, r)) in [
            (rect.y.checked_sub(1), sonar::ABOVE),
            (rect.y.checked_add(1), sonar::BELOW),
        ] {
            lay(left(arc), y, l, faint);
            lay(right(arc), y, r, faint);
        }
    }
}

/// `fade` of the way into the background with a palette; else dim if `dim`.
fn ring_style(colours: Option<&Colours>, fade: f32, dim: bool) -> Style {
    match colours {
        Some(c) => Style::default().fg(rgb(c.ink().lerp(c.palette.bg, fade))),
        None if dim => Style::default().add_modifier(Modifier::DIM),
        None => Style::default(),
    }
}

/// Whether (`x`, `y`) is bare background: on screen above the hint row, blank,
/// unpainted, and not on a row (a blank inside a name is the row's).
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
fn leave(buf: &mut Buffer, rect: Rect, colours: Option<&Colours>, progress: f32) {
    match colours {
        Some(c) => {
            let t = SIFT_FROM + (1.0 - SIFT_FROM) * ease_out(progress);
            let p = &c.palette;
            each_cell(buf, rect, |cell| {
                let fg = rgb(drawn_in(cell, p).lerp(p.bg, t));
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

/// The gap row, `progress` of the way up from the background. Palette only:
/// without one the row is drawn dim from the start.
fn arrive(buf: &mut Buffer, rect: Rect, colours: &Colours, progress: f32) {
    let k = ease_out(progress);
    let p = &colours.palette;
    each_cell(buf, rect, |cell| {
        if !cell.symbol().trim().is_empty() {
            let fg = rgb(p.bg.lerp(drawn_in(cell, p), k));
            cell.set_fg(fg);
        }
    });
}

/// Apply `f` to each cell of `rect` within the buffer.
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

/// Quadratic ease-out: visible change on the first frames, soft end.
fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t)
}

#[cfg(test)]
mod tests {
    use super::super::app::Key;
    use super::super::swap;
    use super::super::test_support::{self, picker};
    use super::*;
    use crate::test_support::palette as test_palette;

    const W: u16 = 40;
    const H: u16 = 8;

    fn plain(palette: Option<&Palette>) -> Option<Colours> {
        palette.map(|p| Colours {
            palette: *p,
            theme: None,
        })
    }

    fn frame(app: &App, palette: Option<&Palette>) -> Buffer {
        test_support::buffer(W, H, |f| {
            draw::draw(f, app);
            paint(f, app, plain(palette).as_ref());
        })
    }

    /// [`frame`] with a theme pass of `ink` before the effects.
    fn themed(app: &App, palette: &Palette, ink: Rgb) -> Buffer {
        let colours = Colours {
            palette: *palette,
            theme: Some(ink),
        };
        test_support::buffer(W, H, |f| {
            draw::draw(f, app);
            crate::theme::paint_in(f.buffer_mut(), ink);
            paint(f, app, Some(&colours));
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

        a.tick(AFTERGLOW / 2);
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

    /// The name returns to the ink and the dim number to the terminal's
    /// foreground, so nothing jumps when the glow ends.
    #[test]
    fn a_themed_row_lets_go_of_an_ink_bar_back_to_its_own_colours() {
        let ink = Rgb(0x7a, 0xa2, 0xf7);
        let p = test_palette();
        let mut a = picker(&["one", "two", "three"]);
        a.on_key(Key::Char('j'));
        a.tick(AFTERGLOW / 2);
        let (_, progress) = a.glow().expect("the row just left glows");
        let t = ease_out(progress);

        let buf = themed(&a, &p, ink);
        let y = row_of(&buf, "one");
        let number = first_glyph(&buf, y);
        let name = &buf[(column_of(&buf, y, "one"), y)];
        let bar = rgb(ink.lerp(p.bg, t));
        assert_eq!((number.bg, name.bg), (bar, bar), "one bar, of ink");
        assert_eq!(number.fg, rgb(p.bg.lerp(p.fg, t)), "{number:?}");
        assert!(number.modifier.contains(Modifier::DIM), "{number:?}");
        assert_eq!(name.fg, rgb(p.bg.lerp(ink, t)), "{name:?}");

        a.tick(Duration::from_secs(1));
        let buf = themed(&a, &p, ink);
        let y = row_of(&buf, "one");
        assert_eq!(first_glyph(&buf, y).fg, Color::Reset, "the number's own");
        assert_eq!(buf[(column_of(&buf, y, "one"), y)].fg, rgb(ink), "the ink");
    }

    #[test]
    fn moving_back_onto_a_glowing_row_ends_its_glow() {
        let mut a = picker(&["one", "two"]);
        a.on_key(Key::Char('j'));
        a.on_key(Key::Char('k'));
        let (id, _) = a.glow().expect("the row just left glows");
        assert_ne!(Some(id), a.selected_row_id());
    }

    #[test]
    fn only_the_row_last_left_glows() {
        let p = test_palette();
        let mut a = picker(&["one", "two", "three", "four"]);
        a.on_key(Key::Char('j'));
        a.on_key(Key::Char('j'));
        a.on_key(Key::Char('j'));
        let (id, progress) = a.glow().expect("the row just left glows");
        assert_eq!(Some(id), a.visible().get(2).map(|s| s.id.as_str()));
        assert_eq!(progress, 0.0, "and from the start");

        let buf = frame(&a, Some(&p));
        for name in ["one", "two"] {
            let cell = first_glyph(&buf, row_of(&buf, name));
            assert_eq!(cell.bg, Color::Reset, "{name} has let go: {cell:?}");
        }
    }

    /// Reversed and dim for its first third, then nothing; never a colour.
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

        a.tick(AFTERGLOW / 2);
        let buf = frame(&a, None);
        let cell = first_glyph(&buf, row_of(&buf, "one"));
        assert!(!cell.modifier.contains(Modifier::REVERSED), "let go");
    }

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

    /// Not the row's first glyph, which can be a spark beside the list.
    fn name_cell<'a>(buf: &'a Buffer, name: &str) -> &'a ratatui::buffer::Cell {
        let y = row_of(buf, name);
        &buf[(column_of(buf, y, name), y)]
    }

    #[test]
    fn the_row_a_session_in_flight_left_lets_its_bar_go() {
        let p = test_palette();
        let mut a = picker(&["one", "two", "three"]);
        a.set_trail(true);
        a.on_key(Key::Char(' '));
        a.on_key(Key::Char('j'));

        let buf = frame(&a, Some(&p));
        let cell = name_cell(&buf, "two");
        assert_eq!(cell.bg, rgb(p.fg), "starts as the bar: {cell:?}");
        assert_eq!(cell.fg, rgb(p.bg));

        a.tick(swap::ECHO / 2);
        let buf = frame(&a, Some(&p));
        let cell = name_cell(&buf, "two");
        let Color::Rgb(r, ..) = cell.bg else {
            panic!("halfway, the bar is still painted: {cell:?}")
        };
        assert!(r > p.bg.0 && r < p.fg.0, "halfway between: {r}");

        a.tick(swap::ECHO);
        let buf = frame(&a, Some(&p));
        let cell = name_cell(&buf, "two");
        assert_eq!(cell.bg, Color::Reset, "let go: {cell:?}");
    }

    #[test]
    fn without_a_palette_the_row_left_holds_the_bar() {
        let mut a = picker(&["one", "two", "three"]);
        a.set_trail(true);
        a.on_key(Key::Char(' '));
        a.on_key(Key::Char('j'));
        let buf = frame(&a, None);
        let cell = name_cell(&buf, "two");
        assert!(cell.modifier.contains(Modifier::REVERSED | Modifier::DIM));
        test_support::assert_no_colour(W, H, |f| {
            draw::draw(f, &a);
            paint(f, &a, None);
        });

        a.tick(swap::ECHO / 2);
        let buf = frame(&a, None);
        let cell = name_cell(&buf, "two");
        assert!(!cell.modifier.contains(Modifier::REVERSED), "let go");
    }

    /// The cursor moves with the session; the trail shows that, not a glow.
    #[test]
    fn moving_a_session_in_flight_leaves_no_glow() {
        let mut a = picker(&["one", "two", "three"]);
        a.on_key(Key::Char(' '));
        a.on_key(Key::Char('j'));
        a.on_key(Key::Char('j'));
        assert!(a.glow().is_none());
    }

    #[test]
    fn with_the_effects_off_nothing_passes() {
        let mut a = picker(&["api-server", "dotfiles", "notes"]);
        a.set_effects(false);
        a.on_key(Key::Char('j'));
        assert!(a.glow().is_none());
        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('n'));
        assert!(a.leaving().is_none());
        assert!(a.rows().iter().all(|r| !r.leaving));
        a.on_key(Key::Esc);
        a.on_key(Key::Char('x'));
        assert!(a.strike().is_none(), "no line through the name");
        a.on_key(Key::Char('n'));
        assert!(!a.animating());
    }

    // --- the strike ---------------------------------------------------------

    /// Second row selected and `x` pressed, the line `ms` ms along.
    fn asking(ms: u64) -> App {
        let mut a = picker(&["api-server", "dotfiles", "notes"]);
        a.on_key(Key::Char('j'));
        a.on_key(Key::Char('x'));
        a.tick(Duration::from_millis(ms));
        a
    }

    /// In cells, not bytes: the marker before a name is three bytes, one cell.
    fn column_of(buf: &Buffer, y: u16, needle: &str) -> u16 {
        let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        let at = line.find(needle).expect("drawn");
        line[..at].chars().count() as u16
    }

    fn crossed(buf: &Buffer, y: u16) -> Vec<u16> {
        (0..buf.area.width)
            .filter(|x| buf[(*x, y)].modifier.contains(Modifier::CROSSED_OUT))
            .collect()
    }

    /// Only the name, not the marker or number; in colour the bar warms
    /// toward red.
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

    #[test]
    fn a_themed_strike_warms_its_bar_from_the_ink() {
        let ink = Rgb(0x7a, 0xa2, 0xf7);
        let mut p = test_palette();
        p.ansi[RED] = Rgb(0xcd, 0, 0);
        let a = asking(STRIKE.as_millis() as u64);
        let buf = themed(&a, &p, ink);
        let y = row_of(&buf, "dotfiles");
        assert_eq!(crossed(&buf, y).len(), 8);
        let marker = first_glyph(&buf, y);
        let warm = ink.lerp(p.ansi[RED], STRIKE_WARMTH);
        assert_eq!(marker.bg, rgb(warm), "{marker:?}");
        assert_ne!(warm, ink, "the bar has to have warmed to say anything");
        assert_eq!(marker.fg, rgb(p.bg));
        assert!(!marker.modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn the_line_goes_through_from_the_left() {
        let a = asking(STRIKE.as_millis() as u64 / 4);
        let buf = frame(&a, None);
        let y = row_of(&buf, "dotfiles");
        let first = crossed(&buf, y);
        assert!(!first.is_empty() && first.len() < 8, "{first:?}");
        assert_eq!(first[0], column_of(&buf, y, "dotfiles"));
    }

    /// The bar stays reversed and no colour is set.
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

    /// No frames are requested while the question waits.
    #[test]
    fn a_line_all_the_way_across_is_still() {
        let a = asking(STRIKE.as_millis() as u64);
        assert!(!a.animating());
        assert!(asking(10).animating());
    }

    /// Any answer but `y` retracts the line from wherever it reached.
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

    /// `y` hands the row to the backspace effect; the line does not retract.
    #[test]
    fn confirming_ends_the_line_at_once() {
        let mut a = asking(STRIKE.as_millis() as u64);
        a.on_key(Key::Char('y'));
        assert!(a.strike().is_none());
    }

    // --- the sonar, kept for later ------------------------------------------

    fn is_braille(symbol: &str) -> bool {
        symbol
            .chars()
            .next()
            .is_some_and(|c| (0x2801..=0x28ff).contains(&(c as u32)))
    }

    fn sonar_at(ms: u64) -> Sonar {
        let mut sonar = Sonar::new();
        sonar.advance(Duration::from_millis(ms));
        sonar
    }

    /// Draw, paint effects, then [`rings`] from session `id`'s row, last, as a
    /// caller would.
    fn ping(f: &mut Frame, app: &App, id: &str, sonar: &Sonar, palette: Option<&Palette>) {
        let colours = plain(palette);
        draw::draw(f, app);
        paint(f, app, colours.as_ref());
        let area = f.area();
        if let Some(rect) = draw::row_rect(app, area, id) {
            rings(f.buffer_mut(), app, area, rect, sonar, colours.as_ref());
        }
    }

    /// Cursor on the second row, which the sonar tests ping from.
    fn on_second(names: &[&str]) -> App {
        let mut a = picker(names);
        a.select_session("id000001");
        a
    }

    /// [`ping`] from the second row with a sonar `ms` ms old.
    fn pinged(names: &[&str], ms: u64, palette: Option<&Palette>) -> Buffer {
        let a = on_second(names);
        test_support::buffer(W, H, |f| ping(f, &a, "id000001", &sonar_at(ms), palette))
    }

    /// Past both ends of the row, and above and below once clear of it.
    #[test]
    fn rings_go_out_from_a_row() {
        let p = test_palette();
        let buf = pinged(&["api-server", "dotfiles", "notes"], 200, Some(&p));
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

    /// Not even in a blank inside a name, nor on the hint row.
    #[test]
    fn rings_never_land_on_a_row() {
        let a = on_second(&["session 3 is long", "x", "y"]);
        for ms in (0..sonar::PULSE.as_millis() as u64).step_by(5) {
            let sonar = sonar_at(ms);
            let lines = test_support::render(W, H, |f| ping(f, &a, "id000001", &sonar, None));
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

    #[test]
    fn without_a_palette_rings_are_dim_braille() {
        let a = on_second(&["api-server", "dotfiles", "notes"]);
        for ms in (0..sonar::PULSE.as_millis() as u64).step_by(20) {
            let sonar = sonar_at(ms);
            test_support::assert_no_colour(W, H, |f| ping(f, &a, "id000001", &sonar, None));
        }
        let buf = pinged(&["api-server", "dotfiles", "notes"], 280, None);
        assert!(
            buf.content
                .iter()
                .any(|c| is_braille(c.symbol()) && c.modifier.contains(Modifier::DIM)),
            "dim far out"
        );
    }

    #[test]
    fn between_pulses_nothing_is_drawn() {
        let names = ["api-server", "dotfiles", "notes"];
        let resting = pinged(&names, sonar::PULSE.as_millis() as u64, None);
        let a = on_second(&names);
        let plain = test_support::buffer(W, H, |f| draw::draw(f, &a));
        assert_eq!(resting, plain);
    }

    /// Returning from a session gives its row a still mark and requests no
    /// frames.
    #[test]
    fn coming_back_sends_no_rings() {
        let mut a = on_second(&["api-server", "dotfiles", "notes"]);
        a.set_came_from("id000001");
        for ms in [0, 200, 400] {
            a.tick(Duration::from_millis(ms));
            let buf = frame(&a, None);
            assert!(
                !buf.content.iter().any(|c| is_braille(c.symbol())),
                "{ms} ms"
            );
            assert!(!a.animating(), "{ms} ms");
        }
    }

    // --- the gap a new session opens ---------------------------------------

    /// A gap made after the second row `ms` ms ago.
    fn room_after(ms: u64) -> App {
        let mut a = picker(&["api-server", "dotfiles", "notes"]);
        a.on_key(Key::Char('j'));
        assert!(a.make_room("session 4"));
        a.tick(Duration::from_millis(ms));
        a
    }

    /// Near the background at first, brighter halfway, and at the end the
    /// renderer's plain dim row with no colour set.
    #[test]
    fn the_row_in_the_gap_comes_up_out_of_the_background() {
        let p = test_palette();
        let red = |a: &App| -> u8 {
            let buf = frame(a, Some(&p));
            match first_glyph(&buf, row_of(&buf, "session 4")).fg {
                Color::Rgb(r, ..) => r,
                other => panic!("painted in colour: {other:?}"),
            }
        };
        let first = red(&room_after(0));
        let halfway = red(&room_after(ROOM.as_millis() as u64 / 2));
        assert_eq!(first, p.bg.0, "starts in the background");
        assert!(halfway > first && halfway < p.fg.0, "on its way: {halfway}");

        let up = room_after(ROOM.as_millis() as u64);
        let buf = frame(&up, Some(&p));
        let cell = first_glyph(&buf, row_of(&buf, "session 4"));
        assert_eq!(cell.fg, Color::Reset, "all the way up: {cell:?}");
        assert!(cell.modifier.contains(Modifier::DIM));
        let other = first_glyph(&buf, row_of(&buf, "notes"));
        assert_eq!(other.fg, Color::Reset, "no other row is touched");
    }

    #[test]
    fn without_a_palette_the_row_in_the_gap_is_simply_there() {
        let a = room_after(0);
        let buf = frame(&a, None);
        let cell = first_glyph(&buf, row_of(&buf, "session 4"));
        assert!(cell.modifier.contains(Modifier::DIM), "{cell:?}");
        test_support::assert_no_colour(W, H, |f| {
            draw::draw(f, &a);
            paint(f, &a, None);
        });
    }
}
