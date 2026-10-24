//! A Neovim grid: rows of cells (text plus highlight), changed only by
//! `grid_line`, `grid_scroll`, `grid_clear` and `grid_resize`. With
//! `ext_multigrid` each window and float has its own (see [`super::compose`]).
//!
//! When the right edge cuts a double-width character in two (the grid
//! narrowed, see [`super::wide`], or a line was sent for the old width), its
//! left half becomes a blank in its highlight, as Neovim's compositor does;
//! otherwise the terminal would draw it two wide and shift the row.

use std::sync::atomic::{AtomicU64, Ordering};

use super::redraw::LineCell;

/// Change clock shared by all grids and the model.
static CLOCK: AtomicU64 = AtomicU64::new(1);

/// A stamp later than all earlier ones.
pub fn tick() -> u64 {
    CLOCK.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Text {
    /// Right half of a double-width character, which the cell to its left
    /// draws. Neovim sends it as `""`.
    Half,
    Char(char),
    /// A grapheme cluster, e.g. a letter with combining marks.
    Cluster(Box<str>),
}

impl Text {
    pub fn new(s: &str) -> Self {
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (None, _) => Text::Half,
            (Some(c), None) => Text::Char(c),
            _ => Text::Cluster(s.into()),
        }
    }

    /// Whether a blended float shows text through it (see
    /// [`super::style::blend_through`]); the braille blank counts, as in
    /// Neovim.
    pub fn is_blank(&self) -> bool {
        matches!(self, Text::Char(' ' | '\u{2800}'))
    }

    pub fn push_to(&self, out: &mut Vec<u8>) {
        match self {
            Text::Half => {}
            Text::Char(c) => {
                let mut b = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
            Text::Cluster(s) => out.extend_from_slice(s.as_bytes()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub text: Text,
    pub hl: u32,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            text: Text::Char(' '),
            hl: 0,
        }
    }
}

/// A grid of cells, row-major.
#[derive(Debug, Clone, Default)]
pub struct Grid {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
    /// Cells Neovim has drawn since the grid was made; `undrawn` counts the
    /// rest. A grid kept from an earlier UI gets only cells that changed (see
    /// `super::App`).
    drawn: Vec<bool>,
    undrawn: usize,
    /// Per-row [`tick`] of the last change, for redrawing only changed rows.
    stamps: Vec<u64>,
    /// [`tick`] of the last size change.
    shaped: u64,
}

/// Ignores change stamps.
impl PartialEq for Grid {
    fn eq(&self, other: &Self) -> bool {
        (self.width, self.height, self.undrawn) == (other.width, other.height, other.undrawn)
            && self.cells == other.cells
            && self.drawn == other.drawn
    }
}

impl Eq for Grid {}

impl Grid {
    pub fn new(width: usize, height: usize) -> Self {
        let now = tick();
        Self {
            width,
            height,
            cells: vec![Cell::default(); width * height],
            drawn: vec![false; width * height],
            undrawn: width * height,
            stamps: vec![now; height],
            shaped: now,
        }
    }

    /// Rows changed after the [`tick`] `since`.
    pub fn changed_since(&self, since: u64) -> impl Iterator<Item = usize> + '_ {
        self.stamps
            .iter()
            .enumerate()
            .filter(move |(_, stamp)| **stamp > since)
            .map(|(r, _)| r)
    }

    pub fn shaped_since(&self, since: u64) -> bool {
        self.shaped > since
    }

    /// Whether Neovim has drawn every cell; it draws a new grid whole, and
    /// after a resize the added cells.
    pub fn whole(&self) -> bool {
        self.undrawn == 0
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// Resize as Neovim does, keeping the top-left cells and their drawn
    /// flags; Neovim draws the added cells right after. A double-width
    /// character cut by the new edge becomes a blank.
    pub fn resize(&mut self, width: usize, height: usize) {
        if (width, height) == (self.width, self.height) {
            return;
        }
        let mut cells = vec![Cell::default(); width * height];
        let mut drawn = vec![false; width * height];
        let n = width.min(self.width);
        for r in 0..height.min(self.height) {
            let (to, from) = (r * width, r * self.width);
            cells[to..to + n].clone_from_slice(&self.cells[from..from + n]);
            drawn[to..to + n].copy_from_slice(&self.drawn[from..from + n]);
            // A double-width character cut by the new right edge.
            if n > 0 && self.is_wide(r, n - 1) {
                cells[to + n - 1].text = Text::Char(' ');
            }
        }
        self.width = width;
        self.height = height;
        self.cells = cells;
        self.undrawn = drawn.iter().filter(|d| !**d).count();
        self.drawn = drawn;
        let now = tick();
        self.stamps = vec![now; height];
        self.shaped = now;
    }

    /// Blank every cell in the default highlight, marking all drawn.
    pub fn clear(&mut self) {
        self.cells.fill(Cell::default());
        self.drawn.fill(true);
        self.undrawn = 0;
        self.stamps.fill(tick());
    }

    /// Row `r`; empty past the last.
    pub fn row(&self, r: usize) -> &[Cell] {
        if r >= self.height {
            return &[];
        }
        &self.cells[r * self.width..(r + 1) * self.width]
    }

    pub fn cell(&self, r: usize, c: usize) -> Option<&Cell> {
        (r < self.height && c < self.width).then(|| &self.cells[r * self.width + c])
    }

    /// Whether `(r, c)` holds a double-width character.
    pub fn is_wide(&self, r: usize, c: usize) -> bool {
        self.cell(r, c + 1)
            .is_some_and(|next| next.text == Text::Half)
    }

    /// `grid_line`: runs from `col`, each in its named highlight or the
    /// previous one; cells past the edge are dropped.
    pub fn put(&mut self, row: usize, col: usize, runs: &[LineCell]) {
        if row >= self.height || col >= self.width {
            return;
        }
        self.stamps[row] = tick();
        let base = row * self.width;
        let mut c = col;
        let mut hl = 0;
        'runs: for run in runs {
            if let Some(h) = run.hl {
                hl = h;
            }
            for _ in 0..run.repeat {
                if c >= self.width {
                    // A right half as the first dropped cell: the last one
                    // put is cut in two.
                    if run.text == Text::Half {
                        self.cells[base + c - 1].text = Text::Char(' ');
                    }
                    break 'runs;
                }
                let cell = &mut self.cells[base + c];
                cell.text.clone_from(&run.text);
                cell.hl = hl;
                c += 1;
            }
        }
        // Nothing to mark on a whole grid, the common case.
        if self.undrawn > 0 {
            for drawn in &mut self.drawn[base + col..base + c] {
                if !*drawn {
                    *drawn = true;
                    self.undrawn -= 1;
                }
            }
        }
    }

    /// `grid_scroll`: move the rectangle up by `rows` (down if negative).
    /// Uncovered rows keep their contents until Neovim redraws them.
    pub fn scroll(&mut self, top: usize, bot: usize, left: usize, right: usize, rows: i64) {
        let bot = bot.min(self.height);
        let right = right.min(self.width);
        if top >= bot || left >= right || rows == 0 {
            return;
        }
        let w = self.width;
        let n = rows.unsigned_abs() as usize;
        if n >= bot - top {
            return;
        }
        self.stamps[top..bot].fill(tick());
        // On a whole grid every flag is true, so they need not move.
        let marked = self.undrawn > 0;
        let mut copy_row = |to: usize, from: usize| {
            copy_cells(&mut self.cells, w, to, from, (left, right));
            if marked {
                copy_cells(&mut self.drawn, w, to, from, (left, right));
            }
        };
        if rows > 0 {
            for r in top..bot - n {
                copy_row(r, r + n);
            }
        } else {
            for r in (top + n..bot).rev() {
                copy_row(r, r - n);
            }
        }
        if marked {
            self.undrawn = self.drawn.iter().filter(|d| !**d).count();
        }
    }

    /// Copy of rows `top..bot`, columns `left..right`, clipped to the grid.
    pub fn lines(&self, top: usize, bot: usize, left: usize, right: usize) -> Vec<Vec<Cell>> {
        (top..bot.min(self.height))
            .map(|r| {
                let row = self.row(r);
                row[left.min(row.len())..right.min(row.len())].to_vec()
            })
            .collect()
    }

    /// The grid as text, one line per row.
    #[cfg(test)]
    pub fn text(&self) -> String {
        (0..self.height)
            .map(|r| {
                let mut line = Vec::new();
                for cell in self.row(r) {
                    cell.text.push_to(&mut line);
                }
                String::from_utf8(line).expect("utf-8")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Copy columns `left..right` of row `from` over row `to`, in a row-major,
/// `width`-wide array of cells or drawn flags.
fn copy_cells<T: Clone>(
    grid: &mut [T],
    width: usize,
    to: usize,
    from: usize,
    (left, right): (usize, usize),
) {
    let (to, from) = (to * width, from * width);
    if to < from {
        let (a, b) = grid.split_at_mut(from);
        a[to + left..to + right].clone_from_slice(&b[left..right]);
    } else {
        let (a, b) = grid.split_at_mut(to);
        b[left..right].clone_from_slice(&a[from + left..from + right]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, hl: Option<u32>, repeat: usize) -> LineCell {
        LineCell {
            text: Text::new(text),
            hl,
            repeat,
        }
    }

    fn lines(g: &mut Grid, rows: &[&str]) {
        for (r, line) in rows.iter().enumerate() {
            let runs: Vec<_> = line.chars().map(|c| run(&c.to_string(), None, 1)).collect();
            g.put(r, 0, &runs);
        }
    }

    #[test]
    fn text_is_a_char_a_cluster_or_half_of_something_wide() {
        assert_eq!(Text::new(""), Text::Half);
        assert_eq!(Text::new("a"), Text::Char('a'));
        assert_eq!(Text::new("中"), Text::Char('中'));
        assert_eq!(Text::new("e\u{301}"), Text::Cluster("e\u{301}".into()));
        assert!(Text::new(" ").is_blank());
        assert!(Text::new("\u{2800}").is_blank());
        assert!(!Text::new("a").is_blank());
    }

    #[test]
    fn a_line_carries_its_highlight_and_stops_at_the_edge() {
        let mut g = Grid::new(5, 1);
        g.put(
            0,
            1,
            &[
                run("a", Some(3), 1),
                run("b", None, 2),
                run("c", Some(4), 9),
            ],
        );
        assert_eq!(g.text(), " abbc");
        let hls: Vec<u32> = g.row(0).iter().map(|c| c.hl).collect();
        assert_eq!(hls, vec![0, 3, 3, 3, 4]);
    }

    #[test]
    fn a_scroll_moves_only_its_rectangle() {
        let mut g = Grid::new(4, 4);
        lines(&mut g, &["a0aa", "b1bb", "c2cc", "d3dd"]);
        g.scroll(0, 4, 1, 3, 1);
        assert_eq!(g.text(), "a1ba\nb2cb\nc3dc\nd3dd");

        let mut g = Grid::new(4, 4);
        lines(&mut g, &["a0aa", "b1bb", "c2cc", "d3dd"]);
        g.scroll(1, 4, 0, 4, -2);
        assert_eq!(g.text(), "a0aa\nb1bb\nc2cc\nb1bb");
    }

    /// Neovim redraws the whole region anyway.
    #[test]
    fn a_scroll_past_the_region_is_nothing() {
        let mut g = Grid::new(2, 2);
        lines(&mut g, &["ab", "cd"]);
        g.scroll(0, 2, 0, 2, 2);
        assert_eq!(g.text(), "ab\ncd");
    }

    #[test]
    fn a_resize_keeps_the_top_left() {
        let mut g = Grid::new(3, 2);
        lines(&mut g, &["abc", "def"]);
        g.resize(2, 3);
        assert_eq!(g.text(), "ab\nde\n  ");
        g.resize(4, 1);
        assert_eq!(g.text(), "ab  ");
    }

    /// A line marks only the cells it drew; a clear marks all.
    #[test]
    fn a_grid_is_whole_once_every_cell_is_drawn() {
        let mut g = Grid::new(3, 2);
        assert!(!g.whole());
        g.put(0, 1, &[run("a", Some(1), 5)]);
        g.put(1, 0, &[run("b", Some(1), 2)]);
        g.put(2, 0, &[run("c", Some(1), 3)]);
        assert!(!g.whole(), "two cells to go");
        g.put(0, 0, &[run("d", None, 1)]);
        assert!(!g.whole(), "one");
        g.put(1, 2, &[run("e", None, 1)]);
        assert!(g.whole());
        assert_eq!(g.text(), "daa\nbbe");

        let mut g = Grid::new(3, 2);
        g.clear();
        assert!(g.whole(), "cleared");
        assert!(Grid::new(4, 0).whole(), "nothing to draw");
    }

    /// Neovim sends a kept grid only what a resize added, so it is not whole
    /// until the kept cells are drawn too.
    #[test]
    fn a_resize_keeps_which_cells_were_drawn() {
        let mut g = Grid::new(3, 2);
        g.resize(3, 4);
        lines(&mut g, &["", "", "ghi", "jkl"]);
        assert!(!g.whole(), "taller: the rows kept");
        lines(&mut g, &["abc", "def"]);
        assert!(g.whole());

        let mut g = Grid::new(3, 2);
        g.resize(5, 2);
        g.put(0, 3, &[run("x", Some(1), 2)]);
        g.put(1, 3, &[run("y", Some(1), 2)]);
        assert!(!g.whole(), "wider: every row drawn, but only in part");
        lines(&mut g, &["abc", "def"]);
        assert!(g.whole());
        assert_eq!(g.text(), "abcxx\ndefyy");

        g.resize(6, 3);
        assert!(!g.whole(), "drawn whole, and then resized");
        g.put(0, 5, &[run("1", Some(1), 1)]);
        g.put(1, 5, &[run("2", Some(1), 1)]);
        g.put(2, 0, &[run("3", Some(1), 6)]);
        assert!(g.whole());
        g.resize(2, 2);
        assert!(g.whole(), "smaller: nothing added");
        assert_eq!(g.text(), "ab\nde");
    }

    #[test]
    fn a_scroll_moves_which_cells_were_drawn() {
        let mut g = Grid::new(2, 4);
        lines(&mut g, &["aa", "bb"]);
        g.scroll(0, 4, 0, 2, 2);
        // Neovim then draws the uncovered rows.
        lines(&mut g, &["", "", "cc", "dd"]);
        assert!(!g.whole(), "the rows scrolled up were never drawn");
        lines(&mut g, &["ee", "ff"]);
        assert!(g.whole());
        assert_eq!(g.text(), "ee\nff\ncc\ndd");

        let mut g = Grid::new(2, 3);
        lines(&mut g, &["aa", "", "cc"]);
        g.scroll(0, 3, 0, 2, -1);
        assert!(!g.whole(), "row 1 down to row 2, never drawn");
        lines(&mut g, &["", "", "yy"]);
        assert!(g.whole(), "and the row uncovered drawn, as it was");
        assert_eq!(g.text(), "aa\naa\nyy");
    }

    #[test]
    fn a_wide_character_is_known_by_its_right_half() {
        let mut g = Grid::new(4, 1);
        g.put(
            0,
            0,
            &[run("中", Some(1), 1), run("", None, 1), run("x", None, 1)],
        );
        assert!(g.is_wide(0, 0));
        assert!(!g.is_wide(0, 2));
        assert!(!g.is_wide(0, 3));
    }

    /// `x中文` in highlights 1, 2, 3; five cells, all drawn.
    fn two_wide() -> Grid {
        let mut g = Grid::new(5, 1);
        g.put(
            0,
            0,
            &[
                run("x", Some(1), 1),
                run("中", Some(2), 1),
                run("", None, 1),
                run("文", Some(3), 1),
                run("", None, 1),
            ],
        );
        g
    }

    /// The blank keeps the character's highlight and drawn flag.
    #[test]
    fn a_resize_cutting_a_wide_character_in_two_leaves_a_blank() {
        for (width, text, hl) in [(2, "x ", 2), (4, "x中 ", 3)] {
            let mut g = two_wide();
            g.resize(width, 1);
            assert_eq!(g.text(), text);
            assert_eq!(
                g.cell(0, width - 1),
                Some(&Cell {
                    text: Text::Char(' '),
                    hl,
                })
            );
            assert!(g.whole(), "{width} wide: drawn, as it was");
        }
    }

    #[test]
    fn a_resize_between_characters_changes_nothing() {
        let before = two_wide();
        for (width, height) in [(0, 1), (1, 1), (3, 1), (5, 2), (7, 1)] {
            let mut g = before.clone();
            g.resize(width, height);
            let n = width.min(5);
            assert_eq!(g.row(0)[..n], before.row(0)[..n], "{width}x{height}");
        }
    }

    /// A line Neovim sent before the client narrowed the grid.
    #[test]
    fn a_line_cut_through_a_wide_character_leaves_a_blank() {
        let line = [
            run("x", Some(1), 1),
            run("中", Some(2), 1),
            run("", None, 1),
            run("y", None, 1),
        ];
        let mut g = Grid::new(3, 1);
        g.put(0, 0, &[run("a", Some(1), 1)]);
        g.put(0, 1, &line);
        assert_eq!(g.text(), "ax ");
        assert_eq!(
            g.cell(0, 2),
            Some(&Cell {
                text: Text::Char(' '),
                hl: 2,
            })
        );
        assert!(g.whole(), "drawn with the rest of the line");

        for (width, text) in [(2, "ax"), (4, "ax中")] {
            let mut g = Grid::new(width, 1);
            g.put(0, 0, &[run("a", Some(1), 1)]);
            g.put(0, 1, &line);
            assert_eq!(g.text(), text, "{width} wide");
        }
    }

    #[test]
    fn lines_are_cut_to_their_rectangle() {
        let mut g = Grid::new(3, 3);
        lines(&mut g, &["abc", "def", "ghi"]);
        let cut = g.lines(1, 3, 1, 3);
        let text: Vec<String> = cut
            .iter()
            .map(|l| {
                l.iter()
                    .map(|c| match &c.text {
                        Text::Char(ch) => *ch,
                        _ => '?',
                    })
                    .collect()
            })
            .collect();
        assert_eq!(text, vec!["ef", "hi"]);
    }

    #[test]
    fn rows_are_stamped_as_they_change() {
        let mut g = Grid::new(4, 4);
        let changed = |g: &Grid, since| g.changed_since(since).collect::<Vec<_>>();
        let t = tick();
        assert_eq!(changed(&g, t), Vec::<usize>::new());
        g.put(2, 0, &[run("a", Some(1), 2)]);
        assert_eq!(changed(&g, t), vec![2]);
        g.put(9, 0, &[run("a", Some(1), 2)]);
        assert_eq!(changed(&g, t), vec![2], "a line past the last row");
        let t = tick();
        g.scroll(1, 4, 0, 4, 1);
        assert_eq!(changed(&g, t), vec![1, 2, 3]);
        g.scroll(0, 1, 0, 4, 1);
        assert_eq!(changed(&g, t), vec![1, 2, 3], "a scroll of nothing");
        let t = tick();
        g.clear();
        assert_eq!(changed(&g, t), vec![0, 1, 2, 3]);
        let t = tick();
        g.resize(4, 4);
        assert!(!g.shaped_since(t), "the same size");
        assert_eq!(changed(&g, t), Vec::<usize>::new());
        g.resize(3, 2);
        assert!(g.shaped_since(t));
        assert_eq!(changed(&g, t), vec![0, 1]);
    }
}
