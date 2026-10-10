//! The picker's passing effects: the afterglow the cursor leaves behind (and a
//! session in flight leaves on the rows it crosses, see [`super::swap`]), the
//! rows a filter keystroke drops fading out where they stood, the line struck
//! through a name a `[y/N]` is asking about, and the row `c` stands in the gap
//! it opens, coming up out of the background.
//!
//! All four are a post-pass over a frame [`super::draw`] has already drawn,
//! the way [`crate::fade::apply`] is, and for the same reason: the screen's own
//! drawing sets no colour of its own — none at all with no `[theme]` — and its
//! tests keep saying so. What is passing —
//! which rows, and how far through — is [`App`]'s, moved on by the caller's
//! clock like the trail is; where those rows are on screen is the renderer's,
//! asked through [`draw::row_rect`] so the pass lands on the row the eye sees.
//!
//! # Colour where the terminal said, modifiers where it did not
//!
//! A real fade needs real colours at both ends, so with the terminal's answer
//! to [`crate::palette::query`] and no `NO_COLOR`, all four paint in colour:
//!
//! - the afterglow runs from the selection's look — the bar's colour as the
//!   background, the background as the foreground — back to the row as it
//!   was drawn;
//! - a dropped row's text runs from most of the way to all of the way into
//!   the background, so it is plainly leaving from its first frame;
//! - the bar of a row a `[y/N]` is asking about warms towards the terminal's
//!   own red — or the theme's fail colour — as the line goes through its
//!   name;
//! - the row in the gap comes up out of the background to the dim it is
//!   drawn in.
//!
//! Each sets out from, or arrives at, the colour a cell was drawn in
//! ([`crate::theme::ink`]): the terminal's own foreground with no `[theme]`,
//! and in one whatever colour the cell's role has — the bar the selection's,
//! a number muted's — so an effect never jumps a cell to the terminal's text
//! colour and back.
//!
//! Without them, each falls back to a modifier, which sets no colour of its
//! own: the afterglow holds the bar, reversed and dim, for the first third of
//! its time and then lets it go; a dropped row is dim until it goes; the
//! struck row is the line alone, which is a modifier to begin with; the row in
//! the gap is there, dim, from the first frame.
//! Coarser, and correct under `NO_COLOR` by construction, as everything else
//! here is.
//!
//! The underline on a filter's matched letters is not here, and is not an
//! effect: it is still, a modifier, and says why a row is still on screen, so
//! [`super::draw`] draws it whatever `[effects]` says.
//!
//! # The rings, kept for later
//!
//! [`rings`] draws a [`super::sonar`] going out from a row, and nothing calls
//! it at the moment. It marked the session you came back to the picker from
//! until a still mark on that row took the job over (see [`super::draw`]),
//! and is kept, tested, for another use. With a palette each
//! ring fades into the background as it spreads; without one, a ring is dim
//! once it is halfway out.

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Frame;

use super::app::{App, GHOST_ID};
use super::draw;
use super::sonar::{self, Sonar};
use crate::palette::{Palette, Rgb};
use crate::theme::{ink, Theme};

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

/// How long the line takes to go all the way through a name a `[y/N]` is
/// asking about, and to come back off it when the answer is no. Quick: the
/// question is already on the hint row, and this only says which row it
/// means.
pub const STRIKE: Duration = Duration::from_millis(180);

/// How much of the way towards red the struck row's bar goes once the line
/// is all the way across. Most of the way to the bar's own colour is kept, so
/// the row still reads as the selection, warned about.
const STRIKE_WARMTH: f32 = 0.55;

/// The terminal's own red, among the sixteen [`Palette::ansi`]: what the
/// struck bar warms towards when the theme has no fail colour.
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
        || effects.move_enabled()
        || effects.filter_enabled()
        || effects.kill_enabled()
        || effects.create_enabled();
    wanted && std::env::var_os("NO_COLOR").is_none()
}

/// Paint whatever is passing over the frame `draw::draw` has just drawn.
pub fn paint(frame: &mut Frame, app: &App, palette: Option<&Palette>) {
    let area = frame.area();
    let buf = frame.buffer_mut();
    let selected = app.selected_row_id();
    let theme = app.theme();

    if let Some((id, progress)) = app.glow() {
        if selected != Some(id) {
            if let Some(rect) = draw::row_rect(app, area, id) {
                glow(buf, rect, palette, &theme, progress);
            }
        }
    }

    // The rows a session in flight crossed, letting its bar go: the same glow,
    // left by a move rather than by the cursor.
    for (id, progress) in app.swap().echoes() {
        if selected != Some(id) {
            if let Some(rect) = draw::row_rect(app, area, id) {
                glow(buf, rect, palette, &theme, progress);
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
            strike(buf, bar, name, warm, &theme, drawn);
        }
    }

    if let (Some(progress), Some(p)) = (app.room(), palette) {
        if let Some(rect) = draw::row_rect(app, area, GHOST_ID) {
            arrive(buf, rect, p, progress);
        }
    }
}

/// The row the cursor left, `progress` of the way back to plain: its ground
/// from the bar's colour to the background, and each cell's text from the
/// background to the colour it was drawn in.
///
/// Without a palette, the bar held for a while instead — in the bar's colour
/// from end to end, so a cell a theme drew in another colour, a number or a
/// match, is not a block of that colour inside it.
fn glow(buf: &mut Buffer, rect: Rect, palette: Option<&Palette>, theme: &Theme, progress: f32) {
    match palette {
        Some(p) => {
            let t = ease_out(progress);
            let bar = theme.select.rgb(p).unwrap_or(p.fg);
            let bg = rgb(bar.lerp(p.bg, t));
            each_cell(buf, rect, |cell| {
                let fg = rgb(p.bg.lerp(ink(p, cell.fg), t));
                cell.set_bg(bg);
                cell.set_fg(fg);
            });
        }
        None if progress < FALLBACK_HOLD => {
            let bar = theme.select.to_color().unwrap_or(Color::Reset);
            each_cell(buf, rect, |cell| {
                cell.modifier.insert(Modifier::REVERSED | Modifier::DIM);
                cell.set_fg(bar);
            });
        }
        None => {}
    }
}

/// The row a `[y/N]` is asking about: a line through its name, `drawn` of
/// the way across, and — given a palette — its bar warmed by as much from its
/// own colour towards the theme's fail colour, or the terminal's red. The line
/// is the crossed-out modifier, drawn in the text's own colour, so it is the
/// same with a palette or without one.
fn strike(
    buf: &mut Buffer,
    bar: Rect,
    name: Rect,
    palette: Option<&Palette>,
    theme: &Theme,
    drawn: f32,
) {
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
        let from = theme.select.rgb(p).unwrap_or(p.fg);
        let to = theme.fail.rgb(p).unwrap_or(p.ansi[RED]);
        let warm = rgb(from.lerp(to, STRIKE_WARMTH * k));
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
///
/// Called last by whatever calls it, so the rings find the screen as
/// everything else left it. Nothing does at the moment: see the module docs.
#[allow(dead_code)]
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
            each_cell(buf, rect, |cell| {
                cell.set_fg(rgb(ink(p, cell.fg).lerp(p.bg, t)));
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
/// the background to the colour it was drawn in. Only with a palette: without
/// one the row is simply there, drawn dim, from the first frame. Once it is
/// all the way up nothing is set, and the row is the plain dim one the
/// renderer drew.
fn arrive(buf: &mut Buffer, rect: Rect, palette: &Palette, progress: f32) {
    let t = ease_out(progress);
    each_cell(buf, rect, |cell| {
        if !cell.symbol().trim().is_empty() {
            cell.set_fg(rgb(palette.bg.lerp(ink(palette, cell.fg), t)));
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

#[cfg(test)]
mod tests {
    use super::super::app::Key;
    use super::super::swap;
    use super::super::test_support::{self, picker};
    use super::*;
    use crate::test_support::palette as test_palette;
    use crate::test_support::theme;

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
    }

    /// With the picker's effects off, a move and a filter keystroke leave
    /// nothing behind, and the picker never asks for a frame's pace.
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
        a.set_came_from("id000000");
        assert!(a.back_to().is_none(), "no mark beside it");
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

    // --- the sonar, kept for later ------------------------------------------

    fn is_braille(symbol: &str) -> bool {
        symbol
            .chars()
            .next()
            .is_some_and(|c| (0x2801..=0x28ff).contains(&(c as u32)))
    }

    /// A sonar `ms` milliseconds old.
    fn sonar_at(ms: u64) -> Sonar {
        let mut sonar = Sonar::new();
        sonar.advance(Duration::from_millis(ms));
        sonar
    }

    /// Draw `app`, paint its effects, and send `sonar` out from the row of
    /// session `id` over the lot, last, the way a caller of [`rings`] would.
    fn ping(f: &mut Frame, app: &App, id: &str, sonar: &Sonar, palette: Option<&Palette>) {
        draw::draw(f, app);
        paint(f, app, palette);
        let area = f.area();
        if let Some(rect) = draw::row_rect(app, area, id) {
            rings(f.buffer_mut(), app, area, rect, sonar, palette);
        }
    }

    /// `names` in the picker with the cursor on the second row, the one the
    /// tests below send a sonar out from.
    fn on_second(names: &[&str]) -> App {
        let mut a = picker(names);
        a.select_session("id000001");
        a
    }

    /// [`ping`] into a buffer, with a sonar `ms` milliseconds old going out
    /// from the second row of `names`.
    fn pinged(names: &[&str], ms: u64, palette: Option<&Palette>) -> Buffer {
        let a = on_second(names);
        test_support::buffer(W, H, |f| ping(f, &a, "id000001", &sonar_at(ms), palette))
    }

    /// Rings go out past both ends of the row, and once they are clear of it,
    /// over the lines above and below as well.
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

    /// A ring never lands on a row — not even in the blank inside a name — and
    /// never on the hint row.
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

    /// Without a palette the rings are braille and the dim modifier, and no
    /// colour is set at any point.
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

    /// Between pulses there are no rings to draw, and the picker is left as
    /// it was.
    #[test]
    fn between_pulses_nothing_is_drawn() {
        let names = ["api-server", "dotfiles", "notes"];
        let resting = pinged(&names, sonar::PULSE.as_millis() as u64, None);
        let a = on_second(&names);
        let plain = test_support::buffer(W, H, |f| draw::draw(f, &a));
        assert_eq!(resting, plain);
    }

    /// The picker sends no rings out of its own: coming back from a session
    /// marks its row, still, and asks for no frames.
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

    // --- in a theme -----------------------------------------------------------

    /// A palette whose sixteen are xterm's rather than all black, so the
    /// theme's muted — bright black — is a colour of its own.
    fn xterm() -> Palette {
        Palette {
            ansi: crate::palette::XTERM_ANSI,
            ..test_palette()
        }
    }

    /// In a theme the row the cursor left glows from the bar's own colour, and
    /// each of its cells comes back to the colour it was drawn in — the
    /// number to muted's, the name to the terminal's text — rather than all of
    /// them to the terminal's text colour.
    #[test]
    fn in_a_theme_the_glow_sets_out_from_the_bars_colour() {
        let (p, t) = (xterm(), theme());
        let mut a = picker(&["one", "two", "three"]);
        a.set_theme(t);
        a.on_key(Key::Char('j'));

        let buf = frame(&a, Some(&p));
        let y = row_of(&buf, "one");
        let number = first_glyph(&buf, y);
        assert_eq!(number.bg, rgb(t.select.rgb(&p).expect("set")), "the bar");
        assert_eq!(number.fg, rgb(p.bg));

        a.tick(AFTERGLOW / 2);
        let buf = frame(&a, Some(&p));
        let k = ease_out(0.5);
        let number = first_glyph(&buf, y);
        let name = &buf[(column_of(&buf, y, "one"), y)];
        assert_eq!(number.fg, rgb(p.bg.lerp(p.index(8), k)), "towards muted");
        assert_eq!(name.fg, rgb(p.bg.lerp(p.fg, k)), "towards the text");
        assert_eq!(number.bg, name.bg, "one ground");
    }

    /// Without a palette the held bar is the bar's colour from end to end:
    /// the number a theme drew muted is not a grey block inside it.
    #[test]
    fn in_a_theme_the_held_bar_is_one_colour() {
        let t = theme();
        let mut a = picker(&["one", "two", "three"]);
        a.set_theme(t);
        a.on_key(Key::Char('j'));
        let buf = frame(&a, None);
        let y = row_of(&buf, "one");
        let held: Vec<_> = (0..W)
            .map(|x| &buf[(x, y)])
            .filter(|c| c.modifier.contains(Modifier::REVERSED))
            .collect();
        assert!(!held.is_empty());
        for cell in held {
            assert_eq!(Some(cell.fg), t.select.to_color(), "{cell:?}");
        }
    }

    /// In a theme the struck bar warms from the selection's colour towards
    /// the fail colour, rather than from the terminal's text towards its red.
    #[test]
    fn in_a_theme_the_strike_warms_towards_fail() {
        let (p, t) = (xterm(), theme());
        let mut a = asking(STRIKE.as_millis() as u64);
        a.set_theme(t);
        let buf = frame(&a, Some(&p));
        let y = row_of(&buf, "dotfiles");
        let from = t.select.rgb(&p).expect("set");
        let to = t.fail.rgb(&p).expect("set");
        let marker = first_glyph(&buf, y);
        assert_eq!(marker.bg, rgb(from.lerp(to, STRIKE_WARMTH)), "{marker:?}");
    }

    /// In a theme a dropped row leaves from the colours it was drawn in.
    #[test]
    fn in_a_theme_dropped_rows_fade_from_their_own_colours() {
        let (p, t) = (xterm(), theme());
        let mut a = picker(&["api-server", "dotfiles", "notes"]);
        a.set_theme(t);
        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('n'));
        let buf = frame(&a, Some(&p));
        let y = row_of(&buf, "dotfiles");
        assert_eq!(
            first_glyph(&buf, y).fg,
            rgb(p.index(8).lerp(p.bg, SIFT_FROM)),
            "the number, from muted"
        );
        assert_eq!(
            buf[(column_of(&buf, y, "dotfiles"), y)].fg,
            rgb(p.fg.lerp(p.bg, SIFT_FROM)),
            "the name, from the text"
        );
    }

    /// In a theme the row in the gap comes up to muted's colour, and is left
    /// in it once it is all the way up.
    #[test]
    fn in_a_theme_the_row_in_the_gap_comes_up_to_muted() {
        let (p, t) = (xterm(), theme());
        let mut a = room_after(ROOM.as_millis() as u64 / 2);
        a.set_theme(t);
        let buf = frame(&a, Some(&p));
        let cell = first_glyph(&buf, row_of(&buf, "session 4"));
        assert_eq!(cell.fg, rgb(p.bg.lerp(p.index(8), ease_out(0.5))));

        let mut up = room_after(ROOM.as_millis() as u64);
        up.set_theme(t);
        let buf = frame(&up, Some(&p));
        let cell = first_glyph(&buf, row_of(&buf, "session 4"));
        assert_eq!(Some(cell.fg), t.muted.to_color(), "as drawn: {cell:?}");
    }
}
