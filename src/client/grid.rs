//! A grid as Neovim keeps it: rows of cells, each a piece of text in a
//! highlight, changed by `grid_line`, `grid_scroll`, `grid_clear` and
//! `grid_resize` and by nothing else.
//!
//! Grid 1 is the editor's whole screen. With `ext_multigrid` every window, the
//! message area and every float (the popup menu among them) has a grid of its
//! own as well, and where each goes is the compositor's to say (see
//! [`super::compose`]).

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
    /// Anything has been drawn on it: a `grid_line` or a `grid_clear`. A grid
    /// Neovim only ever resized is one it thinks the client already has —
    /// see `super::App` on attaching to a session another UI has drawn.
    written: bool,
}

impl Grid {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![Cell::default(); width * height],
            written: false,
        }
    }

    /// Whether Neovim has drawn anything on it.
    pub fn written(&self) -> bool {
        self.written
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// Change size, keeping what is in the top left. Neovim redraws a grid it
    /// resizes, so what is kept only matters for the moment before it does.
    pub fn resize(&mut self, width: usize, height: usize) {
        if (width, height) == (self.width, self.height) {
            return;
        }
        let mut cells = vec![Cell::default(); width * height];
        for r in 0..height.min(self.height) {
            let n = width.min(self.width);
            cells[r * width..r * width + n]
                .clone_from_slice(&self.cells[r * self.width..r * self.width + n]);
        }
        self.width = width;
        self.height = height;
        self.cells = cells;
    }

    /// Every cell blank, in the default highlight.
    pub fn clear(&mut self) {
        self.cells.fill(Cell::default());
        self.written = true;
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
    /// names or the one before it. What would fall past the edge is dropped.
    pub fn put(&mut self, row: usize, col: usize, runs: &[LineCell]) {
        self.written = true;
        if row >= self.height {
            return;
        }
        let base = row * self.width;
        let mut c = col;
        let mut hl = 0;
        for run in runs {
            if let Some(h) = run.hl {
                hl = h;
            }
            for _ in 0..run.repeat {
                if c >= self.width {
                    return;
                }
                let cell = &mut self.cells[base + c];
                cell.text.clone_from(&run.text);
                cell.hl = hl;
                c += 1;
            }
        }
    }

    /// A `grid_scroll`: the rectangle `top..bot` × `left..right` moves up by
    /// `rows`, or down by minus that. The rows it uncovers keep what they had;
    /// Neovim fills them with `grid_line` straight after.
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
        let mut copy_row = |to: usize, from: usize| {
            let (to, from) = (to * w, from * w);
            if to < from {
                let (a, b) = self.cells.split_at_mut(from);
                a[to + left..to + right].clone_from_slice(&b[left..right]);
            } else {
                let (a, b) = self.cells.split_at_mut(to);
                b[left..right].clone_from_slice(&a[from + left..from + right]);
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
