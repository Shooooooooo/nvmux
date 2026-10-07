//! Scrolling ahead of a slow link: a key that scrolls, or a turn of the
//! wheel, scrolls the window at once, before Neovim has heard of it, and
//! Neovim's own scroll, when it comes, lands where the client already is.
//!
//! Neovim decides what a window shows, and over ssh the client hears what it
//! decided a round trip after the key. For most keys that is the only honest
//! answer. A scroll is one of the few whose outcome the client can work out
//! for itself: where the view goes, and the cursor with it, follows from
//! where they are, the window's height and a handful of options, by rules
//! that are Neovim's own and are written out here ([`outcome`]) as Neovim
//! 0.12 was found to keep them —
//!
//! - `<C-e>`, `<C-y>` and the wheel (`'mousescroll'` lines) by lines, no
//!   further than the last line at the top or the first;
//! - `<C-d>` and `<C-u>` by `'scroll'`, the cursor as far, no further down
//!   than to show the last line at the bottom;
//! - `<C-f>`, `<C-b>`, `<PageDown>`, `<PageUp>`, `<S-Down>`, `<S-Up>` and the
//!   wheel with Shift or Ctrl by pages, two lines kept, the cursor to the top
//!   of the new view or the bottom;
//! - and `zt`, `zz`, `zb`, `z<CR>`, `z.` and `z-`, the cursor's line to the
//!   top, the middle or the bottom;
//!
//! — with `'scrolloff'` keeping the cursor in from the edges, and a count
//! where Neovim takes one.
//!
//! Most of the rows a scroll shows are rows the window already shows, moved.
//! What the client does not have is the lines it uncovers. So the agent the
//! client leaves in the editor (see `super::AGENT_LUA`) sends it the text of
//! the lines around each window's view — a page each way, and as the view
//! moves, only the lines it has not sent — with what it takes to draw them as
//! the window would ([`Shape`]). A row drawn from that is the line's text and
//! its number, in the colours the window's own rows are drawn in, and nothing
//! more: no syntax colours, no signs, no virtual text. It is on the screen for
//! the round trip it takes Neovim's own row to arrive.
//!
//! # Only where it can be right
//!
//! A scroll is predicted only where the window's rows and its buffer's lines
//! are one to one: no closed folds, no virtual lines, no diff, no line too
//! long for a window that wraps — the agent checks the lines it sends, and
//! `win_viewport` the view ([`super::model::Viewport::one_to_one`]) — and
//! where nothing maps the key or the wheel. And only once the link is slow
//! enough to be worth it ([`ON`]): what a prediction costs is a few rows
//! without their colours for as long as the link takes, which on a fast one
//! is a flicker, and nothing gained.
//!
//! A key is a scroll only in normal or visual mode with nothing pending, and
//! the client cannot see Neovim's pending state, only its own keys: so it
//! follows them ([`Typed`]), and predicts nothing from where it loses track,
//! nor while a key before has yet to be drawn.
//!
//! Neovim always has the last word. What it scrolls is taken off what the
//! client is ahead by; a prediction it has not confirmed in two round trips
//! and a little more is let go, and the view goes back to Neovim's (see
//! `super::anim::scroll`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rmpv::ValueRef;
use unicode_width::UnicodeWidthChar;

use super::grid::{Cell, Text};
use super::redraw;

/// Predictions start once the smoothed round trip reaches this, and stop
/// once it falls back under [`OFF`]: the gap keeps a link that hovers about
/// the one value from turning them on and off.
pub const ON: Duration = Duration::from_millis(30);
pub const OFF: Duration = Duration::from_millis(20);

/// How long past two round trips a prediction waits for Neovim to confirm
/// it.
const GRACE: Duration = Duration::from_millis(250);

/// How often the round trip is measured again while the user is at work.
pub const PING_EVERY: Duration = Duration::from_secs(2);

/// The most lines kept for a window. The agent sends far fewer; this is what
/// keeps a client from growing should it ever not.
const MAX_LINES: usize = 4096;

/// What it takes to draw a window's lines as the window would, as the agent
/// last said it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    /// The buffer the window shows, and how many lines it had.
    pub buf: i64,
    pub line_count: i64,
    /// The gutter's width: fold, sign and number columns.
    pub textoff: usize,
    pub tabstop: usize,
    pub number: bool,
    pub relativenumber: bool,
    /// No `'statuscolumn'`: the gutter is Neovim's own, and its number is
    /// where the client would put it.
    pub plain_gutter: bool,
    pub wrap: bool,
    /// The columns scrolled off the left of a window that does not wrap.
    pub leftcol: usize,
    /// Rows and lines are one to one around the view, and the buffer is not
    /// a terminal's: see the module docs.
    pub predictable: bool,
    pub scrolloff: usize,
    /// `'mousescroll'`'s `ver`: how many lines a turn of the wheel scrolls.
    pub step: i64,
    /// `'fillchars'`' `eob`: what a row past the buffer's end starts with.
    pub eob: Text,
    /// `'list'`, and its `'listchars'` `tab`, empty for none.
    pub list: bool,
    pub tab: Vec<char>,
    /// `'scroll'`: how far `<C-d>` and `<C-u>` go.
    pub scroll: i64,
    /// How far a page goes when `'window'` says, not the window's height:
    /// nought for the window's height.
    pub page: i64,
    /// `'startofline'`: a page or half of one takes the cursor to its line's
    /// first non-blank.
    pub startofline: bool,
    /// Which of the keys the client follows ([`KEYS`]) something maps, in
    /// the window's buffer or everywhere — or starts a mapping with: Neovim
    /// does with them what the mapping says.
    pub mapped: Vec<String>,
}

impl Shape {
    /// As the agent sends it: `[buf, line_count, textoff, tabstop, number,
    /// relativenumber, plain_gutter, wrap, leftcol, predictable, scrolloff,
    /// step, eob, list, tab, scroll, page, startofline, mapped]`.
    fn read(v: &[ValueRef<'_>]) -> Option<Self> {
        let int = |i: usize| v.get(i).and_then(redraw::int);
        let size = |i: usize| int(i).and_then(|n| usize::try_from(n).ok());
        let flag = |i: usize| matches!(v.get(i), Some(ValueRef::Boolean(true)));
        let text = |i: usize| v.get(i).and_then(redraw::as_str).unwrap_or("");
        Some(Self {
            buf: int(0)?,
            line_count: int(1)?,
            textoff: size(2)?,
            tabstop: size(3)?.max(1),
            number: flag(4),
            relativenumber: flag(5),
            plain_gutter: flag(6),
            wrap: flag(7),
            leftcol: size(8)?,
            predictable: flag(9),
            scrolloff: size(10)?,
            step: int(11)?,
            eob: match Text::new(text(12)) {
                Text::Half => Text::Char(' '),
                t => t,
            },
            list: flag(13),
            tab: text(14).chars().collect(),
            scroll: int(15).unwrap_or(0),
            page: int(16).unwrap_or(0),
            startofline: flag(17),
            mapped: v
                .get(18)
                .and_then(ValueRef::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(redraw::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

/// The lines the agent has sent of a window's buffer: a run of them from
/// `first`, as they were at `tick`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Lines {
    buf: i64,
    tick: i64,
    first: i64,
    text: Vec<String>,
}

impl Lines {
    fn end(&self) -> i64 {
        self.first + self.text.len() as i64
    }

    /// Take a run of lines the agent sent. The agent keeps the same account
    /// of what the client has and follows the same rule: a run of the same
    /// buffer as it was at the same change, touching what is here, joins it;
    /// anything else — or a run it says is to start afresh — replaces it.
    fn take(&mut self, buf: i64, tick: i64, first: i64, text: Vec<String>, fresh: bool) {
        let end = first + text.len() as i64;
        let joins = !fresh
            && buf == self.buf
            && tick == self.tick
            && first <= self.end()
            && end >= self.first;
        let joined = (self.end().max(end) - self.first.min(first)) as usize;
        if !joins || joined > MAX_LINES {
            *self = Lines {
                buf,
                tick,
                first,
                text,
            };
            return;
        }
        let start = self.first.min(first);
        let mut all = vec![String::new(); joined];
        for (i, line) in std::mem::take(&mut self.text).into_iter().enumerate() {
            all[(self.first - start) as usize + i] = line;
        }
        for (i, line) in text.into_iter().enumerate() {
            all[(first - start) as usize + i] = line;
        }
        self.first = start;
        self.text = all;
    }

    fn get(&self, buf: i64, lnum: i64) -> Option<&str> {
        if buf != self.buf || lnum < self.first {
            return None;
        }
        self.text
            .get((lnum - self.first) as usize)
            .map(String::as_str)
    }
}

/// The colours a window's rows are drawn in, read off the rows it shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Look {
    /// Each column of the gutter, in the highlight most rows have there.
    pub gutter: Vec<u32>,
    /// The text, in the highlight most rows end in: the window's `Normal`.
    pub text: u32,
    /// A row past the buffer's end, if the window shows one; or the
    /// highlight to draw one in.
    pub eob: Option<Vec<Cell>>,
    pub eob_hl: u32,
}

impl Look {
    /// Read the colours off `rows`, a window's rows from its top, of which the
    /// first `lines` show the buffer's lines and the rest are past its end —
    /// leaving out the row at `skip`, the cursor's, which `'cursorline'`
    /// draws differently. `eob_hl` is `EndOfBuffer`, for a window that shows
    /// no row past the end.
    pub fn of(
        rows: &[Vec<Cell>],
        lines: usize,
        textoff: usize,
        skip: Option<usize>,
        eob_hl: u32,
    ) -> Self {
        let text_rows: Vec<&Vec<Cell>> = rows
            .iter()
            .take(lines)
            .enumerate()
            .filter(|(r, _)| Some(*r) != skip)
            .map(|(_, row)| row)
            .collect();
        let gutter = (0..textoff)
            .map(|c| most(text_rows.iter().filter_map(|row| row.get(c).map(|x| x.hl))))
            .collect();
        let text = most(text_rows.iter().filter_map(|row| row.last().map(|x| x.hl)));
        let eob = rows.get(lines).cloned();
        let eob_hl = eob
            .as_ref()
            .and_then(|row| row.first())
            .map_or(eob_hl, |c| c.hl);
        Self {
            gutter,
            text,
            eob,
            eob_hl,
        }
    }
}

/// The value seen most often, the lowest of a tie; nought for none.
fn most(values: impl Iterator<Item = u32>) -> u32 {
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for v in values {
        *counts.entry(v).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|(a, n), (b, m)| n.cmp(m).then(b.cmp(a)))
        .map_or(0, |(v, _)| v)
}

/// Buffer line `lnum`, whose text is `text` — `None` for a line past the
/// buffer's end — drawn `width` cells wide as a row of a window of `shape`
/// would draw it, with the cursor on line `cursor`. `None` where a window
/// that wraps would take more than one row for it.
pub fn render(
    text: Option<&str>,
    shape: &Shape,
    lnum: i64,
    width: usize,
    look: &Look,
    cursor: i64,
) -> Option<Vec<Cell>> {
    let Some(text) = text else {
        if let Some(row) = look.eob.as_ref().filter(|row| row.len() == width) {
            return Some(row.clone());
        }
        let mut row = vec![
            Cell {
                text: Text::Char(' '),
                hl: look.eob_hl,
            };
            width
        ];
        if let Some(first) = row.first_mut() {
            first.text = shape.eob.clone();
        }
        return Some(row);
    };
    let textoff = shape.textoff.min(width);
    let mut row: Vec<Cell> = (0..textoff)
        .map(|c| Cell {
            text: Text::Char(' '),
            hl: look.gutter.get(c).copied().unwrap_or(look.text),
        })
        .collect();
    if shape.plain_gutter && (shape.number || shape.relativenumber) {
        let n = if shape.relativenumber && lnum != cursor {
            (lnum - cursor).abs()
        } else {
            lnum + 1
        };
        // Right-aligned, a blank after it, at the end of the gutter: the
        // number column is the last of the gutter's. The cursor's own line
        // with both options set is aligned left, which only a prediction
        // that moves the cursor onto a line it uncovers gets wrong.
        let digits = n.to_string();
        if let Some(start) = textoff.checked_sub(digits.len() + 1) {
            for (i, d) in digits.chars().enumerate() {
                row[start + i].text = Text::Char(d);
            }
        }
    }
    let room = width - textoff;
    let fits = line(text, shape, look.text, room, &mut row);
    if shape.wrap && !fits {
        return None;
    }
    row.resize(
        width,
        Cell {
            text: Text::Char(' '),
            hl: look.text,
        },
    );
    Some(row)
}

/// Append the cells of a line's text in highlight `hl`, from display column
/// `shape.leftcol`, `room` of them at most. Says whether the whole line fits
/// in `room`.
fn line(text: &str, shape: &Shape, hl: u32, room: usize, row: &mut Vec<Cell>) -> bool {
    let from = shape.leftcol;
    let shown = |vcol: usize| vcol >= from && vcol - from < room;
    let put = |row: &mut Vec<Cell>, vcol: usize, text: Text| {
        if shown(vcol) {
            row.push(Cell { text, hl });
        }
    };
    let mut vcol = 0usize;
    // Where the last character went, for a combining mark after it.
    let mut last: Option<usize> = None;
    for c in text.chars() {
        if c.width() != Some(0) {
            last = shown(vcol).then_some(row.len());
        }
        match c {
            '\t' if shape.list && shape.tab.is_empty() => {
                put(row, vcol, Text::Char('^'));
                put(row, vcol + 1, Text::Char('I'));
                vcol += 2;
            }
            '\t' => {
                let n = shape.tabstop - vcol % shape.tabstop;
                for i in 0..n {
                    let ch = match (shape.list, shape.tab.as_slice()) {
                        (false, _) | (true, []) => ' ',
                        (true, [first, ..]) if i == 0 => *first,
                        (true, [_, _, last]) if i == n - 1 => *last,
                        (true, [_, fill, ..]) => *fill,
                        (true, [only]) => *only,
                    };
                    put(row, vcol + i, Text::Char(ch));
                }
                vcol += n;
            }
            // The C0 controls and DEL as `^X`, the C1 as `<xx>`, as Neovim
            // shows them.
            '\0'..='\x1f' | '\x7f' => {
                put(row, vcol, Text::Char('^'));
                put(row, vcol + 1, Text::Char((c as u8 ^ 0x40) as char));
                vcol += 2;
            }
            _ => match c.width() {
                None => {
                    for ch in format!("<{:02x}>", c as u32).chars() {
                        put(row, vcol, Text::Char(ch));
                        vcol += 1;
                    }
                }
                // A combining mark goes with the character before it, if
                // that was drawn.
                Some(0) => {
                    if let Some(cell) = last.and_then(|i| row.get_mut(i)) {
                        let mut b = Vec::new();
                        cell.text.push_to(&mut b);
                        let mut s = String::from_utf8(b).unwrap_or_default();
                        s.push(c);
                        cell.text = Text::Cluster(s.into());
                    }
                }
                Some(w) => {
                    put(row, vcol, Text::Char(c));
                    for i in 1..w {
                        put(row, vcol + i, Text::Half);
                    }
                    vcol += w;
                }
            },
        }
    }
    vcol <= room
}

/// A scroll as Neovim will make it, from a key and a count and the window's
/// options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// `<C-e>`, `<C-y>`, a turn of the wheel: lines, down the buffer above
    /// nought. The cursor stays on its line, if `'scrolloff'` lets it.
    Lines(i64),
    /// `<C-d>`, `<C-u>`: `'scroll'` lines, or as many as a count says, which
    /// is `'scroll'` from then on. The cursor goes as far.
    Half { down: bool, count: Option<i64> },
    /// `<C-f>`, `<C-b>`, `<PageDown>`, `<PageUp>`, `<S-Down>`, `<S-Up>`, the
    /// wheel with Shift or Ctrl: pages. The cursor goes to the top of the
    /// view, or the bottom.
    Page { down: bool, count: i64 },
    /// `zt`, `zz`, `zb`: the cursor's line to the top of the view, the
    /// middle, the bottom; `first` — `z<CR>`, `z.`, `z-` — takes the cursor to
    /// the line's first non-blank as well.
    Line { to: Edge, first: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Middle,
    Bottom,
}

/// Where a view is, as far as a prediction is concerned: the line at its
/// top and the cursor's, from nought.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct At {
    pub top: i64,
    pub cursor: i64,
}

/// Where `command` takes a view of `rows` rows at `at` over `line_count`
/// lines, a line to a row, in a window of `shape` — as Neovim's
/// `scroll_redraw`, `pagescroll` and `scroll_cursor_top`, `_halfway` and
/// `_bot` take it, and as Neovim 0.12 was found to. `None` where the client
/// cannot say.
pub fn outcome(command: Command, at: At, rows: i64, line_count: i64, shape: &Shape) -> Option<At> {
    let (h, last) = (rows.max(1), (line_count - 1).max(0));
    let so = (shape.scrolloff as i64).min((h - 1) / 2);
    // `cursor_correct`: `so` lines from the top unless the top is the first
    // line, from the bottom unless the last line is in view; on a line.
    let correct = |top: i64, cursor: i64| {
        let mut c = cursor.clamp(0, last);
        if top > 0 {
            c = c.max(top + so);
        }
        if top + h - 1 < last {
            c = c.min(top + h - 1 - so);
        }
        c.clamp(0, last)
    };
    let to = match command {
        Command::Lines(n) => {
            let top = (at.top + n).clamp(0, last);
            At {
                top,
                cursor: correct(top, at.cursor),
            }
        }
        Command::Half { down: true, count } => {
            let s = count.unwrap_or(shape.scroll).clamp(1, h);
            // Not so far as to show rows past the end: the cursor still goes
            // the whole way.
            let top = at.top + s.min(line_count - at.top - h).max(0);
            At {
                top,
                cursor: correct(top, at.cursor + s),
            }
        }
        Command::Half { down: false, count } => {
            let s = count.unwrap_or(shape.scroll).clamp(1, h);
            let top = (at.top - s).max(0);
            At {
                top,
                cursor: correct(top, at.cursor - s),
            }
        }
        Command::Page { down, count } => {
            // `get_scroll_overlap`: two lines kept, one in a window of four,
            // none in a smaller one — and none once the last line is in view,
            // which puts it at the top.
            let page = match h {
                _ if shape.page > 0 => shape.page,
                _ if down && at.top + h >= line_count => h,
                5.. => h - 2,
                4 => 3,
                _ => h,
            };
            let top = if down {
                (at.top + count * page).min(last)
            } else {
                (at.top - count * page).max(0)
            };
            // A page that goes nowhere leaves the cursor where it was.
            match (top == at.top, down) {
                (true, _) => at,
                (false, true) => At {
                    top,
                    cursor: correct(top, top),
                },
                (false, false) => At {
                    top,
                    cursor: correct(top, (top + h - 1).min(last)),
                },
            }
        }
        Command::Line { to, .. } => {
            // A 'scrolloff' as tall as half the window keeps the cursor in
            // the middle by rules of its own.
            if shape.scrolloff as i64 > (h - 1) / 2 {
                return None;
            }
            let c = at.cursor;
            let top = match to {
                Edge::Top => c - so,
                Edge::Middle => c - (h - 1) / 2,
                Edge::Bottom => c - (h - 1) + so.min(last - c),
            };
            At {
                top: top.max(0),
                cursor: c,
            }
        }
    };
    Some(to)
}

/// The display columns of a line's characters, as a window of `shape` lays
/// them out from its first column: where each starts, how wide it is, and
/// what it is. Combining marks take no column and are left out.
fn columns(text: &str, shape: &Shape) -> Vec<(usize, usize, char)> {
    let mut out = Vec::new();
    let mut vcol = 0;
    for c in text.chars() {
        let w = match c {
            '\t' if shape.list && shape.tab.is_empty() => 2,
            '\t' => shape.tabstop - vcol % shape.tabstop,
            '\0'..='\x1f' | '\x7f' => 2,
            _ => match c.width() {
                None => format!("<{:02x}>", c as u32).len(),
                Some(0) => continue,
                Some(w) => w,
            },
        };
        out.push((vcol, w, c));
        vcol += w;
    }
    out
}

/// Where in a line, in display columns, the cursor sits on a character: a
/// tab's last column, which is where Neovim shows it in normal mode, or the
/// character's first.
fn sits(start: usize, width: usize, c: char, shape: &Shape) -> usize {
    if c == '\t' && !shape.list {
        start + width - 1
    } else {
        start
    }
}

/// Where on a line the cursor lands, in display columns, wanting to be at
/// `want`: on the character there, or on the last of a line too short.
pub fn landing(text: &str, shape: &Shape, want: usize) -> usize {
    let cols = columns(text, shape);
    let at = cols
        .iter()
        .find(|(start, w, _)| want < start + w)
        .or(cols.last());
    at.map_or(0, |&(start, w, c)| sits(start, w, c, shape))
}

/// Where a line's first non-blank character is, in display columns — or
/// its last character, for a line all blank.
pub fn first_nonblank(text: &str, shape: &Shape) -> usize {
    let cols = columns(text, shape);
    let at = cols
        .iter()
        .find(|(_, _, c)| !matches!(c, ' ' | '\t'))
        .or(cols.last());
    at.map_or(0, |&(start, w, c)| sits(start, w, c, shape))
}

/// Keys that, typed in normal or visual mode with nothing pending, do all
/// they do at once and leave nothing pending — so that the client, having
/// seen one go, knows Neovim is waiting for a fresh command after it.
const WHOLE: &[&str] = &[
    "h", "j", "k", "l", "w", "b", "e", "W", "B", "E", "0", "^", "$", "G", "H", "M", "L", "n", "N",
    "*", "#", "%", "(", ")", "{", "}", "+", "-", "_", "|", ";", ",", "x", "X", "p", "P", "u", "J",
    "~", ".", "v", "V", "<C-v>", "<CR>", "<BS>", " ", "<Space>", "<Left>", "<Right>", "<Up>",
    "<Down>", "<Home>", "<End>", "<Del>", "<C-n>", "<C-p>", "<C-h>", "<C-j>", "<C-r>", "<Esc>",
    "<C-c>",
];

/// The keys that scroll, by themselves.
const SCROLLS: &[&str] = &[
    "<C-e>",
    "<C-y>",
    "<C-d>",
    "<C-u>",
    "<C-f>",
    "<C-b>",
    "<PageDown>",
    "<PageUp>",
    "<S-Down>",
    "<S-Up>",
];

/// The `z` commands that scroll, after their `z`.
const Z: &[&str] = &["t", "<CR>", "z", ".", "b", "-"];

/// The wheel, as a mapping names it.
const WHEEL: &[&str] = &[
    "<ScrollWheelDown>",
    "<ScrollWheelUp>",
    "<S-ScrollWheelDown>",
    "<S-ScrollWheelUp>",
    "<C-ScrollWheelDown>",
    "<C-ScrollWheelUp>",
];

/// Every key, and `z` command, a mapping of which changes what the client
/// makes of it: what the agent is asked to look up (see [`Shape::mapped`]).
pub fn keys() -> Vec<String> {
    let digits = (0..10).map(|d| d.to_string());
    let z = Z.iter().map(|k| format!("z{k}"));
    WHOLE
        .iter()
        .chain(SCROLLS)
        .chain(WHEEL)
        .map(|k| k.to_string())
        .chain(z)
        .chain(digits)
        .collect()
}

/// The name a mapping of the wheel turned `dir` (1 down, -1 up) with
/// modifiers `mods` has, as in [`WHEEL`].
pub fn wheel(dir: i64, mods: &str) -> String {
    let way = if dir > 0 { "Down" } else { "Up" };
    match mods {
        "" => format!("<ScrollWheel{way}>"),
        m => format!("<{m}-ScrollWheel{way}>"),
    }
}

/// The keys in a run of them, in Neovim's notation: a character, or a name
/// in `<…>` — `<lt>` for a `<` itself.
pub fn tokens(keys: &str) -> impl Iterator<Item = &str> {
    let mut rest = keys;
    std::iter::from_fn(move || {
        let c = rest.chars().next()?;
        let end = match c {
            '<' => rest.find('>').map_or(rest.len(), |i| i + 1),
            _ => c.len_utf8(),
        };
        let (key, tail) = rest.split_at(end);
        rest = tail;
        Some(key)
    })
}

/// The longest count kept: past it, a count is as good as endless.
const MAX_COUNT: i64 = 9999;

/// What the client makes of the keys it sends: enough of Neovim's normal
/// mode to know a scroll when one is typed — and to know when it cannot
/// tell.
///
/// From a key it cannot follow on — one that enters insert mode, waits for
/// another, or that something maps — it is lost, and predicts nothing until
/// it knows Neovim is waiting for a fresh command again: after an `<Esc>`,
/// or once a fence it asked after the keys has been answered (see
/// `super::App`), Neovim then in normal or visual mode and the last key one
/// it knows leaves nothing pending.
///
/// The fence is for something else as well. A key other than a scroll moves
/// the cursor, or the view, in a way the client does not predict, and until
/// Neovim has drawn what it did, the client's picture of where things are is
/// out of date: a scroll is not predicted from it ([`Typed::sure`]).
/// Neovim answers a request only once it has taken every key sent before it,
/// and drawn what they did, so the answer to one is when the picture is
/// Neovim's again.
#[derive(Debug, Clone, Default)]
pub struct Typed {
    state: State,
    /// Inputs sent that move things in ways the client does not predict,
    /// counted; and how many of them Neovim is known to have drawn.
    sent: u64,
    drawn: u64,
    /// The fence asked after and not yet answered: the inputs counted when
    /// it was asked.
    fence: Option<u64>,
    /// The last of them leaves nothing pending.
    whole: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    /// Waiting for a command, with the count typed so far.
    Ready(Option<i64>),
    /// A `z`, and the count before it.
    Z(Option<i64>),
    #[default]
    Lost,
}

impl Typed {
    /// A key typed — `mapped` says whether something maps it — as Neovim
    /// in normal or visual mode will take it: the scroll it makes, if it
    /// makes one the client can follow.
    pub fn key(&mut self, key: &str, mapped: impl Fn(&str) -> bool) -> Option<Command> {
        match self.state {
            State::Ready(count) => {
                let d = key
                    .parse::<i64>()
                    .ok()
                    .filter(|d| key.len() == 1 && (*d > 0 || count.is_some()) && !mapped(key));
                if let Some(d) = d {
                    let n = (count.unwrap_or(0) * 10 + d).min(MAX_COUNT);
                    self.state = State::Ready(Some(n));
                    return None;
                }
                if key == "z" {
                    self.state = State::Z(count);
                    return None;
                }
                match scroll(key, count).filter(|_| !mapped(key)) {
                    Some(command) => {
                        self.state = State::Ready(None);
                        self.whole = true;
                        Some(command)
                    }
                    None => {
                        self.moved(key, &mapped);
                        None
                    }
                }
            }
            State::Z(count) => {
                let command = Z
                    .iter()
                    .position(|z| *z == key)
                    .filter(|_| !mapped(&format!("z{key}")));
                match (command, count) {
                    (Some(i), None) => {
                        self.state = State::Ready(None);
                        self.whole = true;
                        let to = [Edge::Top, Edge::Middle, Edge::Bottom][i / 2];
                        Some(Command::Line {
                            to,
                            first: i % 2 == 1,
                        })
                    }
                    // With a count, the cursor goes to that line first.
                    (Some(_), Some(_)) => {
                        self.sent += 1;
                        self.whole = true;
                        self.state = State::Ready(None);
                        None
                    }
                    (None, _) => {
                        self.sent += 1;
                        self.whole = false;
                        self.state = State::Lost;
                        None
                    }
                }
            }
            State::Lost => {
                self.moved(key, &mapped);
                None
            }
        }
    }

    /// A key that moves things the client does not predict.
    fn moved(&mut self, key: &str, mapped: &impl Fn(&str) -> bool) {
        self.sent += 1;
        self.whole = (WHOLE.contains(&key) || SCROLLS.contains(&key)) && !mapped(key);
        self.state = match self.state {
            _ if self.whole && matches!(key, "<Esc>" | "<C-c>") => State::Ready(None),
            State::Ready(_) if self.whole => State::Ready(None),
            _ => State::Lost,
        };
    }

    /// Something else that moves things — a click, a paste — or a scroll
    /// the client made out but did not predict.
    pub fn missed(&mut self) {
        self.sent += 1;
    }

    /// A click or a paste, which may change the mode as well.
    pub fn other(&mut self) {
        self.sent += 1;
        self.whole = true;
        self.state = State::Lost;
    }

    /// A fresh attach: nothing is known of what is pending.
    pub fn attached(&mut self) {
        self.sent += 1;
        self.whole = true;
        self.state = State::Lost;
    }

    /// Whether what the client has of the editor is Neovim's last word on
    /// every key sent, and Neovim is waiting for a fresh command: a scroll
    /// can be predicted from it.
    pub fn sure(&self) -> bool {
        self.drawn == self.sent && self.state != State::Lost
    }

    /// Whether to ask after a fence: there are inputs Neovim is not known to
    /// have drawn, and none is asked after already.
    pub fn wants_fence(&self) -> bool {
        self.fence.is_none() && self.drawn < self.sent
    }

    /// A fence is asked after: says the count it is for.
    pub fn fenced(&mut self) -> u64 {
        self.fence = Some(self.sent);
        self.sent
    }

    /// The fence for the inputs counted `at` is answered, Neovim in normal or
    /// visual mode if `normal`.
    pub fn answered(&mut self, at: u64, normal: bool) {
        self.fence = None;
        self.drawn = self.drawn.max(at);
        self.settle(normal);
    }

    /// Neovim has drawn, and is in normal or visual mode if `normal`: with
    /// every input drawn and the last leaving nothing pending, it is waiting
    /// for a fresh command. A fence can be answered before the frame that
    /// says which mode Neovim is in, which is why this is asked again at
    /// every frame.
    pub fn settle(&mut self, normal: bool) {
        if self.drawn == self.sent && self.state == State::Lost && self.whole && normal {
            self.state = State::Ready(None);
        }
    }
}

/// The scroll a key that scrolls by itself makes, with `count`.
fn scroll(key: &str, count: Option<i64>) -> Option<Command> {
    let n = count.unwrap_or(1);
    Some(match key {
        "<C-e>" => Command::Lines(n),
        "<C-y>" => Command::Lines(-n),
        "<C-d>" => Command::Half { down: true, count },
        "<C-u>" => Command::Half { down: false, count },
        "<C-f>" | "<PageDown>" | "<S-Down>" => Command::Page {
            down: true,
            count: n,
        },
        "<C-b>" | "<PageUp>" | "<S-Up>" => Command::Page {
            down: false,
            count: n,
        },
        _ => return None,
    })
}

/// Everything the client keeps for its predictions.
#[derive(Debug, Default)]
pub struct Predictor {
    /// The round trip to the server, smoothed as TCP smooths its own.
    srtt: Option<Duration>,
    /// Whether it is long enough to predict over: see [`ON`].
    on: bool,
    shapes: HashMap<i64, Shape>,
    lines: HashMap<i64, Lines>,
    /// When the round trip was last asked after.
    pinged: Option<Instant>,
    /// What the client makes of the keys it sends.
    pub typed: Typed,
}

impl Predictor {
    /// A round trip, as one request and its answer took.
    pub fn sample(&mut self, rtt: Duration) {
        let srtt = match self.srtt {
            None => rtt,
            Some(s) => (s * 7 + rtt) / 8,
        };
        self.srtt = Some(srtt);
        self.on = srtt >= if self.on { OFF } else { ON };
    }

    /// Whether the link is slow enough to predict over.
    pub fn active(&self) -> bool {
        self.on
    }

    /// When a prediction made `now` is given up on, unconfirmed.
    pub fn deadline(&self, now: Instant) -> Instant {
        now + self.srtt.unwrap_or_default() * 2 + GRACE
    }

    /// Whether to ask after the round trip again: the user has been at work
    /// since it was last asked after, and long enough ago.
    pub fn wants_ping(&self, now: Instant, typed: Option<Instant>) -> bool {
        typed.is_some_and(|t| self.pinged.is_none_or(|p| t > p && now >= p + PING_EVERY))
    }

    pub fn pinged(&mut self, now: Instant) {
        self.pinged = Some(now);
    }

    /// What the agent says that is the predictions': `lines` and `shape`.
    /// Says whether it was.
    pub fn said(&mut self, what: &str, v: &[ValueRef<'_>]) -> bool {
        let int = |i: usize| v.get(i).and_then(redraw::int);
        match what {
            "lines" => {
                let (Some(win), Some(buf), Some(tick), Some(first)) =
                    (int(0), int(1), int(2), int(3))
                else {
                    return true;
                };
                let text = v
                    .get(4)
                    .and_then(ValueRef::as_array)
                    .map(|a| a.iter().map(lossy).collect())
                    .unwrap_or_default();
                let fresh = matches!(v.get(5), Some(ValueRef::Boolean(true)));
                self.lines
                    .entry(win)
                    .or_default()
                    .take(buf, tick, first, text, fresh);
                true
            }
            "shape" => {
                let shape = v
                    .get(1)
                    .and_then(ValueRef::as_array)
                    .and_then(|a| Shape::read(a));
                if let (Some(win), Some(shape)) = (int(0), shape) {
                    self.shapes.insert(win, shape);
                }
                true
            }
            _ => false,
        }
    }

    /// A window closed.
    pub fn closed(&mut self, win: i64) {
        self.shapes.remove(&win);
        self.lines.remove(&win);
    }

    /// A fresh attach: its agent will say everything again.
    pub fn reset(&mut self) {
        self.shapes.clear();
        self.lines.clear();
    }

    pub fn shape(&self, win: i64) -> Option<&Shape> {
        self.shapes.get(&win)
    }

    /// A key typed while window `win` has the cursor, as [`Typed::key`]
    /// takes it, with that window's mappings.
    pub fn key(&mut self, win: Option<i64>, key: &str) -> Option<Command> {
        let shape = win.and_then(|w| self.shapes.get(&w));
        let mapped = |k: &str| shape.is_some_and(|s| s.mapped.iter().any(|m| m == k));
        self.typed.key(key, mapped)
    }

    /// A count given `<C-d>` or `<C-u>` is window `win`'s `'scroll'` from
    /// then on.
    pub fn set_scroll(&mut self, win: i64, scroll: i64) {
        if let Some(s) = self.shapes.get_mut(&win) {
            s.scroll = scroll;
        }
    }

    /// The text of buffer line `lnum` of window `win`, if the agent has sent
    /// it.
    pub fn text(&self, win: i64, lnum: i64) -> Option<&str> {
        let shape = self.shapes.get(&win)?;
        self.lines.get(&win)?.get(shape.buf, lnum)
    }

    /// Buffer line `lnum` of window `win` as a row of it would draw it — see
    /// [`render`] — or `None` where the client cannot say: a line the agent
    /// has not sent.
    pub fn row(
        &self,
        win: i64,
        lnum: i64,
        width: usize,
        look: &Look,
        cursor: i64,
    ) -> Option<Vec<Cell>> {
        let shape = self.shapes.get(&win)?;
        if lnum < 0 {
            return None;
        }
        let text = if lnum >= shape.line_count {
            None
        } else {
            Some(self.lines.get(&win)?.get(shape.buf, lnum)?)
        };
        render(text, shape, lnum, width, look, cursor)
    }
}

/// A line's text, whatever its bytes: a line of a binary file is still a
/// line to draw.
fn lossy(v: &ValueRef<'_>) -> String {
    match v {
        ValueRef::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
        ValueRef::Binary(b) => String::from_utf8_lossy(b).into_owned(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmpv::Value;

    fn shape() -> Shape {
        Shape {
            buf: 1,
            line_count: 100,
            textoff: 4,
            tabstop: 8,
            number: true,
            relativenumber: false,
            plain_gutter: true,
            wrap: true,
            leftcol: 0,
            predictable: true,
            scrolloff: 0,
            step: 3,
            eob: Text::Char('~'),
            list: false,
            tab: Vec::new(),
            scroll: 11,
            page: 0,
            startofline: false,
            mapped: Vec::new(),
        }
    }

    fn look() -> Look {
        Look {
            gutter: vec![18; 4],
            text: 0,
            eob: None,
            eob_hl: 18,
        }
    }

    fn text(row: &[Cell]) -> String {
        let mut b = Vec::new();
        for c in row {
            c.text.push_to(&mut b);
        }
        String::from_utf8(b).unwrap()
    }

    /// The number right-aligned in the gutter, a blank after it, as Neovim
    /// draws `'number'`; the text in the window's own colour.
    #[test]
    fn a_row_has_its_number_and_its_text() {
        let row = render(Some("let x = 1;"), &shape(), 22, 20, &look(), 3).unwrap();
        assert_eq!(text(&row), " 23 let x = 1;      ");
        assert!(row[..4].iter().all(|c| c.hl == 18));
        assert!(row[4..].iter().all(|c| c.hl == 0));
    }

    /// `'relativenumber'` counts from the cursor's line.
    #[test]
    fn relative_numbers_count_from_the_cursor() {
        let s = Shape {
            relativenumber: true,
            ..shape()
        };
        let row = render(Some("x"), &s, 22, 8, &look(), 30).unwrap();
        assert_eq!(text(&row), "  8 x   ");
        let s = Shape {
            number: false,
            relativenumber: false,
            ..shape()
        };
        let row = render(Some("x"), &s, 22, 8, &look(), 30).unwrap();
        assert_eq!(text(&row), "    x   ");
    }

    /// Tabs to the next stop, controls as `^X`, a character two cells wide as
    /// two, and a combining mark with the character before it.
    #[test]
    fn text_is_laid_out_as_neovim_lays_it_out() {
        let s = Shape {
            textoff: 0,
            tabstop: 4,
            ..shape()
        };
        let row = render(Some("a\tb\x01c"), &s, 0, 12, &look(), 0).unwrap();
        assert_eq!(text(&row), "a   b^Ac    ");
        let row = render(Some("日x"), &s, 0, 5, &look(), 0).unwrap();
        assert_eq!(row[0].text, Text::Char('日'));
        assert_eq!(row[1].text, Text::Half);
        assert_eq!(row[2].text, Text::Char('x'));
        let row = render(Some("e\u{301}x"), &s, 0, 3, &look(), 0).unwrap();
        assert_eq!(row[0].text, Text::Cluster("e\u{301}".into()));
        assert_eq!(row[1].text, Text::Char('x'));
        let listed = Shape {
            list: true,
            tab: vec!['>', '-'],
            ..s.clone()
        };
        let row = render(Some("\tx"), &listed, 0, 6, &look(), 0).unwrap();
        assert_eq!(text(&row), ">---x ");
    }

    /// A window that wraps takes no line longer than it is wide; one that
    /// does not cuts it off, from where it is scrolled to sideways.
    #[test]
    fn a_long_line_is_cut_off_or_refused() {
        let s = Shape {
            textoff: 0,
            ..shape()
        };
        assert_eq!(render(Some("abcdefgh"), &s, 0, 6, &look(), 0), None);
        let s = Shape {
            wrap: false,
            leftcol: 2,
            ..s
        };
        let row = render(Some("abcdefgh"), &s, 0, 4, &look(), 0).unwrap();
        assert_eq!(text(&row), "cdef");
    }

    /// Past the buffer's end, a row as the window draws one: its own if it
    /// shows one, else `'fillchars'`' `eob` in `EndOfBuffer`.
    #[test]
    fn past_the_end_is_the_windows_own_filler() {
        let row = render(None, &shape(), 100, 4, &look(), 0).unwrap();
        assert_eq!(text(&row), "~   ");
        assert!(row.iter().all(|c| c.hl == 18));
        let own = vec![
            Cell {
                text: Text::Char('!'),
                hl: 7,
            };
            4
        ];
        let l = Look {
            eob: Some(own.clone()),
            ..look()
        };
        assert_eq!(render(None, &shape(), 100, 4, &l, 0), Some(own));
    }

    /// The colours are read off the rows the window has: the gutter column
    /// by column, the text by how its rows end, the cursor's row and those
    /// past the end left out of both.
    #[test]
    fn the_colours_are_the_windows_own() {
        let cell = |hl| Cell {
            text: Text::Char('x'),
            hl,
        };
        let row = |g: u32, t: u32| vec![cell(g), cell(g), cell(t), cell(t)];
        let rows = vec![row(5, 0), row(9, 9), row(5, 0), row(5, 0), row(3, 3)];
        let l = Look::of(&rows, 4, 2, Some(1), 18);
        assert_eq!(l.gutter, vec![5, 5]);
        assert_eq!(l.text, 0);
        assert_eq!(l.eob, Some(row(3, 3)));
        assert_eq!(l.eob_hl, 3);
        assert_eq!(Look::of(&rows, 5, 2, None, 18).eob_hl, 18);
    }

    /// What Neovim 0.12 did, found by running each command in a window of
    /// `h` rows over 100 lines with `'scrolloff'` at `so`: from a view whose
    /// top line and cursor are given, counted from one as Neovim counts them,
    /// to the view it left.
    #[test]
    fn scrolls_go_where_neovim_takes_them() {
        use Command::*;
        use Edge::*;
        let half = |down| Half { down, count: None };
        let page = |down, count| Page { down, count };
        let line = |to| Line { to, first: false };
        // 'scrolloff', rows, what is typed, from (top, cursor), to the same.
        type Case = (usize, i64, Command, (i64, i64), (i64, i64));
        #[rustfmt::skip]
        let cases: &[Case] = &[
            (0, 22, Lines(1), (40, 50), (41, 50)),
            (0, 22, Lines(5), (40, 50), (45, 50)),
            (0, 22, Lines(-1), (40, 50), (39, 50)),
            (0, 22, half(true), (40, 50), (51, 61)),
            (0, 22, half(false), (40, 50), (29, 39)),
            (0, 22, Half { down: true, count: Some(7) }, (40, 50), (47, 57)),
            (0, 22, page(true, 1), (40, 50), (60, 60)),
            (0, 22, page(false, 1), (40, 50), (20, 41)),
            (0, 22, page(true, 2), (40, 50), (80, 80)),
            (0, 22, line(Top), (40, 50), (50, 50)),
            (0, 22, line(Middle), (40, 50), (40, 50)),
            (0, 22, line(Bottom), (40, 50), (29, 50)),
            // At the end.
            (0, 22, Lines(1), (85, 90), (86, 90)),
            (0, 22, Lines(1), (98, 100), (99, 100)),
            (0, 22, Lines(1), (79, 100), (80, 100)),
            (0, 22, Lines(10), (95, 98), (100, 100)),
            (0, 22, half(true), (85, 90), (85, 100)),
            (0, 22, half(true), (79, 100), (79, 100)),
            (0, 22, half(true), (66, 74), (77, 85)),
            (0, 22, half(true), (70, 78), (79, 89)),
            (0, 22, half(true), (75, 83), (79, 94)),
            (0, 22, half(true), (78, 86), (79, 97)),
            (0, 22, page(true, 1), (85, 90), (100, 100)),
            (0, 22, page(true, 1), (98, 100), (100, 100)),
            (0, 22, page(true, 1), (79, 100), (100, 100)),
            (0, 22, page(true, 1), (70, 80), (90, 90)),
            (0, 22, page(true, 1), (40, 61), (60, 60)),
            (0, 22, line(Top), (85, 90), (90, 90)),
            (0, 22, line(Top), (79, 100), (100, 100)),
            (0, 22, line(Middle), (85, 90), (80, 90)),
            (0, 22, line(Middle), (98, 100), (90, 100)),
            (0, 22, line(Bottom), (85, 90), (69, 90)),
            (0, 22, line(Bottom), (98, 100), (79, 100)),
            (0, 22, line(Bottom), (79, 100), (79, 100)),
            // At the start.
            (0, 22, Lines(-1), (1, 3), (1, 3)),
            (0, 22, Lines(-1), (3, 4), (2, 4)),
            (0, 22, Lines(-1), (2, 20), (1, 20)),
            (0, 22, half(false), (1, 3), (1, 1)),
            (0, 22, half(false), (3, 4), (1, 1)),
            (0, 22, half(false), (2, 20), (1, 9)),
            (0, 22, half(false), (40, 45), (29, 34)),
            (0, 22, page(false, 1), (1, 3), (1, 3)),
            (0, 22, page(false, 1), (3, 4), (1, 22)),
            (0, 22, page(false, 1), (2, 20), (1, 22)),
            (0, 22, page(false, 1), (40, 58), (20, 41)),
            (0, 22, line(Top), (1, 3), (3, 3)),
            (0, 22, line(Middle), (1, 3), (1, 3)),
            (0, 22, line(Middle), (3, 4), (1, 4)),
            (0, 22, line(Middle), (2, 20), (10, 20)),
            (0, 22, line(Bottom), (3, 4), (1, 4)),
            (0, 22, line(Bottom), (2, 20), (1, 20)),
            // 'scrolloff'.
            (5, 22, Lines(1), (40, 50), (41, 50)),
            (5, 22, half(true), (40, 50), (51, 61)),
            (5, 22, half(true), (70, 78), (79, 89)),
            (5, 22, page(true, 1), (40, 50), (60, 65)),
            (5, 22, page(true, 1), (45, 61), (65, 70)),
            (5, 22, page(true, 1), (70, 80), (90, 95)),
            (5, 22, page(true, 2), (40, 50), (80, 85)),
            (5, 22, page(false, 1), (40, 50), (20, 36)),
            (5, 22, page(false, 1), (42, 58), (22, 38)),
            (5, 22, page(false, 1), (4, 20), (1, 17)),
            (5, 22, line(Top), (40, 50), (45, 50)),
            (5, 22, line(Bottom), (40, 50), (34, 50)),
            (5, 22, Lines(1), (85, 90), (86, 91)),
            (5, 22, Lines(1), (95, 100), (96, 100)),
            (5, 22, Lines(10), (93, 98), (100, 100)),
            (5, 22, line(Top), (85, 90), (85, 90)),
            (5, 22, line(Top), (79, 100), (95, 100)),
            (5, 22, line(Middle), (95, 100), (90, 100)),
            (5, 22, line(Bottom), (85, 90), (74, 90)),
            (5, 22, line(Bottom), (95, 100), (79, 100)),
            (5, 22, Lines(-1), (4, 20), (3, 19)),
            (5, 22, half(false), (4, 20), (1, 9)),
            (5, 22, line(Top), (4, 20), (15, 20)),
            (5, 22, line(Middle), (4, 20), (10, 20)),
            (5, 22, line(Bottom), (4, 20), (4, 20)),
            // Small windows.
            (0, 21, half(true), (40, 50), (50, 60)),
            (0, 21, page(true, 1), (40, 50), (59, 59)),
            (0, 21, page(false, 1), (40, 50), (21, 41)),
            (1, 21, line(Bottom), (40, 50), (31, 50)),
            (0, 5, half(true), (48, 50), (50, 52)),
            (0, 5, page(true, 1), (48, 50), (51, 51)),
            (0, 5, page(false, 1), (48, 50), (45, 49)),
            (1, 5, line(Bottom), (48, 50), (47, 50)),
            (0, 4, half(true), (49, 50), (51, 52)),
            (0, 4, page(true, 1), (49, 50), (52, 52)),
            (0, 4, page(false, 1), (49, 50), (46, 49)),
            (1, 4, line(Bottom), (49, 50), (48, 50)),
            (0, 3, half(true), (49, 50), (50, 51)),
            (0, 3, page(true, 1), (49, 50), (52, 52)),
            (0, 3, page(false, 1), (49, 50), (46, 48)),
            (1, 3, line(Bottom), (49, 50), (49, 50)),
        ];
        for &(so, h, command, (top, cursor), want) in cases {
            let s = Shape {
                scrolloff: so,
                scroll: h / 2,
                ..shape()
            };
            let at = At {
                top: top - 1,
                cursor: cursor - 1,
            };
            let got = outcome(command, at, h, 100, &s).map(|a| (a.top + 1, a.cursor + 1));
            assert_eq!(
                got,
                Some(want),
                "{command:?} from {top}, {cursor} in {h} rows, so={so}"
            );
        }
    }

    /// Where the cursor lands on a line: on the character there, a tab's last
    /// column, the last character of a line too short; the first non-blank.
    #[test]
    fn the_cursor_lands_where_neovim_puts_it() {
        let s = shape();
        assert_eq!(landing("abcdef", &s, 3), 3);
        assert_eq!(landing("ab", &s, 3), 1);
        assert_eq!(landing("", &s, 3), 0);
        assert_eq!(landing("\tx", &s, 3), 7, "on a tab, at its end");
        assert_eq!(landing("日本", &s, 3), 2, "on the second, wide");
        assert_eq!(first_nonblank("    x", &s), 4);
        assert_eq!(first_nonblank("\t x", &s), 9);
        assert_eq!(first_nonblank("   ", &s), 2, "all blank: the last");
    }

    #[test]
    fn keys_are_split_as_neovim_writes_them() {
        let all: Vec<&str> = tokens("5<C-e>z<CR>j<lt>日").collect();
        assert_eq!(all, ["5", "<C-e>", "z", "<CR>", "j", "<lt>", "日"]);
    }

    fn typed(t: &mut Typed, keys: &str) -> Vec<Command> {
        tokens(keys).filter_map(|k| t.key(k, |_| false)).collect()
    }

    /// Scrolls are known by their keys, with their counts and their `z`s;
    /// the keys between them are followed, and a key that cannot be loses
    /// the client until Neovim is known to have caught up.
    #[test]
    fn the_keys_that_scroll_are_told_apart() {
        let mut t = Typed::default();
        assert!(typed(&mut t, "<C-e>").is_empty(), "lost to begin with");
        let at = t.fenced();
        t.answered(at, true);
        assert!(t.sure());
        assert_eq!(
            typed(&mut t, "3<C-e><C-y>10<C-d>zz z.<C-b>"),
            [
                Command::Lines(3),
                Command::Lines(-1),
                Command::Half {
                    down: true,
                    count: Some(10)
                },
                Command::Line {
                    to: Edge::Middle,
                    first: false
                },
                Command::Line {
                    to: Edge::Middle,
                    first: true
                },
                Command::Page {
                    down: false,
                    count: 1
                },
            ]
        );
        assert!(!t.sure(), "the space moved the cursor");
        assert!(t.wants_fence());
        let at = t.fenced();
        assert!(!t.wants_fence(), "one at a time");
        t.answered(at, true);
        assert!(t.sure());
        // A key that waits for another: the scroll after it is not one.
        assert!(typed(&mut t, "f<C-e>").is_empty());
        assert!(!t.sure());
        let at = t.fenced();
        t.answered(at, true);
        assert!(t.sure(), "and once Neovim has caught up, nothing pending");
        // One whose next key the client cannot place.
        assert!(typed(&mut t, "ma").is_empty());
        let at = t.fenced();
        t.answered(at, true);
        assert!(!t.sure(), "'a' may yet be waiting for something");
        assert!(typed(&mut t, "<Esc>").is_empty());
        let at = t.fenced();
        t.answered(at, true);
        assert!(t.sure(), "an escape leaves nothing pending");
        // Insert mode, as far as the client can tell, until it is left.
        assert!(typed(&mut t, "i<C-e>").is_empty());
        let at = t.fenced();
        t.answered(at, false);
        assert!(!t.sure());
        // A mapped key is one the client cannot follow.
        assert!(typed(&mut t, "<Esc>").is_empty());
        let at = t.fenced();
        t.answered(at, true);
        let mapped: Vec<Command> = tokens("<C-d>j<C-e>")
            .filter_map(|k| t.key(k, |k| k == "<C-d>"))
            .collect();
        assert!(mapped.is_empty());
        // A fence answered before the last keys were drawn says nothing of
        // them.
        let at = t.fenced();
        typed(&mut t, "j");
        t.answered(at, true);
        assert!(!t.sure());
    }

    fn said(p: &mut Predictor, what: &str, args: Vec<Value>) {
        let refs: Vec<ValueRef<'_>> = args.iter().map(Value::as_ref).collect();
        assert!(p.said(what, &refs));
    }

    fn lines(win: i64, tick: i64, first: i64, text: &[&str], fresh: bool) -> Vec<Value> {
        vec![
            win.into(),
            1.into(),
            tick.into(),
            first.into(),
            Value::Array(text.iter().map(|t| Value::from(*t)).collect()),
            fresh.into(),
        ]
    }

    fn shape_args(line_count: i64) -> Vec<Value> {
        let v: Vec<Value> = vec![
            1.into(),
            line_count.into(),
            0.into(),
            8.into(),
            false.into(),
            false.into(),
            true.into(),
            false.into(),
            0.into(),
            true.into(),
            2.into(),
            3.into(),
            "~".into(),
            false.into(),
            "".into(),
        ];
        vec![1000.into(), Value::Array(v)]
    }

    /// Runs of lines join what is kept where they touch it, at the same
    /// change of the same buffer; anything else starts afresh.
    #[test]
    fn lines_join_where_they_touch() {
        let mut p = Predictor::default();
        said(&mut p, "shape", shape_args(10));
        assert_eq!(p.shape(1000).map(|s| s.step), Some(3));
        let row = |p: &Predictor, lnum| {
            p.row(1000, lnum, 3, &look(), 0)
                .map(|r| text(&r).trim_end().to_string())
        };
        said(&mut p, "lines", lines(1000, 5, 2, &["c", "d"], true));
        said(&mut p, "lines", lines(1000, 5, 4, &["e"], false));
        said(&mut p, "lines", lines(1000, 5, 0, &["a", "b"], false));
        let got: Vec<_> = (0..5).map(|l| row(&p, l)).collect();
        let want: Vec<_> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|s| Some(s.to_string()))
            .collect();
        assert_eq!(got, want);
        assert_eq!(row(&p, 5), None, "never sent");
        assert_eq!(row(&p, 10).as_deref(), Some("~"), "past the end");
        // Another change: what was kept is gone.
        said(&mut p, "lines", lines(1000, 6, 8, &["i"], false));
        assert_eq!(row(&p, 2), None);
        assert_eq!(row(&p, 8).as_deref(), Some("i"));
        // Not touching: the same.
        said(&mut p, "lines", lines(1000, 6, 1, &["b"], false));
        assert_eq!(row(&p, 8), None);
        p.closed(1000);
        assert_eq!(row(&p, 1), None);
    }

    /// Predictions start past [`ON`] and stop under [`OFF`], the round trip
    /// smoothed.
    #[test]
    fn a_slow_link_turns_predictions_on() {
        let mut p = Predictor::default();
        p.sample(Duration::from_millis(5));
        assert!(!p.active());
        p.sample(Duration::from_millis(400));
        assert!(p.active(), "smoothed past 30 ms");
        for _ in 0..12 {
            p.sample(Duration::from_millis(25));
        }
        assert!(p.active(), "between the two: still on");
        for _ in 0..40 {
            p.sample(Duration::from_millis(5));
        }
        assert!(!p.active());
        let t0 = Instant::now();
        assert!(!p.wants_ping(t0, None), "nobody at work");
        assert!(p.wants_ping(t0, Some(t0)));
        p.pinged(t0);
        assert!(!p.wants_ping(t0 + PING_EVERY, Some(t0)), "nothing since");
        let typed = t0 + Duration::from_millis(10);
        assert!(!p.wants_ping(typed, Some(typed)), "too soon");
        assert!(p.wants_ping(t0 + PING_EVERY, Some(typed)));
    }
}
