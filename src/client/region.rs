//! Scroll regions: the rows a frame has moved, moved by the terminal.
//!
//! A window that scrolls changes nearly every row of the screen, and a frame
//! writes every cell it changed: a scroll writes its window again, whole — a
//! page of text a frame, for each of the frames a smooth scroll slides it
//! through (see [`super::anim::scroll`]). Yet those rows are on the terminal
//! already, a few rows from where the frame wants them. With `[client]
//! scroll_regions` on, a frame has the terminal move them there itself, as
//! Neovim's own TUI scrolls: a scroll region set round them (`DECSTBM`), the
//! rows deleted at its top or inserted there (`DL`, `IL`), and the region the
//! whole screen again. Then it writes whatever is still not as it should be,
//! as any frame does (see [`super::screen`]).
//!
//! # Finding them
//!
//! Each row of the frame shown is known by a print of its text, kept from one
//! frame to the next and made again only for the rows a frame changes. Where
//! a frame changes [`MIN_CHANGED`] rows or more, each of those is looked up
//! among the rows shown, and each row with its text some way off is a vote for
//! the frame having moved its rows that far. The shifts with the most votes
//! are tried: a band is a run of rows each exactly the row shown that far
//! away — cell by cell, colours and all, the print only saying where to look —
//! and the band that saves the most rows from being written is moved. Then
//! the search runs again on what is left: for a second window, or a band a
//! float has cut in two.
//!
//! A band's scroll empties as many rows as it moves it by, and those are then
//! written whole. Where one of them showed what the frame wants already, that
//! is writing the scroll costs, and a band is moved only where it saves
//! [`MIN_SAVED`] rows more than it costs.
//!
//! # What it keeps to
//!
//! - **Whole rows.** A region spans the screen: left and right margins
//!   (`DECSLRM`) are not understood widely enough. Windows side by side that
//!   do not scroll together move no whole rows, and are written as ever.
//! - **Never the last row.** nvmux's relay draws its hint bar over it (see
//!   `crate::hint`), and a scroll over it would carry the bar up into the
//!   editor's rows, where nothing would put back what it hid.
//! - **A plain pen.** The rows a scroll empties take the pen's background on
//!   most terminals and the terminal's own on the rest, and on nvmux's copy of
//!   the screen (see `crate::shadow`); so the pen is reset first, and they are
//!   blank in the terminal's own colours on every one of them.
//! - **A frame that stands alone.** The region is set and put back in the
//!   frame that moves the rows, and the cursor placed afresh after, by the
//!   first cell the frame writes.

use std::ops::Range;

use super::compose::{Frame, Out};
use super::grid::Text;
use super::screen::{push_dec, WHOLE_SCROLL_REGION};
use super::style::Style;

/// How many rows a frame has to change for it to be worth asking whether any
/// of them moved: a line typed changes one, a line opened or joined moves the
/// rest of its window.
const MIN_CHANGED: usize = 3;

/// How many rows a band has to save from being written, over the rows its
/// scroll costs, to be moved: setting the region, moving the rows and putting
/// the region back is twenty bytes or so.
const MIN_SAVED: usize = 3;

/// The most bands one frame moves.
const MAX_BANDS: usize = 4;

/// How many shifts are tried for a band: those with the most votes.
const TRIED: usize = 3;

/// How many rows shown may have a row's text before it is not looked up among
/// them: a blank row, or a `~` past the end of a buffer, says nothing of where
/// it came from.
const FAN_OUT: usize = 8;

/// The pen reset: what the rows a scroll empties are blank in.
const PLAIN_PEN: &[u8] = b"\x1b[0m";

/// In [`Bands::map`], a row a scroll emptied.
const EMPTIED: u32 = u32::MAX;

/// What is known of the rows shown, from one frame to the next, for finding
/// the bands a frame moved among them.
#[derive(Debug, Default)]
pub struct Bands {
    /// A print of each row of the frame the terminal shows, or none at all
    /// where they are not known: after the screen is forgotten, and until a
    /// frame needs them.
    prints: Vec<u64>,
    /// The prints of the frame being drawn, or none where they are not known.
    next: Vec<u64>,
    /// Whether the terminal shows each row of the frame being drawn already,
    /// as the bands moved leave it.
    same: Vec<bool>,
    /// For each row of the screen, the row of the frame shown that the
    /// terminal shows there once the bands have moved: [`EMPTIED`] where a
    /// scroll emptied it.
    map: Vec<u32>,
    /// A row of blanks in the plain pen, as wide as the screen: what a row a
    /// scroll empties shows.
    blank: Vec<Out>,
    /// The rows shown, by print: what a row is looked up among.
    sorted: Vec<(u64, u32)>,
    /// How many rows say they moved each way, by how far: the shift `by` at
    /// `by + height`.
    votes: Vec<u16>,
}

impl Bands {
    /// Forget the rows shown: the terminal shows a frame of another size, or
    /// one nothing is known of.
    pub fn forget(&mut self) {
        self.prints.clear();
        self.next.clear();
    }

    /// Move the bands of rows that `new` has where `old`, the frame the
    /// terminal shows and of the same size, had them a few rows off: the
    /// bytes that move them onto the end of `out`. Says whether it moved any.
    /// After it, [`Bands::was`] says what the terminal shows in each row, and
    /// [`Bands::same`] whether that is the row `new` has there already.
    pub fn moved(&mut self, out: &mut Vec<u8>, old: &Frame, new: &Frame) -> bool {
        let h = new.height;
        self.map.clear();
        self.map.extend(0..h as u32);
        if self.blank.len() != new.width {
            self.blank = vec![Out::blank(Style::default()); new.width];
        }
        self.same.clear();
        self.same.extend((0..h).map(|r| row(old, r) == row(new, r)));
        let changed = self.same.iter().filter(|same| !**same).count();
        if self.prints.len() != h {
            if changed < MIN_CHANGED {
                // Not worth knowing yet: known when a frame that needs them
                // comes, from the frame shown then.
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
        // The lowest row a region may take in: never the screen's last.
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

    /// For each row of the frame [`Bands::moved`] was last given, whether the
    /// terminal shows it already.
    pub fn same(&self) -> &[bool] {
        &self.same
    }

    /// What the terminal shows in row `r`, once the bands [`Bands::moved`]
    /// last moved have moved: a row of `old`, the frame it was given as the
    /// one shown, or a row of blanks.
    pub fn was<'a>(&'a self, old: &'a Frame, r: usize) -> &'a [Out] {
        match self.map[r] {
            EMPTIED => &self.blank,
            s => row(old, s as usize),
        }
    }

    /// The frame [`Bands::moved`] was given is drawn: its rows are the ones
    /// shown.
    pub fn shown(&mut self) {
        std::mem::swap(&mut self.prints, &mut self.next);
        self.next.clear();
    }

    /// The band that saves the most rows from being written, of those the
    /// shifts with the most votes find, where one saves enough.
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
        // A row where it was has not moved.
        votes[h] = 0;
        let mut tried = [(0u16, 0usize); TRIED];
        for (i, &v) in votes.iter().enumerate() {
            if v > tried[TRIED - 1].0 {
                tried[TRIED - 1] = (v, i);
                tried.sort_unstable_by_key(|t| std::cmp::Reverse(t.0));
            }
        }
        // The best band each shift's prints find, best first; then the first
        // that holds cell by cell, or the part of it that does.
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

    /// The part of `band` whose rows are each exactly, cell by cell, the row
    /// the terminal shows `by` rows away that saves the most, where one saves
    /// enough: all of it, but where two rows of the same text differ in
    /// their colours.
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

    /// How many rows moving `band` saves from being written, over those its
    /// scroll costs, where that is [`MIN_SAVED`] or more.
    fn saved(&self, band: Band) -> Option<usize> {
        let saved = band.rows().filter(|&i| !self.same[i]).count();
        let costs = band.emptied().filter(|&i| self.same[i]).count();
        (saved >= costs + MIN_SAVED).then(|| saved - costs)
    }

    /// The band of rows of the frame being drawn whose prints are each those
    /// of the row shown `by` rows below it (above it, where negative) that
    /// saves the most rows, and how many it saves: where one saves
    /// [`MIN_SAVED`] rows or more over what its scroll costs, and its region
    /// keeps above row `last + 1`. Only the prints say so: whether its rows
    /// are those rows is for [`Bands::held`] to say.
    fn band(&self, by: isize, last: usize) -> Option<(usize, Band)> {
        let h = self.next.len() as isize;
        let (lo, hi) = ((-by).max(0), h.min(h - by));
        // Where a region taking a band in ends, at the lowest: the band, and
        // below it the rows its scroll empties where it moves them up.
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

    /// What the terminal shows made what it shows once `band` has moved, and
    /// what is known of each row with it.
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

/// Rows `start..end` of a frame, each the row `by` rows below it on the
/// screen shown, or above it where `by` is negative.
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

    /// The rows its scroll empties: as many as it moves, below it where it
    /// moves up and above it where down.
    fn emptied(&self) -> Range<usize> {
        let n = self.by.unsigned_abs();
        if self.by > 0 {
            self.end..self.end + n
        } else {
            self.start - n..self.start
        }
    }

    /// Its scroll region's top row and bottom one: the band, and the rows
    /// its scroll empties.
    fn region(&self) -> (usize, usize) {
        let n = self.by.unsigned_abs();
        if self.by > 0 {
            (self.start, self.end + n - 1)
        } else {
            (self.start - n, self.end - 1)
        }
    }

    /// The bytes that move it, with the pen already reset: the region set,
    /// the cursor to its top, and the rows deleted there, or inserted.
    fn emit(&self, out: &mut Vec<u8>) {
        let (top, bot) = self.region();
        out.extend_from_slice(b"\x1b[");
        push_dec(out, top + 1);
        out.push(b';');
        push_dec(out, bot + 1);
        out.push(b'r');
        // Setting the region sends the cursor home: the screen's top left on
        // some terminals and the region's on others, and to its top row on
        // both only where that is the screen's.
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

/// Row `r` of `frame`.
fn row(frame: &Frame, r: usize) -> &[Out] {
    &frame.cells[r * frame.width..(r + 1) * frame.width]
}

/// A print of a row's text: the same for rows of the same text, and seldom
/// for any others. The colours are left out: cheaper to leave to the cell by
/// cell check of the rows a print finds than to fold into every one.
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

/// One more word into a print.
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

    /// A frame of these rows, 12 columns wide.
    fn frame(rows: &[&str]) -> Frame {
        frame_of(12, rows)
    }

    /// A frame of these rows, `width` columns wide.
    fn frame_of(width: usize, rows: &[&str]) -> Frame {
        let mut f = Frame::new(width, rows.len(), Style::default());
        for (r, text) in rows.iter().enumerate() {
            for (c, ch) in text.chars().enumerate() {
                f.cells[r * width + c].text = Text::Char(ch);
            }
        }
        f
    }

    /// `rows` lines of their own text, `line 0` and on, from `first`.
    fn lines(first: usize, rows: usize) -> Vec<String> {
        (first..first + rows).map(|n| format!("line {n}")).collect()
    }

    fn refs(rows: &[String]) -> Vec<&str> {
        rows.iter().map(String::as_str).collect()
    }

    /// A screen that moves the rows a frame moved, and a terminal behind it.
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

        /// Draw `f`, and say what the screen wrote for it.
        fn draw(&mut self, f: &Frame) -> String {
            let bytes = self.screen.draw(f.clone(), &HIDDEN, true, &[]);
            self.parser.process(&bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        }

        /// What the terminal shows, a row to a line, blanks at the end of
        /// one left out.
        fn text(&self) -> String {
            let contents = self.parser.screen().contents();
            contents
                .lines()
                .map(str::trim_end)
                .collect::<Vec<_>>()
                .join("\n")
        }

        /// Every cell the terminal shows: its text — a blank where it has
        /// none, as a terminal shows a cell it erased — and its colours and
        /// attributes.
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

    /// A window scrolled up a line has its rows moved up by the terminal —
    /// a region round the window, a line deleted at its top — and the line
    /// it uncovered written, and nothing else: no row it moved is written
    /// again. The region is put back before anything is written.
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

    /// A line opened in a window has the rows under it moved down by the
    /// terminal, the region from it to the window's foot and a line inserted
    /// at its top, and the line opened written.
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

    /// Once rows are moved, what the terminal shows is known row by row: the
    /// row shown that far away where the scroll moved one, and blanks where
    /// it emptied one — what the frame is diffed against, and what a second
    /// band is checked against. Either way the rows move.
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
        assert!(bands.moved(&mut Vec::new(), &before, &up));
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
        assert!(bands.moved(&mut Vec::new(), &before, &down));
        for r in 0..2 {
            assert!(bands.was(&before, r).iter().all(|c| *c == blank), "row {r}");
        }
        for r in 2..9 {
            assert_eq!(bands.was(&before, r), row(&before, r - 2), "row {r}");
        }
    }

    /// A scroll of the whole screen moves every row but the last, which the
    /// relay's hint bar may be over: that row is written, never scrolled.
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

    /// Two windows, one over the other, scrolled different ways are two
    /// bands, each moved by the terminal.
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

    /// Too few rows moved to be worth a scroll are written, as ever.
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

    /// Rows of the same text in other colours are not the rows shown: a row
    /// whose colours changed as it moved is written, and the band is moved
    /// around it.
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

    /// The rows a scroll empties are blank in the terminal's own colours,
    /// and what the frame has there in its own — a theme's background — is
    /// written over them.
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

    /// Off, a screen moves nothing: every row a frame changed is written.
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

    /// A screen forgotten paints every cell again, and moves nothing on the
    /// way: what the terminal shows is not known.
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

    /// The next of a few hundred frames, made by a small random walk: a band
    /// of rows moved some way up or down with what it uncovers made up, cells
    /// changed here and there, a row recoloured, a character two cells wide
    /// put in, or nothing at all.
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

        /// Row `r` made up afresh: text from a small alphabet, so that rows
        /// of the same text come about, and the odd wide character.
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

    /// Whatever a frame moved, and however, the terminal shows after every
    /// frame what it shows with no row moved: every cell, its colours and
    /// its attributes — over thousands of frames of a random walk, in which
    /// rows are moved by the terminal many hundreds of times.
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
