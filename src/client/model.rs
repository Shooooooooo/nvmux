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

use super::grid::Grid;
use super::redraw::{Event, ModeInfo, OptionValue};
use super::style::{Attrs, Colors, DefaultColors, Style};
use crate::palette::Palette;

/// Where a grid goes on the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// A window in the editor's layout, its top left at `row`, `col`.
    Window { row: usize, col: usize },
    /// A float, over everything in the layout.
    Float {
        row: i64,
        col: i64,
        zindex: i64,
        compindex: i64,
        /// Whether the mouse can land on it.
        mouse: bool,
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
            Place::Window { row, col } => (*row as i64, *col as i64),
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

/// The highlights Neovim has defined, by id.
#[derive(Debug, Default)]
struct Highlights {
    defs: Vec<Option<(Attrs, Attrs)>>,
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
    /// `hl_group_set`: the highlight ids of the groups the UI draws itself.
    pub groups: HashMap<String, u32>,
    highlights: Highlights,
    /// Every highlight resolved for drawing, by id, and its blend: rebuilt when
    /// a highlight, the defaults or `'termguicolors'` change (see
    /// [`Model::colors`]).
    styles: Vec<Style>,
    blends: Vec<u8>,
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

    /// Highlight `id` resolved for drawing; an id never defined is the
    /// default.
    pub fn style(&self, id: u32) -> Style {
        self.styles
            .get(id as usize)
            .copied()
            .unwrap_or_else(|| self.styles.first().copied().unwrap_or_default())
    }

    /// Highlight `id` as Neovim defined it, in both its forms.
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
    /// animation of a window's moving counted.
    pub fn cursor_on_screen(&self) -> (i64, i64) {
        let c = self.cursor;
        let (r, k) = self.layout.get(&c.grid).map_or((0, 0), Placement::origin);
        (r + c.row as i64, k + c.col as i64)
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
                cells,
            } => {
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
            Event::HlAttr { id, rgb, cterm } => {
                let defs = &mut self.highlights.defs;
                let i = id as usize;
                if defs.len() <= i {
                    defs.resize(i + 1, None);
                }
                defs[i] = Some((rgb, cterm));
                self.styles_stale = true;
            }
            Event::HlGroup { name, id } => {
                self.groups.insert(name, id);
            }
            Event::WinPos {
                grid,
                win,
                row,
                col,
                ..
            } => self.place(grid, win, Place::Window { row, col }),
            Event::WinFloatPos {
                grid,
                win,
                mouse,
                zindex,
                compindex,
                row,
                col,
            } => self.place(
                grid,
                win,
                Place::Float {
                    row,
                    col,
                    zindex,
                    compindex,
                    mouse,
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
                        place: Place::Window { row: 0, col: 0 },
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
            Event::ModeInfoSet { enabled, modes } => {
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
    /// and not yet placed — and that Neovim has never drawn anything on:
    /// grids it thinks the client already has. See `super::App`.
    pub fn lacking(&self) -> Vec<u64> {
        let blank = |grid: &u64| !self.grids.get(grid).is_some_and(Grid::written);
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
            .filter(blank)
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

    /// Resolve every highlight again, if anything they depend on changed.
    /// Says whether it did: every cell may look different now.
    pub fn refresh_styles(&mut self) -> bool {
        if !self.styles_stale {
            return false;
        }
        self.styles_stale = false;
        let colors = self.colors();
        let rgb = self.options.termguicolors;
        let plain = colors.default_style();
        // Id 0 is never defined, and is the default whatever is sent for it.
        let n = self.highlights.defs.len().max(1);
        self.styles = (0..n)
            .map(|id| match self.highlights.defs.get(id) {
                Some(Some((g, c))) if id > 0 => colors.style(g, c),
                _ => plain,
            })
            .collect();
        // A hyperlink comes with the GUI form only, and is one whichever form
        // is drawn.
        let urls: Vec<(usize, Box<str>)> = self
            .highlights
            .defs
            .iter()
            .enumerate()
            .filter_map(|(id, def)| Some((id, def.as_ref()?.0.url.clone()?)))
            .filter(|(id, _)| *id > 0)
            .collect();
        for (id, url) in urls {
            self.styles[id].link = self.link_number(&url);
        }
        self.blends = (0..n)
            .map(|id| match self.highlights.defs.get(id) {
                Some(Some((g, c))) if id > 0 => {
                    if rgb {
                        g.blend
                    } else {
                        c.blend
                    }
                }
                _ => 0,
            })
            .collect();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::grid::Text;
    use crate::client::redraw::LineCell;
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

    /// What a UI attaching to a session another UI drew is sent: its windows'
    /// grids resized and named, but neither placed nor drawn on. Both are
    /// noticed; drawing on a grid, or hiding it, settles the question.
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
                Event::GridLine {
                    grid: 2,
                    row: 0,
                    col: 0,
                    cells: vec![LineCell {
                        text: Text::Char('a'),
                        hl: Some(0),
                        repeat: 1,
                    }],
                },
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
        assert!(m.lacking().is_empty(), "drawn, or hidden");
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
                },
                Event::GridDestroy { grid: 4 },
            ],
        );
        assert!(!m.grids.contains_key(&4));
        assert!(!m.layout.contains_key(&4));
        assert!(changes.reshaped.contains(&4));
    }
}
