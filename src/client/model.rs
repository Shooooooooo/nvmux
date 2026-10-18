//! The client's picture of the editor: every grid and highlight, where each
//! window is, the cursor, the mode and the handful of options that change how
//! the client draws.
//!
//! Changed a batch at a time. Neovim's events between two `flush`es describe a
//! screen part way through being redrawn, so `super::App` keeps them until the
//! `flush` and applies them all at once — and the frames an animation draws in
//! between come from a picture that is always one Neovim finished.
//!
//! Where a change matters to an animation — a window moved, a cursor jumped, a
//! region scrolled — [`Model::apply`] says so in a [`Changes`], and
//! [`super::anim`] decides what to make of it.

use std::collections::{BTreeMap, HashMap};

use super::grid::{Cell, Grid};
use super::redraw::{Event, ModeInfo, OptionValue};
use super::style::{Attrs, Colors, DefaultColors, Style};
use crate::palette::Palette;

/// Where a grid goes on the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// A window in the editor's layout, its top left at `row`, `col`, and
    /// `width` by `height` cells of the screen its own: its grid's size, but
    /// where the client asked for a narrower grid, to centre the window's
    /// text in it (see [`super::wide`]). Nought for a window placed before
    /// its size was known.
    Window {
        row: usize,
        col: usize,
        width: usize,
        height: usize,
    },
    /// A float, over everything in the layout.
    Float {
        row: i64,
        col: i64,
        zindex: i64,
        compindex: i64,
        /// Whether the mouse can land on it.
        mouse: bool,
        /// The grid it is anchored to, where Neovim said: one anchored to a
        /// window goes where that window's text is drawn.
        anchor: Option<u64>,
    },
    /// The message area: the full width, from `row` down. `scrolled` is
    /// messages having pushed it up over the windows, and then the row above
    /// it is a separator drawn in `sep`.
    Message {
        row: usize,
        scrolled: bool,
        sep: String,
        zindex: i64,
        compindex: i64,
    },
}

/// The rows and columns of a window's grid that are not its text: a winbar,
/// a float's border.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Margins {
    pub top: usize,
    pub bottom: usize,
    pub left: usize,
    pub right: usize,
}

/// A grid that has been given somewhere to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub place: Place,
    pub hidden: bool,
}

impl Placement {
    /// Where its top left is on the screen.
    pub fn origin(&self) -> (i64, i64) {
        match &self.place {
            Place::Window { row, col, .. } => (*row as i64, *col as i64),
            Place::Float { row, col, .. } => (*row, *col),
            Place::Message { row, .. } => (*row as i64, 0),
        }
    }

    /// Whether it is drawn over the layout rather than in it, and in what
    /// order: zindex, then the order Neovim composed it in.
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

/// Where the cursor is: on which grid, and where on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorPos {
    pub grid: u64,
    pub row: usize,
    pub col: usize,
}

/// What part of its buffer a window shows, as `win_viewport` last said: all
/// of it zero-based, `botline` as Neovim counts it (see
/// [`Viewport::one_to_one`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Viewport {
    pub topline: i64,
    pub botline: i64,
    pub curline: i64,
    /// The cursor's column, in bytes.
    pub curcol: i64,
    pub line_count: i64,
}

impl Viewport {
    /// Whether the window's `rows` rows show its buffer's lines one to a row
    /// — nothing folded, wrapped, or put between them — as far as its view
    /// says. A window showing them so has its `botline` one past its last
    /// line, or `line_count` when it ends on the buffer's last line exactly,
    /// or one past `line_count` when there are rows past the end.
    pub fn one_to_one(&self, rows: usize) -> bool {
        let end = self.topline + rows as i64;
        let expected = match end.cmp(&self.line_count) {
            std::cmp::Ordering::Less => end + 1,
            std::cmp::Ordering::Equal => self.line_count,
            std::cmp::Ordering::Greater => self.line_count + 1,
        };
        self.topline >= 0 && self.botline == expected
    }

    /// How many of the window's `rows` rows show the buffer's lines, and not
    /// rows past its end — of a view that is one to one.
    pub fn lines_shown(&self, rows: usize) -> usize {
        (self.line_count - self.topline).clamp(0, rows as i64) as usize
    }
}

/// The options Neovim tells a UI about that change how this one draws.
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

/// What a batch changed that something other than the next frame cares about.
#[derive(Debug, Default)]
pub struct Changes {
    /// Grid 1 was cleared: the terminal may have been drawn on behind the
    /// client's back (`:mode`, which nvmux asks for to repaint), so the next
    /// frame paints every cell rather than the ones that changed.
    pub repaint: bool,
    /// Grids whose size changed, or that went: an animation of one is over.
    pub reshaped: Vec<u64>,
    /// How far each window scrolled, by grid, as `win_viewport` counts it.
    pub scrolled: HashMap<u64, i64>,
    /// The same by window handle, which is all a `win_viewport` names without
    /// `ext_multigrid`.
    pub scrolled_win: HashMap<i64, i64>,
    /// Every `grid_scroll`, as it came: grid, top, bot, left, right, rows.
    pub region_scrolls: Vec<(u64, usize, usize, usize, usize, i64)>,
    /// The bell, as many times as it rang.
    pub bells: usize,
    pub visual_bell: bool,
    /// Bytes for the terminal (`ui_send`), in order.
    pub sent: Vec<Vec<u8>>,
    /// The window title changed.
    pub title: bool,
    /// The mouse was turned on or off.
    pub mouse: bool,
    /// The server is going, with this status.
    pub exit: Option<i64>,
}

/// What [`Highlights::ids`] holds for one of Neovim's ids it has yet to
/// define.
const UNDEFINED: u32 = u32::MAX;

/// How many of the client's ids are retired (see [`Highlights`]) past those
/// the last sweep found still held before the next sweep is made. A sweep
/// reads every cell the client keeps, and a `:terminal` playing an animation
/// in truecolour has Neovim give out thousands of ids again a frame.
const RECLAIM_AT: usize = 4096;

/// The highlights Neovim has defined, under the ids the client's cells hold
/// them by.
///
/// That is Neovim's own id for each, until Neovim gives the id to another
/// highlight. It keeps 65 535 of them, and a `:terminal` drawing in truecolour
/// fills that fast — every pair of colours its cells are in is a highlight of
/// its own, thousands a frame for an animation — after which it clears the
/// table and gives the same ids out again: part way through drawing a frame as
/// often as not, the cells it drew before that still naming them until it
/// draws them again, in a later flush. Neovim's own TUI writes a cell's
/// colours out as the cell comes, so what it has written keeps them. This
/// client resolves its cells once a batch is whole (see `super::App`), and
/// keeps them besides in what its animations show of a moment ago: so an id
/// Neovim defines anew is given another of the client's own, and a cell drawn
/// before keeps the id it had, and what that meant. Left to mean the new
/// highlight, every such cell took its colours for a flush — a flash of wrong
/// colours over a truecolour animation in a `:terminal`, several times a
/// second.
///
/// The client's id a highlight had before is retired, and given out again
/// once nothing holds it: see [`Model::reclaim`].
#[derive(Debug, Default)]
struct Highlights {
    /// Every highlight by the client's id for it; none for an id never used.
    defs: Vec<Option<(Attrs, Attrs)>>,
    /// The client's id for each of Neovim's, by Neovim's: its own until
    /// Neovim defines it anew, and [`UNDEFINED`] until it defines it at all.
    ids: Vec<u32>,
    /// Retired: the client's ids for highlights whose ids Neovim has since
    /// given to others, which cells drawn before may still hold.
    retired: Vec<u32>,
    /// How many of those the last sweep found still held.
    kept: usize,
    /// Ids nothing holds, to be given out again.
    free: Vec<u32>,
    /// Ids defined since the styles were last resolved.
    fresh: Vec<u32>,
    /// The editor's highlight groups the client colours its own rows with,
    /// by group id, as the agent sends them: see [`syntax_hl`].
    groups: HashMap<u32, (Attrs, Attrs)>,
}

impl Highlights {
    /// The client's id for Neovim's highlight `id`. One never defined is its
    /// own, as a cell in it is drawn: in the default colours.
    fn client(&self, id: u32) -> u32 {
        match self.ids.get(id as usize) {
            Some(&k) if k != UNDEFINED => k,
            _ => id,
        }
    }

    /// Neovim defines its highlight `id` as `def`, for the first time or
    /// anew.
    fn define(&mut self, id: u32, def: (Attrs, Attrs)) {
        let i = id as usize;
        if self.ids.len() <= i {
            self.ids.resize(i + 1, UNDEFINED);
        }
        let k = match self.ids[i] {
            // The default, whatever is sent for it: not a cell's to keep.
            _ if id == 0 => 0,
            UNDEFINED if self.unused(id) => id,
            UNDEFINED => self.take(),
            // Sent again as it was: a UI attaching is sent every highlight.
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

    /// Whether the client has never used id `k`, which Neovim's id of the
    /// same number may then have.
    fn unused(&self, k: u32) -> bool {
        self.defs.get(k as usize).is_none_or(Option::is_none)
    }

    /// An id for a highlight Neovim defines anew: one nothing holds any
    /// more, or one past every id used.
    fn take(&mut self) -> u32 {
        self.free.pop().unwrap_or_else(|| {
            self.defs.push(None);
            (self.defs.len() - 1) as u32
        })
    }
}

/// The highlight ids something holds: a cell, a highlight group, a mode's
/// cursor — what a sweep may not give out again (see [`Model::reclaim`]).
#[derive(Debug)]
pub struct Held(Vec<u64>);

impl Held {
    /// Highlight `hl` is held — the one under it, for a group laid over one
    /// (see [`syntax_hl`]).
    pub fn mark(&mut self, hl: u32) {
        let hl = if hl & SYNTAX != 0 { hl & 0xffff } else { hl };
        if let Some(word) = self.0.get_mut(hl as usize / 64) {
            *word |= 1 << (hl % 64);
        }
    }

    /// Every one of `cells`' highlights is held.
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

/// Set in a cell's highlight id that is not Neovim's: a highlight group of
/// the editor's laid over one of Neovim's, as the client draws the rows it
/// draws itself (see `super::predict`) in the colours the editor's syntax
/// highlighting gives them.
const SYNTAX: u32 = 1 << 31;

/// The highlight id of group `group` laid over Neovim's highlight `base`, as
/// Neovim lays a syntax group over a window's text: what the group sets, and
/// what it does not from under it. Ids too big to fit are left as `base`.
pub fn syntax_hl(base: u32, group: u32) -> u32 {
    if base > 0xffff || group == 0 || group > 0x7fff {
        return base;
    }
    SYNTAX | group << 16 | base
}

/// `over` laid over `under`: its colours where it has them, and every
/// attribute either has.
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

/// The editor, as the client knows it.
#[derive(Debug)]
pub struct Model {
    pub grids: BTreeMap<u64, Grid>,
    pub layout: BTreeMap<u64, Placement>,
    /// The window each window's grid shows, as `win_viewport` and the
    /// placements name it.
    pub wins: BTreeMap<u64, i64>,
    /// What each window shows of its buffer, by grid.
    pub viewports: HashMap<u64, Viewport>,
    /// The rows and columns of a window's grid that are not its text, by
    /// grid. Their own map rather than part of a placement: Neovim sends them
    /// before it places a window, and not again when it places it anew.
    pub margins: HashMap<u64, Margins>,
    pub cursor: CursorPos,
    /// `mode_info_set`'s modes, each cursor's highlight by the client's id
    /// for it.
    pub modes: Vec<ModeInfo>,
    pub mode: usize,
    pub mode_name: String,
    /// `mode_info_set`'s `enabled`: whether the cursor's shape is the client's
    /// to set at all.
    pub cursor_styled: bool,
    pub options: Options,
    pub busy: bool,
    pub mouse: bool,
    pub title: String,
    pub defaults: DefaultColors,
    /// `hl_group_set`: the highlights of the groups the UI draws itself, by
    /// the client's ids for them (see [`Highlights`]).
    pub groups: HashMap<String, u32>,
    /// `'laststatus'`, as the editor's measure says it (see
    /// [`super::wide::MEASURE_LUA`]): which windows have a status line of
    /// their own. `None` until it has said.
    pub laststatus: Option<i64>,
    highlights: Highlights,
    /// Every highlight resolved for drawing, by id, and its blend: each as it
    /// is defined, and all of them again when the defaults or
    /// `'termguicolors'` change (see [`Model::colors`]).
    styles: Vec<Style>,
    blends: Vec<u8>,
    /// The defaults or `'termguicolors'` have changed since.
    styles_stale: bool,
    term: Palette,
    /// Every hyperlink a highlight has named, in the order they came: link
    /// `n` (see [`Style::link`]) is `links[n - 1]`.
    links: Vec<Box<str>>,
    link_numbers: HashMap<Box<str>, u32>,
}

impl Model {
    /// An empty editor, with the terminal's own colours to fall back on.
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
            highlights: Highlights::default(),
            styles: Vec::new(),
            blends: Vec::new(),
            styles_stale: true,
            term,
            links: Vec::new(),
            link_numbers: HashMap::new(),
        }
    }

    /// What resolves a style's colours, as things stand.
    pub fn colors(&self) -> Colors {
        Colors {
            rgb: self.options.termguicolors,
            defaults: self.defaults,
            term: self.term,
        }
    }

    /// Highlight `id` resolved for drawing, by the client's id for it, as a
    /// cell holds it (see [`Highlights`]); an id never defined is the
    /// default.
    pub fn style(&self, id: u32) -> Style {
        if id & SYNTAX != 0 {
            return self.syntax_style(id);
        }
        self.styles
            .get(id as usize)
            .copied()
            .unwrap_or_else(|| self.styles.first().copied().unwrap_or_default())
    }

    /// A highlight group laid over one of Neovim's (see [`syntax_hl`]),
    /// resolved for drawing: as the one under it where the group is not
    /// known.
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

    /// The editor's highlight group `group` has these attributes, in both
    /// forms: see [`syntax_hl`].
    pub fn set_group(&mut self, group: u32, rgb: Attrs, cterm: Attrs) {
        self.highlights.groups.insert(group, (rgb, cterm));
    }

    /// Highlight `id` as Neovim defined it, in both its forms, by the
    /// client's id for it.
    pub fn attrs(&self, id: u32) -> Option<&(Attrs, Attrs)> {
        self.highlights.defs.get(id as usize)?.as_ref()
    }

    /// The hyperlinks the styles name, link `n` at `n - 1`.
    pub fn links(&self) -> &[Box<str>] {
        &self.links
    }

    /// A hyperlink's number, given it the first time it is seen. Anything in
    /// it that is not text is left out: it is written inside an OSC 8, and a
    /// control character there would end the sequence early and write the
    /// rest to the screen as something else.
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

    /// How much highlight `id` lets through of what is under it, 0 to 100.
    pub fn blend(&self, id: u32) -> u8 {
        self.blends.get(id as usize).copied().unwrap_or(0)
    }

    /// The mode the editor is in, as `'guicursor'` describes its cursor.
    pub fn mode_info(&self) -> Option<&ModeInfo> {
        self.modes.get(self.mode)
    }

    /// Where the cursor is on the screen, as the editor has laid it out — no
    /// animation of a window's moving counted. No further right than its
    /// grid: one the client has made narrower before Neovim has (see
    /// [`super::wide`]) may have it past that until Neovim moves it.
    pub fn cursor_on_screen(&self) -> (i64, i64) {
        let c = self.cursor;
        let (r, k) = self.drawn_origin(c.grid);
        let col = self
            .grids
            .get(&c.grid)
            .map_or(c.col, |g| c.col.min(g.width().saturating_sub(1)));
        (r + c.row as i64, k + col as i64)
    }

    /// How far in from where the editor put it grid `grid` is drawn: a
    /// window's grid narrower than the window in the middle of it, and a
    /// float anchored to that window as far in as its text (see
    /// [`super::wide`]). Nought for anything else.
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
            Some(Place::Float {
                anchor: Some(anchor),
                ..
            }) if *anchor != grid => window(*anchor),
            _ => 0,
        }
    }

    /// Where grid `grid`'s top left is drawn: where the editor put it, and
    /// [`Model::inset`] in from there.
    pub fn drawn_origin(&self, grid: u64) -> (i64, i64) {
        let (r, c) = self.layout.get(&grid).map_or((0, 0), Placement::origin);
        (r, c + self.inset(grid) as i64)
    }

    /// The cells window grid `grid` has of the screen — its top row, its
    /// first column, and how many of each — as wide as Neovim made the
    /// window, whatever its grid; `None` for a grid that is no window in the
    /// layout.
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

    /// Take one event into the picture, noting in `changes` what an animation
    /// might want to know.
    pub fn apply(&mut self, event: Event, changes: &mut Changes) {
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
                g.resize(width, height);
            }
            Event::GridClear { grid } => {
                if let Some(g) = self.grids.get_mut(&grid) {
                    g.clear();
                }
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
            // A grid hidden before the client knew where it went — one the
            // layout was asked about, on another tab page — is placed hidden:
            // known, and not drawn until Neovim places it.
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

    /// Give a grid somewhere to be. Being placed is what makes it seen again
    /// after a `win_hide`.
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

    /// A grid's margins: none, for a grid Neovim has not said has any.
    pub fn margins(&self, grid: u64) -> Margins {
        self.margins.get(&grid).copied().unwrap_or_default()
    }

    /// The windows Neovim has named that it has not said where to put, with
    /// their grids. A UI attaching to a session that another UI has drawn is
    /// sent no placement for any window already there — see `super::App`.
    pub fn unplaced(&self) -> Vec<(u64, i64)> {
        self.wins
            .iter()
            .filter(|(grid, _)| !self.layout.contains_key(grid))
            .map(|(grid, win)| (*grid, *win))
            .collect()
    }

    /// The window grids that are to be seen — placed and showing, or named
    /// and not yet placed — and that Neovim has not drawn every cell of since
    /// the client made them: grids it kept from a UI before this one, which
    /// it thinks the client has as that UI did, or has but for what a resize
    /// added. See `super::App`.
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

    /// Resolve the highlights defined since — or every one again, if what
    /// they all depend on changed. Says whether it did: cells may look
    /// different now.
    ///
    /// One by one otherwise: Neovim defines thousands a frame for a
    /// `:terminal` playing an animation in truecolour (see [`Highlights`]),
    /// and resolving the whole table again for each frame of them took the
    /// client longer than drawing the frame.
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

    /// Resolve highlight `id` for drawing, as `colors` say. Id 0 is never
    /// defined, and is the default whatever is sent for it.
    fn resolve(&mut self, id: usize, colors: &Colors) {
        let Some(Some((rgb, cterm))) = self.highlights.defs.get(id).filter(|_| id > 0) else {
            return;
        };
        let style = colors.style(rgb, cterm);
        let blend = if colors.rgb { rgb.blend } else { cterm.blend };
        // A hyperlink comes with the GUI form only, and is one whichever form
        // is drawn.
        let link = rgb.url.clone().map_or(0, |url| self.link_number(&url));
        self.styles[id] = Style { link, ..style };
        self.blends[id] = blend;
    }

    /// Whether enough of the client's ids are retired that a sweep is due:
    /// see [`Model::reclaim`].
    pub fn reclaiming(&self) -> bool {
        let h = &self.highlights;
        h.retired.len() >= h.kept + RECLAIM_AT
    }

    /// The highlights the model holds itself — in its grids, its highlight
    /// groups and its modes' cursors — to which whatever else keeps cells
    /// across a flush adds its own before a sweep: see [`Model::reclaim`].
    pub fn held(&self) -> Held {
        let mut held = Held(vec![0; self.highlights.defs.len().div_ceil(64)]);
        for g in self.grids.values() {
            for r in 0..g.height() {
                held.cells(g.row(r));
            }
        }
        for hl in self.groups.values() {
            held.mark(*hl);
        }
        for mode in &self.modes {
            held.mark(mode.attr_id);
        }
        held
    }

    /// Give out again every retired id nothing in `held` holds: no cell, in
    /// the model or in anything that keeps cells across a flush, can be drawn
    /// in it any more. See [`Highlights`].
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
    use crate::client::grid::Text;
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

    /// What Neovim sends for a 22-row window over 30 lines, scrolled to
    /// each kind of place, and for views that are not one line to a row.
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

    /// The cursor's place on the screen is its grid's place plus its own.
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

    /// A window's margins can arrive before the window is placed, and are
    /// kept for it; hiding and placing again shows it.
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

    /// A `grid_line` of `n` cells from `col`.
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

    /// What a UI attaching to a session another UI drew is sent: its windows'
    /// grids resized and named, but neither placed nor drawn on. Both are
    /// noticed; drawing every cell of a grid, or hiding it, settles the
    /// question — and a cell is not every cell.
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
        // Shown again, with nothing ever drawn on it: lacking again.
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
        // A window placed on a tab page the client has never seen has no
        // grid at all.
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

    /// What a UI attaching to a session another UI drew on a smaller terminal
    /// is sent: its windows' grids resized and placed, and drawn on only where
    /// the resize added to them — under the rows a grid kept, or to the right
    /// of every one of them. Each is noticed until the rest of it is drawn.
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

    /// Scrolls are added up per window over a batch, and only grid 1 being
    /// cleared asks for every cell to be painted again.
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

    /// A highlight group of the editor's laid over one of Neovim's: its
    /// colours where it has them, the one under it's where not, and every
    /// attribute either has; in the form `'termguicolors'` says; as the one
    /// under it while the group is not known.
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

    /// Highlights resolve lazily, and `'termguicolors'` decides which form.
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

    /// A highlight's hyperlink gets a number, the same one for the same
    /// target, whichever form of the highlight is drawn — and nothing in it
    /// that could end the OSC 8 it is written in.
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

    /// Neovim's highlight `id`, its foreground `0xRRGGBB`.
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

    /// Row `row` of grid 1, all three cells of it, in Neovim's highlight
    /// `hl`.
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

    /// A model drawing in RGB, its grid 1 three cells by two.
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

    /// The foreground each of `cells` is drawn in.
    fn fgs(m: &Model, cells: &[Cell]) -> Vec<Color> {
        cells.iter().map(|c| m.style(c.hl).fg).collect()
    }

    fn fg(n: u32) -> Color {
        Color::Rgb(crate::client::style::rgb(n))
    }

    /// Neovim's table of highlights full, it clears it part way through
    /// drawing a frame and gives the same ids out again, the cells it drew
    /// before still naming them until it draws them again. They keep the
    /// colours their id had when they were drawn — and so does a copy of
    /// them kept of a moment ago, as an animation keeps one.
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

    /// A highlight sent again as it was — every one is, to a UI attaching —
    /// keeps its id, and gives none up.
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

    /// What names a highlight as it is now — a group the client draws its
    /// own rows in, a mode's cursor — names what Neovim last defined.
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

    /// A session whose table of highlights is cleared and filled again every
    /// few frames, for good, holds no more ids than its table has and its
    /// cells hold: one Neovim has given out again is given back once nothing
    /// holds it, and out again. One a cell holds — or anything else that
    /// keeps cells, as an animation does — is not.
    #[test]
    fn an_id_is_given_out_again_once_nothing_holds_it() {
        // Neovim's table cleared and filled again: ids 1 to 4 given to
        // highlights in colours of the `n`th time.
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

        // Given out again with cells still drawn in them, and given out again
        // once more after they are drawn anew — but kept, as an animation
        // keeps them: they keep their colours through a third time.
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

    /// A sweep waits for [`RECLAIM_AT`] ids to be given out again past those
    /// the last one found still held; a group laid over a highlight holds
    /// the highlight.
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

    /// Highlights resolved one by one as they come resolve as they would all
    /// at once: their colours, blends and links.
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
