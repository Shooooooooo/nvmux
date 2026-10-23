//! Scroll regions: the terminal moves the rows a frame has shifted, as
//! Neovim's TUI does (`DECSTBM` round them, then `DL`/`IL`), instead of the
//! frame rewriting a scrolled window. What still differs is written as usual
//! (see [`super::screen`]).
//!
//! Each shown row keeps a hash of its text. Changed rows are looked up among
//! the shown rows, and each match votes for a shift. For the top-voted shifts,
//! a band is a run of rows equal cell by cell to the shown row that far away;
//! the band saving the most writes is moved, and the search repeats on the
//! rest. A scroll empties rows that must then be written, so a band moves only
//! if it saves [`MIN_SAVED`] rows more than that.
//!
//! - **Whole rows**: left/right margins (`DECSLRM`) are not widely supported.
//! - **Never the last row**: the relay's hint bar may be there (see
//!   `crate::hint`).
//! - **A plain pen**: emptied rows take the pen's background on some
//!   terminals, so the pen is reset first.

use std::ops::Range;

use super::compose::{Frame, Out};
use super::grid::Text;
use super::screen::{push_dec, WHOLE_SCROLL_REGION};
use super::style::Style;

/// Typing changes one row; opening or joining a line moves the window's rest.
const MIN_CHANGED: usize = 3;

/// Setting, scrolling and resetting the region is about 20 bytes.
const MIN_SAVED: usize = 3;

const MAX_BANDS: usize = 4;

const TRIED: usize = 3;

/// A row matching more shown rows than this (a blank, a `~` line) is not
/// looked up.
const FAN_OUT: usize = 8;

const PLAIN_PEN: &[u8] = b"\x1b[0m";

/// In [`Bands::map`], a row a scroll emptied.
const EMPTIED: u32 = u32::MAX;

#[derive(Debug, Default)]
pub struct Bands {
    /// Hashes of the rows the terminal shows; empty when unknown.
    prints: Vec<u64>,
    next: Vec<u64>,
    /// Per new row: the terminal shows it, after the bands moved.
    same: Vec<bool>,
    /// Per new row: equal to the shown row in place, before any move.
    equal: Vec<bool>,
    /// Per screen row: the shown row displayed there after the moves.
    map: Vec<u32>,
    blank: Vec<Out>,
    sorted: Vec<(u64, u32)>,
    /// Votes per shift `by`, at index `by + height`.
    votes: Vec<u16>,
}

impl Bands {
    pub fn forget(&mut self) {
        self.prints.clear();
        self.next.clear();
    }

    /// Append to `out` the bytes that move bands `new` has shifted from `old`
    /// (the shown frame); returns whether any moved. Rows `changed` marks
    /// unchanged are taken as `old`'s without comparing.
    pub fn moved(
        &mut self,
        out: &mut Vec<u8>,
        old: &Frame,
        new: &Frame,
        changed: Option<&[bool]>,
    ) -> bool {
        let h = new.height;
        self.map.clear();
        self.map.extend(0..h as u32);
        if self.blank.len() != new.width {
            self.blank = vec![Out::blank(Style::default()); new.width];
        }
        self.same.clear();
        self.same.extend(
            (0..h)
                .map(|r| changed.is_some_and(|changed| !changed[r]) || row(old, r) == row(new, r)),
        );
        self.equal.clear();
        self.equal.extend_from_slice(&self.same);
        let changed = self.same.iter().filter(|same| !**same).count();
        if self.prints.len() != h {
            if changed < MIN_CHANGED {
                self.next.clear();
                return false;
            }
            self.prints.clear();
            self.prints.extend((0..h).map(|r| print(row(old, r))));
        }
        self.next.clear();
        self.next.extend((0..h).map(|r| {
            if self.same[r] {
                self.prints[r]
            } else {
                print(row(new, r))
            }
        }));
        if changed < MIN_CHANGED || h < 3 {
            return false;
        }
        let last = h - 2;
        let mut moved = 0;
        while moved < MAX_BANDS {
            let Some(band) = self.best(old, new, last) else {
                break;
            };
            if moved == 0 {
                out.extend_from_slice(PLAIN_PEN);
            }
            band.emit(out);
            self.shift(new, band);
            moved += 1;
        }
        if moved > 0 {
            out.extend_from_slice(WHOLE_SCROLL_REGION);
        }
        moved > 0
    }

    pub fn same(&self) -> &[bool] {
        &self.same
    }

    pub fn equal(&self) -> &[bool] {
        &self.equal
    }

    /// What the terminal shows in row `r` after the last [`Bands::moved`].
    pub fn was<'a>(&'a self, old: &'a Frame, r: usize) -> &'a [Out] {
        match self.map[r] {
            EMPTIED => &self.blank,
            s => row(old, s as usize),
        }
    }

    pub fn shown(&mut self) {
        std::mem::swap(&mut self.prints, &mut self.next);
        self.next.clear();
    }

    fn best(&mut self, old: &Frame, new: &Frame, last: usize) -> Option<Band> {
        let h = new.height;
        self.sorted.clear();
        self.sorted
            .extend(self.prints.iter().enumerate().map(|(s, &p)| (p, s as u32)));
        self.sorted.sort_unstable();
        self.votes.clear();
        self.votes.resize(2 * h, 0);
        let (sorted, votes) = (&self.sorted, &mut self.votes);
        for r in (0..h).filter(|&r| !self.same[r]) {
            let p = self.next[r];
            let from = sorted.partition_point(|&(q, _)| q < p);
            let to = sorted.partition_point(|&(q, _)| q <= p);
            if to - from > FAN_OUT {
                continue;
            }
            for &(_, s) in &sorted[from..to] {
                votes[s as usize + h - r] += 1;
            }
        }
        // A row in place has not moved.
        votes[h] = 0;
        let mut tried = [(0u16, 0usize); TRIED];
        for (i, &v) in votes.iter().enumerate() {
            if v > tried[TRIED - 1].0 {
                tried[TRIED - 1] = (v, i);
                tried.sort_unstable_by_key(|t| std::cmp::Reverse(t.0));
            }
        }
        // Best band per shift by hash; then the first that holds cell by
        // cell, or the part of it that does.
        let mut found = tried.map(|(v, i)| {
            (v > 0)
                .then(|| self.band(i as isize - h as isize, last))
                .flatten()
        });
        found.sort_unstable_by_key(|f| std::cmp::Reverse(f.map(|(saved, _)| saved)));
        found
            .into_iter()
            .flatten()
            .find_map(|(_, band)| self.held(old, new, band))
    }

    /// The part of `band` matching cell by cell (rows with equal text may
    /// differ in colour) that saves the most.
    fn held(&self, old: &Frame, new: &Frame, band: Band) -> Option<Band> {
        let mut best: Option<(usize, Band)> = None;
        let mut r = band.start;
        while r < band.end {
            let start = r;
            while r < band.end && row(new, r) == self.was(old, (r as isize + band.by) as usize) {
                r += 1;
            }
            if r > start {
                let part = Band {
                    start,
                    end: r,
                    by: band.by,
                };
                if let Some(saved) = self.saved(part) {
                    if best.is_none_or(|(s, _)| saved > s) {
                        best = Some((saved, part));
                    }
                }
            }
            r += 1;
        }
        best.map(|(_, band)| band)
    }

    fn saved(&self, band: Band) -> Option<usize> {
        let saved = band.rows().filter(|&i| !self.same[i]).count();
        let costs = band.emptied().filter(|&i| self.same[i]).count();
        (saved >= costs + MIN_SAVED).then(|| saved - costs)
    }

    /// The best run of rows whose hashes match the shown row `by` below
    /// (above if negative), with its region above `last + 1`. Hashes only;
    /// [`Bands::held`] checks the cells.
    fn band(&self, by: isize, last: usize) -> Option<(usize, Band)> {
        let h = self.next.len() as isize;
        let (lo, hi) = ((-by).max(0), h.min(h - by));
        // Moving up, the region also covers the emptied rows below.
        let end_at_most = last as isize + 1 - by.max(0);
        let moves = |r: isize| self.next[r as usize] == self.prints[(r + by) as usize];
        let mut best: Option<(usize, Band)> = None;
        let mut r = lo;
        while r < hi {
            let start = r;
            while r < hi && moves(r) {
                r += 1;
            }
            if r == start {
                r += 1;
                continue;
            }
            let end = r.min(end_at_most);
            if end <= start {
                continue;
            }
            let band = Band {
                start: start as usize,
                end: end as usize,
                by,
            };
            if let Some(saved) = self.saved(band) {
                if best.is_none_or(|(s, _)| saved > s) {
                    best = Some((saved, band));
                }
            }
        }
        best
    }

    fn shift(&mut self, new: &Frame, band: Band) {
        let (top, bot) = band.region();
        let n = band.by.unsigned_abs();
        let map = &mut self.map[top..=bot];
        let prints = &mut self.prints[top..=bot];
        let emptied = if band.by > 0 {
            map.rotate_left(n);
            prints.rotate_left(n);
            map.len() - n..map.len()
        } else {
            map.rotate_right(n);
            prints.rotate_right(n);
            0..n
        };
        map[emptied.clone()].fill(EMPTIED);
        prints[emptied].fill(print(&self.blank));
        for r in band.rows() {
            self.same[r] = true;
        }
        for r in band.emptied() {
            self.same[r] = row(new, r) == &self.blank[..];
        }
    }
}

/// Rows `start..end`, each equal to the shown row `by` below (above if
/// negative).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Band {
    start: usize,
    end: usize,
    by: isize,
}

impl Band {
    fn rows(&self) -> Range<usize> {
        self.start..self.end
    }

    fn emptied(&self) -> Range<usize> {
        let n = self.by.unsigned_abs();
        if self.by > 0 {
            self.end..self.end + n
        } else {
            self.start - n..self.start
        }
    }

    /// (top, bottom): the band plus its emptied rows.
    fn region(&self) -> (usize, usize) {
        let n = self.by.unsigned_abs();
        if self.by > 0 {
            (self.start, self.end + n - 1)
        } else {
            (self.start - n, self.end - 1)
        }
    }

    /// The pen must already be reset.
    fn emit(&self, out: &mut Vec<u8>) {
        let (top, bot) = self.region();
        out.extend_from_slice(b"\x1b[");
        push_dec(out, top + 1);
        out.push(b';');
        push_dec(out, bot + 1);
        out.push(b'r');
        // Setting the region homes the cursor to the screen's or the region's
        // top left, depending on the terminal.
        if top > 0 {
            out.extend_from_slice(b"\x1b[");
            push_dec(out, top + 1);
            out.push(b'H');
        }
        out.extend_from_slice(b"\x1b[");
        let n = self.by.unsigned_abs();
        if n > 1 {
            push_dec(out, n);
        }
        out.push(if self.by > 0 { b'M' } else { b'L' });
    }
}

fn row(frame: &Frame, r: usize) -> &[Out] {
    &frame.cells[r * frame.width..(r + 1) * frame.width]
}

/// Hash of a row's text. Colours are left to the cell check, which is cheaper
/// than hashing them for every row.
fn print(cells: &[Out]) -> u64 {
    cells.iter().fold(0, |h, cell| {
        let code = match &cell.text {
            Text::Char(c) => u64::from(*c) | u64::from(cell.wide) << 32,
            Text::Half => 1 << 33,
            Text::Cluster(s) => s.bytes().fold(1 << 34, |h, b| mix(h, u64::from(b))),
        };
        mix(h, code)
    })
}

fn mix(h: u64, word: u64) -> u64 {
    (h.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::screen::{Cursor, Screen};
    use crate::client::style::Color;

    const HIDDEN: Cursor = Cursor {
        visible: false,
        row: 0,
        col: 0,
        shape: 2,
        color: None,
    };

    fn frame(rows: &[&str]) -> Frame {
        frame_of(12, rows)
    }

    fn frame_of(width: usize, rows: &[&str]) -> Frame {
        let mut f = Frame::new(width, rows.len(), Style::default());
        for (r, text) in rows.iter().enumerate() {
            for (c, ch) in text.chars().enumerate() {
                f.cells[r * width + c].text = Text::Char(ch);
            }
        }
        f
    }

    fn lines(first: usize, rows: usize) -> Vec<String> {
        (first..first + rows).map(|n| format!("line {n}")).collect()
    }

    fn refs(rows: &[String]) -> Vec<&str> {
        rows.iter().map(String::as_str).collect()
    }

    struct Term {
        screen: Screen,
        parser: vt100::Parser,
    }

    impl Term {
        fn new(regions: bool, width: usize, height: usize) -> Self {
            let mut screen = Screen::new();
            screen.set_regions(regions);
            Self {
                screen,
                parser: vt100::Parser::new(height as u16, width as u16, 0),
            }
        }

        fn draw(&mut self, f: &Frame) -> String {
            let bytes = self.screen.draw(f.clone(), &HIDDEN, true, &[]);
            self.parser.process(&bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        }

        fn text(&self) -> String {
            let contents = self.parser.screen().contents();
            contents
                .lines()
                .map(str::trim_end)
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn cells(&self) -> Vec<String> {
            let s = self.parser.screen();
            let (rows, cols) = s.size();
            let mut v = Vec::new();
            for r in 0..rows {
                for c in 0..cols {
                    let cell = s.cell(r, c).expect("on the screen");
                    let text = if cell.has_contents() {
                        cell.contents()
                    } else {
                        " "
                    };
                    v.push(format!(
                        "{text}{:?}{:?}{}{}{}{}{}",
                        cell.fgcolor(),
                        cell.bgcolor(),
                        cell.bold(),
                        cell.italic(),
                        cell.underline(),
                        cell.inverse(),
                        cell.is_wide()
                    ));
                }
            }
            v
        }
    }

    fn text_of(f: &Frame) -> String {
        f.text()
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The region is reset before anything is written.
    #[test]
    fn a_window_scrolled_up_has_its_rows_moved_and_the_line_it_uncovered_written() {
        let mut rows = lines(0, 8);
        rows.extend(["status".to_string(), "message".to_string()]);
        let before = frame(&refs(&rows));
        let mut after = lines(0, 1);
        after.extend(lines(2, 6));
        after.push("new".to_string());
        after.extend(["status".to_string(), "message".to_string()]);
        let after = frame(&refs(&after));
        let mut t = Term::new(true, 12, 10);
        t.draw(&before);
        let wrote = t.draw(&after);
        assert!(
            wrote.contains("\x1b[0m\x1b[2;8r\x1b[2H\x1b[M\x1b[r"),
            "{wrote:?}"
        );
        assert!(wrote.contains("new"), "{wrote:?}");
        assert!(
            !wrote.contains("line 3"),
            "a row moved is not written: {wrote:?}"
        );
        assert_eq!(t.text(), text_of(&after));
    }

    #[test]
    fn a_line_opened_has_the_rows_under_it_moved_down() {
        let mut rows = lines(0, 8);
        rows.extend(["status".to_string(), "message".to_string()]);
        let before = frame(&refs(&rows));
        let mut after = lines(0, 2);
        after.push("opened".to_string());
        after.extend(lines(2, 5));
        after.extend(["status".to_string(), "message".to_string()]);
        let after = frame(&refs(&after));
        let mut t = Term::new(true, 12, 10);
        t.draw(&before);
        let wrote = t.draw(&after);
        assert!(wrote.contains("\x1b[3;8r\x1b[3H\x1b[L\x1b[r"), "{wrote:?}");
        assert!(wrote.contains("opened"), "{wrote:?}");
        assert!(
            !wrote.contains("line 4"),
            "a row moved is not written: {wrote:?}"
        );
        assert_eq!(t.text(), text_of(&after));
    }

    #[test]
    fn what_the_terminal_shows_after_a_scroll_is_known_row_by_row() {
        let blank = Out::blank(Style::default());
        let mut rows = lines(0, 9);
        rows.push("message".to_string());
        let before = frame(&refs(&rows));

        let mut up = lines(2, 7);
        up.extend([String::new(), String::new(), "message".to_string()]);
        let up = frame(&refs(&up));
        let mut bands = Bands::default();
        assert!(bands.moved(&mut Vec::new(), &before, &up, None));
        for r in 0..7 {
            assert_eq!(bands.was(&before, r), row(&before, r + 2), "row {r}");
        }
        for r in 7..9 {
            assert!(bands.was(&before, r).iter().all(|c| *c == blank), "row {r}");
        }
        assert_eq!(bands.was(&before, 9), row(&before, 9));

        let mut down = vec![String::new(); 2];
        down.extend(lines(0, 7));
        down.push("message".to_string());
        let down = frame(&refs(&down));
        let mut bands = Bands::default();
        assert!(bands.moved(&mut Vec::new(), &before, &down, None));
        for r in 0..2 {
            assert!(bands.was(&before, r).iter().all(|c| *c == blank), "row {r}");
        }
        for r in 2..9 {
            assert_eq!(bands.was(&before, r), row(&before, r - 2), "row {r}");
        }
    }

    #[test]
    fn the_last_row_is_never_scrolled() {
        let before = frame(&refs(&lines(0, 10)));
        let after = frame(&refs(&lines(3, 10)));
        let mut t = Term::new(true, 12, 10);
        t.draw(&before);
        let wrote = t.draw(&after);
        assert!(wrote.contains("\x1b[1;9r\x1b[3M"), "{wrote:?}");
        assert!(
            wrote.ends_with("\x1b[5C12\x1b[0m\x1b[?2026l"),
            "the last row written: {wrote:?}"
        );
        assert_eq!(t.text(), text_of(&after));
    }

    #[test]
    fn two_windows_scrolled_apart_are_two_bands() {
        let mut rows = lines(0, 6);
        rows.push("split".to_string());
        rows.extend(lines(50, 6));
        rows.push("message".to_string());
        let before = frame(&refs(&rows));
        let mut after = lines(2, 6);
        after.push("split".to_string());
        after.push("above".to_string());
        after.extend(lines(50, 5));
        after.push("message".to_string());
        let after = frame(&refs(&after));
        let mut t = Term::new(true, 12, 14);
        t.draw(&before);
        let wrote = t.draw(&after);
        assert_eq!(wrote.matches("\x1b[r").count(), 1, "{wrote:?}");
        assert!(wrote.contains("\x1b[1;6r\x1b[2M"), "{wrote:?}");
        assert!(wrote.contains("\x1b[8;13r\x1b[8H\x1b[L"), "{wrote:?}");
        assert_eq!(t.text(), text_of(&after));
    }

    #[test]
    fn two_rows_moved_are_written_rather_than_scrolled() {
        let before = frame_of(2, &["aa", "bb", "cc", "dd", "ee"]);
        let after = frame_of(2, &["bb", "cc", "xx", "dd", "ee"]);
        let mut t = Term::new(true, 2, 5);
        t.draw(&before);
        let wrote = t.draw(&after);
        assert!(!wrote.contains('r'), "{wrote:?}");
        assert_eq!(t.text(), text_of(&after));
    }

    #[test]
    fn a_row_that_moved_and_changed_colour_is_written() {
        let before = frame(&refs(&lines(0, 10)));
        let mut after = frame(&refs(&lines(1, 10)));
        let lit = Style {
            bg: Color::Index(4),
            ..Style::default()
        };
        for cell in &mut after.cells[4 * 12..5 * 12] {
            cell.style = lit;
        }
        let mut t = Term::new(true, 12, 10);
        t.draw(&before);
        let wrote = t.draw(&after);
        assert!(
            wrote.contains("line 5"),
            "the row in its new colours: {wrote:?}"
        );
        assert!(!wrote.contains("line 3"), "the rest moved: {wrote:?}");
        let mut whole = Term::new(false, 12, 10);
        whole.draw(&before);
        whole.draw(&after);
        assert_eq!(t.cells(), whole.cells());
    }

    /// A theme's background must be written over the emptied rows.
    #[test]
    fn the_rows_a_scroll_empties_are_written_in_the_frames_colours() {
        let bg = Style {
            bg: Color::Rgb(crate::palette::Rgb(30, 30, 46)),
            ..Style::default()
        };
        let paint = |mut f: Frame| {
            for cell in &mut f.cells {
                cell.style = bg;
            }
            f
        };
        let before = paint(frame(&refs(&lines(0, 10))));
        let mut rows = lines(2, 7);
        rows.extend([String::new(), String::new(), "line 9".to_string()]);
        let after = paint(frame(&refs(&rows)));
        let mut t = Term::new(true, 12, 10);
        t.draw(&before);
        let wrote = t.draw(&after);
        assert!(wrote.contains("\x1b[2M"), "{wrote:?}");
        let mut whole = Term::new(false, 12, 10);
        whole.draw(&before);
        whole.draw(&after);
        assert_eq!(t.cells(), whole.cells());
    }

    #[test]
    fn a_screen_moves_no_rows_unless_asked() {
        let before = frame(&refs(&lines(0, 10)));
        let after = frame(&refs(&lines(1, 10)));
        let mut t = Term::new(false, 12, 10);
        t.draw(&before);
        let wrote = t.draw(&after);
        assert!(
            !wrote.contains("\x1b[M") && !wrote.contains('r'),
            "{wrote:?}"
        );
        assert_eq!(
            wrote.matches('H').count(),
            10,
            "every row written: {wrote:?}"
        );
        assert_eq!(t.text(), text_of(&after));
    }

    #[test]
    fn a_screen_forgotten_moves_nothing() {
        let before = frame(&refs(&lines(0, 10)));
        let after = frame(&refs(&lines(1, 10)));
        let mut t = Term::new(true, 12, 10);
        t.draw(&before);
        t.screen.invalidate();
        let wrote = t.draw(&after);
        assert!(!wrote.contains("\x1b[M"), "{wrote:?}");
        assert_eq!(t.text(), text_of(&after));
    }

    /// Random frames: a band moved, a few cells changed, a row recoloured or
    /// refilled, or nothing.
    struct Walk {
        seed: u64,
        frame: Frame,
    }

    impl Walk {
        fn new(seed: u64, width: usize, height: usize) -> Self {
            let mut w = Self {
                seed,
                frame: Frame::new(width, height, Style::default()),
            };
            for r in 0..height {
                w.fill(r);
            }
            w
        }

        fn next(&mut self) -> u64 {
            self.seed ^= self.seed << 13;
            self.seed ^= self.seed >> 7;
            self.seed ^= self.seed << 17;
            self.seed
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn style(&mut self) -> Style {
            let colours = [Color::Default, Color::Index(1), Color::Index(4)];
            Style {
                fg: colours[self.below(3)],
                bg: colours[self.below(3)],
                bold: self.below(4) == 0,
                ..Style::default()
            }
        }

        /// A small alphabet, so rows of equal text occur.
        fn fill(&mut self, r: usize) {
            let w = self.frame.width;
            let style = self.style();
            let blank = self.below(5) == 0;
            let mut c = 0;
            while c < w {
                let i = r * w + c;
                let wide = !blank && c + 1 < w && self.below(12) == 0;
                let text = if blank {
                    Text::Char(' ')
                } else if wide {
                    Text::Char('中')
                } else {
                    Text::Char(['a', 'b', ' ', '~'][self.below(4)])
                };
                self.frame.cells[i] = Out { text, style, wide };
                if wide {
                    self.frame.cells[i + 1] = Out {
                        text: Text::Half,
                        style,
                        wide: false,
                    };
                    c += 1;
                }
                c += 1;
            }
        }

        fn step(&mut self) -> &Frame {
            let (w, h) = (self.frame.width, self.frame.height);
            match self.below(6) {
                0 | 1 => {
                    let top = self.below(h - 1);
                    let bot = top + 1 + self.below(h - top - 1);
                    let n = 1 + self.below(bot - top);
                    let up = self.below(2) == 0;
                    let band = &mut self.frame.cells[top * w..(bot + 1) * w];
                    let fresh = if up {
                        band.rotate_left(n * w);
                        bot + 1 - n..bot + 1
                    } else {
                        band.rotate_right(n * w);
                        top..top + n
                    };
                    for r in fresh {
                        self.fill(r);
                    }
                }
                2 => {
                    for _ in 0..1 + self.below(4) {
                        let r = self.below(h);
                        let c = self.below(w);
                        let cell = &mut self.frame.cells[r * w + c];
                        if !cell.wide && cell.text != Text::Half {
                            cell.text = Text::Char('z');
                        }
                    }
                }
                3 => {
                    let r = self.below(h);
                    let style = self.style();
                    for cell in &mut self.frame.cells[r * w..(r + 1) * w] {
                        cell.style = style;
                    }
                }
                4 => {
                    let r = self.below(h);
                    self.fill(r);
                }
                _ => {}
            }
            &self.frame
        }
    }

    #[test]
    fn every_frame_leaves_the_screen_it_leaves_with_no_row_moved() {
        let mut scrolls = 0;
        for seed in 1..=8u64 {
            let (w, h) = (16, 12);
            let mut walk = Walk::new(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15), w, h);
            let mut on = Term::new(true, w, h);
            let mut off = Term::new(false, w, h);
            for i in 0..400 {
                let f = walk.step().clone();
                let wrote = on.draw(&f);
                scrolls += wrote.matches("\x1b[r").count();
                off.draw(&f);
                assert_eq!(on.cells(), off.cells(), "seed {seed}, frame {i}: {wrote:?}");
            }
        }
        assert!(scrolls > 300, "rows were moved {scrolls} times");
    }
}
