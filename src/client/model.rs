//! The client's model of the editor: grids, highlights, window placement,
//! cursor, mode and the options that affect drawing.
//!
//! Events between two `flush`es describe a half-redrawn screen, so
//! `super::App` buffers them and applies the batch at `flush`; animation
//! frames always draw from a finished picture. Changes relevant to animation
//! are reported in [`Changes`] by [`Model::apply`], for [`super::anim`].

use std::collections::{BTreeMap, HashMap};

use super::grid::{Cell, Grid};
use super::redraw::{Event, ModeInfo, OptionValue};
use super::style::{Attrs, Colors, DefaultColors, Style};
use super::wide::Tail;
use crate::palette::Palette;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// A window in the layout. `width` x `height` is its screen area: the
    /// grid's size, unless the client asked for a narrower grid to centre the
    /// text (see [`super::wide`]). Zero if placed before its size was known.
    Window {
        row: usize,
        col: usize,
        width: usize,
        height: usize,
    },
    Float {
        row: i64,
        col: i64,
        zindex: i64,
        compindex: i64,
        mouse: bool,
        /// Anchor grid, if given; a float anchored to a window follows where
        /// that window's text is drawn.
        anchor: Option<u64>,
    },
    /// Full width from `row` down. `scrolled`: messages pushed it up over the
    /// windows, and the row above is a separator drawn with `sep`.
    Message {
        row: usize,
        scrolled: bool,
        sep: String,
        zindex: i64,
        compindex: i64,
    },
}

/// Rows and columns of a window's grid outside its text: winbar, border.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Margins {
    pub top: usize,
    pub bottom: usize,
    pub left: usize,
    pub right: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub place: Place,
    pub hidden: bool,
}

impl Placement {
    /// Screen position of the top left.
    pub fn origin(&self) -> (i64, i64) {
        match &self.place {
            Place::Window { row, col, .. } => (*row as i64, *col as i64),
            Place::Float { row, col, .. } => (*row, *col),
            Place::Message { row, .. } => (*row as i64, 0),
        }
    }

    /// `(zindex, compindex)` for grids drawn over the layout; `None` for
    /// windows in it.
    pub fn layer(&self) -> Option<(i64, i64)> {
        match &self.place {
            Place::Window { .. } => None,
            Place::Float {
                zindex, compindex, ..
            }
            | Place::Message {
                zindex, compindex, ..
            } => Some((*zindex, *compindex)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorPos {
    pub grid: u64,
    pub row: usize,
    pub col: usize,
}

/// The last `win_viewport`: zero-based, `botline` as Neovim counts it (see
/// [`Viewport::one_to_one`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Viewport {
    pub topline: i64,
    pub botline: i64,
    pub curline: i64,
    /// In bytes.
    pub curcol: i64,
    pub line_count: i64,
}

impl Viewport {
    /// Whether `rows` rows show one buffer line each (no folds, wraps or
    /// virtual lines), judging by `botline`: `topline + rows + 1`,
    /// `line_count` if the view ends exactly on the last line, or
    /// `line_count + 1` if there are rows past the end.
    pub fn one_to_one(&self, rows: usize) -> bool {
        let end = self.topline + rows as i64;
        let expected = match end.cmp(&self.line_count) {
            std::cmp::Ordering::Less => end + 1,
            std::cmp::Ordering::Equal => self.line_count,
            std::cmp::Ordering::Greater => self.line_count + 1,
        };
        self.topline >= 0 && self.botline == expected
    }

    /// For a one-to-one view, how many of `rows` show buffer lines rather
    /// than rows past the end.
    pub fn lines_shown(&self, rows: usize) -> usize {
        (self.line_count - self.topline).clamp(0, rows as i64) as usize
    }
}

/// UI options that affect how the client draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub termguicolors: bool,
    pub mousemoveevent: bool,
    pub ttimeout: bool,
    pub ttimeoutlen: u64,
    pub termsync: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            termguicolors: false,
            mousemoveevent: false,
            ttimeout: true,
            ttimeoutlen: 50,
            termsync: true,
        }
    }
}

/// What a batch changed that matters beyond the next frame.
#[derive(Debug, Default)]
pub struct Changes {
    /// Grid 1 was cleared: the terminal may have been drawn on behind the
    /// client's back (`:mode`, which nvmux sends to repaint), so the next
    /// frame paints every cell.
    pub repaint: bool,
    /// Grids resized or destroyed; their animations end.
    pub reshaped: Vec<u64>,
    /// Scroll deltas by grid, from `win_viewport`.
    pub scrolled: HashMap<u64, i64>,
    /// The same by window handle: without `ext_multigrid`, the only way
    /// `win_viewport` tells windows apart.
    pub scrolled_win: HashMap<i64, i64>,
    /// Each `grid_scroll`: grid, top, bot, left, right, rows.
    pub region_scrolls: Vec<(u64, usize, usize, usize, usize, i64)>,
    pub bells: usize,
    pub visual_bell: bool,
    /// `ui_send` bytes for the terminal, in order.
    pub sent: Vec<Vec<u8>>,
    pub title: bool,
    pub mouse: bool,
    /// The server is exiting with this status.
    pub exit: Option<i64>,
}

/// [`Highlights::ids`] entry for a Neovim id not yet defined.
const UNDEFINED: u32 = u32::MAX;

/// Retired ids, beyond those the last sweep found held, that trigger the next
/// sweep. A sweep reads every kept cell, and a truecolour `:terminal`
/// animation can make Neovim reissue thousands of ids a frame.
const RECLAIM_AT: usize = 4096;

/// Neovim's highlights, keyed by the client's own ids.
///
/// A client id equals Neovim's until Neovim reuses it. A truecolour
/// `:terminal` exhausts Neovim's 65 535 ids fast, and Neovim then reissues
/// them, often mid-frame, while earlier cells still name them. Since this
/// client resolves cells per batch and keeps old cells for animations, a
/// redefined id gets a fresh client id so old cells keep their colours.
/// Replaced ids are retired and reused once unheld (see [`Model::reclaim`]).
#[derive(Debug, Default)]
struct Highlights {
    /// By client id; `None` if never used.
    defs: Vec<Option<(Attrs, Attrs)>>,
    /// Client id per Neovim id; [`UNDEFINED`] until defined.
    ids: Vec<u32>,
    /// Client ids whose Neovim ids were reissued; old cells may still hold
    /// them.
    retired: Vec<u32>,
    /// How many retired ids the last sweep found still held.
    kept: usize,
    /// Unheld ids, to reuse.
    free: Vec<u32>,
    /// Ids defined since styles were last resolved.
    fresh: Vec<u32>,
    /// Editor highlight groups by group id, sent by the agent, for rows the
    /// client draws itself (see [`syntax_hl`]).
    groups: HashMap<u32, (Attrs, Attrs)>,
    /// Derived highlights (see [`Derive`]) by index, and the reverse map.
    derived: Vec<(u32, Derive)>,
    derivations: HashMap<(u32, Derive), u32>,
}

impl Highlights {
    /// Client id for Neovim id `id`; an undefined id maps to itself and draws
    /// in the default colours.
    fn client(&self, id: u32) -> u32 {
        match self.ids.get(id as usize) {
            Some(&k) if k != UNDEFINED => k,
            _ => id,
        }
    }

    /// Neovim (re)defines its highlight `id` as `def`.
    fn define(&mut self, id: u32, def: (Attrs, Attrs)) {
        let i = id as usize;
        if self.ids.len() <= i {
            self.ids.resize(i + 1, UNDEFINED);
        }
        let k = match self.ids[i] {
            // The default is never remapped.
            _ if id == 0 => 0,
            UNDEFINED if self.unused(id) => id,
            UNDEFINED => self.take(),
            // Unchanged resend, e.g. every highlight on UI attach.
            k if self.defs[k as usize].as_ref() == Some(&def) => return,
            k => {
                self.retired.push(k);
                self.take()
            }
        };
        self.ids[i] = k;
        let n = k as usize;
        if self.defs.len() <= n {
            self.defs.resize(n + 1, None);
        }
        self.defs[n] = Some(def);
        self.fresh.push(k);
    }

    /// Whether client id `k` is unused, so Neovim's id `k` can take it.
    fn unused(&self, k: u32) -> bool {
        self.defs.get(k as usize).is_none_or(Option::is_none)
    }

    /// A free id, or a new one past every id used.
    fn take(&mut self) -> u32 {
        self.free.pop().unwrap_or_else(|| {
            self.defs.push(None);
            (self.defs.len() - 1) as u32
        })
    }
}

/// Bitset of highlight ids still referenced (by cells, groups, mode cursors),
/// which a sweep must not free (see [`Model::reclaim`]).
#[derive(Debug)]
pub struct Held(Vec<u64>);

impl Held {
    /// Mark `hl` held; for a [`syntax_hl`] id, its base.
    pub fn mark(&mut self, hl: u32) {
        let hl = if hl & SYNTAX != 0 { hl & 0xffff } else { hl };
        if let Some(word) = self.0.get_mut(hl as usize / 64) {
            *word |= 1 << (hl % 64);
        }
    }

    pub fn cells<'a>(&mut self, cells: impl IntoIterator<Item = &'a Cell>) {
        for cell in cells {
            self.mark(cell.hl);
        }
    }

    fn holds(&self, hl: u32) -> bool {
        self.0
            .get(hl as usize / 64)
            .is_some_and(|word| word & (1 << (hl % 64)) != 0)
    }
}

/// Flag on a client-made highlight id: an editor group laid over a Neovim
/// highlight, for rows the client draws itself with syntax colours (see
/// `super::predict`); or, with group 0, a derived highlight (see [`Derive`]).
const SYNTAX: u32 = 1 << 31;

/// Id for `group` laid over `base` as Neovim layers syntax groups: the
/// group's attributes, falling back to `base`'s. Ids too large to pack
/// return `base`.
pub fn syntax_hl(base: u32, group: u32) -> u32 {
    if base > 0xffff || group == 0 || group > 0x7fff {
        return base;
    }
    SYNTAX | group << 16 | base
}

/// A Neovim highlight with the `'cursorline'` bar's background added (where
/// the cell has none) or removed (where it equals the bar's), for rows the
/// client moves the cursor onto or off until Neovim redraws them (see
/// `super::App::repaint`). Briefly wrong for cells whose own background is
/// the bar's, and for `hl_mode` `replace` virtual text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Derive {
    CursorLineOn,
    CursorLineOff,
}

/// Max derived highlights: the range of an id's low 16 bits.
const DERIVED: usize = 1 << 16;

/// `over`'s colours where set, else `under`'s; attributes from both.
fn laid(under: &Attrs, over: &Attrs) -> Attrs {
    Attrs {
        fg: over.fg.or(under.fg),
        bg: over.bg.or(under.bg),
        sp: over.sp.or(under.sp),
        reverse: under.reverse || over.reverse,
        bold: under.bold || over.bold,
        italic: under.italic || over.italic,
        strikethrough: under.strikethrough || over.strikethrough,
        altfont: under.altfont || over.altfont,
        dim: under.dim || over.dim,
        blink: under.blink || over.blink,
        conceal: under.conceal || over.conceal,
        overline: under.overline || over.overline,
        underline: if over.underline == Default::default() {
            under.underline
        } else {
            over.underline
        },
        blend: under.blend,
        url: over.url.clone().or_else(|| under.url.clone()),
    }
}

#[derive(Debug)]
pub struct Model {
    pub grids: BTreeMap<u64, Grid>,
    pub layout: BTreeMap<u64, Placement>,
    /// Window handle by grid.
    pub wins: BTreeMap<u64, i64>,
    pub viewports: HashMap<u64, Viewport>,
    /// By grid. Separate from placements: Neovim sends margins before placing
    /// a window and not again when it re-places it.
    pub margins: HashMap<u64, Margins>,
    pub cursor: CursorPos,
    /// From `mode_info_set`, with cursor highlights as client ids.
    pub modes: Vec<ModeInfo>,
    pub mode: usize,
    pub mode_name: String,
    /// `mode_info_set`'s `enabled`: whether the client sets cursor shape.
    pub cursor_styled: bool,
    pub options: Options,
    pub busy: bool,
    pub mouse: bool,
    pub title: String,
    pub defaults: DefaultColors,
    /// `hl_group_set`: client id of each UI-drawn group, by name (see
    /// [`Highlights`]).
    pub groups: HashMap<String, u32>,
    /// `'laststatus'` as reported by the editor's measure (see
    /// [`super::wide::MEASURE_LUA`]); `None` until reported.
    pub laststatus: Option<i64>,
    /// Past-end-of-line text that does not fit in a narrowed window grid, by
    /// grid and row (see [`super::wide`]).
    pub tails: HashMap<u64, BTreeMap<usize, Tail>>,
    highlights: Highlights,
    /// Resolved styles and blends by id; all re-resolved when the defaults or
    /// `'termguicolors'` change (see [`Model::colors`]).
    styles: Vec<Style>,
    blends: Vec<u8>,
    styles_stale: bool,
    term: Palette,
    /// Hyperlinks in arrival order: link `n` (see [`Style::link`]) is
    /// `links[n - 1]`.
    links: Vec<Box<str>>,
    link_numbers: HashMap<Box<str>, u32>,
    /// Last change to frame inputs other than grid cells (which grids stamp
    /// themselves): placements, highlights, options. On the grids' clock (see
    /// [`super::grid::tick`]).
    changed: u64,
}

impl Model {
    /// `term` supplies fallback colours.
    pub fn new(term: Palette) -> Self {
        Self {
            grids: BTreeMap::new(),
            layout: BTreeMap::new(),
            wins: BTreeMap::new(),
            viewports: HashMap::new(),
            margins: HashMap::new(),
            cursor: CursorPos {
                grid: 1,
                row: 0,
                col: 0,
            },
            modes: Vec::new(),
            mode: 0,
            mode_name: String::new(),
            cursor_styled: true,
            options: Options::default(),
            busy: false,
            mouse: false,
            title: String::new(),
            defaults: DefaultColors::default(),
            groups: HashMap::new(),
            laststatus: None,
            tails: HashMap::new(),
            highlights: Highlights::default(),
            styles: Vec::new(),
            blends: Vec::new(),
            styles_stale: true,
            term,
            links: Vec::new(),
            link_numbers: HashMap::new(),
            changed: super::grid::tick(),
        }
    }

    /// Whether nothing but cell contents (tracked per row, see
    /// [`Grid::changed_since`]) changed since `since`.
    pub fn unchanged_since(&self, since: u64) -> bool {
        self.changed <= since && self.grids.values().all(|g| !g.shaped_since(since))
    }

    pub fn colors(&self) -> Colors {
        Colors {
            rgb: self.options.termguicolors,
            defaults: self.defaults,
            term: self.term,
        }
    }

    /// Resolved style for client id `id` (see [`Highlights`]); undefined ids
    /// get the default.
    pub fn style(&self, id: u32) -> Style {
        if id & SYNTAX != 0 && (id >> 16) & 0x7fff == 0 {
            return self.derived_style(id & 0xffff);
        }
        if id & SYNTAX != 0 {
            return self.syntax_style(id);
        }
        self.styles
            .get(id as usize)
            .copied()
            .unwrap_or_else(|| self.styles.first().copied().unwrap_or_default())
    }

    /// Style for a [`syntax_hl`] id; the base's if the group is unknown.
    fn syntax_style(&self, id: u32) -> Style {
        let (base, group) = (id & 0xffff, (id >> 16) & 0x7fff);
        let Some((rgb, cterm)) = self.highlights.groups.get(&group) else {
            return self.style(base);
        };
        let plain = (Attrs::default(), Attrs::default());
        let (under_rgb, under_cterm) = self.attrs(base).unwrap_or(&plain);
        self.colors()
            .style(&laid(under_rgb, rgb), &laid(under_cterm, cterm))
    }

    /// Id of client highlight `base` derived by `d`: `SYNTAX` plus an index
    /// below 2^16 with group 0, which [`syntax_hl`] never produces. `None`
    /// if `base` is not a Neovim highlight or [`DERIVED`] are already made.
    pub fn derived(&mut self, base: u32, d: Derive) -> Option<u32> {
        if base & SYNTAX != 0 {
            return None;
        }
        let h = &mut self.highlights;
        if let Some(&i) = h.derivations.get(&(base, d)) {
            return Some(SYNTAX | i);
        }
        let i = u32::try_from(h.derived.len())
            .ok()
            .filter(|&i| (i as usize) < DERIVED)?;
        h.derived.push((base, d));
        h.derivations.insert((base, d), i);
        Some(SYNTAX | i)
    }

    /// Whether [`Derive`] can model the bar: `CursorLine` is known (via
    /// `hl_group_set`) and sets only a background (one with a foreground is
    /// laid over cells instead), and fewer than [`DERIVED`] exist.
    pub fn bar_derivable(&self) -> bool {
        let Some(&id) = self.groups.get("CursorLine") else {
            return false;
        };
        let plain = |a: &Attrs| {
            *a == Attrs {
                bg: a.bg,
                blend: a.blend,
                ..Attrs::default()
            }
        };
        self.highlights.derived.len() < DERIVED
            && self
                .attrs(id)
                .is_none_or(|(rgb, cterm)| plain(rgb) && plain(cterm))
    }

    /// Style for derived index `i`; the default if never made.
    fn derived_style(&self, i: u32) -> Style {
        let Some(&(base, d)) = self.highlights.derived.get(i as usize) else {
            return self.style(0);
        };
        let plain = (Attrs::default(), Attrs::default());
        let (rgb, cterm) = self.attrs(base).unwrap_or(&plain);
        let bar = self.groups.get("CursorLine").and_then(|&g| self.attrs(g));
        let (bar_rgb, bar_cterm) = bar.unwrap_or(&plain);
        let made = |own: &Attrs, bar: &Attrs| match d {
            Derive::CursorLineOn => Attrs {
                bg: own.bg.or(bar.bg),
                ..own.clone()
            },
            Derive::CursorLineOff => Attrs {
                bg: own.bg.filter(|bg| bar.bg != Some(*bg)),
                ..own.clone()
            },
        };
        let style = self
            .colors()
            .style(&made(rgb, bar_rgb), &made(cterm, bar_cterm));
        Style {
            link: self.style(base).link,
            ..style
        }
    }

    /// Define editor highlight group `group` (see [`syntax_hl`]).
    pub fn set_group(&mut self, group: u32, rgb: Attrs, cterm: Attrs) {
        self.highlights.groups.insert(group, (rgb, cterm));
        self.changed = super::grid::tick();
    }

    pub fn set_laststatus(&mut self, laststatus: Option<i64>) {
        if laststatus != self.laststatus {
            self.laststatus = laststatus;
            self.changed = super::grid::tick();
        }
    }

    /// Replace `grid`'s row tails (see [`Model::tails`]); returns whether
    /// they changed.
    pub fn set_tails(&mut self, grid: u64, tails: BTreeMap<usize, Tail>) -> bool {
        if self
            .tails
            .get(&grid)
            .map_or(tails.is_empty(), |t| *t == tails)
        {
            return false;
        }
        if tails.is_empty() {
            self.tails.remove(&grid);
        } else {
            self.tails.insert(grid, tails);
        }
        self.changed = super::grid::tick();
        true
    }

    /// Drop `grid`'s tails, e.g. after its width changed.
    pub fn forget_tails(&mut self, grid: u64) {
        if self.tails.remove(&grid).is_some() {
            self.changed = super::grid::tick();
        }
    }

    /// `(rgb, cterm)` of client highlight `id`.
    pub fn attrs(&self, id: u32) -> Option<&(Attrs, Attrs)> {
        self.highlights.defs.get(id as usize)?.as_ref()
    }

    /// Link `n` is at index `n - 1`.
    pub fn links(&self) -> &[Box<str>] {
        &self.links
    }

    /// Number for a hyperlink, assigned on first sight. Control characters
    /// are stripped: inside OSC 8 they would end the sequence early.
    fn link_number(&mut self, url: &str) -> u32 {
        let url: Box<str> = url.chars().filter(|c| !c.is_control()).collect();
        if let Some(n) = self.link_numbers.get(&url) {
            return *n;
        }
        self.links.push(url.clone());
        let n = self.links.len() as u32;
        self.link_numbers.insert(url, n);
        n
    }

    /// Transparency of highlight `id`, 0 to 100.
    pub fn blend(&self, id: u32) -> u8 {
        self.blends.get(id as usize).copied().unwrap_or(0)
    }

    pub fn mode_info(&self) -> Option<&ModeInfo> {
        self.modes.get(self.mode)
    }

    /// Cursor screen position, ignoring animations. Clamped to its grid,
    /// which the client may have narrowed before Neovim did (see
    /// [`super::wide`]).
    pub fn cursor_on_screen(&self) -> (i64, i64) {
        let c = self.cursor;
        let (r, k) = self.drawn_origin(c.grid);
        let col = self
            .grids
            .get(&c.grid)
            .map_or(c.col, |g| c.col.min(g.width().saturating_sub(1)));
        (r + c.row as i64, k + col as i64)
    }

    /// Horizontal offset at which `grid` is drawn (see [`super::wide`]): a
    /// window grid narrower than its window is centred, and a float anchored
    /// to it shifts by as much, but not off the screen. Zero otherwise.
    pub fn inset(&self, grid: u64) -> usize {
        let window = |grid: u64| match self.layout.get(&grid).map(|p| &p.place) {
            Some(Place::Window { width, .. }) => self
                .grids
                .get(&grid)
                .map_or(0, |g| super::wide::inset(*width, g.width())),
            _ => 0,
        };
        match self.layout.get(&grid).map(|p| &p.place) {
            Some(Place::Window { .. }) => window(grid),
            // Neovim fitted the float to the screen relative to the
            // unshifted grid; shifting it fully would push floats near the
            // right edge (diagnostics, hover, completion) off screen.
            Some(Place::Float {
                anchor: Some(anchor),
                col,
                ..
            }) if *anchor != grid => {
                let screen = self.grids.get(&1).map_or(0, Grid::width) as i64;
                let width = self.grids.get(&grid).map_or(0, Grid::width) as i64;
                let room = usize::try_from(screen - col - width).unwrap_or(0);
                window(*anchor).min(room)
            }
            _ => 0,
        }
    }

    /// Layout origin plus [`Model::inset`].
    pub fn drawn_origin(&self, grid: u64) -> (i64, i64) {
        let (r, c) = self.layout.get(&grid).map_or((0, 0), Placement::origin);
        (r, c + self.inset(grid) as i64)
    }

    /// `(row, col, width, height)` of a layout window's screen area, at the
    /// window's width whatever its grid's; `None` if not a layout window.
    pub fn window_cells(&self, grid: u64) -> Option<(usize, usize, usize, usize)> {
        let Some(Place::Window {
            row,
            col,
            width,
            height,
        }) = self.layout.get(&grid).map(|p| &p.place)
        else {
            return None;
        };
        let g = self.grids.get(&grid)?;
        let pick = |own: usize, grid: usize| if own > 0 { own } else { grid };
        Some((
            *row,
            *col,
            pick(*width, g.width()),
            pick(*height, g.height()),
        ))
    }

    /// Apply one event, recording in `changes` what animations need.
    pub fn apply(&mut self, event: Event, changes: &mut Changes) {
        // Events that change only grid cells (which grids stamp themselves)
        // or nothing drawn. Anything else, including unlisted events, bumps
        // `changed`.
        let cells_alone = matches!(
            event,
            Event::GridLine { .. }
                | Event::GridScroll { .. }
                | Event::GridClear { .. }
                | Event::GridCursor { .. }
                | Event::WinViewport { .. }
                | Event::ModeChange { .. }
                | Event::BusyStart
                | Event::BusyStop
                | Event::MouseOn
                | Event::MouseOff
                | Event::Bell
                | Event::VisualBell
                | Event::SetTitle(_)
                | Event::UiSend(_)
                | Event::ErrorExit(_)
                | Event::Flush
        );
        if !cells_alone {
            self.changed = super::grid::tick();
        }
        match event {
            Event::GridResize {
                grid,
                width,
                height,
            } => {
                let g = self.grids.entry(grid).or_default();
                if (g.width(), g.height()) != (width, height) {
                    changes.reshaped.push(grid);
                }
                // Tails depend on the width; on a width change Neovim redraws
                // every row and the editor reports them again.
                if g.width() != width {
                    self.tails.remove(&grid);
                } else if let Some(tails) = self.tails.get_mut(&grid) {
                    tails.retain(|&r, _| r < height);
                }
                g.resize(width, height);
            }
            Event::GridClear { grid } => {
                if let Some(g) = self.grids.get_mut(&grid) {
                    g.clear();
                }
                self.tails.remove(&grid);
                if grid == 1 {
                    changes.repaint = true;
                }
            }
            Event::GridDestroy { grid } => {
                self.grids.remove(&grid);
                self.layout.remove(&grid);
                self.wins.remove(&grid);
                self.viewports.remove(&grid);
                self.margins.remove(&grid);
                self.tails.remove(&grid);
                changes.reshaped.push(grid);
            }
            Event::GridLine {
                grid,
                row,
                col,
                mut cells,
            } => {
                for run in &mut cells {
                    if let Some(hl) = &mut run.hl {
                        *hl = self.highlights.client(*hl);
                    }
                }
                if let Some(g) = self.grids.get_mut(&grid) {
                    g.put(row, col, &cells);
                }
            }
            Event::GridScroll {
                grid,
                top,
                bot,
                left,
                right,
                rows,
            } => {
                if let Some(g) = self.grids.get_mut(&grid) {
                    g.scroll(top, bot, left, right, rows);
                }
                self.scroll_tails(grid, (top, bot, left, right), rows);
                changes
                    .region_scrolls
                    .push((grid, top, bot, left, right, rows));
            }
            Event::GridCursor { grid, row, col } => {
                self.cursor = CursorPos { grid, row, col };
            }
            Event::DefaultColors(d) => {
                self.defaults = d;
                self.styles_stale = true;
            }
            Event::HlAttr { id, rgb, cterm } => self.highlights.define(id, (rgb, cterm)),
            Event::HlGroup { name, id } => {
                self.groups.insert(name, self.highlights.client(id));
            }
            Event::WinPos {
                grid,
                win,
                row,
                col,
                width,
                height,
            } => self.place(
                grid,
                win,
                Place::Window {
                    row,
                    col,
                    width,
                    height,
                },
            ),
            Event::WinFloatPos {
                grid,
                win,
                mouse,
                zindex,
                compindex,
                row,
                col,
                anchor,
            } => self.place(
                grid,
                win,
                Place::Float {
                    row,
                    col,
                    zindex,
                    compindex,
                    mouse,
                    anchor,
                },
            ),
            Event::MsgSetPos {
                grid,
                row,
                scrolled,
                sep,
                zindex,
                compindex,
            } => self.place(
                grid,
                None,
                Place::Message {
                    row,
                    scrolled,
                    sep,
                    zindex,
                    compindex,
                },
            ),
            // A grid hidden before it was placed (e.g. on another tab page)
            // is recorded hidden until Neovim places it.
            Event::WinHide { grid } => {
                self.layout
                    .entry(grid)
                    .or_insert(Placement {
                        place: Place::Window {
                            row: 0,
                            col: 0,
                            width: 0,
                            height: 0,
                        },
                        hidden: true,
                    })
                    .hidden = true;
            }
            Event::WinClose { grid } => {
                self.layout.remove(&grid);
                self.wins.remove(&grid);
                self.viewports.remove(&grid);
            }
            Event::WinViewport {
                grid,
                win,
                topline,
                botline,
                curline,
                curcol,
                line_count,
                scroll_delta,
                ..
            } => {
                if let Some(win) = win {
                    self.wins.insert(grid, win);
                }
                self.viewports.insert(
                    grid,
                    Viewport {
                        topline,
                        botline,
                        curline,
                        curcol,
                        line_count,
                    },
                );
                if scroll_delta != 0 {
                    *changes.scrolled.entry(grid).or_default() += scroll_delta;
                    if let Some(win) = win {
                        *changes.scrolled_win.entry(win).or_default() += scroll_delta;
                    }
                }
            }
            Event::WinViewportMargins {
                grid,
                top,
                bottom,
                left,
                right,
            } => {
                self.margins.insert(
                    grid,
                    Margins {
                        top,
                        bottom,
                        left,
                        right,
                    },
                );
            }
            Event::ModeInfoSet { enabled, mut modes } => {
                for m in &mut modes {
                    m.attr_id = self.highlights.client(m.attr_id);
                }
                self.cursor_styled = enabled;
                self.modes = modes;
            }
            Event::ModeChange { name, index } => {
                self.mode_name = name;
                self.mode = index;
            }
            Event::OptionSet { name, value } => self.set_option(&name, &value),
            Event::BusyStart => self.busy = true,
            Event::BusyStop => self.busy = false,
            Event::MouseOn => {
                self.mouse = true;
                changes.mouse = true;
            }
            Event::MouseOff => {
                self.mouse = false;
                changes.mouse = true;
            }
            Event::Bell => changes.bells += 1,
            Event::VisualBell => changes.visual_bell = true,
            Event::SetTitle(title) => {
                if title != self.title {
                    self.title = title;
                    changes.title = true;
                }
            }
            Event::UiSend(bytes) => changes.sent.push(bytes),
            Event::ErrorExit(status) => changes.exit = Some(status),
            Event::Flush => {}
        }
    }

    /// Move tails (see [`Model::tails`]) with a `grid_scroll` of `rect`.
    /// Tails move only with full-width scrolls; those scrolled out of the
    /// rectangle or on partly moved rows are dropped.
    fn scroll_tails(&mut self, grid: u64, rect: (usize, usize, usize, usize), rows: i64) {
        let (top, bot, left, right) = rect;
        let whole = left == 0 && self.grids.get(&grid).is_some_and(|g| right >= g.width());
        let Some(tails) = self.tails.get_mut(&grid) else {
            return;
        };
        *tails = std::mem::take(tails)
            .into_iter()
            .filter_map(|(r, tail)| {
                if !(top..bot).contains(&r) {
                    return Some((r, tail));
                }
                let to = r as i64 - rows;
                (whole && to >= top as i64 && to < bot as i64).then_some((to as usize, tail))
            })
            .collect();
    }

    /// Placing also unhides a grid after `win_hide`.
    fn place(&mut self, grid: u64, win: Option<i64>, place: Place) {
        self.layout.insert(
            grid,
            Placement {
                place,
                hidden: false,
            },
        );
        if let Some(win) = win {
            self.wins.insert(grid, win);
        }
    }

    pub fn margins(&self, grid: u64) -> Margins {
        self.margins.get(&grid).copied().unwrap_or_default()
    }

    /// `(grid, win)` of windows named but never placed: a UI attaching to a
    /// session another UI drew gets no placement for existing windows (see
    /// `super::App`).
    pub fn unplaced(&self) -> Vec<(u64, i64)> {
        self.wins
            .iter()
            .filter(|(grid, _)| !self.layout.contains_key(grid))
            .map(|(grid, win)| (*grid, *win))
            .collect()
    }

    /// Shown or named-but-unplaced window grids Neovim has not fully drawn
    /// since the client created them: kept from an earlier UI, which Neovim
    /// assumes this one has. See `super::App`.
    pub fn lacking(&self) -> Vec<u64> {
        let undrawn = |grid: &u64| !self.grids.get(grid).is_some_and(Grid::whole);
        let shown = self.layout.iter().filter(|(grid, p)| {
            **grid != 1 && !p.hidden && !matches!(p.place, Place::Message { .. })
        });
        let named = self
            .wins
            .keys()
            .filter(|grid| !self.layout.contains_key(grid));
        let mut out: Vec<u64> = shown
            .map(|(grid, _)| *grid)
            .chain(named.copied())
            .filter(undrawn)
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    fn set_option(&mut self, name: &str, value: &OptionValue) {
        let o = &mut self.options;
        match name {
            "termguicolors" => {
                if let Some(b) = value.as_bool() {
                    o.termguicolors = b;
                    self.styles_stale = true;
                }
            }
            "mousemoveevent" => o.mousemoveevent = value.as_bool().unwrap_or(false),
            "ttimeout" => o.ttimeout = value.as_bool().unwrap_or(true),
            "ttimeoutlen" => {
                o.ttimeoutlen = value
                    .as_int()
                    .and_then(|n| u64::try_from(n).ok())
                    .unwrap_or(50)
            }
            "termsync" => o.termsync = value.as_bool().unwrap_or(true),
            _ => {}
        }
    }

    /// Resolve new highlights, or all if colours or `'termguicolors'`
    /// changed; returns whether any were. Incremental because a truecolour
    /// `:terminal` defines thousands a frame (see [`Highlights`]).
    pub fn refresh_styles(&mut self) -> bool {
        let fresh = std::mem::take(&mut self.highlights.fresh);
        if !self.styles_stale && fresh.is_empty() {
            return false;
        }
        let colors = self.colors();
        let plain = colors.default_style();
        let n = self.highlights.defs.len().max(1);
        if self.styles_stale {
            self.styles_stale = false;
            self.styles.clear();
            self.blends.clear();
        }
        let resolved = self.styles.len();
        self.styles.resize(n, plain);
        self.blends.resize(n, 0);
        if resolved == 0 {
            for id in 1..n {
                self.resolve(id, &colors);
            }
        } else {
            for id in fresh {
                self.resolve(id as usize, &colors);
            }
        }
        true
    }

    /// Id 0 is always the default, whatever is sent for it.
    fn resolve(&mut self, id: usize, colors: &Colors) {
        let Some(Some((rgb, cterm))) = self.highlights.defs.get(id).filter(|_| id > 0) else {
            return;
        };
        let style = colors.style(rgb, cterm);
        let blend = if colors.rgb { rgb.blend } else { cterm.blend };
        // Hyperlinks come only in the GUI form but apply to both.
        let link = rgb.url.clone().map_or(0, |url| self.link_number(&url));
        self.styles[id] = Style { link, ..style };
        self.blends[id] = blend;
    }

    /// Whether a sweep is due (see [`Model::reclaim`]).
    pub fn reclaiming(&self) -> bool {
        let h = &self.highlights;
        h.retired.len() >= h.kept + RECLAIM_AT
    }

    /// Highlights the model itself holds (grids, tails, groups, mode
    /// cursors). Anything else keeping cells across a flush must add its own
    /// before [`Model::reclaim`].
    pub fn held(&self) -> Held {
        let mut held = Held(vec![0; self.highlights.defs.len().div_ceil(64)]);
        for g in self.grids.values() {
            for r in 0..g.height() {
                held.cells(g.row(r));
            }
        }
        for tail in self.tails.values().flat_map(BTreeMap::values) {
            held.cells(&tail.cells);
        }
        for hl in self.groups.values() {
            held.mark(*hl);
        }
        for mode in &self.modes {
            held.mark(mode.attr_id);
        }
        held
    }

    /// Free every retired id not in `held` (see [`Highlights`]).
    pub fn reclaim(&mut self, held: &Held) {
        let h = &mut self.highlights;
        let (kept, free): (Vec<u32>, Vec<u32>) = h.retired.iter().partition(|k| held.holds(**k));
        h.free.extend(free);
        h.kept = kept.len();
        h.retired = kept;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::grid::{tick, Text};
    use crate::client::redraw::LineCell;
    use crate::client::style::Color;
    use crate::palette::Rgb;

    fn model() -> Model {
        Model::new(Palette {
            fg: Rgb(255, 255, 255),
            bg: Rgb(0, 0, 0),
            ansi: [Rgb(0, 0, 0); 16],
        })
    }

    fn apply(m: &mut Model, events: Vec<Event>) -> Changes {
        let mut changes = Changes::default();
        for e in events {
            m.apply(e, &mut changes);
        }
        changes
    }

    /// Cell-only events leave `changed` alone; everything else, including
    /// unknown events, bumps it.
    #[test]
    fn only_a_grids_cells_change_without_changing_the_model() {
        let mut m = model();
        apply(
            &mut m,
            vec![Event::GridResize {
                grid: 1,
                width: 4,
                height: 2,
            }],
        );
        let cells_alone = [
            Event::GridLine {
                grid: 1,
                row: 0,
                col: 0,
                cells: vec![LineCell {
                    text: Text::Char('a'),
                    hl: Some(0),
                    repeat: 1,
                }],
            },
            Event::GridScroll {
                grid: 1,
                top: 0,
                bot: 2,
                left: 0,
                right: 4,
                rows: 1,
            },
            Event::GridClear { grid: 1 },
            Event::GridCursor {
                grid: 1,
                row: 1,
                col: 1,
            },
            Event::ModeChange {
                name: "insert".into(),
                index: 1,
            },
            Event::BusyStart,
            Event::Flush,
        ];
        for event in cells_alone {
            let since = tick();
            let name = format!("{event:?}");
            apply(&mut m, vec![event]);
            assert!(m.unchanged_since(since), "{name}");
        }
        let more = [
            Event::WinPos {
                grid: 2,
                win: Some(1000),
                row: 0,
                col: 0,
                width: 4,
                height: 1,
            },
            Event::HlAttr {
                id: 1,
                rgb: Attrs::default(),
                cterm: Attrs::default(),
            },
            Event::GridResize {
                grid: 1,
                width: 3,
                height: 2,
            },
            Event::OptionSet {
                name: "termguicolors".into(),
                value: OptionValue::Bool(true),
            },
        ];
        for event in more {
            let since = tick();
            let name = format!("{event:?}");
            apply(&mut m, vec![event]);
            assert!(!m.unchanged_since(since), "{name}");
        }
        let since = tick();
        m.set_laststatus(Some(2));
        assert!(!m.unchanged_since(since), "'laststatus'");
        let since = tick();
        m.set_laststatus(Some(2));
        assert!(m.unchanged_since(since), "'laststatus' as it was");
        let since = tick();
        m.set_group(7, Attrs::default(), Attrs::default());
        assert!(!m.unchanged_since(since), "a highlight group");
        let since = tick();
        m.grids.get_mut(&1).expect("grid 1").resize(2, 2);
        assert!(!m.unchanged_since(since), "a grid the client cut");
    }

    /// Values Neovim sends for a 22-row window over 30 lines.
    #[test]
    fn a_view_one_to_one_is_told_by_its_botline() {
        let v = |topline, botline| Viewport {
            topline,
            botline,
            curline: 0,
            curcol: 0,
            line_count: 30,
        };
        assert!(v(0, 23).one_to_one(22));
        assert!(v(8, 30).one_to_one(22), "ending on the last line exactly");
        assert!(v(9, 31).one_to_one(22), "a row past the end");
        assert!(v(29, 31).one_to_one(22), "the last line at the top");
        assert!(!v(0, 22).one_to_one(22), "a line taking two rows");
        assert!(!v(0, 25).one_to_one(22), "lines folded away");
        assert_eq!(v(9, 31).lines_shown(22), 21);
        assert_eq!(v(0, 23).lines_shown(22), 22);
    }

    #[test]
    fn the_cursor_is_where_its_grid_is() {
        let mut m = model();
        apply(
            &mut m,
            vec![
                Event::GridResize {
                    grid: 4,
                    width: 10,
                    height: 5,
                },
                Event::WinPos {
                    grid: 4,
                    win: Some(1001),
                    row: 1,
                    col: 31,
                    width: 10,
                    height: 5,
                },
                Event::GridCursor {
                    grid: 4,
                    row: 2,
                    col: 3,
                },
            ],
        );
        assert_eq!(m.cursor_on_screen(), (3, 34));
    }

    #[test]
    fn margins_and_hiding_survive_a_placement() {
        let mut m = model();
        apply(
            &mut m,
            vec![
                Event::WinViewportMargins {
                    grid: 2,
                    top: 1,
                    bottom: 0,
                    left: 0,
                    right: 0,
                },
                Event::WinPos {
                    grid: 2,
                    win: Some(1000),
                    row: 0,
                    col: 0,
                    width: 5,
                    height: 5,
                },
            ],
        );
        assert_eq!(m.margins(2).top, 1);
        assert!(!m.layout[&2].hidden);
        apply(&mut m, vec![Event::WinHide { grid: 2 }]);
        assert!(m.layout[&2].hidden);
        apply(
            &mut m,
            vec![Event::WinPos {
                grid: 2,
                win: Some(1000),
                row: 1,
                col: 0,
                width: 5,
                height: 5,
            }],
        );
        assert!(!m.layout[&2].hidden);
        assert_eq!(m.margins(2).top, 1);
    }

    fn cells(grid: u64, row: usize, col: usize, n: usize) -> Event {
        Event::GridLine {
            grid,
            row,
            col,
            cells: vec![LineCell {
                text: Text::Char('x'),
                hl: Some(0),
                repeat: n,
            }],
        }
    }

    /// A UI attaching to a session another UI drew gets window grids resized
    /// and named, but not placed or drawn. Drawing every cell, or hiding the
    /// grid, settles it.
    #[test]
    fn windows_never_placed_or_drawn_are_noticed() {
        let mut m = model();
        let viewport = |grid, win| Event::WinViewport {
            grid,
            win: Some(win),
            topline: 0,
            botline: 1,
            curline: 0,
            curcol: 0,
            line_count: 1,
            scroll_delta: 0,
        };
        apply(
            &mut m,
            vec![
                Event::GridResize {
                    grid: 1,
                    width: 20,
                    height: 5,
                },
                Event::GridResize {
                    grid: 2,
                    width: 10,
                    height: 4,
                },
                Event::GridResize {
                    grid: 4,
                    width: 9,
                    height: 4,
                },
                viewport(2, 1000),
                viewport(4, 1001),
            ],
        );
        assert_eq!(m.unplaced(), vec![(2, 1000), (4, 1001)]);
        assert_eq!(m.lacking(), vec![2, 4]);
        apply(
            &mut m,
            vec![
                cells(2, 0, 0, 1),
                Event::WinPos {
                    grid: 2,
                    win: Some(1000),
                    row: 0,
                    col: 0,
                    width: 10,
                    height: 4,
                },
                Event::WinHide { grid: 4 },
            ],
        );
        assert!(m.unplaced().is_empty());
        assert_eq!(m.lacking(), vec![2], "a cell drawn of forty");
        apply(&mut m, (0..4).map(|r| cells(2, r, 0, 10)).collect());
        assert!(m.lacking().is_empty(), "drawn whole, or hidden");
        // Shown again, never drawn on: lacking again.
        apply(
            &mut m,
            vec![Event::WinPos {
                grid: 4,
                win: Some(1001),
                row: 0,
                col: 11,
                width: 9,
                height: 4,
            }],
        );
        assert_eq!(m.lacking(), vec![4]);
        // A window on a tab page the client never saw has no grid at all.
        apply(
            &mut m,
            vec![Event::WinPos {
                grid: 7,
                win: Some(1004),
                row: 0,
                col: 0,
                width: 20,
                height: 4,
            }],
        );
        assert_eq!(m.lacking(), vec![4, 7]);
    }

    /// Attaching after a UI on a smaller terminal: Neovim draws only what the
    /// resize added (below or right of the kept cells), so each grid stays
    /// lacking until fully drawn.
    #[test]
    fn windows_drawn_only_where_a_resize_added_are_noticed() {
        let mut m = model();
        let place = |grid, win, col, width| Event::WinPos {
            grid,
            win: Some(win),
            row: 0,
            col,
            width,
            height: 6,
        };
        let resize = |grid, width| Event::GridResize {
            grid,
            width,
            height: 6,
        };
        let mut events = vec![
            Event::GridResize {
                grid: 1,
                width: 30,
                height: 8,
            },
            resize(2, 15),
            place(2, 1000, 0, 15),
            cells(2, 4, 0, 15),
            cells(2, 5, 0, 15),
            resize(4, 14),
            place(4, 1001, 16, 14),
        ];
        events.extend((0..6).map(|r| cells(4, r, 10, 4)));
        apply(&mut m, events);
        assert!(m.unplaced().is_empty(), "both placed");
        assert_eq!(m.lacking(), vec![2, 4]);
        apply(&mut m, (0..4).map(|r| cells(2, r, 0, 15)).collect());
        assert_eq!(m.lacking(), vec![4], "the taller drawn whole");
        apply(&mut m, (0..6).map(|r| cells(4, r, 0, 10)).collect());
        assert!(m.lacking().is_empty(), "and the wider");
    }

    #[test]
    fn a_batch_adds_up_its_scrolls() {
        let mut m = model();
        let viewport = |grid, delta| Event::WinViewport {
            grid,
            win: Some(1000 + grid as i64),
            topline: 0,
            botline: 0,
            curline: 0,
            curcol: 0,
            line_count: 0,
            scroll_delta: delta,
        };
        let changes = apply(
            &mut m,
            vec![
                viewport(2, 3),
                viewport(2, 4),
                viewport(4, -1),
                viewport(5, 0),
                Event::GridClear { grid: 2 },
            ],
        );
        assert_eq!(changes.scrolled.get(&2), Some(&7));
        assert_eq!(changes.scrolled.get(&4), Some(&-1));
        assert!(!changes.scrolled.contains_key(&5));
        assert_eq!(changes.scrolled_win.get(&1002), Some(&7));
        assert!(!changes.repaint);
        let changes = apply(&mut m, vec![Event::GridClear { grid: 1 }]);
        assert!(changes.repaint);
    }

    /// Tails move with full-width scrolls. They are dropped for rows scrolled
    /// out or partly moved, and all of them on a width change or clear; a
    /// shorter grid keeps the remaining rows'. Each update bumps `changed`.
    #[test]
    fn what_runs_past_a_grids_edge_goes_with_its_row() {
        let mut m = model();
        let resize = |width, height| Event::GridResize {
            grid: 2,
            width,
            height,
        };
        let scroll = |top, bot, right, rows| Event::GridScroll {
            grid: 2,
            top,
            bot,
            left: 0,
            right,
            rows,
        };
        let tail = |row: usize| Tail {
            start: 10,
            cells: vec![
                Cell {
                    text: Text::Char('x'),
                    hl: row as u32,
                };
                3
            ],
            sure: true,
        };
        let rows = |m: &Model| -> Vec<(usize, u32)> {
            m.tails
                .get(&2)
                .map(|t| t.iter().map(|(r, tail)| (*r, tail.cells[0].hl)).collect())
                .unwrap_or_default()
        };
        apply(&mut m, vec![resize(10, 6)]);
        let since = tick();
        assert!(m.set_tails(2, BTreeMap::from([(1, tail(1)), (4, tail(4))])));
        assert!(!m.unchanged_since(since), "a change");
        assert!(!m.set_tails(2, BTreeMap::from([(1, tail(1)), (4, tail(4))])));

        apply(&mut m, vec![scroll(0, 6, 10, 1)]);
        assert_eq!(rows(&m), [(0, 1), (3, 4)], "up a row with theirs");
        apply(&mut m, vec![scroll(0, 6, 10, 1)]);
        assert_eq!(rows(&m), [(2, 4)], "the top one out");
        apply(&mut m, vec![scroll(1, 4, 10, -1)]);
        assert_eq!(rows(&m), [(3, 4)], "down, in a rectangle");
        apply(&mut m, vec![scroll(0, 6, 6, 1)]);
        assert_eq!(rows(&m), [], "moved in part");

        m.set_tails(2, BTreeMap::from([(1, tail(1)), (4, tail(4))]));
        apply(&mut m, vec![resize(10, 3)]);
        assert_eq!(rows(&m), [(1, 1)], "shorter");
        apply(&mut m, vec![resize(12, 3)]);
        assert_eq!(rows(&m), [], "wider");
        m.set_tails(2, BTreeMap::from([(1, tail(1))]));
        apply(&mut m, vec![Event::GridClear { grid: 2 }]);
        assert_eq!(rows(&m), [], "cleared");
        m.set_tails(2, BTreeMap::from([(1, tail(1))]));
        apply(&mut m, vec![Event::GridDestroy { grid: 2 }]);
        assert!(m.tails.is_empty(), "gone");
    }

    #[test]
    fn a_group_is_laid_over_the_text_it_colours() {
        use crate::client::style::Color;
        let mut m = model();
        apply(
            &mut m,
            vec![Event::HlAttr {
                id: 3,
                rgb: Attrs {
                    fg: Some(0x010101),
                    bg: Some(0x020202),
                    italic: true,
                    ..Attrs::default()
                },
                cterm: Attrs {
                    fg: Some(1),
                    bg: Some(2),
                    ..Attrs::default()
                },
            }],
        );
        m.refresh_styles();
        let id = syntax_hl(3, 400);
        assert_eq!(m.style(id), m.style(3), "an unknown group");
        m.set_group(
            400,
            Attrs {
                fg: Some(0x0a0b0c),
                bold: true,
                ..Attrs::default()
            },
            Attrs {
                fg: Some(9),
                ..Attrs::default()
            },
        );
        assert_eq!(m.style(id).fg, Color::Index(9));
        assert_eq!(m.style(id).bg, Color::Index(2));
        apply(
            &mut m,
            vec![Event::OptionSet {
                name: "termguicolors".into(),
                value: OptionValue::Bool(true),
            }],
        );
        m.refresh_styles();
        let s = m.style(id);
        assert_eq!(s.fg, Color::Rgb(Rgb(10, 11, 12)));
        assert_eq!(s.bg, Color::Rgb(Rgb(2, 2, 2)));
        assert!(s.bold && s.italic);
        assert_eq!(syntax_hl(1 << 20, 400), 1 << 20, "too big to fit");
        assert_eq!(syntax_hl(3, 0), 3, "no group");
    }

    #[test]
    fn cursorline_laid_on_and_taken_off() {
        use crate::client::style::Color;
        let mut m = model();
        let def = |id: u32, fg: Option<u32>, bg: Option<u32>| Event::HlAttr {
            id,
            rgb: Attrs {
                fg,
                bg,
                ..Attrs::default()
            },
            cterm: Attrs {
                fg: fg.map(|_| 1),
                bg: bg.map(|b| b & 0xff),
                ..Attrs::default()
            },
        };
        apply(
            &mut m,
            vec![
                def(3, Some(0x0a0b0c), None),
                def(4, Some(0x0a0b0c), Some(0x303030)),
                def(5, Some(0x0a0b0c), Some(0x550000)),
                def(6, None, Some(0x303030)),
                Event::HlGroup {
                    name: "CursorLine".into(),
                    id: 6,
                },
                Event::OptionSet {
                    name: "termguicolors".into(),
                    value: OptionValue::Bool(true),
                },
            ],
        );
        m.refresh_styles();
        assert!(m.bar_derivable());
        let rgb = |r: u8, g: u8, b: u8| Color::Rgb(Rgb(r, g, b));
        let on = m.derived(3, Derive::CursorLineOn).expect("made");
        assert_eq!(
            (m.style(on).fg, m.style(on).bg),
            (rgb(10, 11, 12), rgb(0x30, 0x30, 0x30))
        );
        assert_eq!(m.derived(3, Derive::CursorLineOn), Some(on), "made once");
        let own = m.derived(5, Derive::CursorLineOn).expect("made");
        assert_eq!(m.style(own).bg, rgb(0x55, 0, 0), "a background of its own");
        let plain = m.derived(0, Derive::CursorLineOn).expect("made");
        assert_eq!(m.style(plain), m.style(6), "the bar alone");
        let off = m.derived(4, Derive::CursorLineOff).expect("made");
        assert_eq!(m.style(off), m.style(3));
        let kept = m.derived(5, Derive::CursorLineOff).expect("made");
        assert_eq!(m.style(kept), m.style(5));
        let bare = m.derived(6, Derive::CursorLineOff).expect("made");
        assert_eq!(m.style(bare), m.style(0));
        assert_eq!(m.derived(syntax_hl(3, 400), Derive::CursorLineOn), None);
        apply(
            &mut m,
            vec![Event::OptionSet {
                name: "termguicolors".into(),
                value: OptionValue::Bool(false),
            }],
        );
        m.refresh_styles();
        assert_eq!(m.style(on).bg, Color::Index(0x30));
        assert_eq!(m.style(off), m.style(3));
        // Neovim lays a bar with a foreground or attribute over the cell.
        apply(
            &mut m,
            vec![
                def(7, Some(0xffffff), Some(0x303030)),
                Event::HlGroup {
                    name: "CursorLine".into(),
                    id: 7,
                },
            ],
        );
        assert!(!m.bar_derivable());
    }

    #[test]
    fn highlights_resolve_in_the_form_the_option_says() {
        let mut m = model();
        apply(
            &mut m,
            vec![Event::HlAttr {
                id: 3,
                rgb: Attrs {
                    fg: Some(0x010203),
                    blend: 30,
                    ..Attrs::default()
                },
                cterm: Attrs {
                    fg: Some(9),
                    ..Attrs::default()
                },
            }],
        );
        assert!(m.refresh_styles());
        assert!(!m.refresh_styles(), "nothing changed since");
        use crate::client::style::Color;
        assert_eq!(m.style(3).fg, Color::Index(9));
        assert_eq!(m.blend(3), 0, "the cterm form carries no blend here");
        apply(
            &mut m,
            vec![Event::OptionSet {
                name: "termguicolors".into(),
                value: OptionValue::Bool(true),
            }],
        );
        assert!(m.refresh_styles());
        assert_eq!(m.style(3).fg, Color::Rgb(Rgb(1, 2, 3)));
        assert_eq!(m.blend(3), 30);
        assert_eq!(m.style(99), m.style(0), "an unknown id is the default");
    }

    #[test]
    fn hyperlinks_are_numbered_by_target() {
        let mut m = model();
        let link = |id: u32, url: &str| Event::HlAttr {
            id,
            rgb: Attrs {
                url: Some(url.into()),
                ..Attrs::default()
            },
            cterm: Attrs::default(),
        };
        apply(
            &mut m,
            vec![
                link(2, "https://neovim.io"),
                link(3, "https://example.com/\x1b]8;;\x07x"),
                link(4, "https://neovim.io"),
            ],
        );
        m.refresh_styles();
        assert_eq!(m.style(2).link, 1);
        assert_eq!(m.style(4).link, 1, "the same target, the same number");
        assert_eq!(m.style(3).link, 2);
        assert_eq!(m.style(1).link, 0);
        assert_eq!(&*m.links()[1], "https://example.com/]8;;x");
    }

    #[test]
    fn a_destroyed_grid_takes_its_placement_with_it() {
        let mut m = model();
        let changes = apply(
            &mut m,
            vec![
                Event::GridResize {
                    grid: 4,
                    width: 3,
                    height: 1,
                },
                Event::GridLine {
                    grid: 4,
                    row: 0,
                    col: 0,
                    cells: vec![LineCell {
                        text: Text::Char('x'),
                        hl: Some(1),
                        repeat: 3,
                    }],
                },
                Event::WinFloatPos {
                    grid: 4,
                    win: Some(-1),
                    mouse: false,
                    zindex: 100,
                    compindex: 1,
                    row: 2,
                    col: 0,
                    anchor: None,
                },
                Event::GridDestroy { grid: 4 },
            ],
        );
        assert!(!m.grids.contains_key(&4));
        assert!(!m.layout.contains_key(&4));
        assert!(changes.reshaped.contains(&4));
    }

    /// Neovim highlight `id` with foreground `0xRRGGBB`.
    fn coloured(id: u32, fg: u32) -> Event {
        Event::HlAttr {
            id,
            rgb: Attrs {
                fg: Some(fg),
                ..Attrs::default()
            },
            cterm: Attrs::default(),
        }
    }

    /// All three cells of grid 1's row `row` in Neovim highlight `hl`.
    fn row_in(row: usize, hl: u32) -> Event {
        Event::GridLine {
            grid: 1,
            row,
            col: 0,
            cells: vec![LineCell {
                text: Text::Char('x'),
                hl: Some(hl),
                repeat: 3,
            }],
        }
    }

    /// RGB model with a 3x2 grid 1.
    fn in_rgb() -> Model {
        let mut m = model();
        apply(
            &mut m,
            vec![
                Event::OptionSet {
                    name: "termguicolors".into(),
                    value: OptionValue::Bool(true),
                },
                Event::GridResize {
                    grid: 1,
                    width: 3,
                    height: 2,
                },
            ],
        );
        m
    }

    fn fgs(m: &Model, cells: &[Cell]) -> Vec<Color> {
        cells.iter().map(|c| m.style(c.hl).fg).collect()
    }

    fn fg(n: u32) -> Color {
        Color::Rgb(crate::client::style::rgb(n))
    }

    /// Neovim reissues ids mid-frame after its table fills. Cells drawn
    /// before keep their old colours, as do copies an animation keeps.
    #[test]
    fn a_cell_keeps_the_colours_its_highlight_had_when_it_was_drawn() {
        let (red, blue, green) = (fg(0xff0000), fg(0x0000ff), fg(0x00ff00));
        let mut m = in_rgb();
        apply(
            &mut m,
            vec![
                coloured(5, 0xff0000),
                row_in(0, 5),
                coloured(5, 0x0000ff),
                row_in(1, 5),
            ],
        );
        m.refresh_styles();
        assert_eq!(fgs(&m, m.grids[&1].row(0)), vec![red; 3], "drawn before");
        assert_eq!(fgs(&m, m.grids[&1].row(1)), vec![blue; 3], "drawn after");
        let kept = m.grids[&1].lines(0, 1, 0, 3);
        apply(&mut m, vec![coloured(5, 0x00ff00), row_in(0, 5)]);
        m.refresh_styles();
        assert_eq!(fgs(&m, &kept[0]), vec![red; 3], "kept of a moment ago");
        assert_eq!(fgs(&m, m.grids[&1].row(0)), vec![green; 3]);
        assert_eq!(fgs(&m, m.grids[&1].row(1)), vec![blue; 3]);
    }

    /// An unchanged resend (as on attach) keeps its id and retires none.
    #[test]
    fn a_highlight_sent_again_as_it_was_keeps_its_id() {
        let mut m = in_rgb();
        apply(
            &mut m,
            vec![
                coloured(5, 0xff0000),
                row_in(0, 5),
                coloured(5, 0xff0000),
                row_in(1, 5),
            ],
        );
        let g = &m.grids[&1];
        assert!(g.row(0).iter().chain(g.row(1)).all(|c| c.hl == 5));
        assert!(m.highlights.retired.is_empty());
    }

    #[test]
    fn groups_and_cursors_are_in_the_highlight_last_defined() {
        let mut m = in_rgb();
        apply(
            &mut m,
            vec![
                coloured(5, 0xff0000),
                row_in(0, 5),
                coloured(5, 0x0000ff),
                Event::HlGroup {
                    name: "EndOfBuffer".into(),
                    id: 5,
                },
                Event::ModeInfoSet {
                    enabled: true,
                    modes: vec![ModeInfo {
                        attr_id: 5,
                        ..ModeInfo::default()
                    }],
                },
            ],
        );
        m.refresh_styles();
        assert_eq!(m.style(m.groups["EndOfBuffer"]).fg, fg(0x0000ff));
        assert_eq!(m.style(m.modes[0].attr_id).fg, fg(0x0000ff));
        assert_eq!(m.style(m.grids[&1].row(0)[0].hl).fg, fg(0xff0000));
    }

    /// Ids stay bounded under endless table refills: a reissued id is freed
    /// once nothing holds it, and kept while cells (or animation copies) do.
    #[test]
    fn an_id_is_given_out_again_once_nothing_holds_it() {
        // Neovim's table refilled: ids 1 to 4 in the `n`th set of colours.
        fn refill(n: u32) -> Vec<Event> {
            (1..=4).map(|id| coloured(id, n << 8 | id)).collect()
        }
        let mut m = in_rgb();
        for n in 0..100 {
            let mut events = refill(n);
            events.extend([row_in(0, 1), row_in(1, 4)]);
            apply(&mut m, events);
            let held = m.held();
            m.reclaim(&held);
        }
        m.refresh_styles();
        assert_eq!(fgs(&m, m.grids[&1].row(0)), vec![fg(99 << 8 | 1); 3]);
        assert_eq!(fgs(&m, m.grids[&1].row(1)), vec![fg(99 << 8 | 4); 3]);
        assert!(
            m.highlights.defs.len() <= 9,
            "{} ids for a table of 4",
            m.highlights.defs.len()
        );

        // Reissued twice while an animation keeps the old cells: they keep
        // their colours.
        let kept = m.grids[&1].lines(0, 2, 0, 3);
        let shown: Vec<Vec<Color>> = kept.iter().map(|row| fgs(&m, row)).collect();
        apply(&mut m, refill(200));
        let held = m.held();
        m.reclaim(&held);
        let mut events = refill(201);
        events.extend([row_in(0, 1), row_in(1, 4)]);
        apply(&mut m, events);
        let mut held = m.held();
        for row in &kept {
            held.cells(row);
        }
        m.reclaim(&held);
        apply(&mut m, refill(202));
        m.refresh_styles();
        let now: Vec<Vec<Color>> = kept.iter().map(|row| fgs(&m, row)).collect();
        assert_eq!(now, shown);
    }

    /// A sweep waits for [`RECLAIM_AT`] retirements beyond those last found
    /// held; a group laid over a highlight holds it.
    #[test]
    fn a_sweep_waits_for_enough_ids_to_have_been_given_out_again() {
        let mut m = in_rgb();
        let n = RECLAIM_AT as u32;
        apply(&mut m, (1..=n).map(|id| coloured(id, 1)).collect());
        apply(&mut m, vec![row_in(0, 1)]);
        apply(&mut m, (1..n).map(|id| coloured(id, 2)).collect());
        assert!(!m.reclaiming(), "one short");
        apply(&mut m, vec![coloured(n, 2)]);
        assert!(m.reclaiming());
        let mut held = m.held();
        held.mark(syntax_hl(2, 40));
        m.reclaim(&held);
        assert_eq!(
            m.highlights.retired,
            vec![1, 2],
            "held by a cell and by a group laid over it"
        );
        assert!(!m.reclaiming());
        apply(&mut m, (1..n).map(|id| coloured(id, 3)).collect());
        assert!(!m.reclaiming(), "the two still held do not count");
        apply(&mut m, vec![coloured(n, 3)]);
        assert!(m.reclaiming());
    }

    #[test]
    fn highlights_resolved_as_they_come_are_as_resolved_all_at_once() {
        let mut m = in_rgb();
        let def = |id: u32, n: u32| Event::HlAttr {
            id,
            rgb: Attrs {
                fg: Some(n),
                blend: (n % 100) as u8,
                url: n
                    .is_multiple_of(3)
                    .then(|| format!("https://example.com/{}", n % 7).into()),
                ..Attrs::default()
            },
            cterm: Attrs {
                fg: Some(n % 256),
                ..Attrs::default()
            },
        };
        for round in 0..5 {
            apply(
                &mut m,
                (1..=40).map(|id| def(id, round * 100 + id)).collect(),
            );
            apply(&mut m, vec![row_in(0, 1 + round)]);
            m.refresh_styles();
        }
        let (styles, blends) = (m.styles.clone(), m.blends.clone());
        m.styles_stale = true;
        m.refresh_styles();
        assert_eq!(m.styles, styles);
        assert_eq!(m.blends, blends);
    }
}
