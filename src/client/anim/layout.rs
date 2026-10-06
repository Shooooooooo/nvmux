//! Split windows opening, closing and changing size the way animate.nvim's
//! window module draws them, rather than Neovide's slide.
//!
//! Neovim lays split windows out as a tree: a row of windows side by side or
//! a column of them one above the other, each child a window or another such
//! row or column. Every window takes a frame of the screen — its grid, the
//! status line or separator under it, the separator to its right — and the
//! frames tile the space between the tabline and the command line. A UI is
//! told where each window's grid is and nothing of the tree, so the tree is
//! read back off the frames ([`build`]): a column every frame lies wholly to
//! one side of is where a row is split, a line every frame lies wholly above
//! or below is where a column is.
//!
//! What a batch did to the windows drawn last is then one of these:
//!
//! - the same windows, still placed as they were relative to one another, in
//!   new sizes — `<C-w>>`, `:resize`, `<C-w>=`: every edge moves from where it
//!   was to where it goes, eased in and out, over 150 ms, or a frame a cell
//!   for a move of a few cells, and at once for a single cell;
//! - one window more, split off another — `:split`, `:vsplit`, `:new`, a
//!   plugin's sidebar: it starts as a sliver one cell of text deep on its own
//!   side of the space it took (right or below with `'splitright'` and
//!   `'splitbelow'`, left or above without), and grows to its place as every
//!   other window moves to its own, its text fading in out of its background,
//!   over 200 ms, quick to start and slow to land;
//! - windows fewer — `:close`, `:only`: the window before each run of closed
//!   ones, left of it or above it, takes their place at once, and a ghost of
//!   them — what they showed, with the separator before them along its edge —
//!   flies off into their far side, its text dimming into its background as
//!   it goes, over 180 ms, slow to start. Where the closed windows came
//!   first, the window after them moves into their place instead, its text
//!   with it, over their ghost;
//! - anything else — windows rearranged (`<C-w>x`, `<C-w>r`, `<C-w>H` …),
//!   another tab page, two windows opened at once — is nothing animate.nvim
//!   draws, and is left to Neovide's slide ([`super::motion`]).
//!
//! These are animate.nvim's own rules, down to which window a split took its
//! space from and where a closed window's ghost stops, ported from its
//! `window/fly.lua` — but for the ghost, which there is blank in the closed
//! window's background, fading off the window that takes its place, and here
//! shows the closed window's text dimming away. Its windows are real, resized
//! frame by frame, and Neovim draws each frame; the client's are drawn by the
//! client, from what the editor last drew. A window on its way is drawn in the frame it has this
//! frame, from its top left: one no bigger than it was shows what it showed,
//! cut down to size, and one bigger shows what Neovim drew for where it is
//! going ([`draw_window`]). Its status line is the one Neovim drew for where
//! it is going, the fill between its halves stretched or squeezed to the
//! frame's width the way Neovim would have drawn it there ([`stretch`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::Windows;
use crate::client::compose::{self, Frame, Out};
use crate::client::grid::{Cell, Grid, Text};
use crate::client::model::{Model, Place};
use crate::client::redraw::Event;
use crate::client::style::Colors;
use crate::palette::Rgb;

/// animate.nvim's resize runs at 30 frames a second and moves an edge by a
/// cell a frame at least, so a resize of a few cells takes a frame a cell.
const RESIZE_CELL: f32 = 1.0 / 30.0;

/// The same for a split opening or closing, at its 60.
const FLY_CELL: f32 = 1.0 / 60.0;

/// The options of Neovim's that decide how windows are laid out, as the
/// client's agent in the editor reports them (see `crate::client::App`).
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
    /// How far along an animation `t` of the way through it is.
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

    /// Every edge `e` of the way from here to `to`, to the nearest cell. Two
    /// frames that share an edge at both ends share it all the way.
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

/// An eased progress as a percentage.
pub(super) fn percent(e: f32) -> u8 {
    (e * 100.0).round().clamp(0.0, 100.0) as u8
}

/// How a row or column holds its children — side by side, or one above the
/// other: `winlayout()`'s `row` and `col`.
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

/// A split window, as it is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pane {
    /// Its grid, the status line or separator under it, and the separator to
    /// its right.
    pub frame: Area,
    /// A row of the frame under the grid: the status line, or with
    /// `'laststatus'` 3 a separator.
    pub status: bool,
    /// A column of the frame right of the grid: the separator.
    pub vsep: bool,
    /// Rows of the grid above its text: a winbar.
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
    /// The windows along the bottom have a status line too: `'laststatus'` 2,
    /// or 1 with two windows or more.
    last_status: bool,
}

impl Ends {
    /// Whether a frame of a `kind` reaching `pos` ends with a line there: a
    /// separator short of the right edge, or a status line or separator above
    /// another window — or above the command line, where every window has a
    /// status line.
    fn trailing(&self, kind: Kind, pos: i64) -> bool {
        match kind {
            Kind::Row => pos < self.width,
            Kind::Col => self.last_status || pos < self.bottom,
        }
    }
}

/// The split windows of the tab page on show, and where they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub panes: BTreeMap<u64, Pane>,
    /// What the frames tile.
    pub area: Area,
    ends: Ends,
    laststatus: i64,
}

impl Layout {
    /// The windows the model places in the layout — or nothing, where they do
    /// not tile a screen: one part way through being placed, say.
    pub fn of(model: &Model, laststatus: i64) -> Option<Layout> {
        let width = model.grids.get(&1)?.width() as i64;
        let wins: Vec<(u64, i64, i64, i64, i64)> = model
            .layout
            .iter()
            .filter(|(grid, p)| **grid != 1 && !p.hidden)
            .filter_map(|(grid, p)| match p.place {
                Place::Window { row, col } => {
                    let g = model.grids.get(grid)?;
                    Some((
                        *grid,
                        row as i64,
                        col as i64,
                        g.width() as i64,
                        g.height() as i64,
                    ))
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

/// Whether `frames` tile `area`: each in it, none over another, and nothing
/// of it left over.
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

/// The layout's tree.
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

/// The tree of the windows framed by `items`, read off their frames: split at
/// every cut that runs right across them — down them for a row, along them
/// for a column — into what lies between, and so on into each. Neovim never
/// puts a row straight into a row, nor a column into a column, so cutting at
/// every cut at once makes the same tree it keeps — but for one thing frames
/// cannot tell: four windows in a square are a row of two columns or a column
/// of two rows alike, and `prefer` says which to take.
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

/// `node` without the windows in `gone`, as Neovim leaves a layout they
/// closed in: a row or column left with one child is that child, and a child
/// of the same kind as its row or column merges into it.
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

/// Where `node` is in `frames`, if they lay it out: each row or column's
/// children one after the other along it, and all as deep across it.
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

/// A tree in one piece, its nodes numbered, root first: what a close's
/// ghosts find their rows and columns in.
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

/// A run of closed children of a row or column, one after the other.
#[derive(Debug)]
struct Run {
    parent: usize,
    /// The first of them and the last, among its children.
    i: usize,
    j: usize,
    /// The children either side of them.
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

    /// The windows under node `id` still open.
    fn kept(&self, id: usize, gone: &BTreeSet<u64>) -> Vec<u64> {
        let mut wins = self.leaves(id);
        wins.retain(|w| !gone.contains(w));
        wins
    }

    /// Every run of closed children: those inside a row or column before the
    /// row or column's own, since the window that takes their place is
    /// settled first.
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

    /// Where each child but the first of every row and column starts, from
    /// the old layout to the new: a closed one's start goes where the next
    /// child still open starts, or past the end of its row or column — a cell
    /// further where that end has no line of its own, so that the line leaves
    /// the screen.
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

/// The split windows as they were drawn before a batch that may move them,
/// and what each showed.
#[derive(Debug)]
pub struct Shot {
    layout: Layout,
    /// Each window's grid as it was, by grid.
    rows: HashMap<u64, Vec<Vec<Cell>>>,
    /// Grid 1 as it was: the separators and status lines of windows about to
    /// close.
    chrome: Grid,
    /// The grid the cursor was on.
    cursor: u64,
}

impl Shot {
    /// The windows as the model has them — or, part way through animating,
    /// as the animation has them, so that what comes next starts from there.
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
            // A ghost's place is no window's: from where the windows are.
            if let Some(area) = frames.iter().copied().reduce(|a, b| a.union(&b)) {
                if tiles(&frames, &area) {
                    layout.panes = panes;
                    layout.area = area;
                }
            }
        }
        let rows = layout
            .panes
            .keys()
            .filter_map(|g| {
                let grid = model.grids.get(g)?;
                Some((*g, grid.lines(0, grid.height(), 0, grid.width())))
            })
            .collect();
        Some(Shot {
            layout,
            rows,
            chrome: model.grids.get(&1).cloned().unwrap_or_default(),
            cursor: model.cursor.grid,
        })
    }
}

/// Whether a batch may move split windows: it places one, or hides, closes
/// or does away with one that was in the layout.
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
    /// Nothing: they are where they were.
    Same,
    /// Something animate.nvim animates: see the module docs.
    Animate(Box<Transition>),
    /// Something it draws at once: a change of a cell, or a layout the
    /// animation could not make sense of.
    Snap,
    /// Something it does not draw at all: left to Neovide's slide.
    Slide,
}

/// What the windows `old` showed became `new`. See the module docs.
pub fn change(old: Shot, new: &Layout, model: &Model, w: &Windows, opts: Options) -> Change {
    if old.layout.panes == new.panes {
        return Change::Same;
    }
    if old.layout.ends.width != new.ends.width {
        // The screen itself changed: everything is somewhere new.
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

/// Whether every two of `wins` are still placed as they were relative to
/// each other: one still left of the other, or above it, as it was. Windows
/// that change size keep that; windows rearranged do not — one that was left
/// of another is right of it now, or above it.
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

/// The most cells any window's frame changes by, from `start` to `new`.
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

/// The same windows in new sizes: every edge from where it was to where it
/// goes.
fn resize(old: Shot, new: &Layout, secs: f32) -> Option<Transition> {
    let start = old.layout.frames();
    let cells = most_cells(&start, new);
    // A change of one cell has no frame between the two.
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

/// One window more, `win`: it flies in from its own side of the space it took.
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
    // The tree with it, which without it is the tree as it was.
    let items: Vec<(u64, Area)> = new_fr.iter().map(|(g, a)| (*g, *a)).collect();
    let gone = BTreeSet::from([win]);
    let tree = [Kind::Row, Kind::Col].into_iter().find_map(|prefer| {
        let tree = build(&items, prefer)?;
        fits(&prune(&tree, &gone)?, &old_fr)?;
        Some(tree)
    })?;
    let (kind, kids, i) = parent_of(&tree, win)?;
    // The child next to it whose space it took: the only neighbour that
    // changed size along the row or column; with both changed ('equalalways'),
    // the one it was split from, when it is the current window; else the one
    // 'splitright' or 'splitbelow' puts it after.
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

/// Windows fewer, `gone`: each run of them flies out into its far side.
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
    // The tree as it was, which without them is the tree as it is.
    let items: Vec<(u64, Area)> = old_fr.iter().map(|(g, a)| (*g, *a)).collect();
    let tree = [Kind::Row, Kind::Col].into_iter().find_map(|prefer| {
        let tree = build(&items, prefer)?;
        fits(&prune(&tree, gone)?, &new_fr)?;
        Some(tree)
    })?;
    let tree = Tree::from(&tree);
    let mut runs = Vec::new();
    tree.runs(0, gone, &mut runs);
    // The windows start where they were, the window before each run over the
    // run's place too; the one after a run that came first moves into it.
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
            picture: picture(&old, &closed, region),
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

/// The line a frame of `kind` ends with, drawn as `owner`'s was: its
/// separator, or for a status line a blank bar in `StatusLineNC` — or with
/// `'laststatus'` 3, the separator that takes a status line's place.
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

/// The cell of `grid` at `row`, `col`, if there is one there.
fn cell_at(grid: &Grid, row: i64, col: i64) -> Option<&Cell> {
    grid.cell(usize::try_from(row).ok()?, usize::try_from(col).ok()?)
}

/// What the windows `wins` showed of `region` as it was drawn: their text,
/// and around it their separators and status lines.
fn picture(old: &Shot, wins: &[u64], region: Area) -> Vec<Vec<Cell>> {
    let grids: Vec<(Area, &Vec<Vec<Cell>>)> = wins
        .iter()
        .filter_map(|w| {
            let p = old.layout.panes.get(w)?;
            Some((p.grid(p.frame), old.rows.get(w)?))
        })
        .collect();
    (region.top..region.bottom)
        .map(|y| {
            (region.left..region.right)
                .map(|x| {
                    let own = grids
                        .iter()
                        .find(|(g, _)| {
                            (g.top..g.bottom).contains(&y) && (g.left..g.right).contains(&x)
                        })
                        .and_then(|(g, rows)| {
                            rows.get((y - g.top) as usize)?.get((x - g.left) as usize)
                        });
                    own.or_else(|| cell_at(&old.chrome, y, x))
                        .cloned()
                        .unwrap_or_default()
                })
                .collect()
        })
        .collect()
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
    /// Each window, by grid: the frame it starts in, and how it is laid out
    /// at the end.
    wins: BTreeMap<u64, (Area, Pane)>,
    /// What the windows showed before, by grid: what a frame bigger than a
    /// window's grid shows past it.
    rows: HashMap<u64, Vec<Vec<Cell>>>,
    /// A window opened, whose text fades in out of its background — that
    /// highlight's.
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
            veil: None,
            ghosts: None,
            elapsed: 0.0,
            duration,
            easing,
        }
    }

    /// Move on `dt` seconds. Says whether it is still going.
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

    /// Whether window grid `grid` is one of those moving.
    pub fn involves(&self, grid: u64) -> bool {
        self.wins.contains_key(&grid)
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

    /// Draw the windows as they are this frame, over where the compositor put
    /// them.
    pub fn paint(&self, frame: &mut Frame, model: &Model) {
        let e = self.progress();
        let colors = model.colors();
        let now = self.frames_at(e);
        let ghosts = self
            .ghosts
            .as_ref()
            .map_or_else(Vec::new, |g| g.frames(e, &now));
        // What the windows and the ghosts cover this frame — they tile it, but
        // for a cell rounding might lose — blank to start with.
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
            draw_window(frame, model, *grid, pane, at, self.rows.get(grid));
            if let Some(chrome) = chrome {
                draw_lines(frame, model, chrome, pane, at);
            }
        }
        if let Some((grid, bg)) = self.veil {
            if let Some((_, pane)) = self.wins.get(&grid) {
                let to = colors.visual_bg(&model.style(bg));
                fade(frame, &colors, pane.grid(now[&grid]), to, percent(e));
            }
        }
        for (g, at) in &ghosts {
            g.paint(frame, model, &colors, at, e);
        }
    }
}

/// Window `grid`'s text in the frame `at` has for it, from its top left.
///
/// A window no bigger than it was shows what it showed, cut down to size:
/// what Neovim drew for where it is going is laid out for there — its lines
/// wrapped at another width, scrolled to keep the cursor in view — and
/// pieced together with the old it would not read. A window bigger than it
/// was shows its grid, and past that what it showed before. Past both, it is
/// blank in the colours its edge has.
fn draw_window(
    frame: &mut Frame,
    model: &Model,
    grid: u64,
    pane: &Pane,
    at: Area,
    old: Option<&Vec<Vec<Cell>>>,
) {
    let text = pane.grid(at);
    let g = model.grids.get(&grid);
    let (height, width) = (text.height().max(0) as usize, text.width().max(0) as usize);
    let within =
        old.is_some_and(|rows| height <= rows.len() && width <= rows.first().map_or(0, Vec::len));
    for r in 0..height {
        for c in 0..width {
            let own = || g.and_then(|g| Some((g.cell(r, c)?, g.is_wide(r, c))));
            let was = || {
                let row = old?.get(r)?;
                Some((
                    row.get(c)?,
                    row.get(c + 1).is_some_and(|n| n.text == Text::Half),
                ))
            };
            let cell = if within {
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
                None => Out::blank(model.style(edge(g, old, r, c))),
            };
            if let Some(o) = frame.at(text.top + r as i64, text.left + c as i64) {
                *o = out;
            }
        }
    }
}

/// The highlight of the cell nearest `(r, c)` of a window that shows nothing
/// there: what the blank that fills it is drawn in.
fn edge(g: Option<&Grid>, old: Option<&Vec<Vec<Cell>>>, r: usize, c: usize) -> u32 {
    if let Some(g) = g.filter(|g| g.width() > 0 && g.height() > 0) {
        let at = g.cell(r.min(g.height() - 1), c.min(g.width() - 1));
        return at.map_or(0, |cell| cell.hl);
    }
    old.and_then(|rows| rows.get(r.min(rows.len().saturating_sub(1))))
        .and_then(|row| row.get(c.min(row.len().saturating_sub(1))))
        .map_or(0, |cell| cell.hl)
}

/// A window's separator and status line in the frame `at`, as Neovim drew
/// them where the window ends up: the separator as long as the frame is
/// deep, and the status line as wide as it is (see [`stretch`]).
fn draw_lines(frame: &mut Frame, model: &Model, chrome: &Grid, pane: &Pane, at: Area) {
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
    }
}

/// A status line Neovim drew as wide as `cells`, drawn `width` wide instead,
/// the way Neovim would have drawn it: the longest run of one cell in it —
/// the fill between what is on its left and what is on its right — grows or
/// shrinks, down to a cell; and past that it loses what is at its start,
/// with a `<` where it was cut, as Neovim truncates a status line that says
/// nothing of where to. The last `corner` cells, where it meets the
/// separator, stay at its end.
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

/// Where the longest run of one cell repeated starts, and how long it is: the
/// first of the longest, and never the right half of something wide.
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

/// Every cell of `area` moved towards `to`, keeping `keep` percent of its
/// own colours.
fn fade(frame: &mut Frame, colors: &Colors, area: Area, to: Rgb, keep: u8) {
    for r in area.top..area.bottom {
        for c in area.left..area.right {
            if let Some(cell) = frame.at(r, c) {
                fade_cell(colors, cell, to, keep);
            }
        }
    }
}

/// A cell moved towards `to`, keeping `keep` percent of its colours — and
/// with none of them kept, a blank in `to` rather than text nobody can see.
pub(super) fn fade_cell(colors: &Colors, cell: &mut Out, to: Rgb, keep: u8) {
    compose::fade_to(colors, &mut cell.style, to, keep);
    if keep == 0 {
        cell.text = Text::Char(' ');
        cell.wide = false;
    }
}

/// The ghosts of windows closed, and the layout as it was, which they fly
/// out of.
#[derive(Debug)]
struct Ghosts {
    tree: Tree,
    /// Where each child but the first of every row and column starts, from
    /// and to, by node.
    bounds: Vec<Option<(i64, i64)>>,
    /// The layout's area as it was.
    screen: Area,
    list: Vec<Ghost>,
    /// The new layout's lines, which a ghost stops short of.
    ends: Ends,
}

/// A run of windows closed, flying out into its far side.
#[derive(Debug)]
struct Ghost {
    kind: Kind,
    /// The row or column they were in, and the first of them, as nodes.
    parent: usize,
    first: usize,
    /// The child after them, if any.
    after: Option<usize>,
    /// The windows before them still open, which hold their place: the ghost
    /// goes up to their separator. None where the closed windows came first.
    holders: Option<Vec<u64>>,
    /// What they showed of their place, as it was drawn (see [`picture`]):
    /// drawn in the ghost from its near edge, as a window shrinking is drawn
    /// from its top left.
    picture: Vec<Vec<Cell>>,
    /// Their background, as a highlight: past the picture, the ghost is
    /// blank in it.
    bg: u32,
    /// The line along its edge: the separator, or the status line, of the
    /// window before them, or their own.
    line: Cell,
    /// Where that line meets their status lines, for windows side by side.
    corner: Option<Cell>,
}

/// Where a ghost is this frame.
#[derive(Debug)]
struct Spot {
    /// Its cells.
    f: Area,
    /// Where along it its lines are.
    lines: Vec<i64>,
    /// Where its picture's top left is.
    origin: (i64, i64),
}

impl Ghosts {
    /// Every row, column and window of the old layout at progress `e`, and
    /// where each child starts — not kept inside its row or column, so that
    /// a line can leave it at the end.
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
        // A row or column closed whole lies inside its run's ghost.
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

    /// Ghost `g`'s cells this frame, and where along it its lines are: from
    /// the line of the window before it, drawn, to the line after it — the
    /// separator of the window holding its place, or its own, drawn.
    fn frame_of(
        &self,
        g: &Ghost,
        rects: &[Area],
        starts: &[i64],
        now: &BTreeMap<u64, Area>,
    ) -> Option<Spot> {
        let kind = g.kind;
        let p = rects[g.parent];
        // Its picture goes where the closed windows start: with the line
        // before them, flying off; or where their row or column does.
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
        // A ghost of windows one above the other stops before the separator
        // on its right.
        if kind == Kind::Col && self.ends.trailing(Kind::Row, f.right) {
            f.right -= 1;
        }
        (!f.is_empty()).then_some(Spot { f, lines, origin })
    }

    /// Every ghost still to be seen at progress `e`, and where it is.
    fn frames(&self, e: f32, now: &BTreeMap<u64, Area>) -> Vec<(&Ghost, Spot)> {
        let (rects, starts) = self.at(e);
        self.list
            .iter()
            .filter_map(|g| Some((g, self.frame_of(g, &rects, &starts, now)?)))
            .collect()
    }
}

impl Ghost {
    /// Draw it where `at` has it at progress `e`: what the closed windows
    /// showed, its text dimmed that far into its background, and the line
    /// along its edge at full strength.
    fn paint(&self, frame: &mut Frame, model: &Model, colors: &Colors, at: &Spot, e: f32) {
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
                        fade_cell(colors, o, own, keep);
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
    use crate::palette::Palette;

    fn area(top: i64, bottom: i64, left: i64, right: i64) -> Area {
        Area {
            top,
            bottom,
            left,
            right,
        }
    }

    /// A window full height on the left, two one above the other on the
    /// right: a row whose second child is a column.
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
        // Without the window at the bottom right, the one above it is the
        // row's second child itself.
        let gone = BTreeSet::from([5]);
        assert_eq!(
            prune(&tree, &gone),
            Some(Node::Split(Kind::Row, vec![Node::Leaf(2), Node::Leaf(4)]))
        );
    }

    /// Four windows in a square are a row of columns or a column of rows
    /// alike: which one is a preference, and both fit.
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

    /// A status line drawn wider grows its fill, and narrower loses it first,
    /// down to a cell, then what is at its start; the corner stays at its end.
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

    /// Two windows side by side and swapped are rearranged; the same two in
    /// new sizes are not.
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

    /// A window's grid `width` × `height` at `row`, `col`, every row of it
    /// `fill`.
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

    /// A client's animations and model, a screen `W` wide with one window,
    /// grid 2, full of `a` over its status line.
    struct Scene {
        anim: Animator,
        model: Model,
        height: usize,
        /// When the last batch came.
        now: Instant,
        drawn: bool,
    }

    impl Scene {
        /// `H` rows high.
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
            self.anim
                .after(before, &self.model, &changes, true, self.drawn, self.now);
            self.drawn = true;
        }

        /// The screen `ms` after the last batch.
        fn at(&mut self, ms: u64) -> String {
            self.anim.advance(self.now + Duration::from_millis(ms));
            compose::compose(&self.model, &self.anim, W, self.height).text()
        }

        /// Everything moving landed, a while on.
        fn settle(&mut self) -> String {
            self.now += Duration::from_secs(10);
            self.at(0)
        }

        fn rows(&mut self, ms: u64) -> Vec<String> {
            self.at(ms).lines().map(String::from).collect()
        }

        /// How bright the text of cell `row`, `col` is `ms` after the last
        /// batch: the red of its colour.
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
    /// separator, and grid 4 right of it.
    fn vsplit_right() -> Vec<Event> {
        let mut events = window(2, 0, 0, (10, 4), 'a');
        events.extend(window(4, 0, 11, (10, 4), 'b'));
        events.extend((0..4).map(|r| line(1, r, 10, "│")));
        events.push(line(1, 4, 0, "a.txt==1,1 b.txt==All"));
        events.push(cursor(4));
        events
    }

    /// A split flies in from its side: a column of text at the right edge
    /// first, its separator coming left to its place.
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
        // Part way, the separator is between where it started and its place.
        let mid = s.at(40);
        let sep = mid.lines().next().unwrap().chars().position(|c| c == '│');
        assert!(sep.is_some_and(|c| (11..19).contains(&c)), "{mid}");
        let last = s.at(1000);
        assert!(!s.moving());
        let rows: Vec<&str> = last.lines().collect();
        assert_eq!(rows[0], "aaaaaaaaaa│bbbbbbbbbb");
        assert_eq!(rows[4], "a.txt==1,1 b.txt==All");
    }

    /// Without 'splitright' the new window is on the left, and comes in from
    /// the left: the window it split from moves right, its text with it.
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

    /// A closed window flies off into its far side behind the separator
    /// before it, its text going with it and dimming into its background;
    /// the window before it holds its place from the start.
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

    /// The first window closed: the one after it moves into its place, its
    /// text with it, over the closed window's, which stays where it was,
    /// dimming.
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

    /// A resize moves the separator a cell at a time, eased in and out: it
    /// starts where it was and lands where it goes, in between on the way.
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

    /// A resize of a cell has nothing between: it is there at once.
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

    /// Windows swapped round are left to Neovide's slide.
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

    /// A split one above the other: the new window comes up from the bottom
    /// with 'splitbelow', its status line drawn from the first frame.
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

    /// While windows move, the terminal's cursor is hidden: it shows again in
    /// its cell when they land.
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

    /// A window that shows another buffer fades through its background: the
    /// old text first, dimming, then the new, coming up — and a window that
    /// moved meanwhile is not faded.
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
        // More than a few windows at once fade none of them.
        let events = (0..4).map(|r| line(2, r, 0, &"d".repeat(21))).collect();
        s.switched(events, &[2, 2, 2, 2]);
        assert!(s.anim.switches.is_empty());
    }
}
