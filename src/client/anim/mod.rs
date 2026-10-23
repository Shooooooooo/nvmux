//! Everything that moves.
//!
//! Animations hold the difference between the model ([`super::model`]: the
//! editor as Neovim last drew it, never changed here) and this frame. Each
//! frame draws the model through them ([`super::compose::View`]) with the
//! cursor on top ([`Animator::paint`]). Each batch is inspected before it is
//! applied, for what it will lose ([`Animator::before`]), and after, for what
//! to start ([`Animator::after`]); nothing animates on the first.

pub mod blink;
pub mod chain;
pub mod glide;
pub mod layout;
pub mod motion;
pub mod raster;
pub mod scroll;
pub mod side;
pub mod smear;
pub mod spring;
pub mod switch;
pub mod vfx;

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use self::blink::Blink;
use self::chain::{Chain, Step};
use self::layout::{Change, Transition};
use self::motion::Motion;
use self::scroll::Scroll;
pub use self::scroll::{Ahead, Fill, Land, Lander};
use self::side::Side;
use self::smear::{Rect, Smear};
use self::switch::Switch;
use self::vfx::Vfx;
use super::compose::{Frame, Patch, Scrolling, View};
use super::grid::{Cell, Grid, Text};
use super::model::{Changes, Held, Margins, Model, Place};
use super::predict::{Proof, Rest};
use super::redraw::{Event, ModeInfo, Shape};
use super::screen::Cursor;
use super::style::{self, Color};
use crate::palette::Rgb;

pub const FRAME: Duration = crate::fade::FRAME;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Effects {
    pub smear: Option<smear::Settings>,
    pub vfx: Option<vfx::Settings>,
    /// Seconds to settle, and the rows of a long jump animated.
    pub scroll: Option<(f32, usize)>,
    pub windows: Option<Windows>,
    pub blink: bool,
}

/// Effect parameters are fixed; the config only toggles effects and picks
/// the particle mode. The cursor is quicker than Neovide's.
const SMEAR: smear::Settings = smear::Settings {
    duration: 0.1,
    short: 0.04,
    trail: 0.4,
    gradient: 1.0,
};

const PARTICLES: vfx::Settings = vfx::Settings {
    mode: vfx::Mode::Railgun,
    opacity: 0.8,
    lifetime: 0.5,
    density: 2.0,
    speed: 6.0,
};

/// A scroll slides for 100 ms (Neovide: 300). One started mid-slide absorbs
/// what was left, so the last lands at most 100 ms after its key (see
/// [`glide`]). A jump further than the window height slides one row.
const SCROLL: (f32, usize) = (0.1, 1);

/// Neovide's slide timing and animate.nvim's for the rest.
const WINDOWS: Windows = Windows {
    slide: 0.15,
    resize: 0.15,
    open: 0.2,
    close: 0.18,
    switch: 0.2,
};

/// Window animation durations in seconds; 0 for instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Windows {
    /// Floats, the message area and rearranged windows ([`motion`]).
    pub slide: f32,
    /// Splits resizing, opening and closing ([`layout`]), and buffer
    /// switches fading ([`switch`]).
    pub resize: f32,
    pub open: f32,
    pub close: f32,
    pub switch: f32,
}

impl Effects {
    pub fn none() -> Self {
        Self {
            smear: None,
            vfx: None,
            scroll: None,
            windows: None,
            blink: false,
        }
    }

    /// Whether any effect needs windows on their own grids.
    pub fn want_multigrid(&self) -> bool {
        self.windows.is_some() || self.scroll.is_some()
    }

    pub fn from_settings(s: &crate::config::Settings) -> Self {
        let e = &s.effects;
        Self {
            smear: e.smear_enabled().then_some(SMEAR),
            vfx: e.particles_enabled().then(|| vfx::Settings {
                mode: vfx::Mode::named(e.particles.mode.name()).unwrap_or(vfx::Mode::Railgun),
                ..PARTICLES
            }),
            scroll: e.scroll_enabled().then_some(SCROLL),
            windows: e.windows_enabled().then_some(WINDOWS),
            blink: e.blink_enabled(),
        }
    }
}

/// What a batch is about to change, captured before it is applied.
#[derive(Debug, Default)]
pub struct Before {
    /// Each visible grid's position.
    origins: HashMap<u64, (i64, i64)>,
    /// The rows each scrolling rectangle will lose, and by how much.
    scrolls: Vec<Snap>,
    /// The splits and their contents, if the batch may move them.
    layout: Option<layout::Shot>,
    /// Windows about to show another buffer, as they were.
    switched: Vec<switch::Was>,
    /// Windows scrolled ahead, with line counts: an edit or another buffer
    /// ends a prediction.
    predicted: Vec<(u64, i64)>,
    /// Windows about to show another buffer, by grid.
    switching: Vec<u64>,
}

#[derive(Debug)]
struct Snap {
    grid: u64,
    rect: (usize, usize, usize, usize),
    /// The rows scrolled out of view (see [`scroll::leaving`]).
    gone: scroll::Gone,
    delta: i64,
}

impl Snap {
    /// `cursor` is the cursor's buffer line when those rows were drawn.
    fn take(
        grid: u64,
        g: &Grid,
        rect: (usize, usize, usize, usize),
        delta: i64,
        cursor: Option<i64>,
    ) -> Self {
        let (top, bot) = scroll::leaving(rect, delta);
        Self {
            grid,
            rect,
            gone: scroll::Gone {
                rows: g.lines(top, bot, rect.2, rect.3),
                cursor,
            },
            delta,
        }
    }
}

/// A grid as [`Animator::insets`] last saw it: position, window width (0 for
/// a float) and text inset (see [`Model::inset`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Inset {
    origin: (i64, i64),
    width: usize,
    inset: usize,
}

#[derive(Debug)]
pub struct Animator {
    effects: Effects,
    smear: Smear,
    vfx: Vfx,
    scrolls: HashMap<u64, Scroll>,
    /// Windows scrolled sideways ahead of Neovim, by grid.
    sides: HashMap<u64, Side>,
    /// The cursor moved ahead of Neovim, by grid: see [`chain`].
    chains: HashMap<u64, Chain>,
    /// Row overlays by grid and grid row, while a window shows content ahead
    /// of Neovim (see `crate::client::App::repaint`).
    patches: HashMap<u64, BTreeMap<usize, Vec<Patch>>>,
    /// Client key numbers of steps dropped here unjudged, so the client knows
    /// to redo them (see [`Animator::take_dropped`]).
    dropped: Vec<u64>,
    motions: HashMap<u64, Motion>,
    /// Text sliding sideways within its window, by grid (see
    /// [`Animator::insets`]).
    shifts: HashMap<u64, Motion>,
    /// Each grid on show as [`Animator::insets`] last saw it.
    seen: HashMap<u64, Inset>,
    transition: Option<Transition>,
    switches: HashMap<u64, Switch>,
    options: layout::Options,
    blink: Blink,
    /// See [`Animator::hold_blink`].
    blink_held: bool,
    /// When animations last advanced.
    last: Option<Instant>,
    /// The cell the cursor was last in, which particles fly from.
    cell: Option<(i64, i64)>,
    /// The mode last drawn; a change restarts the blink's wait.
    mode: usize,
    predicting: bool,
}

impl Animator {
    pub fn new(effects: Effects) -> Self {
        Self {
            effects,
            smear: Smear::default(),
            vfx: Vfx::default(),
            scrolls: HashMap::new(),
            sides: HashMap::new(),
            chains: HashMap::new(),
            patches: HashMap::new(),
            dropped: Vec::new(),
            motions: HashMap::new(),
            shifts: HashMap::new(),
            seen: HashMap::new(),
            transition: None,
            switches: HashMap::new(),
            options: layout::Options::default(),
            blink: Blink::new(Instant::now()),
            blink_held: false,
            last: None,
            cell: None,
            mode: 0,
            predicting: false,
        }
    }

    /// Running animations finish; disabled ones do not start.
    pub fn set_effects(&mut self, effects: Effects) {
        self.effects = effects;
    }

    /// Whether the client draws ahead of Neovim (see
    /// [`crate::client::predict::Predictor`]). Only then do scrolls keep the
    /// lines a scroll ahead may return over (see [`Scroll::scrolled`]): tens
    /// of MB on a large terminal while a key is held.
    pub fn set_predicting(&mut self, on: bool) {
        self.predicting = on;
    }

    /// Stop everything; the next batch places the cursor without animating.
    pub fn reset(&mut self) {
        self.smear = Smear::default();
        self.vfx.clear();
        self.scrolls.clear();
        self.sides.clear();
        for (_, chain) in self.chains.drain() {
            self.dropped.extend(chain.seqs());
        }
        self.patches.clear();
        self.motions.clear();
        self.shifts.clear();
        self.seen.clear();
        self.transition = None;
        self.switches.clear();
        self.cell = None;
    }

    /// Whether window animations need the in-editor agent's reports.
    pub fn wants_agent(&self) -> bool {
        self.effects.windows.is_some()
    }

    pub fn set_options(&mut self, options: layout::Options) {
        self.options = options;
    }

    /// Protect highlights of cells animations still show from a sweep (see
    /// [`Model::reclaim`]).
    pub fn hold(&self, held: &mut Held) {
        for s in self.scrolls.values() {
            s.hold(held);
        }
        for s in self.sides.values() {
            s.hold(held);
        }
        if let Some(t) = &self.transition {
            t.hold(held);
        }
        for s in self.switches.values() {
            s.hold(held);
        }
    }

    /// A key was typed: do not leave the cursor faded out under it.
    pub fn typed(&mut self, now: Instant) {
        self.blink.reset(now);
    }

    /// Hold the cursor solid (no blink frames) while nobody sees the client;
    /// on release the blink restarts its wait.
    pub fn hold_blink(&mut self, held: bool, now: Instant) {
        if self.blink_held && !held {
            self.blink.reset(now);
        }
        self.blink_held = held;
    }

    /// `switched`: window grids about to show another buffer.
    pub fn before(
        &self,
        model: &Model,
        batch: &[Event],
        multigrid: bool,
        switched: &[u64],
    ) -> Before {
        let origins = model
            .layout
            .iter()
            .filter(|(_, p)| !p.hidden)
            .map(|(g, p)| (*g, p.origin()))
            .collect();
        let mut scrolls = Vec::new();
        // Track windows scrolled ahead even when scrolls do not slide:
        // Neovim's scroll confirms them.
        if self.effects.scroll.is_some() || !self.scrolls.is_empty() {
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
                    if self.effects.scroll.is_none() && !self.scrolls.contains_key(&grid) {
                        continue;
                    }
                    // The cursor line the rows were drawn with.
                    let cursor = model.viewports.get(&grid).map(|v| v.curline);
                    if let Some(rect) = inner(g, model.margins(grid)) {
                        scrolls.push(Snap::take(grid, g, rect, delta, cursor));
                    }
                }
            } else if let Some(snap) =
                linegrid_scroll(model, batch).filter(|_| self.effects.scroll.is_some())
            {
                scrolls.push(snap);
            }
        }
        let predicted = self
            .scrolls
            .iter()
            .filter(|(_, s)| s.ahead() != 0)
            .map(|(g, _)| (*g, model.viewports.get(g).map_or(-1, |v| v.line_count)))
            .collect();
        let windows = self.effects.windows.filter(|_| multigrid);
        let layout = windows
            .filter(|_| layout::moves_windows(model, batch))
            .and_then(|_| {
                layout::Shot::take(model, self.options.laststatus, self.transition.as_ref())
            });
        let switched_grids = switched;
        // Only split windows fade, and only a few at once: `:windo bnext`
        // fades none.
        let mut switched: Vec<switch::Was> = match windows {
            Some(w) if w.switch > 0.0 => switched
                .iter()
                .filter_map(|grid| switch::Was::take(model, *grid))
                .collect(),
            _ => Vec::new(),
        };
        if switched.len() > switch::MAX_BURST {
            switched.clear();
        }
        Before {
            origins,
            scrolls,
            layout,
            switched,
            predicted,
            switching: switched_grids.to_vec(),
        }
    }

    /// `judging`: client number of the last key Neovim's reply in this batch
    /// covers, if any (see [`chain`]).
    pub fn after(
        &mut self,
        before: Before,
        model: &Model,
        changes: &Changes,
        (multigrid, drawn): (bool, bool),
        judging: Option<u64>,
        now: Instant,
    ) {
        if !self.moving() {
            self.last = Some(now);
        }
        for grid in &changes.reshaped {
            self.scrolls.remove(grid);
            self.sides.remove(grid);
            self.motions.remove(grid);
        }
        // A window scrolled sideways ahead that scrolls vertically or
        // switches buffer is left to Neovim.
        for grid in before
            .scrolls
            .iter()
            .map(|s| &s.grid)
            .chain(&before.switching)
        {
            self.sides.remove(grid);
        }

        // Splits opening, closing or resizing animate like animate.nvim;
        // rearranged ones slide like Neovide's.
        let live = drawn && multigrid;
        let mut slide_splits = true;
        let mut started = false;
        if let (Some(w), Some(shot), true) = (self.effects.windows, before.layout, live) {
            let change = match layout::Layout::of(model, self.options.laststatus) {
                Some(new) => layout::change(shot, &new, model, &w, self.options),
                None => Change::Slide,
            };
            match change {
                Change::Same => {}
                Change::Animate(t) => {
                    self.transition = Some(*t);
                    slide_splits = false;
                    started = true;
                }
                Change::Snap => {
                    self.transition = None;
                    slide_splits = false;
                }
                Change::Slide => self.transition = None,
            }
        }
        if !live || self.effects.windows.is_none() {
            self.transition = None;
            self.switches.clear();
        }

        // Buffer switches fade, unless windows are moving.
        if let (Some(w), true, false) = (self.effects.windows, live, started) {
            for was in before.switched {
                if let Some(s) = Switch::start(was, model, w.switch) {
                    self.switches.insert(s.grid(), s);
                }
            }
        }
        if started {
            self.switches.clear();
        }
        self.switches.retain(|_, s| s.still(model));

        // Moved floats, message area and rearranged windows slide.
        match self.effects.windows {
            Some(_) if live => {
                for (grid, p) in &model.layout {
                    if p.hidden {
                        continue;
                    }
                    if !slide_splits && matches!(p.place, Place::Window { .. }) {
                        self.motions.remove(grid);
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

        // Predictions invalidated by a buffer switch or line count change.
        for (grid, lines) in &before.predicted {
            let now_lines = model.viewports.get(grid).map_or(-1, |v| v.line_count);
            if now_lines != *lines || before.switching.contains(grid) {
                if let Some(s) = self.scrolls.get_mut(grid) {
                    s.give_up();
                }
            }
        }

        if self.effects.scroll.is_some() || !self.scrolls.is_empty() {
            let far = self.effects.scroll.map_or(0, |(_, far)| far);
            for snap in before.scrolls {
                let Some(g) = model.grids.get(&snap.grid) else {
                    continue;
                };
                // Margins or size changed too: a layout change, not a scroll.
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
                match self.scrolls.get_mut(&snap.grid) {
                    Some(s) if s.rect() == snap.rect => {
                        s.scrolled(snap.gone, snap.delta, far, self.predicting);
                    }
                    _ if self.effects.scroll.is_some() => {
                        self.scrolls.insert(
                            snap.grid,
                            Scroll::new(snap.rect, snap.gone, snap.delta, far),
                        );
                    }
                    _ => {
                        self.scrolls.remove(&snap.grid);
                    }
                }
            }
            if multigrid {
                self.scrolls
                    .retain(|g, _| model.layout.get(g).is_some_and(|p| !p.hidden));
            }
        }

        // Drop a chain whose basis is gone, snapping cursor and view to
        // Neovim's: window reshaped, hidden or switched; line count changed;
        // view moved other than by its own scrolls ahead (see `Chain::views`);
        // or a stepped line drawn differently than seen, unless it matches as
        // typed or Neovim's reply to its keys (or to an Insert enter/leave,
        // where autocommands may edit) is here to judge it (see
        // `crate::client::App::judge_steps`).
        let gone: Vec<u64> = self
            .chains
            .iter()
            .filter(|(grid, chain)| {
                let judged = judging.and_then(|k| chain.step_for(k));
                let mode = judged.is_some_and(|s| matches!(s.proof, Proof::Enter | Proof::Leave));
                let ahead = self.scrolls.get(grid).map_or(0, Scroll::ahead);
                changes.reshaped.contains(grid)
                    || before.switching.contains(grid)
                    || model.layout.get(grid).is_none_or(|p| p.hidden)
                    || model.viewports.get(grid).map(|v| v.line_count)
                        != chain.head().map(|s| s.line_count)
                    || !model
                        .viewports
                        .get(grid)
                        .is_some_and(|v| chain.views(v.topline + ahead))
                    || chain.lines.iter().any(|(line, drawn)| {
                        let now = line_drawn(model, **grid, *line, chain.textoff);
                        let seen = match drawn {
                            Some(seen) => seen.agrees(now.as_deref()),
                            None => now.is_none(),
                        };
                        !seen
                            && !chain.typed(*line, now.as_deref())
                            && !(mode || (judged.is_some() && chain.types_into(*line)))
                    })
            })
            .map(|(grid, _)| *grid)
            .collect();
        let snap = !gone.is_empty();
        for grid in gone {
            if let Some(chain) = self.chains.remove(&grid) {
                self.dropped.extend(chain.seqs());
            }
            if let Some(s) = self.scrolls.get_mut(&grid) {
                s.give_up();
            }
        }

        // Windows starting to move carry the cursor with them.
        self.cursor_moved(model, drawn && !started && !snap, now);
    }

    /// Move the cursor to its new place, animated only if `allowed`.
    fn cursor_moved(&mut self, model: &Model, allowed: bool, now: Instant) {
        let (row, col) = self.cursor_at(model);
        let Some(target) = cursor_rect(self.mode_info(model), (row, col)) else {
            return;
        };
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
        let mode = self.mode_drawn(model);
        if mode != self.mode {
            self.mode = mode;
            self.blink.reset(now);
        }
    }

    /// Scroll `grid` ahead of Neovim as `go` says (see
    /// [`crate::client::predict`]); `fill` draws uncovered lines. Returns
    /// whether it scrolled.
    pub fn predict(
        &mut self,
        model: &Model,
        grid: u64,
        go: Ahead,
        (fill, land): (&mut Fill, &mut Lander),
        now: Instant,
    ) -> bool {
        let Some(rect) = text_rect(model, grid) else {
            return false;
        };
        if !self.moving() {
            self.last = Some(now);
        }
        let animate = self.effects.scroll.is_some();
        let s = match self.scrolls.get_mut(&grid) {
            Some(s) if s.rect() == rect => s,
            _ => {
                self.scrolls.insert(grid, Scroll::still(rect, animate));
                self.scrolls.get_mut(&grid).expect("just put there")
            }
        };
        let drawn = s.shown_ahead();
        let went = s.predict(go, fill, land);
        if went && s.shown_ahead() != drawn {
            self.cursor_moved(model, true, now);
        }
        went
    }

    /// Move `grid`'s cursor ahead of Neovim by `step` (see [`chain`]). The
    /// first step starts a chain from `shown` (window, gutter width, Neovim's
    /// first display column); each records its line as `drawn` when first
    /// onto it. The cursor travels there if `travel`, else is placed.
    pub fn step(
        &mut self,
        model: &Model,
        grid: u64,
        (shown, travel): ((i64, usize, usize), bool),
        (step, drawn): (Step, impl FnOnce() -> chain::Drawn),
        now: Instant,
    ) {
        let line = step.pos.line;
        let top = model.viewports.get(&grid).map_or(0, |v| v.topline)
            + self.scrolls.get(&grid).map_or(0, Scroll::ahead);
        let chain = match self.chains.entry(grid) {
            Entry::Occupied(c) => {
                let c = c.into_mut();
                c.onto(line, drawn);
                c
            }
            Entry::Vacant(v) => v.insert(Chain::new(shown, line, drawn())),
        };
        chain.push(step);
        chain.top = top;
        self.cursor_moved(model, travel, now);
    }

    /// Overlay `patches` (by grid row) on `grid` while its content ahead of
    /// Neovim stays the same; empty clears them.
    pub fn patch(&mut self, grid: u64, patches: BTreeMap<usize, Vec<Patch>>) {
        if patches.is_empty() {
            self.patches.remove(&grid);
        } else {
            self.patches.insert(grid, patches);
        }
    }

    pub fn patches(&self, grid: u64) -> Option<&BTreeMap<usize, Vec<Patch>>> {
        self.patches.get(&grid)
    }

    pub fn patched(&self) -> Vec<u64> {
        let mut grids: Vec<u64> = self.patches.keys().copied().collect();
        grids.sort_unstable();
        grids
    }

    /// The newest step in `grid` whose view matches the screen.
    pub fn shown_step(&self, model: &Model, grid: u64) -> Option<&Step> {
        let chain = self.chains.get(&grid)?;
        let v = model.viewports.get(&grid)?;
        let shown = v.topline + self.scrolls.get(&grid).map_or(0, Scroll::shown_ahead);
        chain.shown(shown, chain.left)
    }

    pub fn chain(&self, grid: u64) -> Option<&Chain> {
        self.chains.get(&grid)
    }

    /// Drop `grid`'s steps for keys up to `seq`, whose frame has come.
    pub fn drop_upto(&mut self, grid: u64, seq: u64) {
        if let Some(chain) = self.chains.get_mut(&grid) {
            chain.drop_upto(seq);
            if chain.is_empty() {
                self.chains.remove(&grid);
            }
        }
    }

    /// Drop `grid`'s chain, snapping the cursor; returns its key numbers.
    pub fn drop_chain(&mut self, model: &Model, grid: u64, now: Instant) -> Vec<u64> {
        let Some(chain) = self.chains.remove(&grid) else {
            return Vec::new();
        };
        self.cursor_moved(model, false, now);
        chain.seqs()
    }

    /// Whether `grid`'s chain steps are exactly its scrolls ahead, so the
    /// scrolls alone place the cursor (see [`Ahead::cursor`]).
    pub fn scrolls_alone(&self, grid: u64) -> bool {
        let (Some(chain), Some(s)) = (self.chains.get(&grid), self.scrolls.get(&grid)) else {
            return false;
        };
        chain.seqs() == s.seqs()
    }

    /// Drop `grid`'s chain without redoing its keys; its scrolls ahead keep
    /// the cursor where the steps put it.
    pub fn unchain(&mut self, model: &Model, grid: u64, now: Instant) {
        self.chains.remove(&grid);
        self.cursor_moved(model, false, now);
    }

    pub fn chained(&self) -> Vec<u64> {
        let mut grids: Vec<u64> = self.chains.keys().copied().collect();
        grids.sort_unstable();
        grids
    }

    /// Drop chains whose oldest step Neovim has not answered by `now`;
    /// returns each grid with its steps' client key numbers.
    pub fn lapsed(&mut self, model: &Model, now: Instant) -> Vec<(u64, Vec<u64>)> {
        let grids: Vec<u64> = self
            .chains
            .iter()
            .filter(|(_, c)| c.until().is_some_and(|until| until <= now))
            .map(|(g, _)| *g)
            .collect();
        let lapsed: Vec<(u64, Vec<u64>)> = grids
            .into_iter()
            .filter_map(|g| Some((g, self.chains.remove(&g)?.seqs())))
            .collect();
        if !lapsed.is_empty() {
            self.cursor_moved(model, false, now);
        }
        lapsed
    }

    /// Client key numbers of steps dropped unjudged since the last call.
    pub fn take_dropped(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.dropped)
    }

    /// Window grids scrolled ahead further than they can show.
    pub fn held(&self) -> Vec<u64> {
        self.scrolls
            .iter()
            .filter(|(_, s)| s.held())
            .map(|(g, _)| *g)
            .collect()
    }

    /// Draw what `grid` scrolled ahead to but could not show, as far as
    /// `fill` and `land` now allow (see [`Scroll::fill_in`]).
    pub fn fill_in(
        &mut self,
        model: &Model,
        grid: u64,
        (fill, land): (&mut Fill, &mut Lander),
        now: Instant,
    ) {
        let Some(s) = self.scrolls.get_mut(&grid) else {
            return;
        };
        if s.fill_in(fill, land) {
            if !self.moving() {
                self.last = Some(now);
            }
            self.cursor_moved(model, true, now);
        }
    }

    /// Window grids with a scroll ahead from client input `k` or earlier that
    /// Neovim has not made yet (see [`Scroll::unmade`]).
    pub fn unmade(&self, k: u64) -> Vec<u64> {
        let mut grids: Vec<u64> = self
            .scrolls
            .iter()
            .filter(|(_, s)| s.unmade(k))
            .map(|(g, _)| *g)
            .collect();
        grids.sort_unstable();
        grids
    }

    /// Stop scrolling `grid` ahead; returns the client numbers of the inputs
    /// behind the dropped scrolls.
    pub fn give_up(&mut self, grid: u64) -> Vec<u64> {
        let Some(s) = self.scrolls.get_mut(&grid) else {
            return Vec::new();
        };
        let seqs = s.seqs();
        s.give_up();
        seqs
    }

    /// Keep every scroll ahead and chain step until at least `until`: Neovim
    /// is still working on their keys.
    pub fn defer(&mut self, until: Instant) {
        for s in self.scrolls.values_mut() {
            s.defer(until);
        }
        for s in self.sides.values_mut() {
            s.defer(until);
        }
        for c in self.chains.values_mut() {
            c.defer(until);
        }
    }

    /// Rows `grid` is ahead of Neovim, and lines its cursor is.
    pub fn ahead(&self, grid: u64) -> (i64, i64) {
        self.scrolls
            .get(&grid)
            .map_or((0, 0), |s| (s.ahead(), s.cursor()))
    }

    /// The grid column `grid`'s cursor was put in ahead of Neovim, and its
    /// wanted display column, if a prediction set them.
    pub fn col(&self, grid: u64) -> Option<(usize, usize)> {
        self.scrolls.get(&grid).and_then(Scroll::col)
    }

    /// Whether `grid`'s view is sliding (see [`Scroll`]).
    pub fn gliding(&self, grid: u64) -> bool {
        self.scrolls.get(&grid).is_some_and(Scroll::moving)
    }

    /// Whether `grid` was scrolled vertically ahead of Neovim, drawn or not.
    pub fn scrolled_ahead(&self, grid: u64) -> bool {
        self.scrolls.get(&grid).is_some_and(|s| s.until().is_some())
    }

    /// Window grids whose rows show other lines than their grid this frame.
    pub fn scrolled(&self) -> Vec<u64> {
        let mut grids: Vec<u64> = self
            .scrolls
            .iter()
            .filter(|(_, s)| s.shown_ahead() != 0 || s.offset() != 0)
            .map(|(g, _)| *g)
            .collect();
        grids.sort_unstable();
        grids
    }

    /// How many lines further down the buffer `grid`'s rows show than its
    /// grid this frame: scrolled ahead, minus what is left to slide.
    pub fn rows_shown(&self, grid: u64) -> i64 {
        self.scrolls
            .get(&grid)
            .map_or(0, |s| s.shown_ahead() - s.offset())
    }

    /// The line `i` rows below `grid`'s top in Neovim's view, if its rows
    /// show it from outside that view, and who drew it (see
    /// [`scroll::Kept`]); `None` if inside the view or unknown.
    pub fn kept(&self, grid: u64, i: i64) -> Option<&scroll::Kept> {
        self.scrolls.get(&grid)?.kept(i)
    }

    /// The buffer line the cursor is drawn on in `grid` (`h` rows tall): the
    /// shown step's, if the cursor moved ahead (see [`chain`]); if scrolled
    /// ahead, Neovim's line moved by those scrolls and kept in view by
    /// `'scrolloff'`, as [`Animator::cursor_at`] draws it; else Neovim's.
    pub fn cursor_line(&self, model: &Model, grid: u64, h: i64) -> Option<i64> {
        let v = model.viewports.get(&grid)?;
        if let Some(s) = self.shown_step(model, grid) {
            return Some(s.pos.line);
        }
        let Some(s) = self.scrolls.get(&grid).filter(|s| s.shown_ahead() != 0) else {
            return Some(v.curline);
        };
        let top = v.topline + s.shown_ahead();
        let last = v.line_count - 1;
        let so = (s.scrolloff() as i64).min((h - 1) / 2);
        let lo = if top > 0 { top + so } else { top };
        let hi = if top + h - 1 < last {
            top + h - 1 - so
        } else {
            top + h - 1
        };
        Some(
            (v.curline + s.shown_cursor())
                .max(lo)
                .min(hi)
                .min(last)
                .max(top.max(0)),
        )
    }

    /// The rows `grid` shows scrolled vertically ahead, as they will be once
    /// settled, and how many rows ahead of Neovim's view; `None` if none.
    pub fn rows_ahead(&self, model: &Model, grid: u64) -> Option<(Vec<Vec<Cell>>, i64)> {
        let s = self.scrolls.get(&grid).filter(|s| s.shown_ahead() != 0)?;
        Some((s.rows_ahead(model.grids.get(&grid)?)?, s.shown_ahead()))
    }

    /// Stop `grid`'s scroll-ahead slide (see [`Scroll::settle`]).
    pub fn settle(&mut self, grid: u64) {
        if let Some(s) = self.scrolls.get_mut(&grid) {
            s.settle();
        }
    }

    /// Scroll `grid` sideways ahead of Neovim to first column `left`, giving
    /// up at `until`. Its text rectangle shows `rows`, with the cursor at
    /// `cursor` in its grid if there. See [`side`].
    pub fn side(
        &mut self,
        model: &Model,
        grid: u64,
        rows: Vec<Vec<Cell>>,
        (left, until): (usize, Instant),
        cursor: Option<(usize, usize)>,
        now: Instant,
    ) {
        let Some(rect) = text_rect(model, grid) else {
            return;
        };
        let side = self.sides.entry(grid).or_insert_with(|| Side::new(rect));
        side.push(rows, left, until, cursor);
        self.cursor_moved(model, false, now);
    }

    /// The first column `grid`'s view is heading to ahead of Neovim, if any.
    pub fn side_left(&self, grid: u64) -> Option<usize> {
        self.sides.get(&grid).and_then(Side::left)
    }

    pub fn sided(&self) -> Vec<u64> {
        self.sides.keys().copied().collect()
    }

    /// Neovim's view of `grid` now starts at `left` (see [`Side::landed`]).
    pub fn landed(&mut self, grid: u64, left: usize) {
        if let Some(side) = self.sides.get_mut(&grid) {
            if !side.landed(left) {
                self.sides.remove(&grid);
            }
        }
    }

    /// The rows `grid`, scrolled sideways ahead, shows now that Neovim has
    /// redrawn it; `None` if they cannot be drawn, which ends the scroll.
    pub fn refresh_side(&mut self, grid: u64, rows: Option<Vec<Vec<Cell>>>) {
        match rows {
            Some(rows) => {
                if let Some(side) = self.sides.get_mut(&grid) {
                    let cursor = side.cursor();
                    side.refresh(rows, cursor);
                }
            }
            None => {
                self.sides.remove(&grid);
            }
        }
    }

    /// The cursor's screen position, moved by any scrolls or steps ahead and
    /// kept in view by `'scrolloff'`.
    pub fn cursor_at(&self, model: &Model) -> (i64, i64) {
        let (row, col) = model.cursor_on_screen();
        let c = model.cursor;
        // Follow the text if it is sliding sideways.
        let col = col + self.shift_of(c.grid);
        if let Some((r, k)) = self.stepped(model) {
            return (row - c.row as i64 + r, col - c.col as i64 + k);
        }
        if let Some((r, k)) = self.sides.get(&c.grid).and_then(Side::cursor) {
            return (row - c.row as i64 + r as i64, col - c.col as i64 + k as i64);
        }
        let Some(s) = self.scrolls.get(&c.grid).filter(|s| s.shown_ahead() != 0) else {
            return (row, col);
        };
        let (top, bot, _, _) = s.rect();
        if !(top..bot).contains(&c.row) {
            return (row, col);
        }
        let h = (bot - top) as i64;
        let ahead = s.shown_ahead();
        // Rows in the predicted view: the cursor's line, and the buffer's
        // first and last lines.
        let at = c.row as i64 - top as i64 - ahead + s.shown_cursor();
        let so = (s.scrolloff() as i64).min((h - 1) / 2);
        let (first, last) = model
            .viewports
            .get(&c.grid)
            .map_or((i64::MIN, i64::MAX), |v| {
                let topline = v.topline + ahead;
                (-topline, v.line_count - 1 - topline)
            });
        // Keep `so` rows from either edge, except at the buffer's own ends,
        // and on a buffer line.
        let lo = if first < 0 { so } else { 0 };
        let hi = if last >= h { h - 1 - so } else { h - 1 };
        let at = at.max(lo).min(hi).min(last).max(first.max(0));
        let col = s
            .shown_col()
            .map_or(col, |(to, _)| col - c.col as i64 + to as i64);
        (row + at - (c.row as i64 - top as i64), col)
    }

    /// Where in its grid the cursor is drawn when moved ahead (see
    /// [`chain`]); `None` if not, or out of view.
    fn stepped(&self, model: &Model) -> Option<(i64, i64)> {
        let grid = model.cursor.grid;
        let chain = self.chains.get(&grid)?;
        if model.wins.get(&grid) != Some(&chain.win) {
            return None;
        }
        let v = model.viewports.get(&grid)?;
        let (top, bot, _, right) = text_rect(model, grid)?;
        let shown = v.topline + self.scrolls.get(&grid).map_or(0, Scroll::shown_ahead);
        let s = self.shown_step(model, grid)?;
        let row = s.pos.line - shown;
        let col = (chain.textoff + s.vcol).checked_sub(s.pos.left)?;
        ((0..(bot - top) as i64).contains(&row) && col < right)
            .then_some((top as i64 + row, col as i64))
    }

    /// Whether anything is moving, so a frame is wanted. Excludes the blink,
    /// which is clock-driven: counting it would make [`Animator::advance`]
    /// step anything starting after a quiet spell by the whole spell.
    fn moving(&self) -> bool {
        self.smear.moving()
            || self.vfx.moving()
            || self.scrolls.values().any(Scroll::moving)
            || !self.motions.is_empty()
            || !self.shifts.is_empty()
            || self.transition.is_some()
            || !self.switches.is_empty()
    }

    /// Whether a frame is just the editor's grids in place, as frames built
    /// from changed rows only require (see `crate::client::App::frame`).
    pub fn still(&self) -> bool {
        !self.moving()
            && self.scrolls.is_empty()
            && self.sides.is_empty()
            && self.patches.is_empty()
            && !self.paints_over()
    }

    /// Whether the cursor blinks: a blinking `'guicursor'` block, effect on.
    pub fn blinks(&self, model: &Model) -> bool {
        self.blinking_mode(model).is_some()
    }

    fn blinking_mode<'a>(&self, model: &'a Model) -> Option<&'a ModeInfo> {
        self.mode_info(model)
            .filter(|m| self.effects.blink && m.blinks() && m.shape == Some(Shape::Block))
    }

    /// The index in [`Model::modes`] of the mode whose cursor is drawn: the
    /// mode the shown step leaves Neovim in, where that differs from what
    /// Neovim last drew (Insert's bar on `i` before Neovim hears of it,
    /// Normal's block on `<Esc>`; see [`chain`]), else Neovim's.
    fn mode_drawn(&self, model: &Model) -> usize {
        let grid = model.cursor.grid;
        let step = self
            .chains
            .get(&grid)
            .filter(|c| model.wins.get(&grid) == Some(&c.win))
            .and_then(|_| self.shown_step(model, grid));
        let name = match step.map(|s| s.pos.rest) {
            Some(rest) if rest == Rest::drawn(&model.mode_name) => return model.mode,
            Some(Rest::Insert) => "insert",
            Some(Rest::Normal) => "normal",
            _ => return model.mode,
        };
        model
            .modes
            .iter()
            .position(|m| m.name == name)
            .unwrap_or(model.mode)
    }

    /// `'guicursor'`'s entry for [`Animator::mode_drawn`].
    fn mode_info<'a>(&self, model: &'a Model) -> Option<&'a ModeInfo> {
        model.modes.get(self.mode_drawn(model))
    }

    /// When a fading blink wants its next frame; `None` while held.
    fn blink_due(&self, model: &Model, now: Instant) -> Option<Instant> {
        if self.blink_held {
            return None;
        }
        self.blink
            .next_change(self.blinking_mode(model)?, now, self.last, FRAME)
    }

    pub fn next_frame(&self, model: &Model, now: Instant) -> Option<Instant> {
        let frame = self
            .moving()
            .then(|| self.last.map_or(now, |last| last + FRAME));
        // Wake to expire predictions Neovim has not confirmed.
        let expiry = self
            .scrolls
            .values()
            .filter_map(Scroll::until)
            .chain(self.sides.values().filter_map(Side::until))
            .chain(self.chains.values().filter_map(Chain::until))
            .min();
        [frame, self.blink_due(model, now), expiry]
            .into_iter()
            .flatten()
            .min()
    }

    pub fn advance(&mut self, now: Instant) {
        let dt = self.last.map_or(0.0, |last| {
            now.saturating_duration_since(last).as_secs_f32()
        });
        self.last = Some(now);
        self.smear.step(dt);
        self.vfx.step(dt);
        let duration = self.effects.scroll.map_or(0.0, |(duration, _)| duration);
        self.scrolls.retain(|_, s| {
            s.expire(now);
            s.step(dt, duration)
        });
        self.sides
            .retain(|_, s| s.until().is_some_and(|until| until > now));
        if let Some(w) = self.effects.windows {
            self.motions.retain(|_, m| m.step(dt, w.slide));
            self.shifts.retain(|_, m| m.step(dt, w.slide));
        }
        if self.transition.as_mut().is_some_and(|t| !t.step(dt)) {
            self.transition = None;
        }
        self.switches.retain(|_, s| s.step(dt));
    }

    /// Whether [`Animator::paint`] paints over the frame (not just placing
    /// the cursor), which needs the cells under it repaired first.
    pub fn paints_over(&self) -> bool {
        (self.effects.vfx.is_some() && self.vfx.moving())
            || (self.effects.smear.is_some() && self.smear.moving())
            || self.effects.blink
    }

    /// Paint the cursor and particles over `frame`; returns the terminal's.
    pub fn paint(&self, frame: &mut Frame, model: &Model, now: Instant) -> Cursor {
        let colors = model.colors();
        let mode = self.mode_info(model);
        let (color, text) = cursor_colors(model, mode);
        if let (Some(s), true) = (self.effects.vfx, self.vfx.moving()) {
            self.vfx.paint(frame, &colors, color, &s);
        }
        let mut cursor = hardware_cursor(model, mode, frame, self.cursor_at(model));
        if !cursor.visible {
            return cursor;
        }
        // Hidden while its window moves; it reappears where the window lands.
        if self
            .transition
            .as_ref()
            .is_some_and(|t| t.involves(model.cursor.grid))
        {
            cursor.visible = false;
            return cursor;
        }
        if let (true, Some(s)) = (self.smear.moving(), self.effects.smear) {
            self.smear.paint(frame, &colors, color, text, &s);
            cursor.visible = false;
        } else if let Some(mode) = self.blinking_mode(model) {
            let o = if self.blink_held {
                1.0
            } else {
                self.blink.opacity(mode, now)
            };
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

    /// Record each visible grid's text inset. Text moving in or out within
    /// an unchanged window (its grid resized for centred text; see
    /// `crate::client::wide`) slides there ([`motion`]); a new or moved grid
    /// just appears.
    pub fn insets(&mut self, model: &Model, now: Instant) {
        // Step from now, not from when the last animation ended.
        if !self.moving() {
            self.last = Some(now);
        }
        let mut seen = HashMap::new();
        for (&grid, p) in &model.layout {
            if grid == 1 || p.hidden {
                continue;
            }
            let width = match p.place {
                Place::Window { width, .. } => width,
                _ => 0,
            };
            let here = Inset {
                origin: p.origin(),
                width,
                inset: model.inset(grid),
            };
            if let (Some(was), Some(_)) = (self.seen.get(&grid), self.effects.windows) {
                if (was.origin, was.width) == (here.origin, here.width) && was.inset != here.inset {
                    let by = here.inset as i64 - was.inset as i64;
                    self.shifts.entry(grid).or_default().moved((0, by));
                }
            }
            seen.insert(grid, here);
        }
        self.seen = seen;
        let seen = &self.seen;
        self.shifts.retain(|grid, _| seen.contains_key(grid));
    }

    /// Columns `grid`'s text is from its target while sliding.
    fn shift_of(&self, grid: u64) -> i64 {
        self.shifts.get(&grid).map_or(0, |m| m.offset().1)
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
        match self.sides.get(&grid) {
            Some(side) => Some(side as &dyn Scrolling),
            None => self.scrolls.get(&grid).map(|s| s as &dyn Scrolling),
        }
    }

    fn shift(&self, grid: u64) -> i64 {
        self.shift_of(grid)
    }

    fn patches(&self, grid: u64, r: usize) -> &[Patch] {
        self.patches
            .get(&grid)
            .and_then(|p| p.get(&r))
            .map_or(&[], Vec::as_slice)
    }

    fn over_windows(&self, frame: &mut Frame, model: &Model) {
        let cursor = self.cursor_at(model);
        if let Some(t) = &self.transition {
            t.paint(frame, model, &|grid| self.shift_of(grid), cursor);
        }
        for s in self.switches.values() {
            s.paint(frame, model, cursor);
        }
    }
}

/// `grid`'s text for buffer line `line` past the `textoff` gutter (see
/// [`chain`]); `None` if outside Neovim's view or not one to one.
pub fn line_drawn(model: &Model, grid: u64, line: i64, textoff: usize) -> Option<Vec<Text>> {
    let (g, v) = (model.grids.get(&grid)?, model.viewports.get(&grid)?);
    let (top, bot, left, right) = text_rect(model, grid)?;
    let row = usize::try_from(line - v.topline)
        .ok()
        .filter(|r| *r < bot - top && v.one_to_one(bot - top))?;
    let text = g.row(top + row).get((left + textoff).min(right)..right)?;
    Some(text.iter().map(|c| c.text.clone()).collect())
}

/// The scrolling region of window grid `grid`: `(top, bot, left, right)`.
pub fn text_rect(model: &Model, grid: u64) -> Option<(usize, usize, usize, usize)> {
    inner(model.grids.get(&grid)?, model.margins(grid))
}

/// A grid's scrolling region: all but its margins (winbar, float border).
fn inner(g: &Grid, m: Margins) -> Option<(usize, usize, usize, usize)> {
    let (top, bot) = (m.top, g.height().checked_sub(m.bottom)?);
    let (left, right) = (m.left, g.width().checked_sub(m.right)?);
    (top < bot && left < right).then_some((top, bot, left, right))
}

/// Without `ext_multigrid`, the only animatable scroll: a `grid_scroll` of
/// grid 1 whose rows match the current window's `scroll_delta`, summed over
/// the batch. Anything else (a region scrolled to insert or delete lines, a
/// scroll no window claims) is not a view moving; Neovide skips it too.
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
    Some(Snap::take(1, model.grids.get(&1)?, rect, delta, None))
}

fn cursor_rect(mode: Option<&ModeInfo>, (row, col): (i64, i64)) -> Option<Rect> {
    let shape = mode.and_then(|m| m.shape).unwrap_or(Shape::Block);
    let percent = mode.map_or(100, |m| m.percentage);
    Some(smear::shape_rect(row, col, shape, percent))
}

/// The cursor's colour and that of the character under it, from `mode`'s
/// highlight as Neovide reads it: its background (foreground if reversed),
/// else the editor's text colour.
fn cursor_colors(model: &Model, mode: Option<&ModeInfo>) -> (Rgb, Option<Rgb>) {
    let colors = model.colors();
    let plain = colors.visual_fg(&model.style(0));
    let Some(id) = mode.map(|m| m.attr_id).filter(|id| *id != 0) else {
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

/// How to leave the terminal's cursor: at `(row, col)` (Neovim's or
/// predicted), in `mode`'s shape and colour; hidden while Neovim is busy,
/// when `mode`'s highlight blends it away, or when off screen.
fn hardware_cursor(
    model: &Model,
    mode: Option<&ModeInfo>,
    frame: &Frame,
    (row, col): (i64, i64),
) -> Cursor {
    let on_screen =
        row >= 0 && col >= 0 && (row as usize) < frame.height && (col as usize) < frame.width;
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
    // As Neovim's TUI: colour only with RGB, and reversed means the
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
                gradient: 0.9,
            }),
            vfx: None,
            scroll: Some((0.3, 1)),
            windows: Some(Windows {
                slide: 0.15,
                resize: 0.15,
                open: 0.2,
                close: 0.18,
                switch: 0.2,
            }),
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

    /// Apply a batch as the client does: before, apply, after.
    fn batch(a: &mut Animator, m: &mut Model, events: Vec<Event>, drawn: bool, now: Instant) {
        let before = a.before(m, &events, true, &[]);
        let mut changes = Changes::default();
        for e in events {
            m.apply(e, &mut changes);
        }
        m.refresh_styles();
        a.after(before, m, &changes, (true, drawn), None, now);
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
        lines_in(grid, from, rows, 0)
    }

    /// [`lines`], in Neovim's highlight `hl`.
    fn lines_in(grid: u64, from: usize, rows: usize, hl: u32) -> Vec<Event> {
        (0..rows)
            .map(|r| Event::GridLine {
                grid,
                row: r,
                col: 0,
                cells: vec![LineCell {
                    text: Text::Char(char::from_digit(((from + r) % 10) as u32, 10).unwrap()),
                    hl: Some(hl),
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

    #[test]
    fn nothing_animates_into_the_first_frame() {
        let (mut a, mut m, t0) = (Animator::new(effects()), model(), Instant::now());
        setup(&mut a, &mut m, t0);
        assert!(a.next_frame(&m, t0).is_none());
    }

    #[test]
    fn a_moved_float_slides_and_a_jumped_cursor_travels() {
        let (mut a, mut m, t0) = (Animator::new(effects()), model(), Instant::now());
        setup(&mut a, &mut m, t0);
        let float = |col| Event::WinFloatPos {
            grid: 5,
            win: Some(1005),
            mouse: true,
            zindex: 50,
            compindex: 1,
            row: 0,
            col,
            anchor: None,
        };
        let size = Event::GridResize {
            grid: 5,
            width: 4,
            height: 2,
        };
        batch(&mut a, &mut m, vec![size, float(0)], true, t0);
        assert!(a.motions.is_empty(), "a float new is simply there");
        batch(
            &mut a,
            &mut m,
            vec![
                float(15),
                Event::GridCursor {
                    grid: 2,
                    row: 5,
                    col: 5,
                },
            ],
            true,
            t0,
        );
        assert_eq!(a.origin(5, (0, 15)), (0, 0), "drawn where it was");
        assert!(a.smear.moving());
        assert_eq!(a.next_frame(&m, t0), Some(t0 + FRAME));
        a.advance(t0 + Duration::from_secs(1));
        assert_eq!(a.origin(5, (0, 15)), (0, 15), "and arrived");
        assert!(!a.smear.moving());
    }

    /// A grid narrowed or widened in an unmoved window slides its text, the
    /// cursor with it; a new window, or one resized with its grid, does not.
    #[test]
    fn text_moved_across_its_window_slides_there() {
        let (mut a, mut m, t0) = (Animator::new(effects()), model(), Instant::now());
        setup(&mut a, &mut m, t0);
        let grid = |width| Event::GridResize {
            grid: 2,
            width,
            height: 10,
        };
        let secs = |s: u64| t0 + Duration::from_secs(s);
        a.insets(&m, t0);
        assert_eq!(a.shift_of(2), 0, "seen for the first time");

        // Narrowed by the client (no batch), long after anything moved: a
        // 10-wide window on a 6-wide grid, 2 in.
        m.grids.get_mut(&2).expect("window 2").resize(6, 10);
        a.insets(&m, secs(9));
        assert_eq!(a.shift_of(2), -2, "drawn where it was");
        assert_eq!(a.cursor_at(&m), (0, 0), "the cursor with it");
        assert_eq!(a.next_frame(&m, secs(9)), Some(secs(9) + FRAME));
        a.advance(secs(9) + FRAME);
        assert!(a.shift_of(2) < 0, "on its way from then, not from long ago");
        a.advance(secs(10));
        assert_eq!(a.shift_of(2), 0, "and there");
        assert_eq!(a.cursor_at(&m), (0, 2));

        batch(&mut a, &mut m, vec![grid(10)], true, secs(10));
        a.insets(&m, secs(10));
        assert_eq!(a.shift_of(2), 2, "back out, the other way");
        a.advance(secs(11));
        assert_eq!(a.shift_of(2), 0);

        let wider = Event::WinPos {
            grid: 2,
            win: Some(1002),
            row: 0,
            col: 0,
            width: 20,
            height: 10,
        };
        batch(&mut a, &mut m, vec![wider, grid(12)], true, secs(11));
        a.insets(&m, secs(11));
        assert_eq!(a.shift_of(2), 0, "the window's own move");
    }

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
        let frame = compose::compose(&m, &a, 30, 12);
        assert_eq!(
            frame.get(0, 0).map(|c| c.text.clone()),
            Some(Text::Char('1'))
        );
        a.advance(t0 + Duration::from_secs(1));
        let frame = compose::compose(&m, &a, 30, 12);
        assert_eq!(
            frame.get(0, 0).map(|c| c.text.clone()),
            Some(Text::Char('3'))
        );
    }

    /// Highlight `id` with foreground `0xRRGGBB`.
    fn coloured(id: u32, fg: u32) -> Event {
        Event::HlAttr {
            id,
            rgb: style::Attrs {
                fg: Some(fg),
                ..style::Attrs::default()
            },
            cterm: style::Attrs::default(),
        }
    }

    /// Even after Neovim reuses their highlight id; and a sweep mid-slide
    /// reclaims none of what it shows.
    #[test]
    fn a_scroll_slides_from_its_lines_in_the_colours_they_were_drawn_in() {
        let (mut a, mut m, t0) = (Animator::new(effects()), model(), Instant::now());
        setup(&mut a, &mut m, t0);
        let mut events = vec![
            Event::OptionSet {
                name: "termguicolors".into(),
                value: OptionValue::Bool(true),
            },
            coloured(5, 0xff0000),
        ];
        events.extend(lines_in(2, 0, 10, 5));
        batch(&mut a, &mut m, events, true, t0);
        // Table full: Neovim reuses id 5 for another highlight and draws the
        // scrolled window in it.
        let mut events = vec![
            coloured(5, 0x0000ff),
            Event::GridScroll {
                grid: 2,
                top: 0,
                bot: 10,
                left: 0,
                right: 10,
                rows: 3,
            },
        ];
        events.extend(lines_in(2, 3, 10, 5));
        events.push(viewport(3, 3, 3));
        batch(&mut a, &mut m, events, true, t0);
        let mut held = m.held();
        a.hold(&mut held);
        m.reclaim(&held);
        // Had the old lines' id been reclaimed, the next new highlight would
        // reuse it.
        batch(
            &mut a,
            &mut m,
            vec![coloured(6, 0x00ff00), coloured(6, 0xffff00)],
            true,
            t0,
        );
        let top = |a: &Animator, m: &Model| {
            let frame = compose::compose(m, a, 30, 12);
            let cell = frame.get(0, 0).expect("on the frame").clone();
            (cell.text, cell.style.fg)
        };
        let colour = |n| Color::Rgb(style::rgb(n));
        assert_eq!(top(&a, &m), (Text::Char('1'), colour(0xff0000)));
        a.advance(t0 + Duration::from_secs(1));
        assert_eq!(top(&a, &m), (Text::Char('3'), colour(0x0000ff)));
    }

    fn viewport(topline: i64, curline: i64, scroll_delta: i64) -> Event {
        Event::WinViewport {
            grid: 2,
            win: Some(1002),
            topline,
            botline: topline + 11,
            curline,
            curcol: 0,
            line_count: 100,
            scroll_delta,
        }
    }

    /// With slides off. The cursor is kept in by `'scrolloff'`; Neovim's
    /// matching scroll lands on it, and one Neovim never makes expires.
    #[test]
    fn a_window_scrolled_ahead_of_neovim_shows_at_once() {
        let fx = Effects {
            scroll: None,
            smear: None,
            ..effects()
        };
        let (mut a, mut m, t0) = (Animator::new(fx), model(), Instant::now());
        setup(&mut a, &mut m, t0);
        batch(&mut a, &mut m, vec![viewport(0, 0, 0)], true, t0);
        let top = |a: &Animator, m: &Model| {
            compose::compose(m, a, 30, 12)
                .get(0, 0)
                .map(|c| c.text.clone())
        };
        let mut fill = |i: i64| {
            let digit = char::from_digit((i % 10) as u32, 10)?;
            let mut row = vec![Cell::default(); 10];
            row[0].text = Text::Char(digit);
            Some(row)
        };
        let until = t0 + Duration::from_secs(1);
        let go = |rows, until| Ahead {
            rows,
            cursor: 0,
            col: None,
            land: None,
            until,
            scrolloff: 2,
            seq: None,
        };
        assert!(a.predict(&m, 2, go(3, until), (&mut fill, &mut |_, _| None), t0));
        assert_eq!(top(&a, &m), Some(Text::Char('3')));
        assert_eq!(a.cursor_at(&m), (2, 0), "'scrolloff' rows from the top");
        assert_eq!(
            a.next_frame(&m, t0),
            Some(until),
            "nothing to draw till then"
        );

        let mut events = vec![Event::GridScroll {
            grid: 2,
            top: 0,
            bot: 10,
            left: 0,
            right: 10,
            rows: 3,
        }];
        events.extend(lines(2, 3, 10));
        events.push(viewport(3, 5, 3));
        events.push(Event::GridCursor {
            grid: 2,
            row: 2,
            col: 0,
        });
        let t1 = t0 + Duration::from_millis(300);
        batch(&mut a, &mut m, events, true, t1);
        assert_eq!(a.ahead(2), (0, 0));
        assert_eq!(top(&a, &m), Some(Text::Char('3')), "landed where it was");
        assert_eq!(a.cursor_at(&m), (2, 0));

        let until = t1 + Duration::from_secs(1);
        assert!(a.predict(&m, 2, go(-3, until), (&mut fill, &mut |_, _| None), t1));
        assert_eq!(top(&a, &m), Some(Text::Char('0')));
        a.advance(until);
        assert_eq!(a.ahead(2), (0, 0), "never confirmed");
        assert_eq!(top(&a, &m), Some(Text::Char('3')), "Neovim's view again");
        assert_eq!(a.next_frame(&m, until), None);
    }

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
        let mut frame = compose::compose(&m, &a, 30, 12);
        a.advance(t0 + Duration::from_millis(30));
        let cursor = a.paint(&mut frame, &m, t0);
        assert!(!cursor.visible);
        a.advance(t0 + Duration::from_secs(1));
        let mut frame = compose::compose(&m, &a, 30, 12);
        let cursor = a.paint(&mut frame, &m, t0);
        assert!(cursor.visible);
        assert_eq!((cursor.row, cursor.col, cursor.shape), (5, 5, 2));
    }

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
        assert_eq!(
            cursor_colors(&m, m.mode_info()),
            (Rgb(0, 255, 0), Some(Rgb(0, 0, 0x11)))
        );
        let frame = Frame::new(4, 4, Default::default());
        assert_eq!(
            hardware_cursor(&m, m.mode_info(), &frame, m.cursor_on_screen()).color,
            Some(Rgb(0, 255, 0))
        );
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
        assert_eq!(cursor_colors(&m, m.mode_info()), (Rgb(255, 255, 255), None));
        assert_eq!(
            hardware_cursor(&m, m.mode_info(), &frame, m.cursor_on_screen()).color,
            None
        );
    }
}
