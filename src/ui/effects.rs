//! The picker's passing effects: the afterglow the cursor leaves behind, and
//! the rows a filter keystroke drops fading out where they stood.
//!
//! Both are a post-pass over a frame [`super::draw`] has already drawn, the
//! way [`crate::fade::apply`] is, and for the same reason: the screen's own
//! drawing stays colourless, and its tests keep saying so. What is passing —
//! which rows, and how far through — is [`App`]'s, moved on by the caller's
//! clock like the trail is; where those rows are on screen is the renderer's,
//! asked through [`draw::row_rect`] so the pass lands on the row the eye sees.
//!
//! # Colour where the terminal said, modifiers where it did not
//!
//! A real fade needs real colours at both ends, so with the terminal's answer
//! to [`crate::palette::query`] and no `NO_COLOR`, both effects interpolate:
//!
//! - the afterglow runs from the selection's look — the foreground as the
//!   background, the background as the foreground — back to the plain row;
//! - a dropped row's text runs from most of the way to all of the way into
//!   the background, so it is plainly leaving from its first frame.
//!
//! Without them, each falls back to a modifier, which sets no colour: the
//! afterglow holds the bar, reversed and dim, for the first third of its time
//! and then lets it go; a dropped row is dim until it goes. Coarser, and
//! correct under `NO_COLOR` by construction, as everything else here is.
//!
//! The underline on a filter's matched letters is not here, and is not an
//! effect: it is still, a modifier, and says why a row is still on screen, so
//! [`super::draw`] draws it whatever `[effects]` says.

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::Frame;

use super::app::App;
use super::draw;
use crate::palette::{Palette, Rgb};

/// How long a row a filter keystroke dropped takes to fade before the list
/// closes up. Short: the row has already been ruled out, and every
/// millisecond of this is a millisecond the list is longer than the query
/// says. Long enough to see which rows went, which is all it is for.
pub const SIFT: Duration = Duration::from_millis(120);

/// How long the row the cursor leaves glows. Long enough that a quick `j j j`
/// leaves a visible tail, short enough that one key's glow is gone before the
/// next is pressed at an ordinary pace. Not a setting: unlike the fade's, this
/// length holds nothing up, so there is nothing to tune it for but taste.
pub const AFTERGLOW: Duration = Duration::from_millis(180);

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
    let wanted = effects.afterglow_enabled() || effects.filter_enabled();
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

    if let Some(progress) = app.leaving() {
        for row in app.rows().iter().filter(|r| r.leaving) {
            if let Some(rect) = draw::row_rect(app, area, &row.session.id) {
                leave(buf, rect, palette, progress);
            }
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
    }

    /// With the picker's effects off, a move and a filter keystroke leave
    /// nothing behind, and the picker never asks for a frame's pace.
    #[test]
    fn with_the_effects_off_nothing_passes() {
        let mut a = picker(&["api-server", "dotfiles", "notes"]);
        a.set_effects(false);
        a.on_key(Key::Char('j'));
        assert_eq!(a.glows().count(), 0);
        a.on_key(Key::Char('/'));
        a.on_key(Key::Char('n'));
        assert!(a.leaving().is_none());
        assert!(a.rows().iter().all(|r| !r.leaving));
        assert!(!a.animating());
    }
}
