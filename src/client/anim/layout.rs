//! Split windows opening, closing and resizing as animate.nvim's window
//! module draws them, instead of Neovide's slide.
//!
//! A UI learns only where each grid is, so [`build`] reads Neovim's tree of
//! rows and columns back off the frames (grid plus status line/separators).
//! A batch's change to the windows drawn last is then one of:
//!
//! - resized, same arrangement (`<C-w>>`, `:resize`, `<C-w>=`): edges ease in
//!   and out over 150 ms, a frame per cell for small moves, a one-cell move
//!   at once;
//! - one window split off (`:split`, `:vsplit`, a sidebar): it grows from a
//!   one-cell sliver on its side (right/below with `'splitright'` and
//!   `'splitbelow'`) as its text fades in, over 200 ms, easing out;
//! - windows closed (`:close`, `:only`): the window before each closed run
//!   takes its place at once while a ghost of the run (content and leading
//!   separator) flies to its far side, dimming, over 180 ms, easing in; if the
//!   run came first, the window after it moves in over the ghost;
//! - anything else (rearranged windows, another tab page, two splits at once)
//!   is left to Neovide's slide ([`super::motion`]).
//!
//! The rules are ported from animate.nvim's `window/fly.lua`, whose ghost is
//! blank. The client draws each window in its current frame from what Neovim
//! last drew: no bigger than before, its old content cut to size; bigger,
//! Neovim's content for the destination ([`draw_window`]). Status lines are
//! the destination's, their fill stretched to fit ([`stretch`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::Windows;
use crate::client::compose::{self, Frame, Out};
use crate::client::grid::{Cell, Grid, Text};
use crate::client::model::{Held, Model, Place};
use crate::client::redraw::Event;
use crate::client::style::{Colors, Underline};
use crate::palette::Rgb;

/// animate.nvim's resize runs at 30 fps and moves an edge at least a cell a
/// frame, so a resize of a few cells takes a frame per cell.
const RESIZE_CELL: f32 = 1.0 / 30.0;

/// The same for opening or closing, at 60 fps.
const FLY_CELL: f32 = 1.0 / 60.0;

/// Neovim's layout options, as the client's agent reports them (see
/// `crate::client::App`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub laststatus: i64,
    pub splitright: bool,
    pub splitbelow: bool,
}

impl Default for Options {
    /// Neovim's defaults.
    fn default() -> Self {
        Self {
            laststatus: 2,
            splitright: false,
            splitbelow: false,
        }
    }
}

/// animate.nvim's easings, as its `engine/easing.lua` has them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Easing {
    InOutQuad,
    OutCubic,
    InQuad,
}

impl Easing {
    /// Eased progress at fraction `t` of the duration.
    pub fn at(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Easing::InQuad => t * t,
            Easing::OutCubic => 1.0 - (1.0 - t).powi(3),
            Easing::InOutQuad if t < 0.5 => 2.0 * t * t,
            Easing::InOutQuad => 1.0 - (2.0 - 2.0 * t).powi(2) / 2.0,
        }
    }
}

/// Cells of the screen: rows `top..bottom`, columns `left..right`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Area {
    pub top: i64,
    pub bottom: i64,
    pub left: i64,
    pub right: i64,
}

impl Area {
    fn lo(&self, k: Kind) -> i64 {
        match k {
            Kind::Row => self.left,
            Kind::Col => self.top,
        }
    }

    fn hi(&self, k: Kind) -> i64 {
        match k {
            Kind::Row => self.right,
            Kind::Col => self.bottom,
        }
    }

    fn set_lo(&mut self, k: Kind, v: i64) {
        match k {
            Kind::Row => self.left = v,
            Kind::Col => self.top = v,
        }
    }

    fn set_hi(&mut self, k: Kind, v: i64) {
        match k {
            Kind::Row => self.right = v,
            Kind::Col => self.bottom = v,
        }
    }

    fn extent(&self, k: Kind) -> i64 {
        self.hi(k) - self.lo(k)
    }

    fn width(&self) -> i64 {
        self.right - self.left
    }

    fn height(&self) -> i64 {
        self.bottom - self.top
    }

    fn cells(&self) -> i64 {
        self.width().max(0) * self.height().max(0)
    }

    fn is_empty(&self) -> bool {
        self.width() <= 0 || self.height() <= 0
    }

    fn overlaps(&self, o: &Area) -> bool {
        self.top < o.bottom && o.top < self.bottom && self.left < o.right && o.left < self.right
    }

    fn contains(&self, o: &Area) -> bool {
        o.top >= self.top && o.bottom <= self.bottom && o.left >= self.left && o.right <= self.right
    }

    fn union(&self, o: &Area) -> Area {
        Area {
            top: self.top.min(o.top),
            bottom: self.bottom.max(o.bottom),
            left: self.left.min(o.left),
            right: self.right.max(o.right),
        }
    }

    /// Each edge a fraction `e` of the way to `to`, rounded to a cell. Frames
    /// sharing an edge at both ends share it throughout.
    fn towards(&self, to: &Area, e: f32) -> Area {
        Area {
            top: lerp(self.top, to.top, e),
            bottom: lerp(self.bottom, to.bottom, e),
            left: lerp(self.left, to.left, e),
            right: lerp(self.right, to.right, e),
        }
    }
}

fn lerp(a: i64, b: i64, e: f32) -> i64 {
    (a as f32 + (b - a) as f32 * e).round() as i64
}

pub(super) fn percent(e: f32) -> u8 {
    (e * 100.0).round().clamp(0.0, 100.0) as u8
}

/// How a split holds its children: side by side or stacked, `winlayout()`'s
/// `row` and `col`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Row,
    Col,
}

impl Kind {
    fn across(self) -> Kind {
        match self {
            Kind::Row => Kind::Col,
            Kind::Col => Kind::Row,
        }
    }
}

/// A split window's place in the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pane {
    /// Its grid, the status line or separator below, and the separator to its
    /// right.
    pub frame: Area,
    /// The frame has a row below the grid: the status line, or with
    /// `'laststatus'` 3 a separator.
    pub status: bool,
    /// The frame has a separator column right of the grid.
    pub vsep: bool,
    /// Winbar rows above the text.
    pub bar: i64,
}

impl Pane {
    /// The grid's part of `frame`, a frame the window is drawn in.
    fn grid(&self, frame: Area) -> Area {
        Area {
            bottom: frame.bottom - i64::from(self.status),
            right: frame.right - i64::from(self.vsep),
            ..frame
        }
    }
}

/// Where a layout's frames end with a line of their own: animate.nvim's
/// `trailing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ends {
    width: i64,
    bottom: i64,
    /// Bottom windows have a status line too: `'laststatus'` 2, or 1 with two
    /// or more windows.
    last_status: bool,
}

impl Ends {
    /// Whether a frame of `kind` reaching `pos` ends with a line there: a
    /// separator short of the right edge, or a status line or separator above
    /// another window, or above the command line if all windows have one.
    fn trailing(&self, kind: Kind, pos: i64) -> bool {
        match kind {
            Kind::Row => pos < self.width,
            Kind::Col => self.last_status || pos < self.bottom,
        }
    }
}

/// The split windows of the current tab page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub panes: BTreeMap<u64, Pane>,
    /// What the frames tile.
    pub area: Area,
    ends: Ends,
    laststatus: i64,
}

impl Layout {
    /// The model's split windows, or `None` if they do not tile the screen
    /// (e.g. mid-placement).
    pub fn of(model: &Model, laststatus: i64) -> Option<Layout> {
        let width = model.grids.get(&1)?.width() as i64;
        let wins: Vec<(u64, i64, i64, i64, i64)> = model
            .layout
            .iter()
            .filter(|(grid, p)| **grid != 1 && !p.hidden)
            .filter_map(|(grid, p)| match p.place {
                // The window's cells, which its grid fills unless narrowed to
                // centre its text (see `wide`).
                Place::Window { .. } => {
                    let (row, col, w, h) = model.window_cells(*grid)?;
                    Some((*grid, row as i64, col as i64, w as i64, h as i64))
                }
                _ => None,
            })
            .collect();
        let last_status = laststatus == 2 || (laststatus == 1 && wins.len() >= 2);
        let bottom = wins.iter().map(|w| w.1 + w.4).max()? + i64::from(last_status);
        let top = wins.iter().map(|w| w.1).min()?;
        let mut panes = BTreeMap::new();
        for &(grid, row, col, w, h) in &wins {
            let status = row + h < bottom;
            let vsep = col + w < width;
            let frame = Area {
                top: row,
                bottom: row + h + i64::from(status),
                left: col,
                right: col + w + i64::from(vsep),
            };
            let bar = model.margins(grid).top as i64;
            panes.insert(
                grid,
                Pane {
                    frame,
                    status,
                    vsep,
                    bar,
                },
            );
        }
        let area = Area {
            top,
            bottom,
            left: 0,
            right: width,
        };
        let frames: Vec<Area> = panes.values().map(|p| p.frame).collect();
        tiles(&frames, &area).then_some(Layout {
            panes,
            area,
            ends: Ends {
                width,
                bottom,
                last_status,
            },
            laststatus,
        })
    }

    fn frames(&self) -> BTreeMap<u64, Area> {
        self.panes.iter().map(|(g, p)| (*g, p.frame)).collect()
    }
}

/// Whether `frames` tile `area` exactly, without overlap.
fn tiles(frames: &[Area], area: &Area) -> bool {
    frames.iter().all(|f| !f.is_empty() && area.contains(f))
        && frames.iter().map(Area::cells).sum::<i64>() == area.cells()
        && (0..frames.len()).all(|i| (i + 1..frames.len()).all(|j| !frames[i].overlaps(&frames[j])))
}

/// The union of the frames of `wins` that `frames` has.
fn rect(wins: &[u64], frames: &BTreeMap<u64, Area>) -> Option<Area> {
    wins.iter()
        .filter_map(|w| frames.get(w))
        .copied()
        .reduce(|a, b| a.union(&b))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Leaf(u64),
    Split(Kind, Vec<Node>),
}

impl Node {
    fn leaves(&self) -> Vec<u64> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<u64>) {
        match self {
            Node::Leaf(g) => out.push(*g),
            Node::Split(_, kids) => kids.iter().for_each(|k| k.collect(out)),
        }
    }
}

/// The tree of `items`, read off their frames: split at every cut running
/// right across them (down for a row, along for a column), then recurse.
/// Neovim never nests a row directly in a row, or a column in a column, so
/// this gives its tree, except that four windows in a square could be a row
/// of columns or a column of rows; `prefer` picks.
fn build(items: &[(u64, Area)], prefer: Kind) -> Option<Node> {
    if let [(grid, _)] = items {
        return Some(Node::Leaf(*grid));
    }
    let cuts = |k: Kind| {
        let start = items.iter().map(|(_, a)| a.lo(k)).min().unwrap_or(0);
        let mut at: Vec<i64> = items
            .iter()
            .map(|(_, a)| a.lo(k))
            .filter(|x| *x > start)
            .collect();
        at.sort_unstable();
        at.dedup();
        at.retain(|x| items.iter().all(|(_, a)| a.hi(k) <= *x || a.lo(k) >= *x));
        at
    };
    let (row, col) = (cuts(Kind::Row), cuts(Kind::Col));
    let (kind, at) = match (row.is_empty(), col.is_empty()) {
        (true, true) => return None,
        (false, true) => (Kind::Row, row),
        (true, false) => (Kind::Col, col),
        (false, false) if prefer == Kind::Row => (Kind::Row, row),
        (false, false) => (Kind::Col, col),
    };
    let mut groups = vec![Vec::new(); at.len() + 1];
    for &(grid, a) in items {
        groups[at.partition_point(|x| *x <= a.lo(kind))].push((grid, a));
    }
    let kids = groups
        .iter()
        .map(|g| build(g, prefer))
        .collect::<Option<Vec<_>>>()?;
    Some(Node::Split(kind, kids))
}

/// `node` without the windows in `gone`, as Neovim leaves the layout: a split
/// left with one child becomes it, and a child of the same kind merges in.
fn prune(node: &Node, gone: &BTreeSet<u64>) -> Option<Node> {
    match node {
        Node::Leaf(g) => (!gone.contains(g)).then(|| node.clone()),
        Node::Split(kind, kids) => {
            let mut out = Vec::new();
            for kid in kids {
                match prune(kid, gone) {
                    Some(Node::Split(k, inner)) if k == *kind => out.extend(inner),
                    Some(k) => out.push(k),
                    None => {}
                }
            }
            match out.len() {
                0 => None,
                1 => out.pop(),
                _ => Some(Node::Split(*kind, out)),
            }
        }
    }
}

/// The area `node` covers in `frames`, if they lay it out: each split's
/// children consecutive along it and equally deep across it.
fn fits(node: &Node, frames: &BTreeMap<u64, Area>) -> Option<Area> {
    match node {
        Node::Leaf(g) => frames.get(g).copied(),
        Node::Split(kind, kids) => {
            let areas = kids
                .iter()
                .map(|k| fits(k, frames))
                .collect::<Option<Vec<_>>>()?;
            let (first, last) = (areas.first()?, areas.last()?);
            let across = kind.across();
            let along = areas.windows(2).all(|w| w[0].hi(*kind) == w[1].lo(*kind));
            let level = areas
                .iter()
                .all(|a| a.lo(across) == first.lo(across) && a.hi(across) == first.hi(across));
            (along && level).then(|| first.union(last))
        }
    }
}

/// The row or column window `win` is a child of, and where among its children.
fn parent_of(node: &Node, win: u64) -> Option<(Kind, &[Node], usize)> {
    let Node::Split(kind, kids) = node else {
        return None;
    };
    for (i, kid) in kids.iter().enumerate() {
        if *kid == Node::Leaf(win) {
            return Some((*kind, kids, i));
        }
        if let Some(found) = parent_of(kid, win) {
            return Some(found);
        }
    }
    None
}

/// A tree with numbered nodes, root first, for finding a close's ghosts'
/// rows and columns.
#[derive(Debug)]
struct Tree {
    nodes: Vec<Branch>,
}

#[derive(Debug)]
struct Branch {
    /// None for a window.
    kind: Option<Kind>,
    grid: u64,
    kids: Vec<usize>,
}

/// A run of consecutive closed children of a split.
#[derive(Debug)]
struct Run {
    parent: usize,
    /// Index of the first and last of them among the children.
    i: usize,
    j: usize,
    /// The children either side.
    before: Option<usize>,
    after: Option<usize>,
}

impl Tree {
    fn from(node: &Node) -> Tree {
        let mut t = Tree { nodes: Vec::new() };
        t.add(node);
        t
    }

    fn add(&mut self, node: &Node) -> usize {
        let id = self.nodes.len();
        self.nodes.push(Branch {
            kind: None,
            grid: 0,
            kids: Vec::new(),
        });
        match node {
            Node::Leaf(g) => self.nodes[id].grid = *g,
            Node::Split(kind, kids) => {
                let kids = kids.iter().map(|k| self.add(k)).collect();
                self.nodes[id].kind = Some(*kind);
                self.nodes[id].kids = kids;
            }
        }
        id
    }

    fn leaves(&self, id: usize) -> Vec<u64> {
        let node = &self.nodes[id];
        match node.kind {
            None => vec![node.grid],
            Some(_) => node.kids.iter().flat_map(|k| self.leaves(*k)).collect(),
        }
    }

    fn first_leaf(&self, mut id: usize) -> u64 {
        while let Some(&kid) = self.nodes[id].kids.first() {
            id = kid;
        }
        self.nodes[id].grid
    }

    /// The still-open windows under node `id`.
    fn kept(&self, id: usize, gone: &BTreeSet<u64>) -> Vec<u64> {
        let mut wins = self.leaves(id);
        wins.retain(|w| !gone.contains(w));
        wins
    }

    /// Every run of closed children, inner splits' before their parent's,
    /// since the window taking their place is settled first.
    fn runs(&self, id: usize, gone: &BTreeSet<u64>, out: &mut Vec<Run>) {
        let kids = &self.nodes[id].kids;
        let dead: Vec<bool> = kids
            .iter()
            .map(|k| self.kept(*k, gone).is_empty())
            .collect();
        for (k, kid) in kids.iter().enumerate() {
            if !dead[k] {
                self.runs(*kid, gone, out);
            }
        }
        let mut i = 0;
        while i < kids.len() {
            if !dead[i] {
                i += 1;
                continue;
            }
            let mut j = i;
            while dead.get(j + 1) == Some(&true) {
                j += 1;
            }
            out.push(Run {
                parent: id,
                i,
                j,
                before: i.checked_sub(1).map(|b| kids[b]),
                after: kids.get(j + 1).copied(),
            });
            i = j + 1;
        }
    }

    /// Where each split child but the first starts, old layout to new. A
    /// closed child's start goes to the next open child's start, or past its
    /// split's end, a cell further where that end has no line of its own, so
    /// the line leaves the screen.
    fn bounds(
        &self,
        id: usize,
        frames: (&BTreeMap<u64, Area>, &BTreeMap<u64, Area>),
        ends: &Ends,
        gone: &BTreeSet<u64>,
        out: &mut Vec<Option<(i64, i64)>>,
    ) {
        let (old, new) = frames;
        let node = &self.nodes[id];
        let Some(kind) = node.kind else {
            return;
        };
        let mut finish = rect(&self.kept(id, gone), new).map(|r| {
            let end = r.hi(kind);
            end + i64::from(!ends.trailing(kind, end))
        });
        for k in (0..node.kids.len()).rev() {
            let kid = node.kids[k];
            let to = match rect(&self.kept(kid, gone), new) {
                Some(r) => {
                    finish = Some(r.lo(kind));
                    self.bounds(kid, frames, ends, gone, out);
                    Some(r.lo(kind))
                }
                None => finish,
            };
            if k > 0 {
                let from = old.get(&self.first_leaf(kid)).map(|f| f.lo(kind));
                if let (Some(from), Some(to)) = (from, to) {
                    out[kid] = Some((from, to));
                }
            }
        }
    }
}

/// The split windows and their contents as drawn before a batch that may
/// move them.
#[derive(Debug)]
pub struct Shot {
    layout: Layout,
    /// Each window's grid contents, by grid.
    rows: HashMap<u64, Vec<Vec<Cell>>>,
    /// Text inset into the frame, by grid, for windows drawn narrower and
    /// centred (see `crate::client::wide`).
    insets: HashMap<u64, usize>,
    /// Grid 1, for the separators and status lines of windows about to close.
    chrome: Grid,
    /// The grid the cursor was on.
    cursor: u64,
}

impl Shot {
    /// The windows as the model has them, or mid-animation as the animation
    /// has them, so the next change starts from there.
    pub fn take(model: &Model, laststatus: i64, running: Option<&Transition>) -> Option<Shot> {
        let mut layout = Layout::of(model, laststatus)?;
        if let Some(t) = running {
            let now = t.frames();
            let panes: BTreeMap<u64, Pane> = layout
                .panes
                .iter()
                .map(|(g, p)| {
                    let frame = now.get(g).copied().unwrap_or(p.frame);
                    (*g, Pane { frame, ..*p })
                })
                .collect();
            let frames: Vec<Area> = panes.values().map(|p| p.frame).collect();
            // Ghost areas belong to no window: use the windows' union.
            if let Some(area) = frames.iter().copied().reduce(|a, b| a.union(&b)) {
                if tiles(&frames, &area) {
                    layout.panes = panes;
                    layout.area = area;
                }
            }
        }
        // A moving window showing its old content in mid-frame (see
        // `middle`) continues from that.
        let showed = |g: &u64| running.and_then(|t| t.showed(model, *g));
        let rows = layout
            .panes
            .keys()
            .filter_map(|g| {
                if let Some(rows) = showed(g) {
                    return Some((*g, rows.clone()));
                }
                let grid = model.grids.get(g)?;
                Some((*g, grid.lines(0, grid.height(), 0, grid.width())))
            })
            .collect();
        let insets = layout
            .panes
            .iter()
            .filter_map(|(g, p)| {
                let width = match showed(g) {
                    Some(rows) => rows.first().map_or(0, Vec::len),
                    None if model.inset(*g) > 0 => model.grids.get(g)?.width(),
                    None => return None,
                };
                let text = p.grid(p.frame).width().max(0) as usize;
                Some((*g, crate::client::wide::inset(text, width)))
            })
            .collect();
        Some(Shot {
            layout,
            rows,
            insets,
            chrome: model.grids.get(&1).cloned().unwrap_or_default(),
            cursor: model.cursor.grid,
        })
    }
}

/// Whether a batch may move split windows: it places one, or hides, closes
/// or destroys one in the layout.
pub fn moves_windows(model: &Model, batch: &[Event]) -> bool {
    let split = |grid: &u64| {
        model
            .layout
            .get(grid)
            .is_some_and(|p| !p.hidden && matches!(p.place, Place::Window { .. }))
    };
    batch.iter().any(|e| match e {
        Event::WinPos { .. } => true,
        Event::WinFloatPos { grid, .. }
        | Event::WinHide { grid }
        | Event::WinClose { grid }
        | Event::GridDestroy { grid } => split(grid),
        _ => false,
    })
}

/// What a batch did to the split windows.
#[derive(Debug)]
pub enum Change {
    Same,
    /// Animated as animate.nvim does; see the module docs.
    Animate(Box<Transition>),
    /// Drawn at once: a one-cell change, or a layout the animation cannot
    /// make sense of.
    Snap,
    /// Not animate.nvim's: left to Neovide's slide.
    Slide,
}

/// Classify the change from `old` to `new`; see the module docs.
pub fn change(old: Shot, new: &Layout, model: &Model, w: &Windows, opts: Options) -> Change {
    if old.layout.panes == new.panes {
        return Change::Same;
    }
    if old.layout.ends.width != new.ends.width {
        // The screen itself changed size.
        return Change::Slide;
    }
    let was: BTreeSet<u64> = old.layout.panes.keys().copied().collect();
    let is: BTreeSet<u64> = new.panes.keys().copied().collect();
    let kept: Vec<u64> = was.intersection(&is).copied().collect();
    if kept.is_empty() || !in_order(&old.layout, new, &kept) {
        return Change::Slide;
    }
    let opened: Vec<u64> = is.difference(&was).copied().collect();
    let closed: BTreeSet<u64> = was.difference(&is).copied().collect();
    let transition = match (opened.as_slice(), closed.is_empty()) {
        ([], true) => resize(old, new, w.resize),
        ([win], true) => open(old, new, *win, model, opts, w.open),
        ([], false) => close(old, new, &closed, model, w.close),
        _ => return Change::Slide,
    };
    transition.map_or(Change::Snap, |t| Change::Animate(Box::new(t)))
}

/// Whether every pair of `wins` keeps a relative placement (left of, or
/// above) it had: true for resizes, false for rearrangements.
fn in_order(old: &Layout, new: &Layout, wins: &[u64]) -> bool {
    let sides = |a: &Area, b: &Area| {
        [
            a.right <= b.left,
            b.right <= a.left,
            a.bottom <= b.top,
            b.bottom <= a.top,
        ]
    };
    wins.iter().enumerate().all(|(i, a)| {
        wins[i + 1..].iter().all(|b| {
            let was = sides(&old.panes[a].frame, &old.panes[b].frame);
            let is = sides(&new.panes[a].frame, &new.panes[b].frame);
            was.iter().zip(is).any(|(x, y)| *x && y)
        })
    })
}

/// The largest change in any window frame's width or height.
fn most_cells(start: &BTreeMap<u64, Area>, new: &Layout) -> i64 {
    new.panes
        .iter()
        .filter_map(|(g, p)| {
            let s = start.get(g)?;
            let dw = (s.width() - p.frame.width()).abs();
            let dh = (s.height() - p.frame.height()).abs();
            Some(dw.max(dh))
        })
        .max()
        .unwrap_or(0)
}

/// The same windows in new sizes: every edge moves to its new place.
fn resize(old: Shot, new: &Layout, secs: f32) -> Option<Transition> {
    let start = old.layout.frames();
    let cells = most_cells(&start, new);
    // A one-cell change has no frame in between.
    if secs <= 0.0 || cells <= 1 {
        return None;
    }
    let duration = secs.min(cells as f32 * RESIZE_CELL);
    Some(Transition::new(
        start,
        new,
        old,
        duration,
        Easing::InOutQuad,
    ))
}

/// Which side of the space a split took the new window is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Start,
    End,
}

/// A new window `win` flies in from its side of the space it took.
fn open(
    old: Shot,
    new: &Layout,
    win: u64,
    model: &Model,
    opts: Options,
    secs: f32,
) -> Option<Transition> {
    if secs <= 0.0 {
        return None;
    }
    let old_fr = old.layout.frames();
    let new_fr = new.frames();
    // The new tree, which without `win` must be the old one.
    let items: Vec<(u64, Area)> = new_fr.iter().map(|(g, a)| (*g, *a)).collect();
    let gone = BTreeSet::from([win]);
    let tree = [Kind::Row, Kind::Col].into_iter().find_map(|prefer| {
        let tree = build(&items, prefer)?;
        fits(&prune(&tree, &gone)?, &old_fr)?;
        Some(tree)
    })?;
    let (kind, kids, i) = parent_of(&tree, win)?;
    // The neighbour whose space it took: the only one that changed size along
    // the split; if both did ('equalalways'), the one it was split from when
    // it is current; else the one 'splitright' or 'splitbelow' puts it after.
    let (node, side) = if i == 0 {
        (kids.get(1)?, Side::Start)
    } else if i + 1 == kids.len() {
        (&kids[i - 1], Side::End)
    } else {
        let (prev, next) = (&kids[i - 1], &kids[i + 1]);
        let changed = |n: &Node| {
            let wins = n.leaves();
            match (rect(&wins, &old_fr), rect(&wins, &new_fr)) {
                (Some(a), Some(b)) => a.extent(kind) != b.extent(kind),
                _ => false,
            }
        };
        let after = match kind {
            Kind::Row => opts.splitright,
            Kind::Col => opts.splitbelow,
        };
        match (changed(prev), changed(next)) {
            (true, false) => (prev, Side::End),
            (false, true) => (next, Side::Start),
            _ if model.cursor.grid == win && prev.leaves().contains(&old.cursor) => {
                (prev, Side::End)
            }
            _ if model.cursor.grid == win && next.leaves().contains(&old.cursor) => {
                (next, Side::Start)
            }
            _ if after => (prev, Side::End),
            _ => (next, Side::Start),
        }
    };
    let wins = node.leaves();
    let region = rect(&wins, &old_fr)?;
    // Its frame at its smallest: one row or column of text, with its bars
    // and separators.
    let pane = new.panes.get(&win)?;
    let text = pane.grid(pane.frame);
    let deep = match kind {
        Kind::Row => text.width(),
        Kind::Col => text.height() - pane.bar,
    };
    let ext = 1 + pane.frame.extent(kind) - deep;
    let mut start = old_fr.clone();
    let mut sliver = region;
    match side {
        Side::Start => {
            sliver.set_hi(kind, region.lo(kind) + ext);
            for w in &wins {
                if let Some(f) = start.get_mut(w).filter(|f| f.lo(kind) == region.lo(kind)) {
                    f.set_lo(kind, sliver.hi(kind));
                }
            }
        }
        Side::End => {
            sliver.set_lo(kind, region.hi(kind) - ext);
            for w in &wins {
                if let Some(f) = start.get_mut(w).filter(|f| f.hi(kind) == region.hi(kind)) {
                    f.set_hi(kind, sliver.lo(kind));
                }
            }
        }
    }
    start.insert(win, sliver);
    // Every window keeps a cell of text on the way.
    let room = new.panes.iter().all(|(g, p)| {
        start
            .get(g)
            .is_some_and(|s| p.grid(*s).width() >= 1 && p.grid(*s).height() - p.bar >= 1)
    });
    let cells = most_cells(&start, new);
    if !room || cells <= 1 {
        return None;
    }
    let veil = model
        .grids
        .get(&win)
        .map_or(0, |g| background((0..g.height()).flat_map(|r| g.row(r))));
    let duration = secs.min(cells as f32 * FLY_CELL);
    let mut t = Transition::new(start, new, old, duration, Easing::OutCubic);
    t.veil = Some((win, veil));
    Some(t)
}

/// Windows `gone` closed: each run flies out to its far side.
fn close(
    old: Shot,
    new: &Layout,
    gone: &BTreeSet<u64>,
    model: &Model,
    secs: f32,
) -> Option<Transition> {
    if secs <= 0.0 {
        return None;
    }
    let old_fr = old.layout.frames();
    let new_fr = new.frames();
    // The old tree, which without `gone` must be the new one.
    let items: Vec<(u64, Area)> = old_fr.iter().map(|(g, a)| (*g, *a)).collect();
    let tree = [Kind::Row, Kind::Col].into_iter().find_map(|prefer| {
        let tree = build(&items, prefer)?;
        fits(&prune(&tree, gone)?, &new_fr)?;
        Some(tree)
    })?;
    let tree = Tree::from(&tree);
    let mut runs = Vec::new();
    tree.runs(0, gone, &mut runs);
    // Windows start where they were, the one before each run also covering
    // the run; if the run came first, the one after it moves in.
    let mut start: BTreeMap<u64, Area> = new
        .panes
        .keys()
        .filter_map(|g| Some((*g, *old_fr.get(g)?)))
        .collect();
    let mut cells = 0;
    let mut ghosts = Vec::new();
    for run in &runs {
        let parent = &tree.nodes[run.parent];
        let kind = parent.kind?;
        let first = parent.kids[run.i];
        let r0 = old_fr.get(&tree.first_leaf(first))?.lo(kind);
        let r1 = rect(&tree.leaves(parent.kids[run.j]), &old_fr)?.hi(kind);
        cells = cells.max(r1 - r0);
        let holders = run.before.map(|b| tree.kept(b, gone));
        if let Some(wins) = &holders {
            let edge = rect(wins, &start)?.hi(kind);
            for w in wins {
                if let Some(f) = start.get_mut(w).filter(|f| f.hi(kind) == edge) {
                    f.set_hi(kind, r1);
                }
            }
        }
        // The line the window before the run ends with, or the run's own
        // when it comes first.
        let closed: Vec<u64> = parent.kids[run.i..=run.j]
            .iter()
            .flat_map(|k| tree.leaves(*k))
            .collect();
        let owner = holders
            .as_ref()
            .and_then(|h| h.last().copied())
            .or(closed.first().copied())?;
        let bg = old
            .rows
            .get(closed.first()?)
            .map_or(0, |rows| background(rows.iter().flatten()));
        let region = rect(&closed, &old_fr)?;
        // Where that line meets the status lines of windows side by side:
        // the corner as it was drawn.
        let corner = (kind == Kind::Row && old.layout.ends.trailing(Kind::Col, region.bottom))
            .then(|| {
                let col = match holders {
                    Some(_) => region.left - 1,
                    None => region.right - 1,
                };
                cell_at(&old.chrome, region.bottom - 1, col).cloned()
            })
            .flatten();
        ghosts.push(Ghost {
            kind,
            parent: run.parent,
            first,
            after: run.after,
            holders,
            picture: picture(&old, &closed, region, model),
            bg,
            line: line_of(kind, owner, &old, model),
            corner,
        });
    }
    if ghosts.is_empty() {
        return None;
    }
    let mut bounds = vec![None; tree.nodes.len()];
    tree.bounds(0, (&old_fr, &new_fr), &new.ends, gone, &mut bounds);
    let cells = cells.max(most_cells(&start, new));
    let duration = secs.min(cells as f32 * FLY_CELL);
    let screen = old.layout.area;
    let mut t = Transition::new(start, new, old, duration, Easing::InQuad);
    t.ghosts = Some(Ghosts {
        tree,
        bounds,
        screen,
        list: ghosts,
        ends: new.ends,
    });
    Some(t)
}

/// The line a frame of `kind` ends with, as `owner`'s was drawn: its
/// separator, or for a status line a blank `StatusLineNC` bar, or with
/// `'laststatus'` 3 the separator in its place.
fn line_of(kind: Kind, owner: u64, old: &Shot, model: &Model) -> Cell {
    let pane = old.layout.panes.get(&owner);
    let drawn = |row: i64, col: i64| cell_at(&old.chrome, row, col).cloned();
    let group = |name: &str| model.groups.get(name).copied();
    match kind {
        Kind::Row => pane
            .filter(|p| p.vsep)
            .and_then(|p| drawn(p.frame.top, p.frame.right - 1))
            .unwrap_or(Cell {
                text: Text::Char('│'),
                hl: group("WinSeparator").unwrap_or(0),
            }),
        Kind::Col if old.layout.laststatus == 3 => pane
            .filter(|p| p.status)
            .and_then(|p| drawn(p.frame.bottom - 1, p.frame.left))
            .unwrap_or(Cell {
                text: Text::Char('─'),
                hl: group("WinSeparator").unwrap_or(0),
            }),
        Kind::Col => Cell {
            text: Text::Char(' '),
            hl: group("StatusLineNC")
                .or_else(|| {
                    let p = pane.filter(|p| p.status)?;
                    drawn(p.frame.bottom - 1, p.frame.left).map(|c| c.hl)
                })
                .unwrap_or(0),
        },
    }
}

fn cell_at(grid: &Grid, row: i64, col: i64) -> Option<&Cell> {
    grid.cell(usize::try_from(row).ok()?, usize::try_from(col).ok()?)
}

/// What `wins` showed of `region`: their text, separators and status lines,
/// with centred text and its status line's words as drawn (see
/// `crate::client::wide`).
fn picture(old: &Shot, wins: &[u64], region: Area, model: &Model) -> Vec<Vec<Cell>> {
    let grids: Vec<(Area, usize, &Vec<Vec<Cell>>)> = wins
        .iter()
        .filter_map(|w| {
            let p = old.layout.panes.get(w)?;
            let inset = old.insets.get(w).copied().unwrap_or(0);
            Some((p.grid(p.frame), inset, old.rows.get(w)?))
        })
        .collect();
    let mut picture: Vec<Vec<Cell>> = (region.top..region.bottom)
        .map(|y| {
            (region.left..region.right)
                .map(|x| {
                    let own = grids
                        .iter()
                        .find(|(g, _, _)| {
                            (g.top..g.bottom).contains(&y) && (g.left..g.right).contains(&x)
                        })
                        .map(|(g, inset, rows)| {
                            let k = usize::try_from(x - g.left - *inset as i64).ok();
                            let cell = k.and_then(|k| rows.get((y - g.top) as usize)?.get(k));
                            (cell, *inset > 0)
                        });
                    match own {
                        Some((Some(cell), _)) => cell.clone(),
                        // Either side of centred text is the editor's own
                        // background, as in `draw_window`.
                        Some((None, true)) => Cell::default(),
                        _ => cell_at(&old.chrome, y, x).cloned().unwrap_or_default(),
                    }
                })
                .collect()
        })
        .collect();
    for w in wins {
        let (Some(p), Some(&inset), Some(rows)) =
            (old.layout.panes.get(w), old.insets.get(w), old.rows.get(w))
        else {
            continue;
        };
        let g = p.grid(p.frame);
        let row = usize::try_from(p.frame.bottom - 1 - region.top).ok();
        let cols = usize::try_from(g.left - region.left)
            .ok()
            .zip(usize::try_from(g.right - region.left).ok());
        let cells = row
            .zip(cols)
            .filter(|_| p.status && inset > 0)
            .and_then(|(row, (a, b))| picture.get_mut(row)?.get_mut(a..b));
        if let Some(cells) = cells {
            let text = rows.first().map_or(0, Vec::len);
            crate::client::wide::realign_by(cells, inset, text, |c: &Cell| {
                (c.text == Text::Char(' ')).then(|| model.style(c.hl))
            });
        }
    }
    picture
}

/// A window's background: the highlight most of its blank cells are in.
pub(super) fn background<'a>(cells: impl Iterator<Item = &'a Cell>) -> u32 {
    let mut counts: HashMap<u32, usize> = HashMap::new();
    let mut last = None;
    for cell in cells {
        last = Some(cell.hl);
        if cell.text.is_blank() {
            *counts.entry(cell.hl).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(hl, n)| (*n, std::cmp::Reverse(*hl)))
        .map(|(hl, _)| hl)
        .or(last)
        .unwrap_or(0)
}

/// Split windows on their way from one layout to the next.
#[derive(Debug)]
pub struct Transition {
    /// Each window's starting frame and final layout, by grid.
    wins: BTreeMap<u64, (Area, Pane)>,
    /// Each window's previous contents, shown where a frame outgrows its
    /// grid.
    rows: HashMap<u64, Vec<Vec<Cell>>>,
    /// Previous text insets, by grid, of windows drawn narrower and centred
    /// (see `crate::client::wide`).
    insets: HashMap<u64, usize>,
    /// An opened window, and the background highlight its text fades in
    /// from.
    veil: Option<(u64, u32)>,
    ghosts: Option<Ghosts>,
    elapsed: f32,
    duration: f32,
    easing: Easing,
}

impl Transition {
    fn new(
        start: BTreeMap<u64, Area>,
        new: &Layout,
        old: Shot,
        duration: f32,
        easing: Easing,
    ) -> Transition {
        let wins = new
            .panes
            .iter()
            .map(|(g, p)| (*g, (start.get(g).copied().unwrap_or(p.frame), *p)))
            .collect();
        Transition {
            wins,
            rows: old.rows,
            insets: old.insets,
            veil: None,
            ghosts: None,
            elapsed: 0.0,
            duration,
            easing,
        }
    }

    /// Advance `dt` seconds; returns whether it is still running.
    pub fn step(&mut self, dt: f32) -> bool {
        self.elapsed += dt;
        self.elapsed < self.duration
    }

    fn progress(&self) -> f32 {
        if self.duration <= 0.0 {
            return 1.0;
        }
        self.easing.at(self.elapsed / self.duration)
    }

    /// Whether window `grid` is moving.
    pub fn involves(&self, grid: u64) -> bool {
        self.wins.contains_key(&grid)
    }

    /// What window `grid` centres in its frame (see [`middle`]).
    fn middle_of(&self, model: &Model, grid: u64) -> Option<Middle> {
        let was = self.insets.get(&grid).is_some_and(|&inset| inset > 0);
        middle(model, grid, self.rows.get(&grid), was)
    }

    /// Window `grid`'s old content, if that is what it centres in its frame.
    fn showed(&self, model: &Model, grid: u64) -> Option<&Vec<Vec<Cell>>> {
        self.middle_of(model, grid).filter(|m| m.old)?;
        self.rows.get(&grid)
    }

    /// Keep the highlights of old contents, the veil and the ghosts alive
    /// (see [`crate::client::model::Model::reclaim`]).
    pub fn hold(&self, held: &mut Held) {
        for row in self.rows.values().flatten() {
            held.cells(row);
        }
        if let Some((_, veil)) = self.veil {
            held.mark(veil);
        }
        for ghost in self.ghosts.iter().flat_map(|g| &g.list) {
            for row in &ghost.picture {
                held.cells(row);
            }
            held.mark(ghost.bg);
            held.cells(std::iter::once(&ghost.line).chain(&ghost.corner));
        }
    }

    /// Where each window is this frame.
    fn frames(&self) -> BTreeMap<u64, Area> {
        self.frames_at(self.progress())
    }

    fn frames_at(&self, e: f32) -> BTreeMap<u64, Area> {
        self.wins
            .iter()
            .map(|(g, (start, pane))| (*g, start.towards(&pane.frame, e)))
            .collect()
    }

    /// Draw the windows as they are this frame, over the compositor's output.
    /// Each window's text is offset `shift` columns (see
    /// `super::Animator::insets`) plus what ghosts beside it add (see
    /// [`ghost_shift`]); the cursor is at `cursor` (see [`fade_cell`]).
    pub fn paint(
        &self,
        frame: &mut Frame,
        model: &Model,
        shift: &dyn Fn(u64) -> i64,
        cursor: (i64, i64),
    ) {
        let e = self.progress();
        let colors = model.colors();
        let now = self.frames_at(e);
        let ghosts = self
            .ghosts
            .as_ref()
            .map_or_else(Vec::new, |g| g.frames(e, &now));
        // Blank what the windows and ghosts cover first: rounding may leave a
        // cell untiled.
        let covered = now
            .values()
            .copied()
            .chain(ghosts.iter().map(|(_, at)| at.f))
            .reduce(|a, b| a.union(&b));
        if let Some(area) = covered {
            let blank = Out::blank(model.style(0));
            for r in area.top..area.bottom {
                for c in area.left..area.right {
                    if let Some(cell) = frame.at(r, c) {
                        *cell = blank.clone();
                    }
                }
            }
        }
        let chrome = model.grids.get(&1);
        for (grid, (_, pane)) in &self.wins {
            let at = now[grid];
            let shift = shift(*grid) + ghost_shift(model, *grid, pane.grid(at), &ghosts);
            let middle = self.middle_of(model, *grid);
            let old = self.rows.get(grid);
            draw_window(frame, model, *grid, pane.grid(at), old, middle, shift);
            if let Some(chrome) = chrome {
                draw_lines(frame, model, chrome, pane, at, middle, shift);
            }
        }
        if let Some((grid, bg)) = self.veil {
            if let Some((_, pane)) = self.wins.get(&grid) {
                let to = colors.visual_bg(&model.style(bg));
                fade(
                    frame,
                    &colors,
                    pane.grid(now[&grid]),
                    to,
                    percent(e),
                    cursor,
                );
            }
        }
        for (g, at) in &ghosts {
            g.paint(frame, model, &colors, at, e, cursor);
        }
    }
}

/// Extra offset so window `grid`'s centred text (see `crate::client::wide`)
/// centres in the part of `text` no ghost covers: a window taking over closed
/// neighbours (see [`close`]) moves its text with their ghost, not at once.
fn ghost_shift(model: &Model, grid: u64, text: Area, ghosts: &[(&Ghost, Spot)]) -> i64 {
    if model.inset(grid) == 0 {
        return 0;
    }
    let open = ghosts
        .iter()
        .filter(|(g, at)| g.kind == Kind::Row && at.f.overlaps(&text) && at.f.left > text.left)
        .map(|(_, at)| at.f.left)
        .fold(text.right, i64::min);
    let cols = model.grids.get(&grid).map_or(0, Grid::width) as i64;
    let pad = |width: i64| (width - cols).max(0) / 2;
    pad(open - text.left) - pad(text.width())
}

/// Text drawn centred in a moving window's frame (see
/// `crate::client::wide`).
#[derive(Debug, Clone, Copy)]
struct Middle {
    width: usize,
    /// It is the window's old content rather than its grid.
    old: bool,
}

/// What [`draw_window`] centres in window `grid`'s frame (see
/// `crate::client::wide`): its grid if centred at the destination, else its
/// old content if centred before (`was`). Narrowed below its text width, it
/// thus moves its old content to the frame's edge before cutting it to size.
fn middle(model: &Model, grid: u64, old: Option<&Vec<Vec<Cell>>>, was: bool) -> Option<Middle> {
    if model.inset(grid) > 0 {
        let width = model.grids.get(&grid)?.width();
        return Some(Middle { width, old: false });
    }
    let rows = old.filter(|_| was)?;
    let width = rows.first().map_or(0, Vec::len);
    Some(Middle { width, old: true })
}

/// Window `grid`'s text in `text` (the grid's part of its current frame),
/// from the top left.
///
/// No bigger than before, it shows its old content cut to size: Neovim's new
/// content is laid out for the destination (rewrapped, rescrolled) and would
/// not join up with the old. Bigger, it shows its grid, then its old content
/// beyond. Past both, it is blank in its edge's colours.
fn draw_window(
    frame: &mut Frame,
    model: &Model,
    grid: u64,
    text: Area,
    old: Option<&Vec<Vec<Cell>>>,
    middle: Option<Middle>,
    shift: i64,
) {
    let g = model.grids.get(&grid);
    let (height, width) = (text.height().max(0) as usize, text.width().max(0) as usize);
    let within =
        old.is_some_and(|rows| height <= rows.len() && width <= rows.first().map_or(0, Vec::len));
    // Centred text (see `crate::client::wide`) stays centred in the moving
    // frame with nothing either side: its grid or old content (see `middle`).
    let pad = middle.map_or(0, |m| width.saturating_sub(m.width) / 2);
    // Plus `shift` while it moves across the window.
    let pad = (pad as i64 + shift).max(0) as usize;
    let showed = middle.is_some_and(|m| m.old);
    for r in 0..height {
        for c in 0..width {
            let k = c.checked_sub(pad);
            let own = || {
                let k = k?;
                g.and_then(|g| Some((g.cell(r, k)?, g.is_wide(r, k))))
            };
            // Old content where it lines up: from the frame's edge, or
            // centred as before.
            let was = || {
                let k = k.filter(|_| pad == 0 || showed)?;
                let row = old?.get(r)?;
                Some((
                    row.get(k)?,
                    row.get(k + 1).is_some_and(|n| n.text == Text::Half),
                ))
            };
            let cell = if within || showed {
                was().or_else(own)
            } else {
                own().or_else(was)
            };
            let out = match cell {
                Some((cell, wide)) => Out {
                    text: cell.text.clone(),
                    style: model.style(cell.hl),
                    wide,
                },
                // Either side of centred text is the editor's own
                // background, as once the window arrives.
                None if pad > 0 => Out::blank(model.style(0)),
                None => Out::blank(model.style(edge(g, old, r, c))),
            };
            if let Some(o) = frame.at(text.top + r as i64, text.left + c as i64) {
                *o = out;
            }
        }
    }
}

/// The highlight of the cell nearest `(r, c)` in a window showing nothing
/// there, for the blank that fills it.
fn edge(g: Option<&Grid>, old: Option<&Vec<Vec<Cell>>>, r: usize, c: usize) -> u32 {
    if let Some(g) = g.filter(|g| g.width() > 0 && g.height() > 0) {
        let at = g.cell(r.min(g.height() - 1), c.min(g.width() - 1));
        return at.map_or(0, |cell| cell.hl);
    }
    old.and_then(|rows| rows.get(r.min(rows.len().saturating_sub(1))))
        .and_then(|row| row.get(c.min(row.len().saturating_sub(1))))
        .map_or(0, |cell| cell.hl)
}

/// A window's separator and status line in frame `at`, as Neovim drew them
/// at the destination: the separator as deep as the frame, the status line as
/// wide (see [`stretch`]), its words over centred text per `middle`.
fn draw_lines(
    frame: &mut Frame,
    model: &Model,
    chrome: &Grid,
    pane: &Pane,
    at: Area,
    middle: Option<Middle>,
    shift: i64,
) {
    let f = pane.frame;
    let cell = |row: i64, col: i64| {
        let (row, col) = (usize::try_from(row).ok()?, usize::try_from(col).ok()?);
        chrome.cell(row, col)
    };
    let mut put = |row: i64, col: i64, cell: &Cell, wide: bool| {
        if let Some(o) = frame.at(row, col) {
            *o = Out {
                text: cell.text.clone(),
                style: model.style(cell.hl),
                wide,
            };
        }
    };
    let status = i64::from(pane.status);
    if pane.vsep {
        let line: Vec<&Cell> = (f.top..f.bottom - status)
            .filter_map(|r| cell(r, f.right - 1))
            .collect();
        for k in 0..(at.height() - status).max(0) {
            let i = (k as usize).min(line.len().saturating_sub(1));
            if let Some(c) = line.get(i) {
                put(at.top + k, at.right - 1, c, false);
            }
        }
    }
    if pane.status {
        let row = usize::try_from(f.bottom - 1).map_or(&[][..], |r| chrome.row(r));
        let (left, right) = (f.left.max(0) as usize, f.right.max(0) as usize);
        let drawn = row.get(left..right.min(row.len())).unwrap_or(&[]);
        let line = stretch(drawn, at.width().max(0) as usize, usize::from(pane.vsep));
        for (k, c) in line.iter().enumerate() {
            let wide = line.get(k + 1).is_some_and(|n| n.text == Text::Half);
            put(at.bottom - 1, at.left + k as i64, c, wide);
        }
        // A centred window (see `crate::client::wide`) keeps its status
        // line's words over its text while moving.
        let text = middle.map_or(0, |m| m.width);
        let body = (at.width() - i64::from(pane.vsep)).max(0) as usize;
        if let (true, Ok(row), Ok(left)) = (
            text > 0 && text < body,
            usize::try_from(at.bottom - 1),
            usize::try_from(at.left),
        ) {
            let end = (left + body).min(frame.width);
            let inset = (((body - text) / 2) as i64 + shift).max(0) as usize;
            if row < frame.height && left < end {
                let start = row * frame.width;
                crate::client::wide::realign(
                    &mut frame.cells[start + left..start + end],
                    inset,
                    text,
                );
            }
        }
    }
}

/// A status line Neovim drew as wide as `cells`, redrawn `width` wide as
/// Neovim would: the longest run of one cell (the fill between its left and
/// right parts) grows or shrinks down to a cell; beyond that the start is cut
/// with a `<`, as Neovim truncates a status line with no `%<`. The last
/// `corner` cells, where it meets the separator, stay at the end.
fn stretch(cells: &[Cell], width: usize, corner: usize) -> Vec<Cell> {
    let (body, end) = cells.split_at(cells.len() - corner.min(cells.len()));
    let want = width.saturating_sub(end.len());
    let mut out = body.to_vec();
    if let Some((at, len)) = longest_run(body) {
        if want >= body.len() {
            let fill = std::iter::repeat_n(body[at].clone(), want - body.len());
            out.splice(at..at, fill);
        } else {
            out.drain(at..at + (body.len() - want).min(len - 1));
        }
    }
    if out.len() > want {
        out.drain(..out.len() - want);
        if let Some(first) = out.first_mut() {
            first.text = Text::Char('<');
        }
    }
    let fill = body.last().cloned().unwrap_or_default();
    out.resize(want, fill);
    out.extend_from_slice(end);
    out.truncate(width);
    out
}

/// Start and length of the first longest run of one repeated cell, never of
/// right halves of wide characters.
fn longest_run(cells: &[Cell]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    let mut i = 0;
    while i < cells.len() {
        let mut j = i + 1;
        while j < cells.len() && cells[j] == cells[i] {
            j += 1;
        }
        if cells[i].text != Text::Half && best.is_none_or(|(_, len)| j - i > len) {
            best = Some((i, j - i));
        }
        i = j;
    }
    best
}

/// Fade every cell of `area` towards `to`, keeping `keep` percent of its
/// colours; the cursor is at `cursor` (see [`fade_cell`]).
fn fade(frame: &mut Frame, colors: &Colors, area: Area, to: Rgb, keep: u8, cursor: (i64, i64)) {
    for r in area.top..area.bottom {
        for c in area.left..area.right {
            if let Some(cell) = frame.at(r, c) {
                fade_cell(colors, cell, to, keep, (r, c) == cursor);
            }
        }
    }
}

/// Fade a cell towards `to`, keeping `keep` percent of its colours; at 0 it
/// becomes a blank in `to`.
///
/// A blank already on `to` looks the same throughout and is left alone, so
/// most of a window is not rewritten every frame for an invisible foreground
/// (seen only in under/over/strike lines or a link underline). The cell
/// under the cursor is still faded: a terminal may draw a block cursor in the
/// text colour.
pub(super) fn fade_cell(colors: &Colors, cell: &mut Out, to: Rgb, keep: u8, cursor: bool) {
    if !cursor && unseen(colors, cell, to) {
        return;
    }
    compose::fade_to(colors, &mut cell.style, to, keep);
    if keep == 0 {
        cell.text = Text::Char(' ');
        cell.wide = false;
    }
}

/// Whether fading `cell` towards `to` changes nothing visible: a blank on
/// `to` whose foreground shows nowhere (see [`fade_cell`]).
fn unseen(colors: &Colors, cell: &Out, to: Rgb) -> bool {
    let s = &cell.style;
    cell.text == Text::Char(' ')
        && s.underline == Underline::None
        && !s.strikethrough
        && !s.overline
        && s.link == 0
        && colors.visual_bg(s) == to
}

/// The ghosts of closed windows, and the old layout they fly out of.
#[derive(Debug)]
struct Ghosts {
    tree: Tree,
    /// Each split child's start (all but the first), from and to, by node.
    bounds: Vec<Option<(i64, i64)>>,
    /// The old layout's area.
    screen: Area,
    list: Vec<Ghost>,
    /// The new layout's lines, which a ghost stops short of.
    ends: Ends,
}

/// A run of closed windows, flying out to its far side.
#[derive(Debug)]
struct Ghost {
    kind: Kind,
    /// Nodes of their split and of the first of them.
    parent: usize,
    first: usize,
    /// The child after them, if any.
    after: Option<usize>,
    /// The open windows before them, which take their place; the ghost goes
    /// up to their separator. `None` if the closed windows came first.
    holders: Option<Vec<u64>>,
    /// What they showed (see [`picture`]), drawn from the ghost's near edge,
    /// as a shrinking window is drawn from its top left.
    picture: Vec<Vec<Cell>>,
    /// Background highlight, filling the ghost past the picture.
    bg: u32,
    /// The line along its edge: the separator or status line of the window
    /// before them, or their own.
    line: Cell,
    /// Where that line meets their status lines, for windows side by side.
    corner: Option<Cell>,
}

/// Where a ghost is this frame.
#[derive(Debug)]
struct Spot {
    f: Area,
    /// Positions of its lines along it.
    lines: Vec<i64>,
    /// Its picture's top left.
    origin: (i64, i64),
}

impl Ghosts {
    /// Every node of the old layout at progress `e`, and each child's start,
    /// unclamped to its split so a line can leave it at the end.
    fn at(&self, e: f32) -> (Vec<Area>, Vec<i64>) {
        let n = self.tree.nodes.len();
        let mut rects = vec![self.screen; n];
        let mut starts = vec![0; n];
        self.walk(0, self.screen, e, &mut rects, &mut starts);
        (rects, starts)
    }

    fn walk(&self, id: usize, r: Area, e: f32, rects: &mut [Area], starts: &mut [i64]) {
        rects[id] = r;
        let node = &self.tree.nodes[id];
        let Some(kind) = node.kind else {
            return;
        };
        // A split closed whole lies inside its run's ghost.
        if node.kids.get(1).is_none_or(|k| self.bounds[*k].is_none()) {
            return;
        }
        let mut b = vec![r.lo(kind)];
        for kid in &node.kids[1..] {
            let (from, to) = self.bounds[*kid].unwrap_or((r.lo(kind), r.lo(kind)));
            b.push(lerp(from, to, e));
        }
        b.push(r.hi(kind));
        for (k, kid) in node.kids.iter().enumerate() {
            starts[*kid] = b[k];
            let lo = b[k].clamp(r.lo(kind), r.hi(kind));
            let hi = b[k + 1].clamp(lo, r.hi(kind));
            let mut c = r;
            c.set_lo(kind, lo);
            c.set_hi(kind, hi);
            self.walk(*kid, c, e, rects, starts);
        }
    }

    /// Ghost `g`'s place this frame: from the line of the window before it to
    /// the line after it (the holder's separator, or its own).
    fn frame_of(
        &self,
        g: &Ghost,
        rects: &[Area],
        starts: &[i64],
        now: &BTreeMap<u64, Area>,
    ) -> Option<Spot> {
        let kind = g.kind;
        let p = rects[g.parent];
        // Picture origin: along, where the closed windows start (moving with
        // the line before them); across, where their split starts.
        let origin = match kind {
            Kind::Row => (p.top, starts[g.first]),
            Kind::Col => (starts[g.first], p.left),
        };
        let mut a = starts[g.first];
        let mut b = g.after.map_or(p.hi(kind), |n| starts[n]);
        let mut lines = Vec::new();
        match &g.holders {
            Some(wins) => {
                a -= 1;
                lines.push(a);
                let end = rect(wins, now)?.hi(kind);
                b = end - i64::from(self.ends.trailing(kind, end));
            }
            None if g.after.is_some() => lines.push(b - 1),
            None => b -= i64::from(self.ends.trailing(kind, b)),
        }
        let (a, b) = (a.max(p.lo(kind)), b.min(p.hi(kind)));
        if b <= a {
            return None;
        }
        let mut f = p;
        f.set_lo(kind, a);
        f.set_hi(kind, b);
        // A ghost of stacked windows stops before the separator on its right.
        if kind == Kind::Col && self.ends.trailing(Kind::Row, f.right) {
            f.right -= 1;
        }
        (!f.is_empty()).then_some(Spot { f, lines, origin })
    }

    /// Every ghost still visible at progress `e`, and where it is.
    fn frames(&self, e: f32, now: &BTreeMap<u64, Area>) -> Vec<(&Ghost, Spot)> {
        let (rects, starts) = self.at(e);
        self.list
            .iter()
            .filter_map(|g| Some((g, self.frame_of(g, &rects, &starts, now)?)))
            .collect()
    }
}

impl Ghost {
    /// Draw it at `at` for progress `e`: the closed windows' content dimmed
    /// that far into its background, the edge line at full strength; the
    /// cursor is at `cursor` (see [`fade_cell`]).
    fn paint(
        &self,
        frame: &mut Frame,
        model: &Model,
        colors: &Colors,
        at: &Spot,
        e: f32,
        cursor: (i64, i64),
    ) {
        let keep = percent(1.0 - e);
        let out = |cell: &Cell, wide: bool| Out {
            text: cell.text.clone(),
            style: model.style(cell.hl),
            wide,
        };
        let f = at.f;
        for r in f.top..f.bottom {
            let row = usize::try_from(r - at.origin.0)
                .ok()
                .and_then(|i| self.picture.get(i));
            for c in f.left..f.right {
                let Some(o) = frame.at(r, c) else {
                    continue;
                };
                let along = match self.kind {
                    Kind::Row => c,
                    Kind::Col => r,
                };
                if at.lines.contains(&along) {
                    *o = match &self.corner {
                        Some(corner) if r == f.bottom - 1 => out(corner, false),
                        _ => out(&self.line, false),
                    };
                    continue;
                }
                let i = usize::try_from(c - at.origin.1).ok();
                match row
                    .zip(i)
                    .and_then(|(row, i)| Some((row.get(i)?, row.get(i + 1))))
                {
                    Some((cell, next)) => {
                        *o = out(cell, next.is_some_and(|n| n.text == Text::Half));
                        let own = colors.visual_bg(&o.style);
                        fade_cell(colors, o, own, keep, (r, c) == cursor);
                    }
                    None => *o = Out::blank(model.style(self.bg)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::client::anim::{Animator, Effects};
    use crate::client::compose::View;
    use crate::client::model::Changes;
    use crate::client::redraw::LineCell;
    use crate::client::style::{Color, Style};
    use crate::palette::Palette;

    fn area(top: i64, bottom: i64, left: i64, right: i64) -> Area {
        Area {
            top,
            bottom,
            left,
            right,
        }
    }

    /// One full-height window left, two stacked right: a row whose second
    /// child is a column.
    #[test]
    fn the_tree_is_read_off_the_frames() {
        let items = [
            (2, area(0, 10, 0, 11)),
            (4, area(0, 5, 11, 21)),
            (5, area(5, 10, 11, 21)),
        ];
        let tree = build(&items, Kind::Col).expect("a tree");
        assert_eq!(
            tree,
            Node::Split(
                Kind::Row,
                vec![
                    Node::Leaf(2),
                    Node::Split(Kind::Col, vec![Node::Leaf(4), Node::Leaf(5)]),
                ]
            )
        );
        let frames: BTreeMap<u64, Area> = items.into_iter().collect();
        assert_eq!(fits(&tree, &frames), Some(area(0, 10, 0, 21)));
        // Without the bottom-right window, the one above it is the row's
        // second child.
        let gone = BTreeSet::from([5]);
        assert_eq!(
            prune(&tree, &gone),
            Some(Node::Split(Kind::Row, vec![Node::Leaf(2), Node::Leaf(4)]))
        );
    }

    #[test]
    fn a_square_is_either_tree() {
        let items = [
            (2, area(0, 5, 0, 11)),
            (4, area(0, 5, 11, 21)),
            (5, area(5, 10, 0, 11)),
            (6, area(5, 10, 11, 21)),
        ];
        let frames: BTreeMap<u64, Area> = items.into_iter().collect();
        for prefer in [Kind::Row, Kind::Col] {
            let tree = build(&items, prefer).expect("a tree");
            let Node::Split(kind, _) = &tree else {
                panic!("a split")
            };
            assert_eq!(*kind, prefer);
            assert!(fits(&tree, &frames).is_some());
        }
    }

    #[test]
    fn a_status_line_stretches_at_its_fill() {
        let cells = |s: &str| -> Vec<Cell> {
            s.chars()
                .map(|c| Cell {
                    text: Text::Char(c),
                    hl: 0,
                })
                .collect()
        };
        let text = |v: Vec<Cell>| -> String {
            v.iter()
                .map(|c| match &c.text {
                    Text::Char(c) => *c,
                    _ => '?',
                })
                .collect()
        };
        let line = cells("a.txt===1,1|");
        assert_eq!(text(stretch(&line, 15, 1)), "a.txt======1,1|");
        assert_eq!(text(stretch(&line, 10, 1)), "a.txt=1,1|");
        assert_eq!(text(stretch(&line, 7, 1)), "<t=1,1|");
        assert_eq!(text(stretch(&line, 4, 1)), "<,1|");
        assert_eq!(text(stretch(&line, 1, 1)), "|");
        assert_eq!(text(stretch(&line[..0], 3, 0)), "   ");
    }

    #[test]
    fn a_rearrangement_is_told_from_a_resize() {
        let layout = |a: Area, b: Area| Layout {
            panes: [(2, a), (4, b)]
                .into_iter()
                .map(|(g, frame)| {
                    let pane = Pane {
                        frame,
                        status: true,
                        vsep: frame.right < 21,
                        bar: 0,
                    };
                    (g, pane)
                })
                .collect(),
            area: area(0, 5, 0, 21),
            ends: Ends {
                width: 21,
                bottom: 5,
                last_status: true,
            },
            laststatus: 2,
        };
        let old = layout(area(0, 5, 0, 11), area(0, 5, 11, 21));
        let resized = layout(area(0, 5, 0, 16), area(0, 5, 16, 21));
        let swapped = layout(area(0, 5, 11, 21), area(0, 5, 0, 11));
        assert!(in_order(&old, &resized, &[2, 4]));
        assert!(!in_order(&old, &swapped, &[2, 4]));
    }

    const W: usize = 21;
    const H: usize = 6;

    fn line(grid: u64, row: usize, col: usize, text: &str) -> Event {
        Event::GridLine {
            grid,
            row,
            col,
            cells: text
                .chars()
                .map(|c| LineCell {
                    text: Text::Char(c),
                    hl: Some(0),
                    repeat: 1,
                })
                .collect(),
        }
    }

    /// A window's grid of `size` at `row`, `col`, filled with `fill`.
    fn window(grid: u64, row: usize, col: usize, size: (usize, usize), fill: char) -> Vec<Event> {
        let (width, height) = size;
        let mut events = vec![Event::GridResize {
            grid,
            width,
            height,
        }];
        let text: String = std::iter::repeat_n(fill, width).collect();
        events.extend((0..height).map(|r| line(grid, r, 0, &text)));
        events.push(Event::WinPos {
            grid,
            win: Some(1000 + grid as i64),
            row,
            col,
            width,
            height,
        });
        events
    }

    fn cursor(grid: u64) -> Event {
        Event::GridCursor {
            grid,
            row: 0,
            col: 0,
        }
    }

    /// A client's animations and model: a screen `W` wide with one window,
    /// grid 2, full of `a`, above its status line.
    struct Scene {
        anim: Animator,
        model: Model,
        height: usize,
        /// When the last batch arrived.
        now: Instant,
        drawn: bool,
    }

    impl Scene {
        fn new(options: Options) -> Scene {
            Scene::high(options, H)
        }

        fn high(options: Options, height: usize) -> Scene {
            let mut anim = Animator::new(Effects {
                windows: Some(Windows {
                    slide: 0.15,
                    resize: 0.15,
                    open: 0.2,
                    close: 0.18,
                    switch: 0.2,
                }),
                ..Effects::none()
            });
            anim.set_options(options);
            let model = Model::new(Palette {
                fg: Rgb(255, 255, 255),
                bg: Rgb(0, 0, 0),
                ansi: [Rgb(0, 0, 0); 16],
            });
            let mut scene = Scene {
                anim,
                model,
                height,
                now: Instant::now(),
                drawn: false,
            };
            let mut events = vec![Event::GridResize {
                grid: 1,
                width: W,
                height,
            }];
            events.push(line(1, height - 2, 0, "a.txt==========1,1 Al"));
            events.extend(window(2, 0, 0, (21, height - 2), 'a'));
            events.push(cursor(2));
            scene.batch(events);
            scene
        }

        fn batch(&mut self, events: Vec<Event>) {
            self.switched(events, &[]);
        }

        fn switched(&mut self, events: Vec<Event>, switched: &[u64]) {
            let before = self.anim.before(&self.model, &events, true, switched);
            let mut changes = Changes::default();
            for e in events {
                self.model.apply(e, &mut changes);
            }
            self.model.refresh_styles();
            self.anim.after(
                before,
                &self.model,
                &changes,
                (true, self.drawn),
                None,
                self.now,
            );
            self.drawn = true;
        }

        /// The screen `ms` after the last batch.
        fn at(&mut self, ms: u64) -> String {
            self.anim.advance(self.now + Duration::from_millis(ms));
            compose::compose(&self.model, &self.anim, W, self.height).text()
        }

        /// The screen once everything has landed.
        fn settle(&mut self) -> String {
            self.now += Duration::from_secs(10);
            self.at(0)
        }

        fn rows(&mut self, ms: u64) -> Vec<String> {
            self.at(ms).lines().map(String::from).collect()
        }

        /// The red of cell `row`, `col`'s text colour `ms` after the last
        /// batch, as its brightness.
        fn red(&mut self, ms: u64, row: usize, col: usize) -> u8 {
            self.anim.advance(self.now + Duration::from_millis(ms));
            let f = compose::compose(&self.model, &self.anim, W, self.height);
            let cell = f.get(row, col).expect("a cell");
            self.model.colors().visual_fg(&cell.style).0
        }

        fn moving(&self) -> bool {
            self.anim.transition.is_some()
        }
    }

    /// `:vsplit` as Neovim draws it with 'splitright': the window halved, a
    /// separator, and grid 4 on the right.
    fn vsplit_right() -> Vec<Event> {
        let mut events = window(2, 0, 0, (10, 4), 'a');
        events.extend(window(4, 0, 11, (10, 4), 'b'));
        events.extend((0..4).map(|r| line(1, r, 10, "│")));
        events.push(line(1, 4, 0, "a.txt==1,1 b.txt==All"));
        events.push(cursor(4));
        events
    }

    /// It starts as one column of text at the right edge, its separator
    /// moving left into place.
    #[test]
    fn a_split_flies_in_from_its_side() {
        let mut s = Scene::new(Options {
            splitright: true,
            ..Options::default()
        });
        s.batch(vsplit_right());
        assert!(s.moving());
        let first = s.at(0);
        let rows: Vec<&str> = first.lines().collect();
        assert_eq!(
            rows[0], "aaaaaaaaaaaaaaaaaaa│ ",
            "the new window's text not in yet"
        );
        assert_eq!(rows[4], "a.txt===========1,1 <");
        // Midway, the separator is between its start and its place.
        let mid = s.at(40);
        let sep = mid.lines().next().unwrap().chars().position(|c| c == '│');
        assert!(sep.is_some_and(|c| (11..19).contains(&c)), "{mid}");
        let last = s.at(1000);
        assert!(!s.moving());
        let rows: Vec<&str> = last.lines().collect();
        assert_eq!(rows[0], "aaaaaaaaaa│bbbbbbbbbb");
        assert_eq!(rows[4], "a.txt==1,1 b.txt==All");
    }

    #[test]
    fn without_splitright_a_split_comes_from_the_left() {
        let mut s = Scene::new(Options::default());
        let mut events = window(4, 0, 0, (10, 4), 'b');
        events.extend(window(2, 0, 11, (10, 4), 'a'));
        events.extend((0..4).map(|r| line(1, r, 10, "│")));
        events.push(line(1, 4, 0, "b.txt==1,1 a.txt==All"));
        events.push(cursor(4));
        s.batch(events);
        let first = s.at(0);
        assert_eq!(first.lines().next().unwrap(), " │aaaaaaaaaaaaaaaaaaa");
        let last = s.at(1000);
        assert_eq!(last.lines().next().unwrap(), "bbbbbbbbbb│aaaaaaaaaa");
    }

    /// `:close` of the window right of the separator.
    fn close_right() -> Vec<Event> {
        let mut events = vec![Event::WinClose { grid: 4 }, Event::GridDestroy { grid: 4 }];
        events.extend(window(2, 0, 0, (21, 4), 'a'));
        events.push(line(1, 4, 0, "a.txt==========1,1 Al"));
        events.push(cursor(2));
        events
    }

    /// The closed window flies off to its far side behind the preceding
    /// separator, its text dimming; the window before it fills its place from
    /// the start.
    #[test]
    fn a_closed_window_flies_out_into_its_side() {
        let mut s = Scene::new(Options {
            splitright: true,
            ..Options::default()
        });
        s.batch(vsplit_right());
        s.settle();
        s.batch(close_right());
        assert!(s.moving());
        let rows = s.rows(0);
        assert_eq!(rows[0], "aaaaaaaaaa│bbbbbbbbbb", "as it was");
        assert_eq!(rows[4], "a.txt===== b.txt==All", "its status line too");
        assert_eq!(s.red(0, 0, 15), 255, "at full strength");
        let mid = s.rows(100);
        let sep = mid[0].chars().position(|c| c == '│').expect("a separator");
        assert!(sep > 10, "{mid:?}");
        assert!(mid[0].starts_with("aaaaaaaaaaaa"), "{mid:?}");
        assert!(
            mid[0].ends_with("│bbbbbb"),
            "its text goes with it: {mid:?}"
        );
        let dim = s.red(100, 0, W - 1);
        assert!(dim > 0 && dim < 255, "dimming: {dim}");
        assert!(s.red(150, 0, W - 1) < dim, "and dimmer still");
        let last = s.settle();
        assert!(!s.moving());
        assert_eq!(last.lines().next(), Some("aaaaaaaaaaaaaaaaaaaaa"));
        assert_eq!(last.lines().nth(4), Some("a.txt==========1,1 Al"));
    }

    /// A window `width` wide at the top at `col`, with a `text`-wide grid of
    /// `fill` drawn centred (see `crate::client::wide`).
    fn centred(grid: u64, col: usize, (width, text): (usize, usize), fill: char) -> Vec<Event> {
        let mut events = window(grid, 0, col, (text, 4), fill);
        if let Some(Event::WinPos { width: w, .. }) = events.last_mut() {
            *w = width;
        }
        events
    }

    /// Grids 2 and 4 side by side, 10 columns each with 4-wide centred text
    /// and status line words over it: the settled scene and its screen.
    fn centred_split() -> (Scene, String) {
        let mut s = Scene::new(Options {
            splitright: true,
            ..Options::default()
        });
        s.model.set_laststatus(Some(2));
        let mut events = centred(2, 0, (10, 4), 'a');
        events.extend(centred(4, 11, (10, 4), 'b'));
        events.extend((0..4).map(|r| line(1, r, 10, "│")));
        events.push(line(1, 4, 0, "x        y x        y"));
        events.push(cursor(4));
        s.batch(events);
        let screen = s.settle();
        let rows: Vec<&str> = screen.lines().collect();
        assert_eq!(rows[0], "   aaaa   │   bbbb   ");
        assert_eq!(rows[4], "   x  y       x  y   ");
        (s, screen)
    }

    /// `:close` of [`centred_split`]'s window `gone`: `kept` takes the whole
    /// width, its 4-wide text of `fill` still centred.
    fn close_centred(gone: u64, kept: u64, fill: char) -> Vec<Event> {
        let mut events = vec![
            Event::WinClose { grid: gone },
            Event::GridDestroy { grid: gone },
        ];
        events.extend(centred(kept, 0, (21, 4), fill));
        events.push(line(1, 4, 0, "x                   y"));
        events.push(cursor(kept));
        events
    }

    fn col_of(row: &str, c: char) -> Option<usize> {
        row.chars().position(|k| k == c)
    }

    fn cols_of(row: &str, c: char) -> Vec<usize> {
        row.chars()
            .enumerate()
            .filter(|(_, k)| *k == c)
            .map(|(i, _)| i)
            .collect()
    }

    /// The left window takes the space at once, but its text and status words
    /// move gradually, centred in what the ghost leaves uncovered.
    #[test]
    fn centred_text_left_of_a_window_closed_goes_to_its_middle() {
        let (mut s, _) = centred_split();
        s.batch(close_centred(4, 2, 'a'));
        assert!(s.moving());
        // Where the text and the status line's words start each frame.
        let mut seen = Vec::new();
        for ms in (0..=180).step_by(10) {
            let rows = s.rows(ms);
            seen.push((col_of(&rows[0], 'a'), col_of(&rows[4], 'x')));
        }
        assert_eq!(seen[0], (Some(3), Some(3)), "where it was: {seen:?}");
        assert!(
            seen.iter().all(|(text, words)| text == words),
            "the words over the text: {seen:?}"
        );
        assert!(
            seen.windows(2).all(|w| w[0].0 <= w[1].0),
            "only ever right: {seen:?}"
        );
        assert!(
            seen.iter().any(|(c, _)| c.is_some_and(|c| c > 3 && c < 8)),
            "on its way: {seen:?}"
        );
        let rows: Vec<String> = s.settle().lines().map(String::from).collect();
        assert!(!s.moving());
        assert_eq!(rows[0], "        aaaa         ", "in the middle of it all");
        assert_eq!(rows[4], "        x  y         ");
    }

    /// A closed centred window's ghost shows it as drawn (centred text,
    /// status words over it), so the first frame matches the screen before,
    /// whichever window closes; text and words move with the ghost.
    #[test]
    fn a_closed_window_with_centred_text_goes_as_it_was_drawn() {
        let (mut s, before) = centred_split();
        s.batch(close_centred(4, 2, 'a'));
        assert!(s.moving());
        assert_eq!(s.at(0), before, "the right one closed");
        let mid = s.rows(100);
        let line = col_of(&mid[0], '│').expect("its line");
        assert!(line > 10, "flying off: {mid:?}");
        assert_eq!(col_of(&mid[0], 'b'), Some(line + 4), "{mid:?}");
        assert_eq!(cols_of(&mid[4], 'x').last(), Some(&(line + 4)), "{mid:?}");

        let (mut s, before) = centred_split();
        s.batch(close_centred(2, 4, 'b'));
        assert!(s.moving());
        assert_eq!(s.at(0), before, "the left one closed");
        let mid = s.rows(100);
        assert_eq!(col_of(&mid[0], 'a'), Some(3), "where it was: {mid:?}");
    }

    /// The window with 13-wide centred text, split with 'splitright' into
    /// two 10 wide: grid 2 on the left, narrower than its text, which Neovim
    /// keeps 13 wide until asked again (see `crate::client::wide`); grid 4 on
    /// the right, full of `b`. Mid-animation.
    fn split_narrower_than_its_text() -> Scene {
        let mut s = Scene::new(Options {
            splitright: true,
            ..Options::default()
        });
        s.model.set_laststatus(Some(2));
        let mut events = centred(2, 0, (21, 13), 'a');
        events.push(line(1, 4, 0, "x                   y"));
        s.batch(events);
        let rows: Vec<String> = s.settle().lines().map(String::from).collect();
        assert_eq!(rows[0], "    aaaaaaaaaaaaa    ");
        assert_eq!(rows[4], "    x           y    ");
        let mut events = centred(2, 0, (10, 13), 'a');
        events.extend(window(4, 0, 11, (10, 4), 'b'));
        events.extend((0..4).map(|r| line(1, r, 10, "│")));
        events.push(line(1, 4, 0, "x        y x        y"));
        events.push(cursor(4));
        s.batch(events);
        assert!(s.moving());
        s
    }

    /// The text and its status line's words reach the edge gradually.
    #[test]
    fn centred_text_of_a_window_split_narrower_than_it_goes_to_its_edge() {
        let mut s = split_narrower_than_its_text();
        let mut seen = Vec::new();
        for ms in (0..=150).step_by(10) {
            let rows = s.rows(ms);
            seen.push((col_of(&rows[0], 'a'), col_of(&rows[4], 'x')));
        }
        assert_eq!(seen[0], (Some(3), Some(3)), "where it was: {seen:?}");
        assert!(
            seen.iter().all(|(text, words)| text == words),
            "the words over the text: {seen:?}"
        );
        assert!(
            seen.windows(2).all(|w| w[0].0 >= w[1].0),
            "only ever left: {seen:?}"
        );
        assert!(
            seen.iter().any(|(c, _)| c.is_some_and(|c| c > 0 && c < 3)),
            "on its way: {seen:?}"
        );
        let rows: Vec<String> = s.settle().lines().map(String::from).collect();
        assert!(!s.moving());
        assert_eq!(col_of(&rows[0], 'a'), Some(0), "at its edge: {rows:?}");
        assert_eq!(rows[4], "x        y x        y");
    }

    /// The same, with the window redrawn and re-placed mid-animation, as a
    /// refresh does: the old text carries on to the edge, not jumping there
    /// nor showing what was drawn since.
    #[test]
    fn centred_text_of_a_window_split_narrower_than_it_goes_on_when_placed_again() {
        let mut s = split_narrower_than_its_text();
        s.at(10);
        s.now += Duration::from_millis(10);
        s.batch((0..4).map(|r| line(2, r, 0, &"c".repeat(13))).collect());
        let before = s.rows(10);
        let from = col_of(&before[0], 'a').expect("what it showed");
        assert!(from > 0, "{before:?}");
        s.now += Duration::from_millis(10);
        let place = |grid: u64, col: usize| Event::WinPos {
            grid,
            win: Some(1000 + grid as i64),
            row: 0,
            col,
            width: 10,
            height: 4,
        };
        s.batch(vec![place(2, 0), place(4, 11)]);
        assert!(s.moving(), "still on its way");
        let mut seen = Vec::new();
        for ms in (0..=140).step_by(10) {
            let rows = s.rows(ms);
            assert!(!rows[0].contains('c'), "at {ms} ms: {rows:?}");
            seen.push((col_of(&rows[0], 'a'), col_of(&rows[4], 'x')));
        }
        assert_eq!(seen[0], (Some(from), Some(from)), "where it was: {seen:?}");
        assert!(
            seen.iter().all(|(text, words)| text == words),
            "the words over the text: {seen:?}"
        );
        assert!(
            seen.windows(2).all(|w| w[0].0 >= w[1].0),
            "only ever left: {seen:?}"
        );
        assert!(seen.contains(&(Some(0), Some(0))), "{seen:?}");
        let rows: Vec<String> = s.settle().lines().map(String::from).collect();
        assert!(!s.moving());
        assert_eq!(col_of(&rows[0], 'c'), Some(0), "landed: {rows:?}");
        assert_eq!(rows[4], "x        y x        y");
    }

    /// The same, with Neovim re-laying out the grid before it lands: it shows
    /// its old content cut to size, not the new grid pieced with the old.
    #[test]
    fn a_window_split_narrower_than_its_text_shows_what_it_showed() {
        let mut s = split_narrower_than_its_text();
        s.at(10);
        s.now += Duration::from_millis(10);
        let mut events = vec![Event::GridResize {
            grid: 2,
            width: 10,
            height: 4,
        }];
        events.extend((0..4).map(|r| line(2, r, 0, &"c".repeat(10))));
        s.batch(events);
        assert!(s.moving(), "still on its way");
        for ms in (0..=130).step_by(10) {
            let rows = s.rows(ms);
            let left: String = rows[0].chars().take_while(|c| *c != '│').collect();
            let text = left.trim_matches(' ');
            assert!(
                !text.is_empty() && text.chars().all(|c| c == 'a'),
                "at {ms} ms: {rows:?}"
            );
        }
        let rows: Vec<String> = s.settle().lines().map(String::from).collect();
        assert!(!s.moving());
        assert_eq!(rows[0], "cccccccccc│bbbbbbbbbb");
    }

    /// It moves over the closed window's ghost, which stays put, dimming.
    #[test]
    fn the_window_after_a_first_one_closed_moves_into_its_place() {
        let mut s = Scene::new(Options {
            splitright: true,
            ..Options::default()
        });
        s.batch(vsplit_right());
        s.settle();
        let mut events = vec![Event::WinClose { grid: 2 }, Event::GridDestroy { grid: 2 }];
        events.extend(window(4, 0, 0, (21, 4), 'b'));
        events.push(line(1, 4, 0, "b.txt==========1,1 Al"));
        events.push(cursor(4));
        s.batch(events);
        assert!(s.moving());
        let rows = s.rows(0);
        assert_eq!(rows[0], "aaaaaaaaaa│bbbbbbbbbb", "as it was");
        let mid = s.rows(100);
        let sep = mid[0].chars().position(|c| c == '│').expect("a separator");
        assert!(sep < 10, "{mid:?}");
        assert!(mid[0].starts_with(&"a".repeat(sep)), "{mid:?}");
        assert!(mid[0].ends_with("bbbbbbbbbbbb"), "{mid:?}");
        let dim = s.red(100, 0, 0);
        assert!(dim > 0 && dim < 255, "dimming: {dim}");
        let last = s.settle();
        assert_eq!(last.lines().next(), Some("bbbbbbbbbbbbbbbbbbbbb"));
    }

    /// The separator moves a cell at a time, eased.
    #[test]
    fn a_resize_moves_the_separator_there() {
        let mut s = Scene::new(Options {
            splitright: true,
            ..Options::default()
        });
        s.batch(vsplit_right());
        s.settle();
        let mut events = window(2, 0, 0, (15, 4), 'a');
        events.extend(window(4, 0, 16, (5, 4), 'b'));
        events.extend((0..4).map(|r| line(1, r, 15, "│")));
        events.push(line(1, 4, 0, "a.txt======1,1 b.txt"));
        s.batch(events);
        assert!(s.moving());
        let sep = |row: &str| row.chars().position(|c| c == '│');
        let mut seen = Vec::new();
        for ms in [0, 30, 60, 90, 120, 1000] {
            seen.push(sep(&s.rows(ms)[0]));
        }
        assert_eq!(seen.first(), Some(&Some(10)));
        assert_eq!(seen.last(), Some(&Some(15)));
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
        assert!(
            seen.iter().any(|c| c.is_some_and(|c| c > 10 && c < 15)),
            "{seen:?}"
        );
    }

    #[test]
    fn a_resize_of_one_cell_is_at_once() {
        let mut s = Scene::new(Options {
            splitright: true,
            ..Options::default()
        });
        s.batch(vsplit_right());
        s.settle();
        let mut events = window(2, 0, 0, (11, 4), 'a');
        events.extend(window(4, 0, 12, (9, 4), 'b'));
        s.batch(events);
        assert!(!s.moving());
    }

    #[test]
    fn windows_rearranged_slide_as_neovides_do() {
        let mut s = Scene::new(Options {
            splitright: true,
            ..Options::default()
        });
        s.batch(vsplit_right());
        s.settle();
        let mut events = window(2, 0, 11, (10, 4), 'a');
        events.extend(window(4, 0, 0, (10, 4), 'b'));
        s.batch(events);
        assert!(!s.moving());
        assert!(!s.anim.motions.is_empty(), "sliding");
        assert_eq!(s.anim.origin(2, (0, 11)), (0, 0), "from where it was");
    }

    /// With 'splitbelow' the new window comes up from the bottom, its status
    /// line drawn from the first frame.
    #[test]
    fn a_split_below_comes_up_from_the_bottom() {
        let mut s = Scene::high(
            Options {
                splitbelow: true,
                ..Options::default()
            },
            12,
        );
        let mut events = window(2, 0, 0, (21, 5), 'a');
        events.extend(window(4, 6, 0, (21, 4), 'b'));
        events.push(line(1, 5, 0, "a.txt==========1,1 Al"));
        events.push(line(1, 10, 0, "b.txt==========0,0 Al"));
        events.push(cursor(4));
        s.batch(events);
        assert!(s.moving());
        let rows = s.rows(0);
        assert_eq!(
            rows[7],
            "a".repeat(21),
            "the window above, as deep as it was"
        );
        assert_eq!(rows[8], "a.txt==========1,1 Al", "its status line");
        assert_eq!(
            rows[9],
            " ".repeat(21),
            "the new window's row of text, veiled"
        );
        assert_eq!(rows[10], "b.txt==========0,0 Al", "and its status line");
        let rows: Vec<String> = s.settle().lines().map(String::from).collect();
        assert_eq!(rows[4], "a".repeat(21));
        assert_eq!(rows[5], "a.txt==========1,1 Al");
        assert_eq!(rows[6], "b".repeat(21));
        assert_eq!(rows[10], "b.txt==========0,0 Al");
    }

    /// The terminal cursor is hidden until the windows land.
    #[test]
    fn the_cursor_waits_for_its_window_to_land() {
        let mut s = Scene::new(Options {
            splitright: true,
            ..Options::default()
        });
        s.batch(vsplit_right());
        let mut frame = compose::compose(&s.model, &s.anim, W, H);
        assert!(!s.anim.paint(&mut frame, &s.model, s.now).visible);
        s.settle();
        let mut frame = compose::compose(&s.model, &s.anim, W, H);
        let cursor = s.anim.paint(&mut frame, &s.model, s.now);
        assert!(cursor.visible);
        assert_eq!((cursor.row, cursor.col), (0, 11));
    }

    /// A buffer switch fades through the background: old text dims, then new
    /// text rises.
    #[test]
    fn a_buffer_switch_fades_through_the_background() {
        let mut s = Scene::new(Options::default());
        s.model.apply(
            Event::DefaultColors(crate::client::style::DefaultColors {
                rgb_fg: Some(0xffffff),
                rgb_bg: Some(0x000000),
                ..Default::default()
            }),
            &mut Changes::default(),
        );
        s.model.apply(
            Event::OptionSet {
                name: "termguicolors".into(),
                value: crate::client::redraw::OptionValue::Bool(true),
            },
            &mut Changes::default(),
        );
        let events = (0..4).map(|r| line(2, r, 0, &"c".repeat(21))).collect();
        s.switched(events, &[2]);
        assert_eq!(s.anim.switches.len(), 1);
        let fg = |s: &mut Scene, ms: u64| {
            s.anim.advance(s.now + Duration::from_millis(ms));
            let f = compose::compose(&s.model, &s.anim, W, H);
            let cell = f.get(0, 0).expect("a cell").clone();
            (cell.text, s.model.colors().visual_fg(&cell.style))
        };
        let (text, start) = fg(&mut s, 0);
        assert_eq!(text, Text::Char('a'), "the old text, as it was");
        assert_eq!(start, Rgb(255, 255, 255));
        let (text, dim) = fg(&mut s, 70);
        assert_eq!(text, Text::Char('a'));
        assert!(dim.0 < 255, "dimming: {dim:?}");
        let (text, rising) = fg(&mut s, 130);
        assert_eq!(text, Text::Char('c'), "the new text");
        assert!(rising.0 < 255, "coming up: {rising:?}");
        let (text, end) = fg(&mut s, 1000);
        assert_eq!((text, end), (Text::Char('c'), Rgb(255, 255, 255)));
        assert!(s.anim.switches.is_empty());
        // Too many windows switching at once: none fade.
        let events = (0..4).map(|r| line(2, r, 0, &"d".repeat(21))).collect();
        s.switched(events, &[2, 2, 2, 2]);
        assert!(s.anim.switches.is_empty());
    }

    /// Every cell a fade skips looks as if it had been faded, and every other
    /// cell (including the cursor's) is faded, over every mix of text,
    /// colours, reverse, lines and links, at every strength.
    #[test]
    fn a_fade_leaves_alone_only_blanks_it_would_change_nothing_seen_in() {
        let colors = Colors {
            rgb: true,
            defaults: crate::client::style::DefaultColors {
                rgb_fg: Some(0xffffff),
                rgb_bg: Some(0x000000),
                ..Default::default()
            },
            term: Palette {
                fg: Rgb(255, 255, 255),
                bg: Rgb(0, 0, 0),
                ansi: [Rgb(0, 0, 0); 16],
            },
        };
        let grey = Color::Rgb(Rgb(40, 40, 40));
        // What a cell visibly shows: text, background, and the foreground
        // with everything drawn in it, where it can be seen.
        let looks = |o: &Out| {
            let s = &o.style;
            let fg_shows = o.text != Text::Char(' ')
                || s.underline != Underline::None
                || s.strikethrough
                || s.overline
                || s.link != 0;
            let fg = fg_shows.then(|| {
                (
                    colors.visual_fg(s),
                    colors.visual_sp(s),
                    s.underline,
                    (s.bold, s.italic, s.dim, s.strikethrough, s.overline),
                    s.link,
                )
            });
            (o.text.clone(), o.wide, colors.visual_bg(s), fg)
        };
        // A blank and a letter, in every mix of colours, reverse, and
        // decorations drawn in the text colour.
        let mut styles = Vec::new();
        for fg in [Color::Default, Color::Rgb(Rgb(10, 200, 30))] {
            for bg in [Color::Default, Color::Rgb(Rgb(0, 0, 0)), grey] {
                for reverse in [false, true] {
                    for underline in [Underline::None, Underline::Line, Underline::Curl] {
                        styles.push(Style {
                            fg,
                            bg,
                            reverse,
                            underline,
                            ..Style::default()
                        });
                    }
                    for (strikethrough, overline, link) in
                        [(true, false, 0), (false, true, 0), (false, false, 1)]
                    {
                        styles.push(Style {
                            fg,
                            bg,
                            reverse,
                            strikethrough,
                            overline,
                            link,
                            ..Style::default()
                        });
                    }
                }
            }
        }
        let cells = styles.iter().flat_map(|&style| {
            [' ', 'x'].map(|c| Out {
                text: Text::Char(c),
                style,
                wide: false,
            })
        });
        let mut left_alone = 0;
        for cell in cells {
            for to in [Rgb(0, 0, 0), Rgb(40, 40, 40)] {
                for keep in [0, 37, 100] {
                    // The fade without the skip.
                    let mut was = cell.clone();
                    compose::fade_to(&colors, &mut was.style, to, keep);
                    if keep == 0 {
                        was.text = Text::Char(' ');
                    }
                    for cursor in [false, true] {
                        let mut now = cell.clone();
                        fade_cell(&colors, &mut now, to, keep, cursor);
                        let case = format!("{cell:?} to {to:?} keep {keep} cursor {cursor}");
                        if !cursor && unseen(&colors, &cell, to) {
                            left_alone += 1;
                            assert_eq!(now, cell, "{case}: left as it was");
                        } else {
                            assert_eq!(now, was, "{case}: moved as ever");
                        }
                        assert_eq!(looks(&now), looks(&was), "{case}");
                    }
                }
            }
        }
        assert!(left_alone > 0, "some blanks were left alone");
    }

    /// A window of `first` rows then blanks, cursor at `cursor`, about to
    /// switch to rows of `then`; settled before the switch.
    fn switching(first: char, then: char, cursor: (usize, usize)) -> Scene {
        let mut s = Scene::new(Options::default());
        s.model.apply(
            Event::DefaultColors(crate::client::style::DefaultColors {
                rgb_fg: Some(0xffffff),
                rgb_bg: Some(0x000000),
                ..Default::default()
            }),
            &mut Changes::default(),
        );
        s.model.apply(
            Event::OptionSet {
                name: "termguicolors".into(),
                value: crate::client::redraw::OptionValue::Bool(true),
            },
            &mut Changes::default(),
        );
        let rows = |c: char| (0..4).map(move |r| line(2, r, 0, &format!("{c}{:20}", "")));
        let mut events: Vec<Event> = rows(first).collect();
        events.push(Event::GridCursor {
            grid: 2,
            row: cursor.0,
            col: cursor.1,
        });
        s.batch(events);
        s.settle();
        s.switched(rows(then).collect(), &[2]);
        assert_eq!(s.anim.switches.len(), 1, "a switch fades");
        s
    }

    #[test]
    fn a_buffer_switch_leaves_the_blanks_as_they_are_but_the_cursors() {
        let mut s = switching('a', 'c', (1, 5));
        let frame = |s: &mut Scene, ms: u64| {
            s.anim.advance(s.now + Duration::from_millis(ms));
            compose::compose(&s.model, &s.anim, W, H)
        };
        let after = frame(&mut s, 1000);
        assert!(s.anim.switches.is_empty(), "settled");
        let mut s = switching('a', 'c', (1, 5));
        for ms in (0..=200).step_by(10) {
            let f = frame(&mut s, ms);
            for r in 0..4 {
                for c in 1..W {
                    let (now, then) = (f.get(r, c), after.get(r, c));
                    if (r, c) == (1, 5) {
                        if (20..180).contains(&ms) {
                            assert_ne!(now, then, "the cursor's blank fades at {ms} ms");
                        }
                    } else {
                        assert_eq!(now, then, "row {r} column {c} at {ms} ms");
                    }
                }
            }
        }
    }

    #[test]
    fn a_buffer_switch_writes_no_blanks() {
        use crate::client::screen::{Cursor, Screen};
        let cursor = Cursor {
            visible: false,
            row: 0,
            col: 0,
            shape: 2,
            color: None,
        };
        // What a terminal prints of `bytes`: all but escape sequences.
        let printed = |bytes: &[u8]| {
            let mut text = Vec::new();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                } else if bytes[i] >= b' ' {
                    text.push(bytes[i]);
                }
                i += 1;
            }
            String::from_utf8_lossy(&text).into_owned()
        };
        let mut s = switching('a', 'c', (0, 0));
        let mut screen = Screen::new();
        // The window before the switch, as the terminal shows it.
        let before = {
            let mut s = switching('a', 'a', (0, 0));
            s.anim.switches.clear();
            compose::compose(&s.model, &s.anim, W, H)
        };
        screen.draw(before, &cursor, false, &[]);
        let mut wrote = String::new();
        for ms in (0..=240).step_by(16) {
            s.anim.advance(s.now + Duration::from_millis(ms));
            let f = compose::compose(&s.model, &s.anim, W, H);
            wrote += &printed(&screen.draw(f, &cursor, false, &[]));
        }
        assert!(s.anim.switches.is_empty(), "settled");
        assert!(wrote.contains('a') && wrote.contains('c'), "{wrote:?}");
        assert!(!wrote.contains(' '), "a blank written: {wrote:?}");
    }
}
