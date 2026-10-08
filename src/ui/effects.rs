//! The picker's passing effects: the afterglow the cursor leaves behind (and a
//! session in flight leaves on the rows it crosses, see [`super::swap`]), the
//! glint it lands with, the rows a filter keystroke drops fading out where
//! they stood, the line struck through a name a `[y/N]` is asking about, the
//! rings that go out from the session you came back to the picker from, and
//! the row `c` stands in the gap it opens, coming up out of the background.
//!
//! All six are a post-pass over a frame [`super::draw`] has already drawn,
//! the way [`crate::fade::apply`] is, and for the same reason: the screen's own
//! drawing stays colourless, and its tests keep saying so. What is passing —
//! which rows, and how far through — is [`App`]'s, moved on by the caller's
//! clock like the trail is; where those rows are on screen is the renderer's,
//! asked through [`draw::row_rect`] so the pass lands on the row the eye sees.
//!
//! # Colour where the terminal said, modifiers where it did not
//!
//! A real fade needs real colours at both ends, so with the terminal's answer
//! to [`crate::palette::query`] and no `NO_COLOR`, all six paint in colour:
//!
//! - the afterglow runs from the selection's look — the foreground as the
//!   background, the background as the foreground — back to the plain row;
//! - the glint is a band a few cells wide that crosses the new selection's
//!   bar from left to right, brightening it towards [`SHINE`] at its middle;
//! - a dropped row's text runs from most of the way to all of the way into
//!   the background, so it is plainly leaving from its first frame;
//! - the bar of a row a `[y/N]` is asking about warms towards the terminal's
//!   own red as the line goes through its name;
//! - each ring fades into the background as it spreads;
//! - the row in the gap comes up out of the background to the dim it is
//!   drawn in.
//!
//! A `[theme] highlight` (see [`crate::theme`]) changes the colours, never
//! what is painted. Everything that comes off the selection's bar starts
//! from the bar as the theme colours it rather than from the terminal's
//! foreground: the afterglow lets go of the highlight and the text on it,
//! the glint brightens the highlight, the strike warms it, and the rings go
//! out in it, each cell as far from it as its own brightness says; and the
//! stars, sparks and dust of a session being moved take it too, the dim ones
//! halfway into the background ([`particles`]). The rows a filter drops and
//! the row in a gap are no bar, and keep the terminal's colours. That is
//! [`Ink`], read once a run.
//!
//! Without them, each falls back to a modifier, which sets no colour: the
//! afterglow holds the bar, reversed and dim, for the first third of its time
//! and then lets it go; the glint is an underline running under the bar where
//! the band would be; a dropped row is dim until it goes; the struck row is the
//! line alone, which is a modifier to begin with; a ring is dim once it is
//! halfway out; the row in the gap is there, dim, from the first frame.
//! Coarser, and correct under `NO_COLOR` by construction, as everything else
//! here is.
//!
//! The underline on a filter's matched letters is not here, and is not an
//! effect: it is still, a modifier, and says why a row is still on screen, so
//! [`super::draw`] draws it whatever `[effects]` says.

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Frame;

use super::app::{App, GHOST_ID};
use super::draw;
use super::sonar::{self, Sonar};
use crate::palette::{Palette, Rgb};
use crate::theme::Highlight;

/// How long a row a filter keystroke dropped takes to fade before the list
/// closes up. Short: the row has already been ruled out, and every
/// millisecond of this is a millisecond the list is longer than the query
/// says. Long enough to see which rows went, which is all it is for.
pub const SIFT: Duration = Duration::from_millis(120);

/// How long the row `c` stands in the gap it opens takes to come up out of the
/// background (see [`App::make_room`]). Long enough to read the name and
/// the number on it before the picker closes onto it; it is waited on before
/// the prompt every time, so no longer than that.
pub const ROOM: Duration = Duration::from_millis(130);

/// How long the gap stays up, all told, when there is no fade to close the
/// picker onto it: the row is there at once, dim, and this is how long it is
/// shown before the prompt cuts in. A little longer than [`ROOM`], which the
/// close would otherwise have followed.
pub const ROOM_HELD: Duration = Duration::from_millis(200);

/// How long the row the cursor leaves glows. Only that row glows: a quick
/// `j j j`, or a held key, moves the glow along with the cursor rather than
/// leaving a tail. Short enough that one key's glow is gone before the next is
/// pressed at an ordinary pace. Fixed, where the fade's length is
/// configurable: the fade is waited on at every switch, and this holds nothing
/// up.
pub const AFTERGLOW: Duration = Duration::from_millis(100);

/// How long the glint takes to cross the row the cursor lands on. Longer
/// than the afterglow, so the two read as one movement: the old row letting
/// go quickly while the light runs across the new one. Like the afterglow it
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

/// How far into the background a dim particle is drawn under a highlight,
/// where the terminal said what its colours are: about where its own dim would
/// have put it, made exact, as everything else here is.
const PARTICLE_DIM: f32 = 0.5;

/// What the effects paint with: the terminal's colours, and the bar's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ink {
    /// The terminal's colours: what a plain row is drawn in, and the
    /// background everything fades into.
    palette: Palette,
    /// The selection's bar: `[theme] highlight`, or the terminal's foreground,
    /// which is what a reversed bar shows.
    bar: Rgb,
    /// The text on the bar: the theme's choice for it, or the terminal's
    /// background, which is what a reversed bar shows.
    on_bar: Rgb,
}

impl Ink {
    /// The terminal's palette, and the bar as `highlight` colours it. Pure, so
    /// a test can reach every colour without a terminal or a config.
    pub fn new(terminal: Palette, highlight: Option<Highlight>) -> Self {
        let bar = highlight.map_or(terminal.fg, |h| h.bar);
        let on_bar = highlight.and_then(|h| h.text).unwrap_or(terminal.bg);
        Self {
            palette: terminal,
            bar,
            on_bar,
        }
    }
}

/// What the effects paint with, or `None` to fall back to modifiers: none
/// when `NO_COLOR` is set or the terminal did not say what its colours are.
pub fn ink() -> Option<Ink> {
    if std::env::var_os("NO_COLOR").is_some() {
        return None;
    }
    crate::palette::get().map(|p| Ink::new(*p, crate::theme::highlight()))
}

/// Whether the config wants an effect that paints in colour, so the terminal
/// should be asked what its colours are even with the fade switched off.
/// `NO_COLOR` answers no for all of them, as it does for the fade.
pub fn want_palette() -> bool {
    let effects = &crate::config::get().effects;
    let wanted = effects.cursor_enabled()
        || effects.move_enabled()
        || effects.filter_enabled()
        || effects.kill_enabled()
        || effects.back_enabled()
        || effects.create_enabled();
    wanted && std::env::var_os("NO_COLOR").is_none()
}

/// Paint whatever is passing over the frame `draw::draw` has just drawn.
pub fn paint(frame: &mut Frame, app: &App, ink: Option<&Ink>) {
    let area = frame.area();
    let buf = frame.buffer_mut();
    let selected = app.selected_row_id();

    if let Some((id, progress)) = app.glow() {
        if selected != Some(id) {
            if let Some(rect) = draw::row_rect(app, area, id) {
                glow(buf, rect, ink, progress);
            }
        }
    }

    // The rows a session in flight crossed, letting its bar go: the same glow,
    // left by a move rather than by the cursor.
    for (id, progress) in app.swap().echoes() {
        if selected != Some(id) {
            if let Some(rect) = draw::row_rect(app, area, id) {
                glow(buf, rect, ink, progress);
            }
        }
    }

    if let Some((id, progress)) = app.glint() {
        // The bar is the selection's: a glint left on a row the cursor has
        // since left would be lighting a plain row.
        if selected == Some(id) {
            if let Some(rect) = draw::row_rect(app, area, id) {
                glint(buf, rect, ink, progress);
            }
        }
    }

    if let Some(progress) = app.leaving() {
        for row in app.rows().iter().filter(|r| r.leaving) {
            if let Some(rect) = draw::row_rect(app, area, &row.session.id) {
                leave(buf, rect, ink, progress);
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
            let warm = ink.filter(|_| selected == Some(id));
            strike(buf, bar, name, warm, drawn);
        }
    }

    if let (Some(progress), Some(ink)) = (app.room(), ink) {
        if let Some(rect) = draw::row_rect(app, area, GHOST_ID) {
            arrive(buf, rect, ink, progress);
        }
    }

    // Last, so the rings find the screen as everything else left it, and only
    // ever take what is still the terminal's own background. Not while a gap
    // is open for a new session: the picker is on its way to the prompt, and
    // the gap is the one thing moving. The rings pick up again if the prompt
    // is cancelled, since `Esc` still goes back where they say.
    if let Some((id, sonar)) = app.sonar().filter(|_| !app.has_room()) {
        if let Some(rect) = draw::row_rect(app, area, id) {
            rings(buf, app, area, rect, sonar, ink);
        }
    }
}

/// Under a `[theme] highlight`, the braille the picker scatters beside the
/// list in the highlight: the stars off a session in flight, the sparks off
/// the seams it slides past, the dust it lands in, and the rings, if they
/// have no colour yet. A bright one is the highlight itself, and a dim one
/// the highlight `PARTICLE_DIM` of the way into the background — or, not
/// knowing the background, the highlight left to the terminal to dim.
///
/// All of it is braille, and nothing else on the picker is but a name that
/// happens to be, so that is how it is found: a braille cell above the hint
/// row, in no name, that nothing has coloured — the stars beside a row that
/// has stepped aside included, and a spark in the blank of a row's indent. [`super::draw`] draws them with modifiers alone,
/// as it draws everything; this is their colour, the way [`paint`] is the
/// sonar's. Without a highlight, nothing.
pub fn particles(frame: &mut Frame, app: &App, ink: Option<&Ink>, highlight: Option<&Highlight>) {
    let Some(highlight) = highlight else {
        return;
    };
    let area = frame.area();
    let buf = frame.buffer_mut();
    let screen = buf.area.intersection(area);
    let above_hint = screen.y + screen.height.saturating_sub(1);
    let bright = rgb(highlight.bar);
    let dim = ink.map(|ink| rgb(highlight.bar.lerp(ink.palette.bg, PARTICLE_DIM)));
    for y in screen.y..above_hint {
        for x in screen.x..screen.x + screen.width {
            let cell = &mut buf[(x, y)];
            if !braille(cell.symbol())
                || cell.fg != Color::Reset
                || cell.modifier.contains(Modifier::REVERSED)
                || draw::in_a_name(app, area, x, y)
            {
                continue;
            }
            match dim {
                Some(dim) if cell.modifier.contains(Modifier::DIM) => {
                    cell.modifier.remove(Modifier::DIM);
                    cell.set_fg(dim);
                }
                _ => {
                    cell.set_fg(bright);
                }
            }
        }
    }
}

/// Whether `symbol` is one braille cell with a dot in it: what every particle
/// on the picker is drawn in.
fn braille(symbol: &str) -> bool {
    let mut chars = symbol.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(c), None) if ('\u{2801}'..='\u{28ff}').contains(&c)
    )
}

/// The row the cursor left, `progress` of the way back to plain.
fn glow(buf: &mut Buffer, rect: Rect, ink: Option<&Ink>, progress: f32) {
    match ink {
        Some(ink) => {
            let p = &ink.palette;
            let t = ease_out(progress);
            let bg = rgb(ink.bar.lerp(p.bg, t));
            let fg = rgb(ink.on_bar.lerp(p.fg, t));
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
/// across it. `rect` is the bar: [`draw::row_rect`] measures a row the whole
/// width of the list, which is as far as the selection's reversed bar runs.
fn glint(buf: &mut Buffer, rect: Rect, ink: Option<&Ink>, progress: f32) {
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
        match ink {
            // The bar is the reversed default colours. Painting it means
            // setting both ends outright, the background now the brightened
            // foreground, so the reverse comes off first.
            Some(ink) => {
                cell.modifier.remove(Modifier::REVERSED);
                cell.set_bg(rgb(ink.bar.lerp(SHINE, GLINT_PEAK * lit)));
                cell.set_fg(rgb(ink.on_bar));
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
fn strike(buf: &mut Buffer, bar: Rect, name: Rect, ink: Option<&Ink>, drawn: f32) {
    let k = ease_out(drawn);
    let across = ((k * f32::from(name.width)).ceil() as u16).min(name.width);
    let line = Rect {
        width: across,
        ..name
    };
    each_cell(buf, line, |cell| {
        cell.modifier.insert(Modifier::CROSSED_OUT)
    });
    if let Some(ink) = ink {
        let warm = rgb(ink.bar.lerp(ink.palette.ansi[RED], STRIKE_WARMTH * k));
        let text = rgb(ink.on_bar);
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
fn rings(buf: &mut Buffer, app: &App, area: Rect, rect: Rect, sonar: &Sonar, ink: Option<&Ink>) {
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
        let edge = ring_style(ink, RING_FADE * ring.life, ring.life > RING_FAR);
        lay(left(ring.out), Some(rect.y), ring.left, edge);
        lay(right(ring.out), Some(rect.y), ring.right, edge);
        let Some(arc) = ring.arc() else {
            continue;
        };
        let faint = ring_style(ink, RING_FADE * ring.life + ARC_FAINTER, true);
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
fn ring_style(ink: Option<&Ink>, fade: f32, dim: bool) -> Style {
    match ink {
        Some(ink) => Style::default().fg(rgb(ink.bar.lerp(ink.palette.bg, fade))),
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
fn leave(buf: &mut Buffer, rect: Rect, ink: Option<&Ink>, progress: f32) {
    match ink {
        Some(ink) => {
            let p = &ink.palette;
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

/// The row in a gap made for a new session, `progress` of the way up out of
/// the background. Only with a palette: without one the row is simply there,
/// drawn dim, from the first frame. Once it is all the way up nothing is set,
/// and the row is the plain dim one the renderer drew.
fn arrive(buf: &mut Buffer, rect: Rect, ink: &Ink, progress: f32) {
    let p = &ink.palette;
    let fg = rgb(p.bg.lerp(p.fg, ease_out(progress)));
    each_cell(buf, rect, |cell| {
        if !cell.symbol().trim().is_empty() {
            cell.set_fg(fg);
        }
    });
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
    use super::super::swap;
    use super::super::test_support::{self, picker};
    use super::*;
    use crate::test_support::palette as test_palette;

    const W: u16 = 40;
    const H: u16 = 8;

    /// Draw `app` and paint its effects, the way the picker does, in the
    /// terminal's colours with no theme.
    fn frame(app: &App, palette: Option<&Palette>) -> Buffer {
        themed(app, palette, None)
    }

    /// [`frame`], under a `[theme] highlight`.
    fn themed(app: &App, palette: Option<&Palette>, highlight: Option<Highlight>) -> Buffer {
        let ink = palette.map(|p| Ink::new(*p, highlight));
        test_support::buffer(W, H, |f| {
            draw::draw(f, app);
            paint(f, app, ink.as_ref());
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

    /// The row now selected never glows, even if it was glowing a moment ago:
    /// moving back onto a row takes its glow away.
    #[test]
    fn moving_back_onto_a_glowing_row_ends_its_glow() {
        let mut a = picker(&["one", "two"]);
        a.on_key(Key::Char('j'));
        a.on_key(Key::Char('k'));
        let (id, _) = a.glow().expect("the row just left glows");
        assert_ne!(Some(id), a.selected_row_id());
    }

    /// Only the row the cursor last left glows: a run of moves, as a held key
    /// makes, carries the glow along rather than leaving a tail behind it.
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

        a.tick(AFTERGLOW / 2);
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

    /// The first cell of `name` where it is drawn: not the row's first glyph,
    /// which can be a spark beside the list.
    fn name_cell<'a>(buf: &'a Buffer, name: &str) -> &'a ratatui::buffer::Cell {
        let y = row_of(buf, name);
        &buf[(column_of(buf, y, name), y)]
    }

    /// The row a session in flight leaves keeps its bar for a moment, under the
    /// session that moved into it, and lets it go: the cursor's afterglow, left
    /// by the move. In colour, it runs from the bar back to plain.
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

    /// Without a palette the row holds the bar, reversed and dim, for the first
    /// third, as the afterglow does, and sets no colour.
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

    /// A session in flight carries the cursor with it, and that is the trail's
    /// to show: moving it leaves no glow behind.
    #[test]
    fn moving_a_session_in_flight_leaves_no_glow() {
        let mut a = picker(&["one", "two", "three"]);
        a.on_key(Key::Char(' '));
        a.on_key(Key::Char('j'));
        a.on_key(Key::Char('j'));
        assert!(a.glow().is_none());
        assert!(a.glint().is_none());
    }

    /// With the picker's effects off, a move and a filter keystroke leave
    /// nothing behind, and the picker never asks for a frame's pace.
    #[test]
    fn with_the_effects_off_nothing_passes() {
        let mut a = picker(&["api-server", "dotfiles", "notes"]);
        a.set_effects(false);
        a.on_key(Key::Char('j'));
        assert!(a.glow().is_none());
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
        for ms in (0..sonar::PULSE.as_millis() as u64).step_by(5) {
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
        for ms in (0..sonar::PULSE.as_millis() as u64).step_by(20) {
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

    /// The rings pulse, then rest without asking for frames — saying instead
    /// when the next pulse is due — and pulse again. A key does not stop them:
    /// `Esc` still goes back to that session after one.
    #[test]
    fn rings_pulse_on_a_period_and_go_on_through_keys() {
        let mut a = back_over(&["one", "two"], 1, sonar::PULSE.as_millis() as u64);
        assert!(a.sonar().is_some(), "still there between pulses");
        assert!(!a.animating(), "nothing moving between pulses");
        assert_eq!(a.wake_in(), Some(sonar::PERIOD - sonar::PULSE));

        a.on_key(Key::Char('j'));
        a.tick(sonar::PERIOD - sonar::PULSE);
        assert!(a.animating(), "the next pulse, on time");
        assert!(a.sonar().is_some_and(|(_, s)| !s.rings().is_empty()));
        assert_eq!(a.wake_in(), Some(Duration::ZERO));
    }

    /// A fresh listing keeps the rings while the session they mark is in it,
    /// and ends them once it is not.
    #[test]
    fn a_fresh_listing_keeps_the_rings_while_their_session_is_listed() {
        let mut a = back_over(&["one", "two"], 1, 50);
        let listed = a.sessions().to_vec();
        a.set_sessions(listed.clone());
        assert!(a.sonar().is_some(), "still listed");
        a.set_sessions(vec![listed[0].clone()]);
        assert!(a.sonar().is_none(), "the session they marked has gone");
        assert_eq!(a.wake_in(), None);
    }

    /// A session that has gone from the list sends out no rings: there is no
    /// row to send them from.
    #[test]
    fn no_rings_from_a_session_not_listed() {
        let mut a = picker(&["one", "two"]);
        a.set_came_from("gone");
        assert!(a.sonar().is_none());
    }

    // --- the gap a new session opens ---------------------------------------

    /// [`picker`] with a gap made after the second row, `ms` milliseconds ago.
    fn room_after(ms: u64) -> App {
        let mut a = picker(&["api-server", "dotfiles", "notes"]);
        a.on_key(Key::Char('j'));
        assert!(a.make_room("session 4"));
        a.tick(Duration::from_millis(ms));
        a
    }

    /// In colour, the row in the gap comes up out of the background: nearly
    /// the background on its first frame, brighter halfway, and once it is all
    /// the way up, the plain dim row the renderer drew, with no colour set.
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

    /// The rings from the session the picker came back over hold off while
    /// the gap is open, and pick up again once it closes.
    #[test]
    fn the_rings_hold_off_while_the_gap_is_open() {
        let braille = |a: &App| {
            frame(a, None)
                .content
                .iter()
                .any(|c| is_braille(c.symbol()))
        };
        let mut a = back_over(&["api-server", "dotfiles", "notes"], 1, 200);
        assert!(braille(&a), "pulsing");
        assert!(a.make_room("session 4"));
        assert!(!braille(&a), "held off");
        a.close_room();
        assert!(braille(&a), "back");
    }

    /// Without a palette, nothing is painted: the row is there, dim, from the
    /// first frame.
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

    /// A `[theme] highlight` and the text the theme puts on it: no red in
    /// the bar, so its shades are easy to tell from the test palette's greys.
    const HIGHLIGHT: Highlight = Highlight {
        bar: Rgb(100, 0, 200),
        text: Some(ON_BAR),
    };
    const ON_BAR: Rgb = Rgb(250, 250, 250);

    fn rgb_of(colour: Color) -> Rgb {
        match colour {
            Color::Rgb(r, g, b) => Rgb(r, g, b),
            other => panic!("not painted in colour: {other:?}"),
        }
    }

    /// With no highlight the bar is the terminal's own colours reversed, as
    /// it always was; with one, the bar and its text are the theme's, and
    /// everything still fades into the terminal's background.
    #[test]
    fn the_bar_is_the_highlight_or_the_terminals_reversed() {
        let p = test_palette();
        let plain = Ink::new(p, None);
        assert_eq!((plain.bar, plain.on_bar), (p.fg, p.bg));
        assert_eq!(plain.palette, p);

        let ink = Ink::new(p, Some(HIGHLIGHT));
        assert_eq!((ink.bar, ink.on_bar), (HIGHLIGHT.bar, ON_BAR));
        assert_eq!(ink.palette, p);

        let dark_text = Highlight {
            text: None,
            ..HIGHLIGHT
        };
        assert_eq!(Ink::new(p, Some(dark_text)).on_bar, p.bg);
    }

    /// The row the cursor left lets go of the highlight and its text, into
    /// the plain row.
    #[test]
    fn under_a_highlight_the_afterglow_lets_go_of_it() {
        let p = test_palette();
        let mut a = picker(&["one", "two", "three"]);
        a.on_key(Key::Char('j'));
        let buf = themed(&a, Some(&p), Some(HIGHLIGHT));
        let cell = first_glyph(&buf, row_of(&buf, "one"));
        assert_eq!(cell.bg, rgb(HIGHLIGHT.bar), "starts as the bar");
        assert_eq!(cell.fg, rgb(ON_BAR), "and its text");

        a.tick(AFTERGLOW / 2);
        let buf = themed(&a, Some(&p), Some(HIGHLIGHT));
        let Rgb(r, g, b) = rgb_of(first_glyph(&buf, row_of(&buf, "one")).bg);
        assert!(
            g == 0 && b > 0 && b < 200,
            "the highlight, going: ({r},{g},{b})"
        );
    }

    /// The glint brightens the highlight, not the terminal's foreground, and
    /// leaves its text as the theme set it.
    #[test]
    fn under_a_highlight_the_glint_brightens_it() {
        let p = test_palette();
        let mut a = picker(&["api-server", "dotfiles", "notes"]);
        a.on_key(Key::Char('j'));
        a.tick(GLINT / 2);
        let buf = themed(&a, Some(&p), Some(HIGHLIGHT));
        let y = row_of(&buf, "dotfiles");
        let cell = &buf[(brightest(&buf, y).expect("the band is on the bar"), y)];
        let Rgb(r, g, b) = rgb_of(cell.bg);
        assert!(
            r > HIGHLIGHT.bar.0 && g > 0 && b > HIGHLIGHT.bar.2,
            "the highlight, brighter: ({r},{g},{b})"
        );
        assert_eq!(cell.fg, rgb(ON_BAR));
    }

    /// At a `[y/N]` the highlight warms towards the terminal's red.
    #[test]
    fn under_a_highlight_the_strike_warms_it() {
        let p = test_palette();
        let a = asking(STRIKE.as_millis() as u64);
        let buf = themed(&a, Some(&p), Some(HIGHLIGHT));
        let marker = first_glyph(&buf, row_of(&buf, "dotfiles"));
        assert_eq!(
            marker.bg,
            rgb(HIGHLIGHT.bar.lerp(p.ansi[RED], STRIKE_WARMTH))
        );
        assert_eq!(marker.fg, rgb(ON_BAR));
    }

    /// The rings go out in the highlight, each as far into the terminal's
    /// background as its own fade: every one on the line from the one to the
    /// other.
    #[test]
    fn under_a_highlight_the_rings_go_out_in_it() {
        let p = test_palette();
        let a = back_over(&["api-server", "dotfiles", "notes"], 1, 200);
        let buf = themed(&a, Some(&p), Some(HIGHLIGHT));
        let rings: Vec<_> = buf
            .content
            .iter()
            .filter(|c| is_braille(c.symbol()))
            .collect();
        assert!(!rings.is_empty(), "pulsing");
        for ring in rings {
            // The highlight is (100, 0, 200) and the background black.
            let Rgb(r, g, b) = rgb_of(ring.fg);
            assert!(
                g == 0 && b > 0 && (i32::from(b) - 2 * i32::from(r)).abs() <= 2,
                "{ring:?}"
            );
        }
    }

    /// A dropped row is no bar, and fades in the terminal's colours whatever
    /// the highlight.
    #[test]
    fn under_a_highlight_a_dropped_row_is_as_it_was() {
        let p = test_palette();
        let mut a = picker(&["api-server", "dotfiles", "notes"]);
        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('n'));
        let buf = themed(&a, Some(&p), Some(HIGHLIGHT));
        let dropped = first_glyph(&buf, row_of(&buf, "dotfiles"));
        assert_eq!(dropped.fg, rgb(p.fg.lerp(p.bg, SIFT_FROM)));
    }

    /// [`themed`], and the particles beside the list coloured as the picker
    /// colours them.
    fn scattered(app: &App, palette: Option<&Palette>, highlight: Option<Highlight>) -> Buffer {
        let ink = palette.map(|p| Ink::new(*p, highlight));
        test_support::buffer(W, H, |f| {
            draw::draw(f, app);
            paint(f, app, ink.as_ref());
            particles(f, app, ink.as_ref(), highlight.as_ref());
        })
    }

    /// The second of two sessions picked up, its stars well on their way
    /// off the bar.
    fn in_flight() -> App {
        let mut a = picker(&["api-server", "docs"]);
        a.on_key(Key::Char('j'));
        a.on_key(Key::Char(' '));
        a.set_trail(true);
        for _ in 0..40 {
            a.tick(Duration::from_millis(16));
        }
        a
    }

    fn particles_of(buf: &Buffer) -> Vec<&ratatui::buffer::Cell> {
        buf.content.iter().filter(|c| braille(c.symbol())).collect()
    }

    /// The stars off a session in flight are the highlight: the bright half
    /// the highlight itself, the dim half halfway into the background, set
    /// exactly rather than left to the terminal's dim.
    #[test]
    fn under_a_highlight_the_stars_are_its_colour() {
        let p = test_palette();
        let buf = scattered(&in_flight(), Some(&p), Some(HIGHLIGHT));
        let stars = particles_of(&buf);
        assert!(!stars.is_empty(), "a trail");
        let bright = rgb(HIGHLIGHT.bar);
        let dim = rgb(HIGHLIGHT.bar.lerp(p.bg, PARTICLE_DIM));
        for star in &stars {
            assert!(star.fg == bright || star.fg == dim, "{star:?}");
            assert!(!star.modifier.contains(Modifier::DIM), "{star:?}");
        }
        assert!(stars.iter().any(|s| s.fg == bright), "a bright half");
        assert!(stars.iter().any(|s| s.fg == dim), "a dim half");
    }

    /// Not knowing the terminal's background, a dim star is the highlight
    /// left dim, for the terminal to dim as it dims everything.
    #[test]
    fn without_a_palette_the_stars_are_the_highlight_dimmed_by_the_terminal() {
        let buf = scattered(&in_flight(), None, Some(HIGHLIGHT));
        let stars = particles_of(&buf);
        assert!(!stars.is_empty(), "a trail");
        for star in &stars {
            assert_eq!(star.fg, rgb(HIGHLIGHT.bar), "{star:?}");
        }
        assert!(
            stars.iter().any(|s| s.modifier.contains(Modifier::DIM)),
            "the far half is still dim"
        );
    }

    /// No highlight, no colour: the stars are the modifiers they always were.
    #[test]
    fn without_a_highlight_the_stars_are_as_they_were() {
        let p = test_palette();
        let a = in_flight();
        assert_eq!(scattered(&a, Some(&p), None), frame(&a, Some(&p)));
    }

    /// The sparks off the seams a session slides past, and the dust it lands
    /// in, are the highlight too, frame after frame until they settle.
    #[test]
    fn under_a_highlight_the_sparks_and_the_dust_are_its_colour() {
        let p = test_palette();
        let bright = rgb(HIGHLIGHT.bar);
        let dim = rgb(HIGHLIGHT.bar.lerp(p.bg, PARTICLE_DIM));
        let mut a = picker(&["api-server", "docs", "notes"]);
        a.on_key(Key::Char('j'));
        a.on_key(Key::Char(' '));
        a.set_trail(true);
        a.on_key(Key::Char('j'));
        let mut off_the_line = 0;
        for put_down in [false, true] {
            if put_down {
                a.on_key(Key::Enter);
                assert!(a.landing().is_some(), "landing");
            }
            for _ in 0..30 {
                let buf = scattered(&a, Some(&p), Some(HIGHLIGHT));
                let y = row_of(&buf, "docs");
                for py in 0..buf.area.height {
                    for px in 0..buf.area.width {
                        let cell = &buf[(px, py)];
                        if !braille(cell.symbol()) {
                            continue;
                        }
                        assert!(cell.fg == bright || cell.fg == dim, "({px},{py}) {cell:?}");
                        off_the_line += usize::from(py != y);
                    }
                }
                a.tick(Duration::from_millis(16));
            }
        }
        assert!(off_the_line > 0, "sparks or splash on the rows beside");
    }

    /// A row stepping aside takes its trail with it, and the stars nearest
    /// the bar go where the row stood a moment ago: they are still stars, and
    /// still the highlight. Names all the one length, so the bar is the
    /// list's whole width and the stars come right up to where it was.
    #[test]
    fn stars_beside_a_row_stepping_aside_are_its_colour() {
        let p = test_palette();
        let bright = rgb(HIGHLIGHT.bar);
        let dim = rgb(HIGHLIGHT.bar.lerp(p.bg, PARTICLE_DIM));
        for key in ['j', 'k'] {
            let mut a = picker(&["session 1", "session 2", "session 3"]);
            a.on_key(Key::Char('j'));
            a.on_key(Key::Char(' '));
            a.set_trail(true);
            for _ in 0..20 {
                a.tick(Duration::from_millis(16));
            }
            a.on_key(Key::Char(key));
            let mut stepped = false;
            for _ in 0..30 {
                stepped |= a
                    .selected_row_id()
                    .is_some_and(|id| a.swap().aside(id) != 0);
                let buf = scattered(&a, Some(&p), Some(HIGHLIGHT));
                for (i, cell) in buf.content.iter().enumerate() {
                    if braille(cell.symbol()) {
                        assert!(
                            cell.fg == bright || cell.fg == dim,
                            "{key}: cell {i} {cell:?}"
                        );
                    }
                }
                a.tick(Duration::from_millis(16));
            }
            assert!(stepped, "{key}: the bar never stepped aside");
        }
    }

    /// Braille in a name is the name's, not a particle: a row is never
    /// coloured.
    #[test]
    fn braille_in_a_name_is_left_alone() {
        let p = test_palette();
        let a = picker(&["⠿⠿⠿", "docs"]);
        let buf = scattered(&a, Some(&p), Some(HIGHLIGHT));
        let y = row_of(&buf, "⠿⠿⠿");
        for x in 0..W {
            if braille(buf[(x, y)].symbol()) {
                let cell = &buf[(x, y)];
                assert!(
                    cell.fg == Color::Reset || cell.modifier.contains(Modifier::REVERSED),
                    "{cell:?}"
                );
            }
        }
        assert_eq!(
            buf,
            frame(&a, Some(&p)),
            "nothing scattered, nothing coloured"
        );
    }
}
