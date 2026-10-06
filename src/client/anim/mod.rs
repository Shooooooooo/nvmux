//! Everything that moves.
//!
//! The model ([`super::model`]) is the editor as Neovim last finished drawing
//! it; nothing here changes it. What an animation keeps is the difference
//! between that and what is on the screen this frame — how far a window still
//! has to slide, how many rows behind a scroll still is, where the cursor's
//! corners have got to — and every frame is the model drawn through those
//! differences ([`super::compose::View`]), with the cursor and anything flying off it
//! painted over the top ([`Animator::paint`]).
//!
//! Each batch from Neovim is looked at twice: before it is applied, for what
//! is about to be lost — where the windows were, the lines a scroll is about
//! to take away ([`Animator::before`]) — and after, for what to start
//! ([`Animator::after`]). Nothing animates on the first batch: the editor's
//! first frame is where everything starts, not something to arrive at.

pub mod blink;
pub mod motion;
pub mod raster;
pub mod scroll;
pub mod smear;
pub mod spring;
pub mod vfx;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use self::blink::Blink;
use self::motion::Motion;
use self::scroll::Scroll;
use self::smear::{Rect, Smear};
use self::vfx::Vfx;
use super::compose::{Frame, Scrolling, View};
use super::grid::{Cell, Grid};
use super::model::{Changes, Margins, Model};
use super::redraw::{Event, Shape};
use super::screen::Cursor;
use super::style::{self, Color};
use crate::palette::Rgb;

/// How often a frame goes out while something moves: the fade's pace.
pub const FRAME: Duration = crate::fade::FRAME;

/// The effects as the config sets them, in the units the animations use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Effects {
    pub smear: Option<smear::Settings>,
    pub smear_in_insert: bool,
    pub smear_in_cmdline: bool,
    pub vfx: Option<vfx::Settings>,
    /// Seconds to settle, and the rows of a long jump animated.
    pub scroll: Option<(f32, usize)>,
    /// Seconds to settle.
    pub windows: Option<f32>,
    pub shadow: bool,
    pub blink: bool,
}

impl Effects {
    /// None of them: the client draws, and nothing moves.
    pub fn none() -> Self {
        Self {
            smear: None,
            smear_in_insert: true,
            smear_in_cmdline: true,
            vfx: None,
            scroll: None,
            windows: None,
            shadow: false,
            blink: false,
        }
    }

    /// Whether any of them needs windows on grids of their own: a window
    /// sliding, a float's shadow, a scroll told apart by window.
    pub fn want_multigrid(&self) -> bool {
        self.windows.is_some() || self.shadow || self.scroll.is_some()
    }

    pub fn from_settings(s: &crate::config::Settings) -> Self {
        let e = &s.effects;
        let secs = |ms: u64| ms as f32 / 1000.0;
        Self {
            smear: e.smear_enabled().then(|| smear::Settings {
                duration: secs(e.smear.duration_ms),
                short: secs(e.smear.short_ms),
                trail: e.smear.trail as f32,
            }),
            smear_in_insert: e.smear.insert,
            smear_in_cmdline: e.smear.cmdline,
            vfx: e.particles_enabled().then(|| vfx::Settings {
                mode: vfx::Mode::named(e.particles.mode.name()).unwrap_or(vfx::Mode::Railgun),
                opacity: e.particles.opacity as f32,
                lifetime: secs(e.particles.lifetime_ms),
                density: e.particles.density as f32,
                speed: e.particles.speed as f32,
            }),
            scroll: e
                .scroll_enabled()
                .then(|| (secs(e.scroll.duration_ms), e.scroll.far_lines as usize)),
            windows: e.windows_enabled().then(|| secs(e.windows.duration_ms)),
            shadow: e.shadow_enabled(),
            blink: e.blink_enabled(),
        }
    }
}

/// What a batch is about to change, kept from before it is applied.
#[derive(Debug, Default)]
pub struct Before {
    /// Where every grid on show was.
    origins: HashMap<u64, (i64, i64)>,
    /// The lines of each rectangle about to scroll, with how far.
    scrolls: Vec<Snap>,
}

#[derive(Debug)]
struct Snap {
    grid: u64,
    rect: (usize, usize, usize, usize),
    lines: Vec<Vec<Cell>>,
    delta: i64,
}

/// Every animation in flight.
#[derive(Debug)]
pub struct Animator {
    effects: Effects,
    smear: Smear,
    vfx: Vfx,
    scrolls: HashMap<u64, Scroll>,
    motions: HashMap<u64, Motion>,
    blink: Blink,
    /// When the animations were last moved on.
    last: Option<Instant>,
    /// The cell the cursor was last in, which particles fly from.
    cell: Option<(i64, i64)>,
    /// The mode it was last in: a change of mode starts the blink's wait
    /// again.
    mode: usize,
}

impl Animator {
    pub fn new(effects: Effects) -> Self {
        Self {
            effects,
            smear: Smear::default(),
            vfx: Vfx::default(),
            scrolls: HashMap::new(),
            motions: HashMap::new(),
            blink: Blink::new(Instant::now()),
            last: None,
            cell: None,
            mode: 0,
        }
    }

    /// Stop everything, and forget where the cursor was: the next batch
    /// places it, rather than sending it anywhere.
    pub fn reset(&mut self) {
        self.smear = Smear::default();
        self.vfx.clear();
        self.scrolls.clear();
        self.motions.clear();
        self.cell = None;
    }

    pub fn shadows(&self) -> bool {
        self.effects.shadow
    }

    /// A key was typed: the cursor is not left faded out under it.
    pub fn typed(&mut self, now: Instant) {
        self.blink.reset(now);
    }

    /// Look at a batch before it is applied: see the module docs.
    pub fn before(&self, model: &Model, batch: &[Event], multigrid: bool) -> Before {
        let origins = model
            .layout
            .iter()
            .filter(|(_, p)| !p.hidden)
            .map(|(g, p)| (*g, p.origin()))
            .collect();
        let mut scrolls = Vec::new();
        if self.effects.scroll.is_some() {
            if multigrid {
                let mut deltas: HashMap<u64, i64> = HashMap::new();
                for e in batch {
                    if let Event::WinViewport {
                        grid, scroll_delta, ..
                    } = e
                    {
                        *deltas.entry(*grid).or_default() += scroll_delta;
                    }
                }
                for (grid, delta) in deltas {
                    let (Some(g), Some(p)) = (model.grids.get(&grid), model.layout.get(&grid))
                    else {
                        continue;
                    };
                    if delta == 0 || p.hidden {
                        continue;
                    }
                    if let Some(rect) = inner(g, model.margins(grid)) {
                        scrolls.push(Snap {
                            grid,
                            rect,
                            lines: g.lines(rect.0, rect.1, rect.2, rect.3),
                            delta,
                        });
                    }
                }
            } else if let Some(snap) = linegrid_scroll(model, batch) {
                scrolls.push(snap);
            }
        }
        Before { origins, scrolls }
    }

    /// Start whatever the batch just applied set off: see the module docs.
    /// `first` is the editor's first frame, which nothing animates into.
    pub fn after(
        &mut self,
        before: Before,
        model: &Model,
        changes: &Changes,
        multigrid: bool,
        drawn: bool,
        now: Instant,
    ) {
        if !self.animating(model, now) {
            self.last = Some(now);
        }
        for grid in &changes.reshaped {
            self.scrolls.remove(grid);
            self.motions.remove(grid);
        }

        // Windows that moved slide; one hidden or gone stops sliding.
        match self.effects.windows {
            Some(_) if drawn && multigrid => {
                for (grid, p) in &model.layout {
                    if p.hidden {
                        continue;
                    }
                    let Some(&was) = before.origins.get(grid) else {
                        continue;
                    };
                    let now = p.origin();
                    if now != was {
                        self.motions
                            .entry(*grid)
                            .or_default()
                            .moved((now.0 - was.0, now.1 - was.1));
                    }
                }
                self.motions
                    .retain(|g, _| model.layout.get(g).is_some_and(|p| !p.hidden));
            }
            _ => self.motions.clear(),
        }

        // Scrolls.
        if let Some((_, far)) = self.effects.scroll {
            let mut started = Vec::new();
            for snap in before.scrolls {
                let Some(g) = model.grids.get(&snap.grid) else {
                    continue;
                };
                // Margins or a size changed with it: a change of layout, not
                // a scroll, and nothing to slide.
                let rect = if multigrid {
                    model
                        .layout
                        .contains_key(&snap.grid)
                        .then(|| inner(g, model.margins(snap.grid)))
                        .flatten()
                } else {
                    Some(snap.rect)
                };
                if rect != Some(snap.rect) || !drawn {
                    self.scrolls.remove(&snap.grid);
                    continue;
                }
                let (top, bot, left, right) = snap.rect;
                let lines = g.lines(top, bot, left, right);
                match self.scrolls.get_mut(&snap.grid) {
                    Some(s) if s.rect() == snap.rect => s.scrolled(lines, snap.delta, far),
                    _ => {
                        self.scrolls.insert(
                            snap.grid,
                            Scroll::new(snap.rect, snap.lines, lines, snap.delta, far),
                        );
                    }
                }
                started.push(snap.grid);
            }
            // The rest are redrawn under their scroll as they are.
            for (grid, s) in &mut self.scrolls {
                if started.contains(grid) {
                    continue;
                }
                if let Some(g) = model.grids.get(grid) {
                    let (top, bot, left, right) = s.rect();
                    s.refresh(g.lines(top, bot, left, right));
                }
            }
            if multigrid {
                self.scrolls
                    .retain(|g, _| model.layout.get(g).is_some_and(|p| !p.hidden));
            }
        }

        // The cursor.
        let Some(target) = cursor_rect(model) else {
            return;
        };
        let (row, col) = model.cursor_on_screen();
        let insert = matches!(model.mode_name.as_str(), "insert" | "replace");
        let allowed = drawn
            && !(insert && !self.effects.smear_in_insert)
            && !(model.cursor_in_messages() && !self.effects.smear_in_cmdline);
        if self.smear.target() != Some(target) {
            match self.effects.smear {
                Some(s) if allowed => self.smear.go(target, &s),
                _ => self.smear.snap(target),
            }
        }
        if self.cell != Some((row, col)) {
            if let (Some(s), Some((r0, c0)), true) = (self.effects.vfx, self.cell, allowed) {
                let centre = |r: i64, c: i64| (c as f32 + 0.5, r as f32 + 0.5);
                self.vfx.moved(centre(r0, c0), centre(row, col), &s);
            }
            self.cell = Some((row, col));
            self.blink.reset(now);
        }
        if model.mode != self.mode {
            self.mode = model.mode;
            self.blink.reset(now);
        }
    }

    /// Whether anything is moving: a frame is wanted.
    fn animating(&self, model: &Model, now: Instant) -> bool {
        self.smear.moving()
            || self.vfx.moving()
            || !self.scrolls.is_empty()
            || !self.motions.is_empty()
            || self.blink_due(model, now).is_some()
    }

    /// When a blink fading in or out wants its next frame.
    fn blink_due(&self, model: &Model, now: Instant) -> Option<Instant> {
        if !self.effects.blink {
            return None;
        }
        let mode = model.mode_info()?;
        if mode.shape != Some(Shape::Block) {
            return None;
        }
        self.blink.next_change(mode, now, FRAME)
    }

    /// When the next frame is due, if anything moves.
    pub fn next_frame(&self, model: &Model, now: Instant) -> Option<Instant> {
        let moving = self.smear.moving()
            || self.vfx.moving()
            || !self.scrolls.is_empty()
            || !self.motions.is_empty();
        let frame = moving.then(|| self.last.map_or(now, |last| last + FRAME));
        [frame, self.blink_due(model, now)]
            .into_iter()
            .flatten()
            .min()
    }

    /// Move everything on to `now`.
    pub fn advance(&mut self, now: Instant) {
        let dt = self.last.map_or(0.0, |last| {
            now.saturating_duration_since(last).as_secs_f32()
        });
        self.last = Some(now);
        self.smear.step(dt);
        self.vfx.step(dt);
        if let Some((duration, _)) = self.effects.scroll {
            self.scrolls.retain(|_, s| s.step(dt, duration));
        }
        if let Some(duration) = self.effects.windows {
            self.motions.retain(|_, m| m.step(dt, duration));
        }
    }

    /// Paint the cursor and what flies off it over a composed frame, and say
    /// how the terminal's own cursor is to be left.
    pub fn paint(&self, frame: &mut Frame, model: &Model, now: Instant) -> Cursor {
        let colors = model.colors();
        let (color, text) = cursor_colors(model);
        if let (Some(s), true) = (self.effects.vfx, self.vfx.moving()) {
            self.vfx.paint(frame, &colors, color, &s);
        }
        let mut cursor = hardware_cursor(model, frame);
        if !cursor.visible {
            return cursor;
        }
        if self.smear.moving() {
            self.smear.paint(frame, &colors, color, text);
            cursor.visible = false;
        } else if let Some(mode) = model
            .mode_info()
            .filter(|m| self.effects.blink && m.blinks() && m.shape == Some(Shape::Block))
        {
            let o = self.blink.opacity(mode, now);
            if let Some(cell) = frame.get_mut(cursor.row, cursor.col) {
                let pct = (o * 100.0) as u8;
                let fg = colors.visual_fg(&cell.style);
                let bg = colors.visual_bg(&cell.style);
                cell.style.fg = Color::Rgb(style::mix(pct, text.unwrap_or(bg), fg));
                cell.style.bg = Color::Rgb(style::mix(pct, color, bg));
                cell.style.reverse = false;
            }
            cursor.visible = false;
        }
        cursor
    }
}

impl View for Animator {
    fn origin(&self, grid: u64, placed: (i64, i64)) -> (i64, i64) {
        match self.motions.get(&grid) {
            Some(m) => {
                let (r, c) = m.offset();
                (placed.0 + r, placed.1 + c)
            }
            None => placed,
        }
    }

    fn scroll(&self, grid: u64) -> Option<&dyn Scrolling> {
        self.scrolls.get(&grid).map(|s| s as &dyn Scrolling)
    }
}

/// The rows and columns of a window's grid that scroll: all of it but its
/// margins — a winbar, a float's border.
fn inner(g: &Grid, m: Margins) -> Option<(usize, usize, usize, usize)> {
    let (top, bot) = (m.top, g.height().checked_sub(m.bottom)?);
    let (left, right) = (m.left, g.width().checked_sub(m.right)?);
    (top < bot && left < right).then_some((top, bot, left, right))
}

/// Without `ext_multigrid`, the one scroll there is to animate: a
/// `grid_scroll` of grid 1 whose rows match the current window's
/// `scroll_delta`, summed over the batch. Anything else — a region scrolled to
/// insert or delete lines, a scroll no window owns up to — is not a view
/// moving, and Neovide does not animate it either.
fn linegrid_scroll(model: &Model, batch: &[Event]) -> Option<Snap> {
    let mut wins: HashMap<i64, i64> = HashMap::new();
    let mut rects: Vec<((usize, usize, usize, usize), i64)> = Vec::new();
    for e in batch {
        match e {
            Event::WinViewport {
                win: Some(win),
                scroll_delta,
                ..
            } => *wins.entry(*win).or_default() += scroll_delta,
            Event::GridScroll {
                grid: 1,
                top,
                bot,
                left,
                right,
                rows,
            } => {
                let rect = (*top, *bot, *left, *right);
                match rects.iter_mut().find(|(r, _)| *r == rect) {
                    Some((_, sum)) => *sum += rows,
                    None => rects.push((rect, *rows)),
                }
            }
            _ => {}
        }
    }
    let mut moved = wins.values().filter(|d| **d != 0);
    let delta = *moved.next()?;
    if moved.next().is_some() {
        return None;
    }
    let (rect, _) = rects.into_iter().find(|(_, sum)| *sum == delta)?;
    let g = model.grids.get(&1)?;
    Some(Snap {
        grid: 1,
        rect,
        lines: g.lines(rect.0, rect.1, rect.2, rect.3),
        delta,
    })
}

/// The cursor's rectangle on the screen, in its mode's shape.
fn cursor_rect(model: &Model) -> Option<Rect> {
    let (row, col) = model.cursor_on_screen();
    let mode = model.mode_info();
    let shape = mode.and_then(|m| m.shape).unwrap_or(Shape::Block);
    let percent = mode.map_or(100, |m| m.percentage);
    Some(smear::shape_rect(row, col, shape, percent))
}

/// The cursor's colour, and the colour of a character it covers — from its
/// mode's highlight, as Neovide takes them: the highlight's background (its
/// foreground, reversed), or the editor's text colour where it has none.
fn cursor_colors(model: &Model) -> (Rgb, Option<Rgb>) {
    let colors = model.colors();
    let plain = colors.visual_fg(&model.style(0));
    let Some(id) = model.mode_info().map(|m| m.attr_id).filter(|id| *id != 0) else {
        return (plain, None);
    };
    let s = model.style(id);
    let Some((rgb, cterm)) = model.attrs(id) else {
        return (plain, None);
    };
    let a = if model.options.termguicolors {
        rgb
    } else {
        cterm
    };
    let set = |c: Option<u32>, fg: bool| {
        c.map(|_| {
            if fg {
                colors.resolve(s.fg, true)
            } else {
                colors.resolve(s.bg, false)
            }
        })
    };
    let (back, front) = if a.reverse {
        (set(a.fg, true), set(a.bg, false))
    } else {
        (set(a.bg, false), set(a.fg, true))
    };
    (back.unwrap_or(plain), front)
}

/// How the terminal's own cursor is to be left: where Neovim put it, in its
/// mode's shape and colour — or hidden, while Neovim is busy, for a mode whose
/// highlight blends it away, or with nowhere on the screen to be.
fn hardware_cursor(model: &Model, frame: &Frame) -> Cursor {
    let (row, col) = model.cursor_on_screen();
    let on_screen =
        row >= 0 && col >= 0 && (row as usize) < frame.height && (col as usize) < frame.width;
    let mode = model.mode_info();
    let attrs = mode
        .map(|m| m.attr_id)
        .filter(|id| *id != 0)
        .and_then(|id| model.attrs(id));
    let invisible = attrs.is_some_and(|(rgb, _)| rgb.blend >= 100);
    let shape = match (model.cursor_styled, mode.and_then(|m| m.shape)) {
        (false, _) | (_, None) => 0,
        (true, Some(shape)) => {
            let m = mode.expect("a shape came from a mode");
            let steady = u8::from(m.blinkon == 0 || m.blinkoff == 0);
            match shape {
                Shape::Block => 1 + steady,
                Shape::Horizontal => 3 + steady,
                Shape::Vertical => 5 + steady,
            }
        }
    };
    // As Neovim's TUI does: its colour only with RGB, and reversed is the
    // terminal's own.
    let color = attrs
        .filter(|_| model.options.termguicolors)
        .and_then(|(rgb, _)| (!rgb.reverse).then_some(rgb.bg).flatten())
        .map(style::rgb);
    Cursor {
        visible: on_screen && !model.busy && !invisible,
        row: row.max(0) as usize,
        col: col.max(0) as usize,
        shape,
        color,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::compose;
    use crate::client::grid::Text;
    use crate::client::model::Changes;
    use crate::client::redraw::{LineCell, ModeInfo, OptionValue};
    use crate::palette::Palette;

    fn effects() -> Effects {
        Effects {
            smear: Some(smear::Settings {
                duration: 0.15,
                short: 0.04,
                trail: 0.8,
            }),
            smear_in_insert: true,
            smear_in_cmdline: true,
            vfx: None,
            scroll: Some((0.3, 1)),
            windows: Some(0.15),
            shadow: true,
            blink: false,
        }
    }

    fn model() -> Model {
        Model::new(Palette {
            fg: Rgb(255, 255, 255),
            bg: Rgb(0, 0, 0),
            ansi: [Rgb(0, 0, 0); 16],
        })
    }

    /// Apply a batch the way the client does: looked at before, applied, and
    /// looked at after.
    fn batch(a: &mut Animator, m: &mut Model, events: Vec<Event>, drawn: bool, now: Instant) {
        let before = a.before(m, &events, true);
        let mut changes = Changes::default();
        for e in events {
            m.apply(e, &mut changes);
        }
        m.refresh_styles();
        a.after(before, m, &changes, true, drawn, now);
    }

    fn window(grid: u64, row: usize, col: usize) -> Event {
        Event::WinPos {
            grid,
            win: Some(1000 + grid as i64),
            row,
            col,
            width: 10,
            height: 10,
        }
    }

    fn lines(grid: u64, from: usize, rows: usize) -> Vec<Event> {
        (0..rows)
            .map(|r| Event::GridLine {
                grid,
                row: r,
                col: 0,
                cells: vec![LineCell {
                    text: Text::Char(char::from_digit(((from + r) % 10) as u32, 10).unwrap()),
                    hl: Some(0),
                    repeat: 1,
                }],
            })
            .collect()
    }

    fn setup(a: &mut Animator, m: &mut Model, t0: Instant) {
        let mut events = vec![
            Event::GridResize {
                grid: 1,
                width: 30,
                height: 12,
            },
            Event::GridResize {
                grid: 2,
                width: 10,
                height: 10,
            },
            window(2, 0, 0),
            Event::ModeInfoSet {
                enabled: true,
                modes: vec![ModeInfo {
                    name: "normal".into(),
                    shape: Some(Shape::Block),
                    ..ModeInfo::default()
                }],
            },
            Event::GridCursor {
                grid: 2,
                row: 0,
                col: 0,
            },
        ];
        events.extend(lines(2, 0, 10));
        batch(a, m, events, false, t0);
    }

    /// The first batch places everything; nothing animates into it.
    #[test]
    fn nothing_animates_into_the_first_frame() {
        let (mut a, mut m, t0) = (Animator::new(effects()), model(), Instant::now());
        setup(&mut a, &mut m, t0);
        assert!(a.next_frame(&m, t0).is_none());
    }

    /// A window the editor moves slides there; the cursor's jump travels.
    #[test]
    fn a_moved_window_slides_and_a_jumped_cursor_travels() {
        let (mut a, mut m, t0) = (Animator::new(effects()), model(), Instant::now());
        setup(&mut a, &mut m, t0);
        batch(
            &mut a,
            &mut m,
            vec![
                window(2, 0, 15),
                Event::GridCursor {
                    grid: 2,
                    row: 5,
                    col: 5,
                },
            ],
            true,
            t0,
        );
        assert_eq!(a.origin(2, (0, 15)), (0, 0), "drawn where it was");
        assert!(a.smear.moving());
        assert_eq!(a.next_frame(&m, t0), Some(t0 + FRAME));
        a.advance(t0 + Duration::from_secs(1));
        assert_eq!(a.origin(2, (0, 15)), (0, 15), "and arrived");
        assert!(!a.smear.moving());
    }

    /// A window that scrolls shows the lines it showed, then slides to its
    /// new ones; a change of margins with it is a layout change, not a scroll.
    #[test]
    fn a_window_that_scrolls_slides_its_lines() {
        let (mut a, mut m, t0) = (Animator::new(effects()), model(), Instant::now());
        setup(&mut a, &mut m, t0);
        let mut events = vec![Event::GridScroll {
            grid: 2,
            top: 0,
            bot: 10,
            left: 0,
            right: 10,
            rows: 3,
        }];
        events.extend(lines(2, 3, 10));
        events.push(Event::WinViewport {
            grid: 2,
            win: Some(1002),
            topline: 3,
            botline: 13,
            curline: 3,
            curcol: 0,
            line_count: 100,
            scroll_delta: 3,
        });
        batch(&mut a, &mut m, events, true, t0);
        let frame = compose::compose(&m, &a, false, 30, 12);
        assert_eq!(
            frame.get(0, 0).map(|c| c.text.clone()),
            Some(Text::Char('0'))
        );
        a.advance(t0 + Duration::from_secs(1));
        let frame = compose::compose(&m, &a, false, 30, 12);
        assert_eq!(
            frame.get(0, 0).map(|c| c.text.clone()),
            Some(Text::Char('3'))
        );
    }

    /// The terminal's cursor is hidden while the drawn one travels, and is
    /// back in its cell, in its shape, when it arrives.
    #[test]
    fn the_terminals_cursor_waits_for_the_drawn_one() {
        let (mut a, mut m, t0) = (Animator::new(effects()), model(), Instant::now());
        setup(&mut a, &mut m, t0);
        batch(
            &mut a,
            &mut m,
            vec![Event::GridCursor {
                grid: 2,
                row: 5,
                col: 5,
            }],
            true,
            t0,
        );
        let mut frame = compose::compose(&m, &a, false, 30, 12);
        a.advance(t0 + Duration::from_millis(30));
        let cursor = a.paint(&mut frame, &m, t0);
        assert!(!cursor.visible);
        a.advance(t0 + Duration::from_secs(1));
        let mut frame = compose::compose(&m, &a, false, 30, 12);
        let cursor = a.paint(&mut frame, &m, t0);
        assert!(cursor.visible);
        assert_eq!((cursor.row, cursor.col, cursor.shape), (5, 5, 2));
    }

    /// In insert mode with `insert = false` the cursor jumps.
    #[test]
    fn insert_mode_can_be_left_alone() {
        let mut e = effects();
        e.smear_in_insert = false;
        let (mut a, mut m, t0) = (Animator::new(e), model(), Instant::now());
        setup(&mut a, &mut m, t0);
        batch(
            &mut a,
            &mut m,
            vec![
                Event::ModeChange {
                    name: "insert".into(),
                    index: 0,
                },
                Event::GridCursor {
                    grid: 2,
                    row: 3,
                    col: 3,
                },
            ],
            true,
            t0,
        );
        assert!(!a.smear.moving());
    }

    /// Without `ext_multigrid`, a `grid_scroll` is the current window's when
    /// its rows are the window's `scroll_delta`, and not when they are not.
    #[test]
    fn a_linegrid_scroll_is_known_by_its_delta() {
        let mut m = model();
        let mut changes = Changes::default();
        m.apply(
            Event::GridResize {
                grid: 1,
                width: 10,
                height: 10,
            },
            &mut changes,
        );
        let scroll = |rows| Event::GridScroll {
            grid: 1,
            top: 1,
            bot: 9,
            left: 0,
            right: 10,
            rows,
        };
        let viewport = |delta| Event::WinViewport {
            grid: 2,
            win: Some(1000),
            topline: 0,
            botline: 0,
            curline: 0,
            curcol: 0,
            line_count: 0,
            scroll_delta: delta,
        };
        let snap = linegrid_scroll(&m, &[scroll(3), viewport(3)]).expect("a scroll");
        assert_eq!(snap.rect, (1, 9, 0, 10));
        assert_eq!(snap.delta, 3);
        assert!(
            linegrid_scroll(&m, &[scroll(3), viewport(0)]).is_none(),
            "lines deleted"
        );
        assert!(linegrid_scroll(&m, &[scroll(2), viewport(3)]).is_none());
    }

    /// The cursor's colour is its highlight's background, and the editor's
    /// text colour where it has no highlight.
    #[test]
    fn the_cursor_is_drawn_in_its_highlights_colour() {
        let mut m = model();
        let mut changes = Changes::default();
        for e in [
            Event::OptionSet {
                name: "termguicolors".into(),
                value: OptionValue::Bool(true),
            },
            Event::HlAttr {
                id: 5,
                rgb: crate::client::style::Attrs {
                    bg: Some(0x00ff00),
                    fg: Some(0x000011),
                    ..Default::default()
                },
                cterm: Default::default(),
            },
            Event::ModeInfoSet {
                enabled: true,
                modes: vec![ModeInfo {
                    shape: Some(Shape::Block),
                    attr_id: 5,
                    ..ModeInfo::default()
                }],
            },
        ] {
            m.apply(e, &mut changes);
        }
        m.refresh_styles();
        assert_eq!(cursor_colors(&m), (Rgb(0, 255, 0), Some(Rgb(0, 0, 0x11))));
        let frame = Frame::new(4, 4, Default::default());
        assert_eq!(hardware_cursor(&m, &frame).color, Some(Rgb(0, 255, 0)));
        m.apply(
            Event::ModeInfoSet {
                enabled: true,
                modes: vec![ModeInfo {
                    shape: Some(Shape::Block),
                    ..ModeInfo::default()
                }],
            },
            &mut changes,
        );
        assert_eq!(cursor_colors(&m), (Rgb(255, 255, 255), None));
        assert_eq!(hardware_cursor(&m, &frame).color, None);
    }
}
