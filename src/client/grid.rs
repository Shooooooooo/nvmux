//! A grid as Neovim keeps it: rows of cells, each a piece of text in a
//! highlight, changed by `grid_line`, `grid_scroll`, `grid_clear` and
//! `grid_resize` and by nothing else.
//!
//! Grid 1 is the editor's whole screen. With `ext_multigrid` every window, the
//! message area and every float (the popup menu among them) has a grid of its
//! own as well, and where each goes is the compositor's to say (see
//! [`super::compose`]).
//!
//! A character two cells wide is its own cell and the [`Text::Half`] after
//! it. Where the right edge cuts one in two, its left half is made a blank in
//! its own highlight, as [`super::compose::Frame::mend`] makes one a float
//! cuts, and Neovim's compositor too: on its own it would still be drawn two
//! cells wide, by a terminal that knows nothing of the cut, and the rest of
//! its row a cell to the right of where the frame has it. The edge cuts one
//! where a grid is made narrower — as the client makes its own copy of one it
//! asks Neovim to narrow (see [`super::wide`]) — and where a line drawn for
//! the grid as wide as it was comes after, one Neovim sent before it had the
//! ask. Neovim's own grid keeps the half as it was, but draws that cell again
//! with the rest of its row at the new width: it has no room for a character
//! two cells wide in a row's last column.

use super::redraw::LineCell;

/// What one cell shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Text {
    /// The right half of a character two cells wide: the cell to its left
    /// draws all of it, and this one draws nothing. Neovim sends it as `""`.
    Half,
    /// One character, which is every cell but a few.
    Char(char),
    /// A grapheme cluster: a letter with its combining marks, an emoji with
    /// its modifiers.
    Cluster(Box<str>),
}

impl Text {
    /// The text Neovim sent for a cell.
    pub fn new(s: &str) -> Self {
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (None, _) => Text::Half,
            (Some(c), None) => Text::Char(c),
            _ => Text::Cluster(s.into()),
        }
    }

    /// A blank: what a blended float lets the text under it show through
    /// (see [`super::style::blend_through`]). The braille blank counts, as it
    /// does for Neovim's own compositor.
    pub fn is_blank(&self) -> bool {
        matches!(self, Text::Char(' ' | '\u{2800}'))
    }

    /// Append the text as UTF-8.
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

/// One cell: its text, and the highlight it is drawn in.
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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Grid {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
    /// Which cells Neovim has drawn since the client made the grid, a flag a
    /// cell like `cells`, and how many it has not. Of a grid Neovim kept from
    /// a UI before this one it sends only the cells that differ from what it
    /// holds, which is what that UI was sent: none, the cells a resize added,
    /// a few that changed since — see `super::App` on attaching to a session
    /// another UI has drawn.
    drawn: Vec<bool>,
    undrawn: usize,
}

impl Grid {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![Cell::default(); width * height],
            drawn: vec![false; width * height],
            undrawn: width * height,
        }
    }

    /// Whether Neovim has drawn every cell of it since the client made it.
    /// Of a grid it makes it draws every cell in the flush that makes it, and
    /// of one it resizes every cell the resize added.
    pub fn whole(&self) -> bool {
        self.undrawn == 0
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// Change size, keeping what is in the top left as Neovim keeps it, and
    /// which of it was drawn. What a resize adds is blank, and not drawn until
    /// Neovim draws it, which it does straight after; of what the grid kept,
    /// it sends only what has changed. A character two cells wide that a
    /// narrower grid cuts in two is left a blank: see the module docs.
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
            // A character two cells wide in the last column kept, cut in two.
            if n > 0 && self.is_wide(r, n - 1) {
                cells[to + n - 1].text = Text::Char(' ');
            }
        }
        self.width = width;
        self.height = height;
        self.cells = cells;
        self.undrawn = drawn.iter().filter(|d| !**d).count();
        self.drawn = drawn;
    }

    /// Every cell blank, in the default highlight.
    pub fn clear(&mut self) {
        self.cells.fill(Cell::default());
        self.drawn.fill(true);
        self.undrawn = 0;
    }

    /// Row `r`, or nothing past the last.
    pub fn row(&self, r: usize) -> &[Cell] {
        if r >= self.height {
            return &[];
        }
        &self.cells[r * self.width..(r + 1) * self.width]
    }

    pub fn cell(&self, r: usize, c: usize) -> Option<&Cell> {
        (r < self.height && c < self.width).then(|| &self.cells[r * self.width + c])
    }

    /// Whether the cell at `(r, c)` holds a character two cells wide: its
    /// right half is the next cell along.
    pub fn is_wide(&self, r: usize, c: usize) -> bool {
        self.cell(r, c + 1)
            .is_some_and(|next| next.text == Text::Half)
    }

    /// A `grid_line`: runs of cells from `col` on, each in the highlight it
    /// names or the one before it. What would fall past the edge is dropped,
    /// and a character two cells wide cut in two there is left a blank: see
    /// the module docs.
    pub fn put(&mut self, row: usize, col: usize, runs: &[LineCell]) {
        if row >= self.height || col >= self.width {
            return;
        }
        let base = row * self.width;
        let mut c = col;
        let mut hl = 0;
        'runs: for run in runs {
            if let Some(h) = run.hl {
                hl = h;
            }
            for _ in 0..run.repeat {
                if c >= self.width {
                    // Cut in two: the first cell dropped is the right half
                    // of the last one put.
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
        // Nothing to mark of a grid drawn whole, which is nearly every line.
        if self.undrawn > 0 {
            for drawn in &mut self.drawn[base + col..base + c] {
                if !*drawn {
                    *drawn = true;
                    self.undrawn -= 1;
                }
            }
        }
    }

    /// A `grid_scroll`: the rectangle `top..bot` × `left..right` moves up by
    /// `rows`, or down by minus that. The rows it uncovers keep what they had;
    /// Neovim fills them with `grid_line` straight after. Which cells were
    /// drawn moves with them.
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
        // Of a grid drawn whole, every cell is drawn wherever it goes.
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

    /// The rows `top..bot`, columns `left..right`, as owned lines: what a
    /// scroll animation keeps of a region as it was (see
    /// [`super::anim::scroll`]).
    pub fn lines(&self, top: usize, bot: usize, left: usize, right: usize) -> Vec<Vec<Cell>> {
        (top..bot.min(self.height))
            .map(|r| {
                let row = self.row(r);
                row[left.min(row.len())..right.min(row.len())].to_vec()
            })
            .collect()
    }

    /// The whole grid as text, a row to a line, for the tests.
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

/// Columns `left..right` of row `from` copied over those of row `to`, in a
/// grid's cells, or what it keeps of each, row-major and `width` wide.
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

    /// The highlight carries from run to run until one names another, and a
    /// run past the edge is cut there.
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

    /// Up and down, inside a rectangle that leaves the columns either side
    /// alone, and the uncovered rows keep what they had.
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

    /// A scroll by the whole region or more moves nothing: Neovim redraws it
    /// all anyway.
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

    /// A grid is whole once Neovim has drawn every cell of it, whatever the
    /// lines it took: a line marks the cells it drew and no others, and a
    /// clear marks them all.
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

    /// What a grid kept from another UI and resized is sent: the rows a
    /// resize added, or the columns, or both; never what it kept. Until that
    /// is drawn as well it is not whole — nor a grid drawn whole and resized,
    /// until the cells the resize added are drawn.
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

    /// Which cells were drawn moves with them in a scroll; the rows it
    /// uncovers keep theirs, as they keep their cells.
    #[test]
    fn a_scroll_moves_which_cells_were_drawn() {
        let mut g = Grid::new(2, 4);
        lines(&mut g, &["aa", "bb"]);
        g.scroll(0, 4, 0, 2, 2);
        // What Neovim draws after it: the rows it uncovered.
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

    /// `x`, then two characters two cells wide, each in a highlight of its
    /// own: five cells, every one drawn.
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

    /// A character two cells wide that a narrower grid cuts in two leaves a
    /// blank in the last column, in its highlight, and drawn as it was.
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

    /// A resize that cuts no character in two keeps every cell it keeps as
    /// it was: cut before one or after one, or none at all, the grid made
    /// taller or wider.
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

    /// A line cut at the edge through a character two cells wide, as one
    /// Neovim drew for a grid the client has since made narrower is, leaves
    /// a blank in the last column, in its highlight, and drawn with the rest
    /// of the line. Cut before one or after one, it is put as it came.
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
}
