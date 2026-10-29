//! Client-side prediction for slow links: scrolls, cursor motions and
//! Insert-mode typing are drawn at once, and Neovim's redraw then lands where
//! the client already is.
//!
//! # Scrolls
//!
//! [`outcome`] and [`across`] reproduce Neovim 0.12's rules for `<C-e>`,
//! `<C-y>`, `<C-d>`, `<C-u>`, the page keys, `zt`/`zz`/`zb` and kin, the
//! sideways `z` commands and the wheel, in Normal and Insert mode, with
//! `'scrolloff'`, `'sidescrolloff'` and counts. Scrolls chain up to 32 windows
//! ahead of Neovim (see `super::anim::scroll::HOLD`). A scroll to another line
//! puts the cursor in its wanted column ([`Predictor::want`], [`showing`]).
//!
//! The agent (`super::AGENT_LUA`) sends each window's lines around its view,
//! as far as a held key pages in a round trip ([`Predictor::reach`]), as
//! incremental splices, with drawing options ([`Shape`]) and tree-sitter and
//! extmark highlights. A predicted row shows only text, number and those
//! colours (no signs, virtual text or `:syntax`) until Neovim's row arrives.
//! Highlighting costs Neovim ~60 µs a line, so a newly shown buffer gets two
//! windows each way first and the rest later in batches.
//!
//! Scrolls are predicted only where rows and lines are one to one (see
//! [`super::model::Viewport::one_to_one`]), the key is unmapped, and the
//! client can tell from its own keys that nothing is pending ([`Typed`]).
//! Neovim always wins: an unconfirmed prediction is dropped after two round
//! trips plus a margin ([`Predictor::deadline`]). Neovim sends no frame while
//! working through typed-ahead keys, so the agent reports its view moves and
//! waiting predictions get a fresh deadline; with acks, one past its deadline
//! waits until a fence says Neovim has taken its key, however long Neovim is
//! busy (see `super::anim::Animator::idle`).
//!
//! # Acks, motions and typing
//!
//! With `[client] predict_motions` or `predict_typing`, the agent sends an
//! [`Ack`] whenever Neovim waits for input with nothing pending: keys taken,
//! the last few, and the resulting cursor, view and mode. [`Sync`] ties that
//! count to the client's key numbers at quiet fences. From each ack the
//! client replays the keys sent since, so a key it cannot follow costs one
//! round trip. Normal-mode motions ([`super::moves`], [`super::words`]) and
//! Insert-mode typing ([`typing`]) are drawn ahead where the client can tell
//! exactly what Neovim will do, judged against each ack ([`Shadow`]), and
//! must prove themselves again after a miss ([`Trust`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use rmpv::ValueRef;
use unicode_width::UnicodeWidthChar;

use super::grid::{Cell, Text};
use super::moves;
use super::redraw;
use super::typing;
use crate::config::Predictions;

/// Slack past two round trips, or 4× the round-trip variation if larger.
const GRACE: Duration = Duration::from_millis(250);

/// Linux's minimum TCP retransmission timeout: without it, predictions that
/// waited out a lost segment would be dropped just before Neovim's answer.
const LOSS: Duration = Duration::from_millis(200);

/// Ping interval while the user is active.
pub const PING_EVERY: Duration = Duration::from_secs(2);

/// Key-repeat pages a second the agent's lookahead is sized for.
const PAGES_A_SECOND: u128 = 50;

/// Cap on [`Predictor::reach`] (a 400+ ms round trip). Below
/// `super::anim::scroll::HOLD`, so pages past the lines are still predicted
/// and held until the lines arrive.
const MOST_REACH: i64 = 24;

/// Hard cap per window; the agent itself keeps at most 4000.
const MAX_LINES: usize = 8192;

/// A window's drawing options, as last sent by the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    pub buf: i64,
    pub line_count: i64,
    /// Gutter width: fold, sign and number columns.
    pub textoff: usize,
    pub tabstop: usize,
    pub number: bool,
    pub relativenumber: bool,
    /// No `'statuscolumn'`, so the number is where the client would put it.
    pub plain_gutter: bool,
    pub wrap: bool,
    pub leftcol: usize,
    /// Rows and lines are one to one and the buffer is not a terminal.
    pub predictable: bool,
    pub scrolloff: usize,
    /// `'mousescroll'` `ver`: lines per wheel turn.
    pub step: i64,
    /// `'fillchars'` `eob`: first cell of rows past the buffer's end.
    pub eob: Text,
    pub list: bool,
    pub tab: Vec<char>,
    pub lcs: Listchars,
    /// Neovim inspects every character for its own highlighting
    /// (`'linebreak'`, `'spell'`, syntax), so blanks too: see [`Blanks`].
    pub checks: bool,
    pub scroll: i64,
    /// Page size from `'window'`; 0 means the window's height.
    pub page: i64,
    pub startofline: bool,
    /// Followed keys ([`keys`]) mapped, or starting a mapping, in Normal or
    /// Visual mode.
    pub mapped: Vec<String>,
    /// The same for Insert-mode keys ([`insert_keys`]).
    pub imapped: Vec<String>,
    /// `'sidescrolloff'`, `'sidescroll'`, `'mousescroll'` `hor`.
    pub sidescrolloff: usize,
    pub sidescroll: usize,
    pub hor: i64,
    /// Sent only when acks are on.
    pub stepping: Option<Stepping>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stepping {
    pub whichwrap: String,
    pub virtualedit: String,
    pub cursorcolumn: bool,
    /// `'scrollbind'` or `'cursorbind'`.
    pub bound: bool,
    pub langmap: bool,
    /// Neovim may measure a character's width differently from the client:
    /// `'ambiwidth'` double, `setcellwidths()`, or `'emoji'` off.
    pub widths: bool,
    /// Minimum, including the blank after the number.
    pub numberwidth: usize,
    /// `'signcolumn'` `number`: signs drawn over line numbers.
    pub number_signs: bool,
    pub cursorline: Cursorline,
    pub idioms: Vec<Idiom>,
    /// `'winhighlight'` or a window namespace: the cursor line's gutter and
    /// bar use groups named by `hl_group_set` (see
    /// `super::model::Model::groups`).
    pub own_highlights: bool,
    /// The cursor line conceals differently (`'concealcursor'` without `n`).
    pub conceals: bool,
    /// 0-based display columns, `'textwidth'`-relative ones included.
    pub colorcolumn: Vec<usize>,
    /// Negative is a percentage of the window height.
    pub scrolljump: i64,
    /// `'conceallevel'` > 0: uncovered lines may conceal text the client shows.
    pub concealing: bool,
    /// `'iskeyword'` class per character below 256 (see
    /// `super::words::class`); empty if not sent.
    pub classes: Vec<u8>,
    /// `'iminsert'` is 1: `:lmap` applies to the character after `f` etc.
    pub lmap: bool,
    /// `None` if not sent.
    pub sections: Option<String>,
    pub paragraphs: Option<String>,
    /// In a wrapping window: Neovim centres views by rows.
    pub smoothscroll: bool,
}

/// `'cursorlineopt'` parts drawn (none when off); `number` covers
/// `CursorLineNr`, `CursorLineSign` and `CursorLineFold`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cursorline {
    pub number: bool,
    pub line: bool,
    pub screenline: bool,
}

impl Cursorline {
    /// From the agent's bit flags: 1 `number`, 2 `line`, 4 `screenline`.
    fn read(flags: i64) -> Self {
        Self {
            number: flags & 1 != 0,
            line: flags & 2 != 0,
            screenline: flags & 4 != 0,
        }
    }
}

/// How `j`, `k`, `<Down>` or `<Up>` is mapped. `alone`: no longer mapping
/// starts with it, so Neovim does not wait for another key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Idiom {
    pub key: String,
    pub rhs: String,
    pub expr: bool,
    pub noremap: bool,
    pub alone: bool,
}

impl Idiom {
    /// From the agent's `[key, rhs, expr, noremap, alone]`.
    fn read(v: &ValueRef<'_>) -> Option<Self> {
        let a = v.as_array()?;
        let text = |i: usize| a.get(i).and_then(redraw::as_str).map(String::from);
        let flag = |i: usize| matches!(a.get(i), Some(ValueRef::Boolean(true)));
        Some(Self {
            key: text(0)?,
            rhs: text(1)?,
            expr: flag(2),
            noremap: flag(3),
            alone: flag(4),
        })
    }

    /// Whether the key maps exactly to `gj`/`gk`, always or (as LazyVim does)
    /// only without a count, non-recursive and with no longer mapping
    /// pending: over one-row lines that moves as the bare key does. The edge
    /// cases where `gj` differs from `j` (Neovim 0.12.6's `nv_screengo`) are
    /// handled in `super::App::predict_move`.
    pub fn same(&self) -> bool {
        let (key, screen) = match self.key.as_str() {
            "j" | "<Down>" => ("j", "gj"),
            "k" | "<Up>" => ("k", "gk"),
            _ => return false,
        };
        let rhs = self.rhs.as_str();
        let made = if self.expr {
            rhs == format!("v:count == 0 ? '{screen}' : '{key}'")
                || rhs == format!("v:count ? '{key}' : '{screen}'")
        } else {
            rhs == screen
        };
        made && self.noremap && self.alone
    }
}

impl Stepping {
    /// No `'virtualedit'` beyond `block`, `'cursorcolumn'`, bound windows or
    /// `'langmap'`.
    pub fn plain(&self) -> bool {
        let virtual_edit = self
            .virtualedit
            .split(',')
            .any(|v| !matches!(v, "" | "block" | "none" | "NONE"));
        !virtual_edit && !self.cursorcolumn && !self.bound && !self.langmap
    }

    pub fn macros(&self) -> Option<moves::Macros<'_>> {
        Some(moves::Macros {
            sections: self.sections.as_deref()?,
            paragraphs: self.paragraphs.as_deref()?,
        })
    }

    pub fn scrolling(&self) -> moves::Scrolling {
        moves::Scrolling {
            jump: self.scrolljump,
            smooth: self.smoothscroll,
        }
    }
}

impl Shape {
    /// From the agent's `shape` array; the [`Stepping`] fields follow from
    /// index 25 when it sends acks.
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
            lcs: Listchars::read(&strings(v.get(23))),
            checks: flag(24),
            scroll: int(15).unwrap_or(0),
            page: int(16).unwrap_or(0),
            startofline: flag(17),
            mapped: strings(v.get(18)),
            imapped: strings(v.get(19)),
            sidescrolloff: size(20).unwrap_or(0),
            sidescroll: size(21).unwrap_or(1),
            hor: int(22).unwrap_or(6),
            stepping: v.get(25).and_then(redraw::as_str).map(|ww| Stepping {
                whichwrap: ww.into(),
                virtualedit: text(26).into(),
                cursorcolumn: flag(27),
                bound: flag(28),
                langmap: flag(29),
                widths: flag(30),
                numberwidth: size(31).map_or(4, |n| n.min(MOST_NUMBERWIDTH)),
                number_signs: flag(32),
                cursorline: Cursorline::read(int(33).unwrap_or(0)),
                idioms: v
                    .get(34)
                    .and_then(ValueRef::as_array)
                    .map(|a| a.iter().filter_map(Idiom::read).collect())
                    .unwrap_or_default(),
                own_highlights: flag(35),
                conceals: flag(36),
                colorcolumn: v
                    .get(37)
                    .and_then(ValueRef::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|c| redraw::int(c).and_then(|c| usize::try_from(c).ok()))
                            .collect()
                    })
                    .unwrap_or_default(),
                scrolljump: int(38).unwrap_or(0),
                concealing: flag(39),
                classes: text(40).bytes().map(|b| b.saturating_sub(b'0')).collect(),
                lmap: flag(41),
                sections: v.get(42).and_then(redraw::as_str).map(String::from),
                paragraphs: v.get(43).and_then(redraw::as_str).map(String::from),
                smoothscroll: flag(44),
            }),
        })
    }

    /// Whether the client redraws the gutter for the cursor's line
    /// ([`regutter`]): Neovim's own, numbered relative to it or by
    /// `'cursorline'`. A `'statuscolumn'` can draw anything, so at most it
    /// is moved (`super::App::patches_of`).
    pub fn renumbered(&self) -> bool {
        let numbered = self.stepping.as_ref().is_some_and(|s| s.cursorline.number);
        self.plain_gutter && (self.relativenumber || numbered)
    }

    /// For a line sent cut short as `text`, whether the missing rest may
    /// change how `'listchars'` draws its blanks (`trail` if `text` ends
    /// blank, lead settings if it is all blank).
    fn unsure(&self, text: &str) -> bool {
        let l = &self.lcs;
        let blank = |c: char| matches!(c, ' ' | '\t');
        self.list
            && ((l.trail.is_some() && text.ends_with(blank))
                || (l.leads() && text.chars().all(blank)))
    }
}

/// `'listchars'` besides `tab`; all empty without `'list'`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listchars {
    pub space: Option<char>,
    pub multispace: Vec<char>,
    pub lead: Option<char>,
    pub leadmultispace: Vec<char>,
    pub leadtab: Vec<char>,
    pub trail: Option<char>,
    pub nbsp: Option<char>,
    pub eol: Option<char>,
    pub extends: Option<char>,
    pub precedes: Option<char>,
}

impl Listchars {
    /// From the agent's array; empty strings for none.
    fn read(v: &[String]) -> Self {
        let many = |i: usize| -> Vec<char> { v.get(i).map_or(Vec::new(), |s| s.chars().collect()) };
        let one = |i: usize| many(i).first().copied();
        Self {
            space: one(0),
            multispace: many(1),
            lead: one(2),
            leadmultispace: many(3),
            trail: one(4),
            nbsp: one(5),
            eol: one(6),
            extends: one(7),
            precedes: one(8),
            leadtab: many(9),
        }
    }

    /// Whether any lead setting depends on where a line's leading blanks end.
    fn leads(&self) -> bool {
        self.lead.is_some() || !self.leadmultispace.is_empty() || !self.leadtab.is_empty()
    }
}

fn strings(v: Option<&ValueRef<'_>>) -> Vec<String> {
    v.and_then(ValueRef::as_array)
        .map(|a| {
            a.iter()
                .filter_map(redraw::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// A highlighted byte range; `end` is `usize::MAX` to the line's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub group: u32,
}

/// A line the agent sent; later spans win. Whether it is [`ordinary`] and
/// all ASCII is worked out once, on arrival.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    text: String,
    spans: Vec<Span>,
    ordinary: bool,
    ascii: bool,
}

impl Line {
    fn new(text: String, spans: Vec<Span>) -> Self {
        Line {
            ordinary: ordinary(&text),
            ascii: text.is_ascii(),
            text,
            spans,
        }
    }
}

impl Default for Line {
    fn default() -> Self {
        Line::new(String::new(), Vec::new())
    }
}

/// From the agent's flat `[start, end, group, ...]`; end -1 is the line's end.
fn spans(v: Option<&ValueRef<'_>>) -> Vec<Span> {
    let Some(a) = v.and_then(ValueRef::as_array) else {
        return Vec::new();
    };
    a.as_chunks::<3>()
        .0
        .iter()
        .filter_map(|[start, end, group]| {
            let (start, end, group) = (redraw::int(start)?, redraw::int(end)?, redraw::int(group)?);
            Some(Span {
                start: usize::try_from(start).ok()?,
                end: usize::try_from(end).unwrap_or(usize::MAX),
                group: u32::try_from(group).ok()?,
            })
        })
        .collect()
}

/// The agent's lines of a window's buffer: from `first`, at change `tick`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Lines {
    buf: i64,
    tick: i64,
    first: i64,
    text: Vec<Line>,
    /// Lines are cut to about what the window can show, so one this long
    /// may be longer in the buffer.
    cap: usize,
}

impl Lines {
    fn end(&self) -> i64 {
        self.first + self.text.len() as i64
    }

    /// Join a run that touches these lines at the same buffer and tick (the
    /// agent's own rule); otherwise, or if `fresh`, replace them.
    fn take(&mut self, buf: i64, tick: i64, first: i64, text: Vec<Line>, cap: usize, fresh: bool) {
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
                cap,
            };
            return;
        }
        self.cap = self.cap.min(cap);
        let start = self.first.min(first);
        let mut all = vec![Line::default(); joined];
        for (i, line) in std::mem::take(&mut self.text).into_iter().enumerate() {
            all[(self.first - start) as usize + i] = line;
        }
        for (i, line) in text.into_iter().enumerate() {
            all[(first - start) as usize + i] = line;
        }
        self.first = start;
        self.text = all;
    }

    /// Apply an edit by the agent's rule, so both agree on what the client
    /// has; drop everything if it cannot apply.
    fn splice(&mut self, s: Splice) {
        let delta = s.count - (s.last - s.first);
        let touches = s.first <= self.end() && s.last >= self.first;
        if s.buf != self.buf || s.base != self.tick || (touches && s.text.len() as i64 != s.count) {
            *self = Lines::default();
            return;
        }
        if touches {
            let keep = |n: i64| n.clamp(0, self.text.len() as i64) as usize;
            let before = keep(s.first - self.first);
            let after = keep(s.last - self.first);
            let mut text = std::mem::take(&mut self.text);
            let tail = text.split_off(after);
            text.truncate(before);
            text.extend(s.text.into_iter().map(|text| Line::new(text, Vec::new())));
            text.extend(tail);
            if text.len() > MAX_LINES {
                *self = Lines::default();
                return;
            }
            self.first = self.first.min(s.first);
            self.text = text;
        } else if s.last < self.first {
            self.first += delta;
        }
        self.tick = s.tick;
        self.cap = self.cap.min(s.cap);
    }

    /// Keep only `from..to`, as the agent does; drop all if the buffer or
    /// tick differs.
    fn keep(&mut self, buf: i64, tick: i64, from: i64, to: i64) {
        if buf != self.buf || tick != self.tick {
            *self = Lines::default();
            return;
        }
        let from = from.clamp(self.first, self.end());
        let to = to.clamp(from, self.end());
        self.text.truncate((to - self.first) as usize);
        self.text.drain(..(from - self.first) as usize);
        self.first = from;
    }

    fn line(&self, buf: i64, lnum: i64) -> Option<&Line> {
        if buf != self.buf || lnum < self.first {
            return None;
        }
        self.text.get((lnum - self.first) as usize)
    }

    fn get(&self, buf: i64, lnum: i64) -> Option<&str> {
        self.line(buf, lnum).map(|l| l.text.as_str())
    }

    /// Replace highlights from `first`; skipped if the buffer or tick differs.
    fn recolour(&mut self, buf: i64, tick: i64, first: i64, spans: Vec<Vec<Span>>) {
        if buf != self.buf || tick != self.tick {
            return;
        }
        for (i, spans) in spans.into_iter().enumerate() {
            let lnum = first + i as i64;
            if lnum >= self.first {
                if let Some(line) = self.text.get_mut((lnum - self.first) as usize) {
                    line.spans = spans;
                }
            }
        }
    }

    /// Whether line `lnum` may have been cut short. A character split by the
    /// cut arrives as U+FFFD, three bytes.
    fn cut(&self, buf: i64, lnum: i64) -> bool {
        self.get(buf, lnum).is_some_and(|t| t.len() + 3 >= self.cap)
    }
}

/// Lines `first..last` at `base` become `count` lines at `tick`; `text` is
/// sent only where it touches the client's lines.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Splice {
    buf: i64,
    base: i64,
    tick: i64,
    first: i64,
    last: i64,
    count: i64,
    text: Vec<String>,
    cap: usize,
}

/// A window's highlights, read off the rows it shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Look {
    pub gutter: Vec<u32>,
    /// The window's `Normal`.
    pub text: u32,
    /// The shown past-end row, else the highlight to draw one in.
    pub eob: Option<Vec<Cell>>,
    pub eob_hl: u32,
    /// `NonText`, else the text's highlight.
    pub nontext: u32,
    /// `SpecialKey`, else the text's highlight.
    pub specialkey: u32,
    /// `Whitespace`, else the text's highlight.
    pub whitespace: u32,
    /// Text rows; sets the number width with `'relativenumber'` alone.
    pub rows: i64,
}

impl Look {
    /// Read off `rows` (the first `lines` show buffer lines), skipping the
    /// cursor's row `skip`, which `'cursorline'` draws differently. `group`
    /// maps a group name to the editor's `hl_group_set` id, if sent.
    pub fn of<R: AsRef<[Cell]>>(
        rows: &[R],
        lines: usize,
        textoff: usize,
        skip: Option<usize>,
        group: impl Fn(&str) -> Option<u32>,
    ) -> Self {
        let text_rows: Vec<&[Cell]> = rows
            .iter()
            .take(lines)
            .enumerate()
            .filter(|(r, _)| Some(*r) != skip)
            .map(|(_, row)| row.as_ref())
            .collect();
        let gutter = (0..textoff)
            .map(|c| most(text_rows.iter().filter_map(|row| row.get(c).map(|x| x.hl))))
            .collect();
        let text = most(text_rows.iter().filter_map(|row| row.last().map(|x| x.hl)));
        let eob = rows.get(lines).map(|row| row.as_ref().to_vec());
        let eob_hl = eob
            .as_ref()
            .and_then(|row| row.first())
            .map_or_else(|| group("EndOfBuffer").unwrap_or(0), |c| c.hl);
        Self {
            gutter,
            text,
            eob,
            eob_hl,
            nontext: group("NonText").unwrap_or(text),
            specialkey: group("SpecialKey").unwrap_or(text),
            whitespace: group("Whitespace").unwrap_or(text),
            rows: rows.len() as i64,
        }
    }
}

/// The most frequent value, lowest on a tie; 0 for none.
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

/// Line `lnum` (`text` is `None` past the end) as a `width`-cell row; `None`
/// if a wrapping window needs more than one row for it.
pub fn render(
    text: Option<(&str, &[Span])>,
    shape: &Shape,
    lnum: i64,
    width: usize,
    look: &Look,
    cursor: i64,
) -> Option<Vec<Cell>> {
    let Some((text, spans)) = text else {
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
        // Lay out the number column as Neovim does where its width is known.
        let ids = Gutter::default();
        let laid = numbers(lnum, cursor, shape, look.rows, &ids)
            .and_then(|cells| Some((textoff.checked_sub(cells.len())?, cells)));
        if let Some((start, cells)) = laid {
            for (cell, number) in row[start..].iter_mut().zip(cells) {
                cell.text = number.text;
            }
        } else {
            // Fallback: right-aligned; wrong for the cursor line with
            // 'relativenumber' (Neovim left-aligns it, or shows 0).
            let n = if shape.relativenumber && lnum != cursor {
                (lnum - cursor).abs()
            } else {
                lnum + 1
            };
            let digits = n.to_string();
            if let Some(start) = textoff.checked_sub(digits.len() + 1) {
                for (i, d) in digits.chars().enumerate() {
                    row[start + i].text = Text::Char(d);
                }
            }
        }
    }
    let room = width - textoff;
    let fits = line(text, spans, shape, look, room, &mut row);
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

/// Client ids (see `super::model::Model::groups`) of the gutter groups that
/// depend on the cursor's line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gutter {
    pub line_nr: u32,
    pub above: u32,
    pub below: u32,
    pub cursor_nr: u32,
    pub sign: u32,
    pub cursor_sign: u32,
    pub fold: u32,
    pub cursor_fold: u32,
}

impl Gutter {
    /// `None` unless the editor has sent every group.
    pub fn of(group: impl Fn(&str) -> Option<u32>) -> Option<Self> {
        Some(Self {
            line_nr: group("LineNr")?,
            above: group("LineNrAbove")?,
            below: group("LineNrBelow")?,
            cursor_nr: group("CursorLineNr")?,
            sign: group("SignColumn")?,
            cursor_sign: group("CursorLineSign")?,
            fold: group("FoldColumn")?,
            cursor_fold: group("CursorLineFold")?,
        })
    }
}

/// Neovim's maximum `'numberwidth'`; larger values are capped.
const MOST_NUMBERWIDTH: usize = 20;

/// Number column width as Neovim's `number_width`, trailing blank included;
/// 0 without one, `None` if `'numberwidth'` is unknown ([`Shape::stepping`]).
pub fn number_width(shape: &Shape, rows: i64) -> Option<usize> {
    if !shape.number && !shape.relativenumber {
        return Some(0);
    }
    let least = shape.stepping.as_ref()?.numberwidth.saturating_sub(1);
    let n = if shape.relativenumber && !shape.number {
        rows
    } else {
        shape.line_count
    };
    Some(n.max(0).to_string().len().max(least) + 1)
}

/// The number column on line `lnum` as Neovim draws it: right-aligned, but
/// the cursor's line is left-aligned with both 'number' and 'relativenumber'
/// and 0 with 'relativenumber' alone. `None` without one or if its width is
/// unknown.
pub fn numbers(
    lnum: i64,
    cursor: i64,
    shape: &Shape,
    rows: i64,
    ids: &Gutter,
) -> Option<Vec<Cell>> {
    let width = number_width(shape, rows)?.checked_sub(1)?;
    let rnu = shape.relativenumber;
    let away = (lnum - cursor).abs();
    let (n, left) = match (rnu, shape.number) {
        (true, _) if away > 0 => (away, false),
        (true, true) => (lnum + 1, true),
        (true, false) => (0, false),
        (false, _) => (lnum + 1, false),
    };
    let numbered = shape.stepping.as_ref().is_some_and(|s| s.cursorline.number);
    let hl = match lnum.cmp(&cursor) {
        std::cmp::Ordering::Less if rnu => ids.above,
        std::cmp::Ordering::Greater if rnu => ids.below,
        std::cmp::Ordering::Equal if numbered => ids.cursor_nr,
        _ => ids.line_nr,
    };
    // The cells of `format!("{n:>width$} ")` (`<` if `left`) cut to
    // `width + 1`, without building the `String`.
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    let mut m = n.unsigned_abs();
    loop {
        i -= 1;
        digits[i] = b'0' + (m % 10) as u8;
        m /= 10;
        if m == 0 {
            break;
        }
    }
    let sign: &[u8] = if n < 0 { b"-" } else { b"" };
    let pad = width.saturating_sub(sign.len() + digits.len() - i);
    let blanks = || std::iter::repeat_n(&b' ', pad);
    let (before, after) = if left { (0, pad) } else { (pad, 0) };
    let mut out = Vec::with_capacity(width + 1);
    out.extend(
        blanks()
            .take(before)
            .chain(sign)
            .chain(&digits[i..])
            .chain(blanks().take(after))
            .chain(b" ")
            .take(width + 1)
            .map(|&b| Cell {
                text: Text::Char(char::from(b)),
                hl,
            }),
    );
    Some(out)
}

/// Redraw gutter `drawn` of line `lnum` for the cursor moving from `from` to
/// `to`: renumber it per [`numbers`] and, when `'cursorline'` numbers the
/// cursor line, switch the sign and fold groups of the lines left and
/// entered. A number off the rules' highlight is a sign's `numhl` and keeps
/// its colours. `None` if the number column is not laid out as the client
/// would, or a sign is on a line left or entered with `'cursorline'`
/// numbering, where Neovim blends highlights the client has no id for.
pub fn regutter(
    drawn: &[Cell],
    lnum: i64,
    (from, to): (i64, i64),
    shape: &Shape,
    rows: i64,
    ids: &Gutter,
) -> Option<Vec<Cell>> {
    let numbered = shape.stepping.as_ref()?.cursorline.number;
    let marks = drawn.len().checked_sub(number_width(shape, rows)?)?;
    let at_cursor = lnum == from || lnum == to;
    let mut out = drawn.to_vec();
    if let (Some(was), Some(now)) = (
        numbers(lnum, from, shape, rows, ids),
        numbers(lnum, to, shape, rows, ids),
    ) {
        let cells = &mut out[marks..];
        if cells.iter().zip(&was).any(|(a, b)| a.text != b.text) {
            return None;
        }
        let ruled = cells.iter().zip(&was).all(|(a, b)| a.hl == b.hl);
        if !ruled && at_cursor && numbered {
            return None;
        }
        for (cell, n) in cells.iter_mut().zip(now) {
            cell.text = n.text;
            if ruled {
                cell.hl = n.hl;
            }
        }
    }
    if numbered && at_cursor && from != to {
        // Plain groups on the line left, cursor-line groups on the line
        // entered; any other highlight is a sign's, so give up.
        let pairs = [(ids.cursor_sign, ids.sign), (ids.cursor_fold, ids.fold)];
        let swaps: Vec<(u32, u32)> = if lnum == from {
            pairs.to_vec()
        } else {
            pairs.iter().map(|&(a, b)| (b, a)).collect()
        };
        for cell in &mut out[..marks] {
            let mut onto = swaps.iter().filter(|(a, _)| *a == cell.hl).map(|(_, b)| *b);
            let first = onto.next()?;
            if onto.any(|b| b != first) {
                return None;
            }
            cell.hl = first;
        }
    }
    Some(out)
}

/// Append a line's cells from display column `shape.leftcol`, at most
/// `room`, in the text highlight with `spans` over it. Returns whether the
/// whole line, with any `'listchars'` end, fits.
///
/// As Neovim: a double-width character cut by the view's edge is `>` or `<`
/// in `NonText` (half of one would shift the row; see `super::grid`); `^X`,
/// `^I` and `<xx>` are `SpecialKey`; `'listchars'` blanks are `Whitespace`,
/// and `eol`, `precedes` and `extends` are `NonText`. Neovim also blends
/// these with the window's `Normal` and the character's colours, which the
/// client has no ids for.
pub(super) fn line(
    text: &str,
    spans: &[Span],
    shape: &Shape,
    look: &Look,
    room: usize,
    row: &mut Vec<Cell>,
) -> bool {
    let from = shape.leftcol;
    let base = row.len();
    let lcs = &shape.lcs;
    let mut blanks = Blanks::of(text, spans, shape);
    let shown = |vcol: usize| vcol >= from && vcol - from < room;
    let put = |row: &mut Vec<Cell>, vcol: usize, text: Text, hl: u32| {
        if shown(vcol) {
            row.push(Cell { text, hl });
        }
    };
    let cut = |row: &mut Vec<Cell>, vcol: usize, mark: char| {
        put(row, vcol, Text::Char(mark), look.nontext);
    };
    let special = |row: &mut Vec<Cell>, vcol: usize, shown_as: char| {
        put(row, vcol, Text::Char(shown_as), look.specialkey);
    };
    let listed = |row: &mut Vec<Cell>, vcol: usize, shown_as: char| {
        put(row, vcol, Text::Char(shown_as), look.whitespace);
    };
    let mut vcol = 0usize;
    // Where the last character went, for a combining mark after it.
    let mut last: Option<usize> = None;
    let mut prev = None;
    for (at, c) in text.char_indices() {
        let hl = spans
            .iter()
            .rev()
            .find(|s| s.start <= at && at < s.end)
            .map_or(look.text, |s| super::model::syntax_hl(look.text, s.group));
        let put = |row: &mut Vec<Cell>, vcol: usize, text: Text| put(row, vcol, text, hl);
        if c.width() != Some(0) {
            last = shown(vcol).then_some(row.len());
        }
        let blank = blanks.draw(at, c, prev);
        prev = Some(c);
        if let Some(shown_as) = blank {
            listed(row, vcol, shown_as);
            vcol += 1;
            continue;
        }
        match c {
            '\t' if shape.list && shape.tab.is_empty() => {
                special(row, vcol, '^');
                special(row, vcol + 1, 'I');
                vcol += 2;
            }
            '\t' => {
                let n = shape.tabstop - vcol % shape.tabstop;
                // `leadtab` replaces `tab` before the first non-blank.
                let tab = if blanks.leading(at) && !lcs.leadtab.is_empty() {
                    &lcs.leadtab
                } else {
                    &shape.tab
                };
                for i in 0..n {
                    // A three-character `tab:` ends with its third; a
                    // one-cell tab shows only that.
                    let ch = match (shape.list, tab.as_slice()) {
                        (false, _) | (true, []) => ' ',
                        (true, [_, _, last]) if i == n - 1 => *last,
                        (true, [first, ..]) if i == 0 => *first,
                        (true, [_, fill, ..]) => *fill,
                        (true, [only]) => *only,
                    };
                    if shape.list {
                        listed(row, vcol + i, ch);
                    } else {
                        put(row, vcol + i, Text::Char(ch));
                    }
                }
                vcol += n;
            }
            // C0 controls and DEL as `^X`, C1 as `<xx>`, as Neovim shows them.
            '\0'..='\x1f' | '\x7f' => {
                special(row, vcol, '^');
                special(row, vcol + 1, (c as u8 ^ 0x40) as char);
                vcol += 2;
            }
            _ => match c.width() {
                None => {
                    for ch in format!("<{:02x}>", c as u32).chars() {
                        special(row, vcol, ch);
                        vcol += 1;
                    }
                }
                // A combining mark joins the previous character, if drawn.
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
                    let (left, right) = (shown(vcol), shown(vcol + w - 1));
                    if left && !right {
                        // Its combining marks are dropped with it.
                        cut(row, vcol, '>');
                        last = None;
                    } else if right && !left {
                        cut(row, from, '<');
                    } else {
                        put(row, vcol, Text::Char(c));
                        for i in 1..w {
                            put(row, vcol + i, Text::Half);
                        }
                    }
                    vcol += w;
                }
            },
        }
    }
    // Line end: in the first column if scrolled past it.
    let eol = lcs.eol.filter(|_| shape.list);
    if let Some(e) = eol {
        let at = if shape.wrap { vcol } else { vcol.max(from) };
        put(row, at, Text::Char(e), look.nontext);
    }
    let end = vcol + usize::from(eol.is_some());
    if shape.list && !shape.wrap {
        let nontext = |c: char| Cell {
            text: Text::Char(c),
            hl: look.nontext,
        };
        if let Some(p) = lcs.precedes.filter(|_| from > 0 && row.len() > base) {
            if row.get(base + 1).is_some_and(|c| c.text == Text::Half) {
                row[base + 1] = nontext('<');
            }
            row[base] = nontext(p);
        }
        // Applied after `precedes`, as Neovim does, for a view a column wide.
        if let Some(x) = lcs.extends.filter(|_| room > 0 && end > from + room) {
            if let Some(cell) = row.get_mut(base + room - 1) {
                if cell.text != Text::Half {
                    *cell = nontext(x);
                }
            }
        }
    }
    end <= room
}

/// How `'list'` draws a line's blanks, as Neovim 0.12's `win_line`: `trail`
/// after the last non-blank; before the first, with a lead setting,
/// `leadmultispace` (runs), `lead`, `space` or unchanged, always in
/// `Whitespace`, and `leadtab` for tabs; elsewhere `multispace` (runs) or
/// `space`; `nbsp`; nothing for a blank followed by a combining mark.
///
/// Neovim inspects blanks only if `'listchars'` has a blank setting besides
/// `leadtab`, or it inspects every character anyway ([`Shape::checks`], or
/// decorations such as tree-sitter), so with only `leadtab` leading spaces
/// are `Whitespace` on some lines only. The client takes sent colours to mean
/// decorations, which misses ones without colours, such as virtual text.
struct Blanks<'a> {
    text: &'a str,
    lcs: &'a Listchars,
    on: bool,
    /// First non-blank byte if a lead setting applies (`None` if all blank);
    /// start of the trailing blanks if `trail` is set.
    first: Option<usize>,
    trail: Option<usize>,
    /// Position in the `multispace` or `leadmultispace` cycle.
    run: usize,
}

impl<'a> Blanks<'a> {
    fn of(text: &'a str, spans: &[Span], shape: &'a Shape) -> Self {
        let lcs = &shape.lcs;
        let blank = |c: char| matches!(c, ' ' | '\t');
        let any = lcs.space.is_some()
            || !lcs.multispace.is_empty()
            || lcs.lead.is_some()
            || !lcs.leadmultispace.is_empty()
            || lcs.trail.is_some()
            || lcs.nbsp.is_some();
        Self {
            text,
            lcs,
            on: shape.list && (any || shape.checks || !spans.is_empty()),
            first: text
                .find(|c| !blank(c))
                .filter(|_| shape.list && lcs.leads()),
            trail: (shape.list && lcs.trail.is_some()).then(|| text.trim_end_matches(blank).len()),
            run: 0,
        }
    }

    /// Before the first non-blank, with a lead setting.
    fn leading(&self, at: usize) -> bool {
        self.first.is_some_and(|f| at < f)
    }

    /// What `c` at byte `at`, after `prev`, is drawn as; `None` to leave it.
    fn draw(&mut self, at: usize, c: char, prev: Option<char>) -> Option<char> {
        // A combining mark goes with the character before it.
        if !self.on || c.width() == Some(0) {
            return None;
        }
        let rest = &self.text[at + c.len_utf8()..];
        let marked = rest.chars().next().is_some_and(|n| n.width() == Some(0));
        let next = rest.chars().find(|n| n.width() != Some(0));
        let multi = c == ' ' && (next == Some(' ') || prev == Some(' '));
        if !multi {
            self.run = 0;
        }
        if marked {
            return None;
        }
        let leading = self.leading(at);
        let trailing = self.trail.is_some_and(|t| at >= t);
        let lcs = self.lcs;
        let mut turn = |list: &[char]| {
            let ch = list[self.run % list.len()];
            self.run = (self.run + 1) % list.len();
            ch
        };
        match c {
            '\u{a0}' | '\u{202f}' => lcs.nbsp,
            ' ' if leading && multi && !lcs.leadmultispace.is_empty() => {
                Some(turn(&lcs.leadmultispace))
            }
            ' ' if trailing => lcs.trail,
            ' ' if leading => Some(lcs.lead.or(lcs.space).unwrap_or(' ')),
            ' ' if multi && !lcs.multispace.is_empty() => Some(turn(&lcs.multispace)),
            ' ' => lcs.space,
            _ => None,
        }
    }
}

/// A predicted key's effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// `<C-e>`, `<C-y>`, the wheel: lines, positive down.
    Lines(i64),
    /// The Shift/Ctrl wheel in Insert mode: a window's height, 1 down, -1 up.
    Screen(i64),
    /// `<C-d>`, `<C-u>`: a count sets `'scroll'`; the cursor moves as far.
    Half { down: bool, count: Option<i64> },
    /// Page keys and the Shift/Ctrl wheel; the cursor goes to the top or
    /// bottom.
    Page { down: bool, count: i64 },
    /// `zt`/`zz`/`zb`, or with `first` `z<CR>`/`z.`/`z-` (also to the first
    /// non-blank); `line` is a count's line.
    Line {
        to: Edge,
        first: bool,
        line: Option<i64>,
    },
    /// A sideways scroll in a window that does not wrap: see [`across`].
    Side(Side),
    /// A Normal-mode motion; `screen` if from a screen-row mapping ([`Idiom`]).
    Move {
        motion: moves::Motion,
        count: Option<i64>,
        screen: bool,
    },
    /// An Insert-mode key that edits the cursor's line: see [`typing`].
    Type(typing::Edit),
    /// `i`, `a`, `A` or `I` entering Insert mode: see [`typing::entered`].
    Enter(typing::Enter),
    /// `<Esc>` leaving Insert mode: see [`typing::left`].
    Leave,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// `zl`, `zh`: columns, positive moving the view right.
    Cols(i64),
    /// `zL`, `zH`: half-widths.
    Halves(i64),
    /// `zs`, `ze`, keeping `'sidescrolloff'`.
    Start,
    End,
    /// The sideways wheel: `hor` columns, or a width with Shift/Ctrl.
    Wheel(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Middle,
    Bottom,
}

/// A view's top line and cursor line, 0-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct At {
    pub top: i64,
    pub cursor: i64,
}

/// Where `command` takes a view of `rows` rows at `at`, one row per line, as
/// Neovim 0.12's `scroll_redraw`, `pagescroll` and `scroll_cursor_*`. `None`
/// if the client cannot tell.
pub fn outcome(command: Command, at: At, rows: i64, line_count: i64, shape: &Shape) -> Option<At> {
    let (h, last) = (rows.max(1), (line_count - 1).max(0));
    let so = (shape.scrolloff as i64).min((h - 1) / 2);
    // `cursor_correct`: keep the cursor `so` lines from the top unless the
    // top is line 0, and from the bottom unless the last line is in view.
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
        Command::Lines(_) | Command::Screen(_) => {
            let n = match command {
                Command::Screen(dir) => dir * h.min(line_count - at.top),
                Command::Lines(n) => n,
                _ => 0,
            };
            let top = (at.top + n).clamp(0, last);
            At {
                top,
                cursor: correct(top, at.cursor),
            }
        }
        Command::Half { down: true, count } => {
            let s = count.unwrap_or(shape.scroll).clamp(1, h);
            // Stop before rows past the end; the cursor still moves all the
            // way.
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
            // `get_scroll_overlap`: two lines kept, one in a 4-row window, none
            // if smaller or once the last line is in view.
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
        Command::Side(_)
        | Command::Move { .. }
        | Command::Type(_)
        | Command::Enter(_)
        | Command::Leave => return None,
        Command::Line { to, line, .. } => {
            // A `'scrolloff'` over half the window has rules of its own.
            if shape.scrolloff as i64 > (h - 1) / 2 {
                return None;
            }
            let c = line.map_or(at.cursor, |l| (l - 1).clamp(0, last));
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

/// A cursor step on a line: its start byte and display column, width, and
/// character. Combining marks belong to the character before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ch {
    pub byte: usize,
    pub vcol: usize,
    pub width: usize,
    pub c: char,
}

impl Ch {
    /// The Normal-mode cursor column on it, which becomes `curswant`.
    pub fn at(&self, shape: &Shape) -> usize {
        sits(self.vcol, self.width, self.c, shape, false)
    }
}

/// Whether `c` joins the previous character: zero width, not a tab or control.
pub fn joins(c: char) -> bool {
    !matches!(c, '\t' | '\0'..='\x1f' | '\x7f') && c.width() == Some(0)
}

/// A line's characters from column 0; a leading combining mark is dropped.
pub fn chars(text: &str, shape: &Shape) -> Vec<Ch> {
    let mut out = Vec::new();
    let mut vcol = 0;
    for (byte, c) in text.char_indices() {
        if joins(c) {
            continue;
        }
        let width = cells(c, vcol, shape);
        out.push(Ch {
            byte,
            vcol,
            width,
            c,
        });
        vcol += width;
    }
    out
}

/// Display columns `c` takes at column `vcol`, as [`chars`] lays it out.
#[inline(always)]
fn cells(c: char, vcol: usize, shape: &Shape) -> usize {
    match c {
        '\t' if shape.list && shape.tab.is_empty() => 2,
        '\t' => shape.tabstop - vcol % shape.tabstop,
        '\0'..='\x1f' | '\x7f' => 2,
        _ => match c.width() {
            None => format!("<{:02x}>", c as u32).len(),
            Some(w) => w,
        },
    }
}

/// Whether Neovim 0.12 steps over the line as [`chars`] does. Not if it has
/// emoji sequences (ZWJ, flags, skin tones, VS16), scripts whose marks Neovim
/// 0.12.6 was found to join (excluded whole), interlinear annotation anchors,
/// zero-width non-marks (a soft hyphen, `<200b>`), or a combining mark on
/// nothing, a blank or a character shown as something else (after a tab or
/// control it is its own character; on a space it stops `^`).
pub fn ordinary(text: &str) -> bool {
    // The character following marks attach to.
    let mut base: Option<char> = None;
    text.chars().all(|c| {
        let joined = matches!(c,
            '\u{900}'..='\u{fff}'           // Devanagari to Tibetan
            | '\u{1000}'..='\u{109f}'       // Myanmar
            | '\u{1780}'..='\u{17ff}'       // Khmer
            | '\u{1900}'..='\u{194f}'       // Limbu
            | '\u{1a00}'..='\u{1aaf}'       // Buginese, Tai Tham
            | '\u{1b00}'..='\u{1bff}'       // Balinese, Sundanese, Batak
            | '\u{1c00}'..='\u{1c4f}'       // Lepcha
            | '\u{1cd0}'..='\u{1cff}'       // Vedic
            | '\u{2d7f}'                    // Tifinagh's joiner
            | '\u{a800}'..='\u{a82f}'       // Syloti Nagri
            | '\u{a880}'..='\u{a8df}'       // Saurashtra
            | '\u{a930}'..='\u{a95f}'       // Rejang
            | '\u{a980}'..='\u{a9df}'       // Javanese
            | '\u{aa00}'..='\u{aa5f}'       // Cham
            | '\u{aae0}'..='\u{aaff}'       // Meetei Mayek
            | '\u{abc0}'..='\u{abff}'
            | '\u{fff9}'..='\u{fffb}'       // interlinear annotation
            | '\u{11000}'..='\u{11fff}'     // Brahmi to Kawi
            | '\u{16f00}'..='\u{16f9f}'     // Miao
            | '\u{1f1e6}'..='\u{1f1ff}'     // a flag's letters
            | '\u{1f3fb}'..='\u{1f3ff}'     // skin tones
        );
        let mark = matches!(c,
            '\u{300}'..='\u{36f}'
            | '\u{1ab0}'..='\u{1aff}'
            | '\u{1dc0}'..='\u{1dff}'
            | '\u{20d0}'..='\u{20ff}'
            | '\u{fe20}'..='\u{fe2f}'
        );
        // The base must not be a space or have no width of its own (a tab or
        // control character, shown as `^X` or `<80>`).
        let carried = base.is_some_and(|b| b != ' ' && b.width().is_some_and(|w| w > 0));
        if c.width() != Some(0) {
            base = Some(c);
        }
        !joined && (c.width() != Some(0) || (mark && carried))
    })
}

/// `(start, width, char)` of each character, as in [`chars`].
fn columns(text: &str, shape: &Shape) -> Vec<(usize, usize, char)> {
    chars(text, shape)
        .into_iter()
        .map(|ch| (ch.vcol, ch.width, ch.c))
        .collect()
}

/// Whether the line fits one row: always without `'wrap'`, else if its text
/// and any `eol` fit beside the gutter.
pub fn fits(text: &str, shape: &Shape, width: usize) -> bool {
    !shape.wrap || reach(text, shape) <= width.saturating_sub(shape.textoff)
}

/// Display columns a line takes: its text plus a `'listchars'` `eol`.
pub fn reach(text: &str, shape: &Shape) -> usize {
    let eol = shape.list && shape.lcs.eol.is_some();
    width(text, shape) + usize::from(eol)
}

/// Display columns of a line's characters; matches the agent's measure of
/// Neovim's (see [`Ack::width`]) where Neovim draws nothing between them.
pub fn width(text: &str, shape: &Shape) -> usize {
    // Printable ASCII is a column a byte. `fold`, not `all`, so the check
    // does not stop early and the compiler can vectorise it.
    if text
        .bytes()
        .fold(true, |all, b| all & matches!(b, b' '..=b'~'))
    {
        return text.len();
    }
    text.chars()
        .filter(|&c| !joins(c))
        .fold(0, |vcol, c| vcol + cells(c, vcol, shape))
}

/// The display column the cursor sits in on a character: a tab's last in
/// Normal mode without `'list'`, as Neovim shows it, else the first.
fn sits(start: usize, width: usize, c: char, shape: &Shape, insert: bool) -> usize {
    if c == '\t' && !shape.list && !insert {
        start + width - 1
    } else {
        start
    }
}

/// The display column the cursor lands on wanting `want`: the character
/// there or the last one; in Insert mode, possibly just past it.
pub fn landing(text: &str, shape: &Shape, want: usize, insert: bool) -> usize {
    let cols = columns(text, shape);
    let end = cols.last().map_or(0, |(start, w, _)| start + w);
    if insert && want >= end {
        return end;
    }
    let at = cols
        .iter()
        .find(|(start, w, _)| want < start + w)
        .or(cols.last());
    at.map_or(0, |&(start, w, c)| sits(start, w, c, shape, insert))
}

/// Display column of the character at `byte` (Neovim's `curcol`), or of the
/// last one for a byte past the end.
pub fn vcol_at(text: &str, shape: &Shape, byte: i64) -> usize {
    let n = text
        .char_indices()
        .take_while(|(i, _)| (*i as i64) < byte)
        .filter(|(_, c)| c.width() != Some(0))
        .count();
    let cols = columns(text, shape);
    cols.get(n)
        .or(cols.last())
        .map_or(0, |&(start, _, _)| start)
}

/// Display column of the first non-blank, else of the last character.
pub fn first_nonblank(text: &str, shape: &Shape) -> usize {
    let cols = columns(text, shape);
    let at = cols
        .iter()
        .find(|(_, _, c)| !matches!(c, ' ' | '\t'))
        .or(cols.last());
    at.map_or(0, |&(start, w, c)| sits(start, w, c, shape, false))
}

/// A view's first display column, and the cursor's line and column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Across {
    pub left: usize,
    pub line: i64,
    pub vcol: usize,
}

/// Display span of the character at `vcol` (or the last); `(0, 0)` if empty.
fn span(cols: &[(usize, usize, char)], vcol: usize) -> (usize, usize) {
    cols.iter()
        .find(|(start, w, _)| vcol < start + w)
        .or(cols.last())
        .map_or((0, 0), |&(start, w, _)| (start, start + w - 1))
}

/// Where `side` takes a view at `at`, as Neovim 0.12's `set_leftcol`, `zs`,
/// `ze` and `do_mousescroll_horiz`. `shown` is the lines in view, for the
/// wheel's longest-line search. `None` if the client cannot tell.
pub fn across<'a>(
    side: Side,
    at: Across,
    width: usize,
    shape: &Shape,
    text: impl Fn(i64) -> Option<&'a str>,
    shown: std::ops::Range<i64>,
) -> Option<Across> {
    let tw = width.saturating_sub(shape.textoff);
    if shape.wrap || tw == 0 {
        return Some(at);
    }
    let siso = shape.sidescrolloff;
    if 2 * siso + 1 > tw {
        return None;
    }
    let cols = columns(text(at.line)?, shape);
    let shift = |n: i64| (at.left as i64 + n).max(0) as usize;
    let left = match side {
        // These only set the column.
        Side::Start => {
            let (s, _) = span(&cols, at.vcol);
            return Some(Across {
                left: s.saturating_sub(siso),
                ..at
            });
        }
        Side::End => {
            let (_, e) = span(&cols, at.vcol);
            let left = if e + siso < tw { 0 } else { e + siso - tw + 1 };
            return Some(Across { left, ..at });
        }
        Side::Cols(n) | Side::Wheel(n) => shift(n),
        Side::Halves(n) => shift(n * (width / 2) as i64),
    };
    if left == at.left {
        return Some(at);
    }
    let mut to = Across { left, ..at };
    let mut cols = cols;
    // The wheel past the end of the cursor's line moves the cursor to the
    // start of the longest line in view, the nearest on a tie.
    if matches!(side, Side::Wheel(_)) && left > end(&cols) {
        let mut best: Option<(usize, i64)> = None;
        for l in shown {
            let n = end(&columns(text(l)?, shape));
            let nearer = |b: i64| (l - at.line).abs() < (b - at.line).abs();
            if best.is_none_or(|(m, b)| n > m || (n == m && nearer(b))) {
                best = Some((n, l));
            }
        }
        to.line = best?.1;
        to.vcol = 0;
        cols = columns(text(to.line)?, shape);
    }
    // `set_leftcol`: keep the cursor `'sidescrolloff'` inside the view, on a
    // character wholly in view if there is one.
    let last = left + tw - 1;
    let land = |vcol: usize| span(&cols, vcol).0;
    let mut v = span(&cols, to.vcol).0;
    if v > last - siso {
        v = land(last - siso);
    } else if v < left + siso {
        v = land(left + siso);
    }
    let (s, e) = span(&cols, v);
    if e > last {
        v = land(s.saturating_sub(1));
    } else if s < left {
        match cols.iter().find(|(start, ..)| *start > e) {
            Some(&(next, ..)) => v = next,
            None => to.left = s,
        }
    }
    // Too near an edge, the view moves at least `'sidescroll'`; Neovim
    // centres it instead (not predicted) with that 0 or half a view away.
    if v < to.left + siso {
        let diff = to.left + siso - v;
        if shape.sidescroll == 0 || diff >= tw / 2 {
            return None;
        }
        to.left = to.left.saturating_sub(diff.max(shape.sidescroll));
    }
    let sits = cols
        .iter()
        .find(|(start, ..)| *start == v)
        .map_or(v, |&(start, w, c)| sits(start, w, c, shape, false));
    to.vcol = sits;
    Some(to)
}

/// The display column a line's text ends at.
fn end(cols: &[(usize, usize, char)]) -> usize {
    cols.last().map_or(0, |(s, w, _)| s + w)
}

/// The view's first column after Neovim draws the cursor at `vcol`, as
/// `curs_columns`: `left` if the cursor is wholly in view and at least
/// `'sidescrolloff'` from both edges, else moved at least `'sidescroll'`, or
/// centred if that is 0 or the cursor is half a view away.
pub fn showing(text: &str, shape: &Shape, vcol: usize, left: usize, width: usize) -> usize {
    let tw = width.saturating_sub(shape.textoff) as i64;
    if shape.wrap || tw == 0 {
        return left;
    }
    let cols = columns(text, shape);
    // Past the line's end, as in Insert mode, the cursor is one empty cell.
    let (first, last) = if vcol >= end(&cols) {
        (vcol, vcol)
    } else {
        span(&cols, vcol)
    };
    let (first, last, from) = (first as i64, last as i64, left as i64);
    let siso = shape.sidescrolloff as i64;
    let off_left = first - from - siso;
    let off_right = last + 1 - from - (tw - siso);
    if off_left >= 0 && off_right <= 0 {
        return left;
    }
    let diff = if off_left < 0 { -off_left } else { off_right };
    let by = diff.max(shape.sidescroll as i64);
    let to = if shape.sidescroll == 0 || diff >= tw / 2 || off_right >= off_left {
        vcol as i64 - tw / 2
    } else if off_left < 0 {
        from - by
    } else {
        from + by
    };
    to.max(0) as usize
}

/// Whether `vcol` is on or past the last character.
pub fn at_end(text: &str, shape: &Shape, vcol: usize) -> bool {
    columns(text, shape)
        .last()
        .is_none_or(|&(start, ..)| vcol >= start)
}

/// Whether a sideways scroll from `at` to `to` is safe with cut lines
/// (`cut`): each one in view or under the cursor must reach past the new
/// right edge, and the wheel must not have jumped lines.
pub fn known<'a>(
    at: Across,
    to: Across,
    width: usize,
    shape: &Shape,
    text: impl Fn(i64) -> Option<&'a str>,
    cut: impl Fn(i64) -> bool,
    shown: std::ops::Range<i64>,
) -> bool {
    if to.line != at.line && shown.clone().any(&cut) {
        return false;
    }
    let right = to.left + width.saturating_sub(shape.textoff);
    shown
        .chain([at.line, to.line])
        .filter(|&l| cut(l))
        .all(|l| text(l).is_some_and(|t| end(&columns(t, shape)) > right))
}

/// `rows` drawn from column `from`, moved to show column `to`: cells still in
/// view keep their colours, new ones are drawn from `text` (`None` past the
/// end). Double-width characters cut by only one view's edge, and edge cells
/// `'listchars'` may mark, are redrawn. `None` if a line was not sent.
pub fn shifted<'a>(
    rows: &[Vec<Cell>],
    from: usize,
    to: usize,
    shape: &Shape,
    look: &Look,
    text: impl Fn(usize) -> Option<Option<(&'a str, &'a [Span])>>,
) -> Option<Vec<Vec<Cell>>> {
    let moved = Shape {
        leftcol: to,
        ..shape.clone()
    };
    let l = &shape.lcs;
    let edges = shape.list && (l.eol.is_some() || l.extends.is_some() || l.precedes.is_some());
    rows.iter()
        .enumerate()
        .map(|(r, row)| {
            let Some(line) = text(r)? else {
                return Some(row.clone());
            };
            let width = row.len();
            let tw = width.saturating_sub(shape.textoff);
            let fresh = render(Some(line), &moved, 0, width, look, 0)?;
            let half =
                |cells: &[Cell], c: usize| cells.get(c).is_some_and(|x| x.text == Text::Half);
            let edge = |c: usize| edges && (c == shape.textoff || c + 1 == width);
            let mut out = row.clone();
            for c in shape.textoff..width {
                let v = to + (c - shape.textoff);
                // Keep a cell only where both rows agree on halves here and
                // next; otherwise an edge cuts a character in only one.
                let kept = (from..from + tw)
                    .contains(&v)
                    .then(|| shape.textoff + v - from)
                    .filter(|&o| {
                        half(row, o) == half(&fresh, c) && half(row, o + 1) == half(&fresh, c + 1)
                    })
                    .filter(|&o| !edge(o) && !edge(c));
                out[c] = match kept {
                    Some(o) => row[o].clone(),
                    None => fresh[c].clone(),
                };
            }
            Some(out)
        })
        .collect()
}

/// Normal/Visual keys that, with nothing pending, act at once and leave
/// nothing pending, so after one Neovim awaits a fresh command.
const WHOLE: &[&str] = &[
    "h", "j", "k", "l", "w", "b", "e", "W", "B", "E", "0", "^", "$", "G", "H", "M", "L", "n", "N",
    "*", "#", "%", "(", ")", "{", "}", "+", "-", "_", "|", ";", ",", "x", "X", "p", "P", "u", "J",
    "~", ".", "v", "V", "<C-v>", "<CR>", "<BS>", " ", "<Space>", "<Left>", "<Right>", "<Up>",
    "<Down>", "<Home>", "<End>", "<Del>", "<C-n>", "<C-p>", "<C-h>", "<C-j>", "<C-r>", "<Esc>",
    "<C-c>",
];

/// Shift/Ctrl arrows, word motions (see [`super::words`]); whole like
/// [`WHOLE`].
const WORD_ARROWS: &[&str] = &["<S-Right>", "<C-Right>", "<S-Left>", "<C-Left>"];

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

/// `z` scroll commands after the `z`, and the sideways ones.
const Z: &[&str] = &["t", "<CR>", "z", ".", "b", "-"];
const Z_SIDE: &[&str] = &["l", "h", "L", "H", "s", "e", "<Right>", "<Left>"];

/// Insert-mode scroll keys: alone, and after `<C-x>`.
const INSERT_SCROLLS: &[&str] = &["<PageDown>", "<PageUp>", "<S-Down>", "<S-Up>"];
const CTRL_X: &[&str] = &["<C-e>", "<C-y>"];

/// Insert-mode keys that wait for another or run a command.
const INSERT_WAITS: &[&str] = &[
    "<C-r>",
    "<C-v>",
    "<C-q>",
    "<C-k>",
    "<C-o>",
    "<C-x>",
    "<C-\\>",
    "<C-Bslash>",
    "<C-g>",
];

const WHEEL: &[&str] = &[
    "<ScrollWheelDown>",
    "<ScrollWheelUp>",
    "<S-ScrollWheelDown>",
    "<S-ScrollWheelUp>",
    "<C-ScrollWheelDown>",
    "<C-ScrollWheelUp>",
    "<ScrollWheelRight>",
    "<ScrollWheelLeft>",
    "<S-ScrollWheelRight>",
    "<S-ScrollWheelLeft>",
    "<C-ScrollWheelRight>",
    "<C-ScrollWheelLeft>",
];

/// Keys and `z` commands whose mappings the agent must look up (see
/// [`Shape::mapped`]); with `acks`, also multi-key motions and Insert entries.
pub fn keys(acks: bool) -> Vec<String> {
    let digits = (0..10).map(|d| d.to_string());
    let z = Z.iter().chain(Z_SIDE).map(|k| format!("z{k}"));
    let acked = [
        "g_", "ge", "gE", "gg", "f", "t", "F", "T", "i", "a", "A", "I",
    ]
    .into_iter()
    .filter(|_| acks);
    WHOLE
        .iter()
        .chain(WORD_ARROWS)
        .chain(SCROLLS)
        .chain(WHEEL)
        .map(|k| k.to_string())
        .chain(z)
        .chain(digits)
        .chain(acked.map(String::from))
        .chain([MOUSE_MOVE.to_string()])
        .collect()
}

/// Insert-mode keys whose mappings the agent looks up ([`Shape::imapped`]).
pub fn insert_keys() -> Vec<String> {
    INSERT_SCROLLS
        .iter()
        .chain(CTRL_X)
        .chain(&["<C-x>", "<Esc>", "<C-c>", MOUSE_MOVE])
        .map(|k| k.to_string())
        .collect()
}

/// A mouse move with no button pressed, as `'mousemoveevent'` reports it.
const MOUSE_MOVE: &str = "<MouseMove>";

/// The wheel's mapping name for `way` and `mods`, as in [`WHEEL`].
pub fn wheel(way: &str, mods: &str) -> String {
    match mods {
        "" => format!("<ScrollWheel{way}>"),
        m => format!("<{m}-ScrollWheel{way}>"),
    }
}

/// Split keys in Neovim notation into characters and `<…>` names.
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

/// Neovim's mode, as far as predictions care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Normal or visual mode.
    Normal,
    /// Insert or replace mode.
    Insert,
    Other,
}

impl Mode {
    /// From a `mode_change` name.
    pub fn of(name: &str) -> Self {
        match name {
            "normal" | "visual" => Mode::Normal,
            "insert" | "replace" => Mode::Insert,
            _ => Mode::Other,
        }
    }
}

/// Count cap; beyond it a count is effectively infinite for a scroll, not
/// for every motion.
pub const MAX_COUNT: i64 = 9999;

/// The client's reading of the keys it sends: enough of Neovim's Normal mode
/// to recognise a scroll, and to know when it cannot tell.
///
/// Without acks, an unfollowable key (entering Insert mode, waiting for
/// another, mapped) makes it lost until `<Esc>`, or until a fence sent after
/// the keys is answered with Neovim waiting for a command. Neovim answers
/// only after drawing every earlier key, so the fence also ends the
/// staleness after an unpredicted move ([`Typed::sure`]).
///
/// With acks, it replays from the last [`Ack`] ([`Typed::rebase`]), and an
/// unfollowable key makes it *blind* only until an ack covers that key.
#[derive(Debug, Clone, Default)]
pub struct Typed {
    state: State,
    /// Inputs with unpredicted effects sent, and how many Neovim has drawn.
    sent: u64,
    drawn: u64,
    /// The `sent` count when the pending fence was sent.
    fence: Option<u64>,
    /// The last input leaves nothing pending (Normal), or waits for another
    /// key (Insert).
    whole: bool,
    waits: bool,
    /// Input number, as the agent counts keys (see [`Sync`]).
    seq: u64,
    /// What to predict; with motions or typing, acks replace fences.
    predictions: Predictions,
    /// With acks: an input since the last ack has an unknown effect.
    blind: bool,
    /// Whether the input being made out is one the client cannot follow.
    lost: bool,
    /// Insert mode was entered by a count-1 `i`, `a`, `A` or `I` from Normal
    /// mode, so `<Esc>` repeats nothing. Acks do not say how it was entered,
    /// so this is the client's own reading (see [`Sent::once`],
    /// [`Typed::unentered`]).
    once: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    /// Waiting for a command, with the count typed so far.
    Ready(Option<i64>),
    /// A `z`, and the count before it.
    Z(Option<i64>),
    /// A `g` and its count; followed only with acks.
    G(Option<i64>),
    /// `f`, `t`, `F`, `T` awaiting the character; followed only with acks.
    Find {
        till: bool,
        back: bool,
        count: Option<i64>,
    },
    /// Insert or replace mode, after a `<C-x>` if `x`.
    Insert { x: bool },
    #[default]
    Lost,
}

impl Typed {
    /// Read a typed key as Neovim will take it; returns its [`Command`] if
    /// followable. `mapped(insert, key)` says if mapped.
    pub fn key(&mut self, key: &str, mapped: impl Fn(bool, &str) -> bool) -> Option<Command> {
        self.seq += 1;
        let command = self.step(key, mapped);
        if self.lost {
            self.sent += 1;
        }
        command
    }

    /// Read a key, typed or replayed from an ack ([`Typed::rebase`]); with
    /// acks, an unfollowable one blinds the client.
    pub fn step(&mut self, key: &str, mapped: impl Fn(bool, &str) -> bool) -> Option<Command> {
        self.lost = false;
        let command = self.parse(key, mapped);
        if self.lost {
            self.blinded();
        }
        command
    }

    /// What a key makes; an unfollowable one sets `lost`.
    fn parse(&mut self, key: &str, mapped: impl Fn(bool, &str) -> bool) -> Option<Command> {
        if let State::Insert { x } = self.state {
            return self.insert_key(key, x, |k| mapped(true, k));
        }
        let mapped = |k: &str| mapped(false, k);
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
                let motions = self.predictions.motions;
                if motions && key == "g" {
                    self.state = State::G(count);
                    return None;
                }
                let find = moves::finds(key).filter(|_| motions && !mapped(key));
                if let Some((till, back)) = find {
                    self.state = State::Find { till, back, count };
                    return None;
                }
                let enter = typing::Enter::of(key)
                    .filter(|_| self.predictions.typing && count.is_none_or(|n| n == 1));
                if let Some(e) = enter.filter(|_| !mapped(key)) {
                    self.state = State::Insert { x: false };
                    self.whole = true;
                    self.once = true;
                    return Some(Command::Enter(e));
                }
                let scrolls = self.predictions.scrolls;
                let command = scroll(key, count).filter(|_| scrolls).or_else(|| {
                    let motion = moves::motion(key).filter(|_| motions)?;
                    Some(Command::Move {
                        motion,
                        count,
                        screen: false,
                    })
                });
                match command.filter(|_| !mapped(key)) {
                    Some(command) => {
                        self.state = State::Ready(None);
                        self.whole = true;
                        Some(command)
                    }
                    None => {
                        self.moved(key, &mapped);
                        self.once = false;
                        None
                    }
                }
            }
            // `g_`, `ge`, `gE`, `gg` are looked up whole: Neovim's own `g…`
            // mappings make `g` alone a prefix, and only the user maps these.
            State::G(count) => {
                let motion = moves::g_motion(key).filter(|_| !mapped(&format!("g{key}")));
                if let Some(motion) = motion {
                    self.state = State::Ready(None);
                    self.whole = true;
                    return Some(Command::Move {
                        motion,
                        count,
                        screen: false,
                    });
                }
                self.lost = true;
                self.whole = false;
                self.once = false;
                self.state = State::Lost;
                None
            }
            // The sought character; only a language mapping can remap it,
            // which the motion handles (see `moves::Ctx::lmap`).
            State::Find { till, back, count } => {
                if let Some(c) = moves::sought(key) {
                    self.state = State::Ready(None);
                    self.whole = true;
                    return Some(Command::Move {
                        motion: moves::Motion::Find { c, till, back },
                        count,
                        screen: false,
                    });
                }
                self.lost = true;
                self.whole = false;
                self.once = false;
                self.state = State::Lost;
                None
            }
            State::Z(count) => {
                let n = count.unwrap_or(1);
                let side = match key {
                    "l" | "<Right>" => Some(Side::Cols(n)),
                    "h" | "<Left>" => Some(Side::Cols(-n)),
                    "L" => Some(Side::Halves(n)),
                    "H" => Some(Side::Halves(-n)),
                    "s" => Some(Side::Start),
                    "e" => Some(Side::End),
                    _ => None,
                };
                let scrolls = self.predictions.scrolls;
                if let Some(side) = side.filter(|_| scrolls && !mapped(&format!("z{key}"))) {
                    self.state = State::Ready(None);
                    self.whole = true;
                    return Some(Command::Side(side));
                }
                let command = Z
                    .iter()
                    .position(|z| *z == key)
                    .filter(|_| scrolls && !mapped(&format!("z{key}")));
                match command {
                    Some(i) => {
                        self.state = State::Ready(None);
                        self.whole = true;
                        let to = [Edge::Top, Edge::Middle, Edge::Bottom][i / 2];
                        Some(Command::Line {
                            to,
                            first: i % 2 == 1,
                            line: count,
                        })
                    }
                    None => {
                        self.lost = true;
                        self.whole = false;
                        self.once = false;
                        self.state = State::Lost;
                        None
                    }
                }
            }
            State::Lost | State::Insert { .. } => {
                self.moved(key, &mapped);
                self.once = false;
                None
            }
        }
    }

    /// A key typed in insert mode, after a `<C-x>` if `x`.
    fn insert_key(&mut self, key: &str, x: bool, mapped: impl Fn(&str) -> bool) -> Option<Command> {
        let plain = !mapped(key);
        let scrolls = self.predictions.scrolls;
        if x && plain && scrolls {
            match key {
                "<C-e>" => return Some(Command::Lines(1)),
                "<C-y>" => return Some(Command::Lines(-1)),
                _ => {}
            }
        }
        if plain {
            if let Some(command) = (scrolls && INSERT_SCROLLS.contains(&key))
                .then(|| scroll(key, None))
                .flatten()
            {
                self.state = State::Insert { x: false };
                self.whole = true;
                return Some(command);
            }
            if key == "<C-x>" {
                self.state = State::Insert { x: true };
                return None;
            }
        }
        // Typed or deleted text; mappings and the like are checked when it
        // is predicted (see `typing::refused`).
        let typing = !x && self.predictions.typing;
        if let Some(e) = typing::Edit::of(key).filter(|_| typing) {
            self.state = State::Insert { x: false };
            self.whole = true;
            self.waits = false;
            return Some(Command::Type(e));
        }
        // `<Esc>` after a count-1 entry; with a count the text would repeat.
        if typing && plain && self.once && matches!(key, "<Esc>" | "<C-[>") {
            self.state = State::Ready(None);
            self.whole = true;
            self.waits = false;
            self.once = false;
            return Some(Command::Leave);
        }
        // Unpredicted text or cursor keys, or a key that waits for another.
        self.lost = true;
        self.waits = !plain || INSERT_WAITS.contains(&key);
        self.whole = !self.waits;
        self.state = match key {
            "<Esc>" | "<C-c>" if plain => State::Ready(None),
            _ if self.whole => State::Insert { x: false },
            _ => State::Lost,
        };
        // Out of Insert mode, or in a Normal command from it.
        if !matches!(self.state, State::Insert { .. }) {
            self.once = false;
        }
        None
    }

    /// The last key may make Neovim wait, as a mapping prefix does.
    pub fn may_wait(&mut self) {
        self.whole = false;
    }

    /// A mouse report sent, numbered like a key; returns its number.
    pub fn input(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn inserting(&self) -> bool {
        matches!(self.state, State::Insert { .. })
    }

    /// A key that moves things the client does not predict.
    fn moved(&mut self, key: &str, mapped: &impl Fn(&str) -> bool) {
        self.lost = true;
        self.waits = INSERT_WAITS.contains(&key);
        let whole = [WHOLE, WORD_ARROWS, SCROLLS]
            .iter()
            .any(|keys| keys.contains(&key));
        self.whole = whole && !mapped(key);
        self.state = match self.state {
            _ if self.whole && matches!(key, "<Esc>" | "<C-c>") => State::Ready(None),
            State::Ready(_) if self.whole => State::Ready(None),
            _ => State::Lost,
        };
    }

    /// Another input that moves things (click, paste), or an unpredicted
    /// scroll.
    pub fn missed(&mut self) {
        self.sent += 1;
        self.blinded();
    }

    /// A click or a paste, which may change the mode as well.
    pub fn other(&mut self) {
        self.sent += 1;
        self.clicked();
    }

    /// A mouse move, live or replayed: nothing in any mode, nor to a count
    /// or command awaiting its next key (Neovim's `plain_vgetc` skips it),
    /// unless mapped.
    pub fn hovered(&mut self, mapped: impl Fn(bool, &str) -> bool) {
        self.lost = mapped(self.inserting(), MOUSE_MOVE);
        if self.lost {
            self.other();
        }
    }

    /// A click, live or replayed: cursor and mode unknown.
    pub fn clicked(&mut self) {
        self.whole = true;
        self.waits = false;
        self.once = false;
        self.state = State::Lost;
        self.blinded();
    }

    /// With acks, the last input's effect, and all after, is unknown until
    /// acked.
    pub fn blinded(&mut self) {
        if self.predictions.acks() {
            self.blind = true;
        }
    }

    /// Restart from an ack resting in `rest`; later inputs are then replayed
    /// with [`Typed::step`]. `once` is cleared until shown otherwise.
    pub fn rebase(&mut self, rest: Rest) {
        self.state = match rest {
            Rest::Normal | Rest::Visual => State::Ready(None),
            Rest::Insert => State::Insert { x: false },
            Rest::Busy => State::Lost,
        };
        self.whole = true;
        self.waits = false;
        self.blind = false;
        self.once = false;
    }

    /// The last Insert entry's step was not made: `<Esc>` is left to Neovim.
    pub fn unentered(&mut self) {
        self.once = false;
    }

    /// Whether every input since the last ack is followable.
    pub fn followed(&self) -> bool {
        !self.blind && self.state != State::Lost
    }

    /// Blindness is kept: scroll-only prediction never sets it nor reads it,
    /// and a key whose effect was unknown stays unknown.
    pub fn set_predictions(&mut self, predictions: Predictions) {
        self.predictions = predictions;
    }

    /// A fresh attach: nothing is known of what is pending.
    pub fn attached(&mut self) {
        self.sent += 1;
        self.whole = true;
        self.waits = false;
        self.once = false;
        self.state = State::Lost;
    }

    /// Whether the client's state is Neovim's after every key sent and Neovim
    /// awaits a command, so a scroll can be predicted.
    pub fn sure(&self) -> bool {
        self.drawn == self.sent && self.state != State::Lost
    }

    /// Whether to send a fence: inputs not known drawn, and no fence pending.
    pub fn wants_fence(&self) -> bool {
        self.fence.is_none() && self.drawn < self.sent
    }

    /// Whether the last key may leave Neovim waiting for another (a count,
    /// `z`, an operator, a mapping, an Insert key that waits, or an
    /// unfollowable one). Neovim acks such a key only after the next one.
    pub fn pending(&self) -> bool {
        !self.whole
            || matches!(
                self.state,
                State::Ready(Some(_))
                    | State::Z(_)
                    | State::G(_)
                    | State::Find { .. }
                    | State::Insert { x: true }
            )
    }

    /// Whether a fence is pending.
    pub fn fencing(&self) -> bool {
        self.fence.is_some()
    }

    /// Record a fence sent; returns the count it covers.
    pub fn fenced(&mut self) -> u64 {
        self.fence = Some(self.sent);
        self.sent
    }

    /// The fence for the inputs counted `at` is answered, Neovim in `mode`.
    pub fn answered(&mut self, at: u64, mode: Mode) {
        self.fence = None;
        self.drawn = self.drawn.max(at);
        self.settle(mode);
    }

    /// Neovim has drawn in `mode`: with every input drawn and nothing pending,
    /// it awaits a command or typing. Called every frame, since a fence can
    /// be answered before the mode arrives. Unused with acks, as a frame may
    /// be part way through the keys.
    pub fn settle(&mut self, mode: Mode) {
        if self.predictions.acks() {
            return;
        }
        if self.drawn == self.sent && self.state == State::Lost {
            self.state = match mode {
                Mode::Normal if self.whole => State::Ready(None),
                Mode::Insert if !self.waits => State::Insert { x: false },
                _ => State::Lost,
            };
        }
    }
}

/// The scroll made by a key that scrolls on its own, with `count`.
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

/// The wanted display column (`curswant`): a column or, after `$`, the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    Col(usize),
    End,
}

impl Want {
    /// For [`landing`]: the end is past every column.
    pub fn col(self) -> usize {
        match self {
            Want::Col(c) => c,
            Want::End => usize::MAX,
        }
    }
}

/// What Neovim is waiting for. `Busy` (operator pending, command line,
/// completion, Insert `<C-o>`, Replace) predicts nothing; Visual mode
/// predicts only scrolls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rest {
    Normal,
    Visual,
    Insert,
    Busy,
}

impl Rest {
    /// From `nvim_get_mode()`: `n`, `v`/`V`/`<C-v>`, `i`; anything else is
    /// busy.
    pub fn of(mode: &str) -> Self {
        match mode {
            "n" => Rest::Normal,
            "v" | "V" | "\x16" => Rest::Visual,
            "i" => Rest::Insert,
            _ => Rest::Busy,
        }
    }

    /// From the `mode_change` name of the mode Neovim last drew, which says
    /// less: `<C-o>`'s Normal mode is `normal`, completion `insert`.
    pub fn drawn(name: &str) -> Self {
        match name {
            "normal" => Rest::Normal,
            "visual" => Rest::Visual,
            "insert" => Rest::Insert,
            _ => Rest::Busy,
        }
    }
}

/// A window's cursor and view, in buffer terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    pub win: i64,
    /// 0-based cursor line and its byte column.
    pub line: i64,
    pub byte: usize,
    pub want: Want,
    /// 0-based top line, and the first display column when not wrapping.
    pub top: i64,
    pub left: usize,
    pub rest: Rest,
}

/// The agent's `ack` (see `super::AGENT_LUA`), sent when Neovim waits for a
/// key with nothing pending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ack {
    pub count: u64,
    /// The last keys taken (up to four), as `keytrans` writes them (see
    /// [`Sync::heard`]).
    pub tail: Box<str>,
    pub pos: Pos,
    /// The window's buffer, its `b:changedtick` and how many lines it has.
    pub buf: i64,
    pub tick: i64,
    pub line_count: i64,
    /// Layout width of the cursor's line: the 1-based column its last
    /// character ends in, inline virtual text included (it may be invisible).
    /// `None` if too long to measure (see [`Predictor::cut`]).
    pub width: Option<usize>,
    /// The same for lines in view ending in blanks, where blank virtual text
    /// is invisible.
    pub blanks: Vec<(i64, usize)>,
    /// Unknown after a key that may have moved it (see [`Stop`]).
    pub insstart: Option<Stop>,
}

/// Where `<C-w>`/`<C-u>` stop once in Insert mode (`Insstart_orig`).
/// `sticks` after a `<C-g>u`, as Neovim's default mappings of them type; it
/// then moves back only as text before it is deleted (see
/// `super::typing::rubbed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stop {
    pub line: i64,
    pub byte: usize,
    pub sticks: bool,
}

impl Stop {
    /// After a deleting key (with `<C-g>u` first if `undo`) leaves the cursor
    /// at `(line, byte)`: a sticking stop follows the cursor back.
    pub fn rubbed(self, (line, byte): (i64, usize), undo: bool) -> Self {
        let sticks = self.sticks || undo;
        let back = sticks && line == self.line && byte < self.byte;
        Self {
            byte: if back { byte } else { self.byte },
            sticks,
            ..self
        }
    }
}

impl Ack {
    /// From the agent's array: 0-based lines, byte columns, and -1 for
    /// `want`'s end of line, an unmeasured `width` or an unknown [`Stop`].
    pub fn read(v: &[ValueRef<'_>]) -> Option<Self> {
        let int = |i: usize| v.get(i).and_then(redraw::int);
        let size = |i: usize| int(i).and_then(|n| usize::try_from(n).ok());
        let line = |i: usize| int(i).filter(|n| *n >= 0);
        let text = |i: usize| v.get(i).and_then(redraw::as_str);
        let want = match int(9)? {
            -1 => Want::End,
            n => Want::Col(usize::try_from(n).ok()?),
        };
        Some(Self {
            count: u64::try_from(int(0)?).ok()?,
            tail: text(1)?.into(),
            pos: Pos {
                win: int(3)?,
                line: line(7)?,
                byte: size(8)?,
                want,
                top: line(10)?,
                left: size(11)?,
                rest: Rest::of(text(2)?),
            },
            buf: int(4)?,
            tick: int(5)?,
            line_count: line(6)?,
            width: size(12),
            blanks: v
                .get(13)
                .and_then(ValueRef::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|b| {
                            let b = b.as_array()?;
                            let n = |i: usize| b.get(i).and_then(redraw::int);
                            Some((n(0)?, usize::try_from(n(1)?).ok()?))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            insstart: line(14).zip(size(15)).map(|(line, byte)| Stop {
                line,
                byte,
                sticks: matches!(v.get(16), Some(ValueRef::Boolean(true))),
            }),
        })
    }

    /// Whether this ack, for later keys, finds Neovim as `was` did.
    pub fn unchanged(&self, was: &Ack) -> bool {
        self.pos == was.pos
            && (self.buf, self.tick, self.line_count) == (was.buf, was.tick, was.line_count)
            && (self.width, &self.blanks, self.insstart) == (was.width, &was.blanks, was.insstart)
    }

    /// Layout width of `line`, if measured.
    pub fn width_of(&self, line: i64) -> Option<usize> {
        if line == self.pos.line {
            return self.width;
        }
        self.blanks
            .iter()
            .find(|(l, _)| *l == line)
            .map(|&(_, w)| w)
    }
}

/// Whether Neovim may count this key as more than one: `<C-c>` (counted
/// twice) and Alt keys (`<Esc>` plus the key unless mapped as such).
pub fn uncertain(key: &str) -> bool {
    let Some(inner) = key.strip_prefix('<').and_then(|k| k.strip_suffix('>')) else {
        return false;
    };
    let mut rest = inner;
    let mut mods = String::new();
    while let Some((m, after)) = rest
        .split_once('-')
        .filter(|(m, a)| m.len() == 1 && !a.is_empty())
    {
        mods.push_str(m);
        rest = after;
    }
    let mods = mods.to_ascii_uppercase();
    mods.contains(['M', 'A']) || (mods == "C" && rest.eq_ignore_ascii_case("c"))
}

/// Whether `key` interrupts Neovim (`<C-c>`), flushing its typeahead: keys
/// sent before it may be dropped, so the counts are in doubt before it too.
fn interrupts(key: &str) -> bool {
    key.eq_ignore_ascii_case("<C-c>")
}

/// Whether a sent key matches one `keytrans` wrote. Mouse reports (`None`)
/// match anything, and chords are not compared (modifiers are spelt
/// differently); `<…>` names compare case-insensitively, and `<Space>`,
/// `<lt>`, `<Bar>`, `<Bslash>` as their characters.
fn alike(sent: Option<&str>, taken: &str) -> bool {
    let Some(sent) = sent else {
        return true;
    };
    let chord = |k: &str| k.len() > 2 && k.starts_with('<') && k[1..].contains('-');
    let norm = |k: &str| -> String {
        if !(k.len() > 2 && k.starts_with('<') && k.ends_with('>')) {
            return k.to_string();
        }
        let name = k.to_ascii_uppercase();
        match name.as_str() {
            "<SPACE>" => " ".into(),
            "<LT>" => "<".into(),
            "<BAR>" => "|".into(),
            "<BSLASH>" => "\\".into(),
            _ => name,
        }
    };
    chord(sent) || chord(taken) || norm(sent) == norm(taken)
}

/// Inputs [`Sync`] keeps for checking the agent's count.
const RECENT: usize = 64;

/// Ties the client's input numbers ([`Typed::seq`]) to the agent's count of
/// Neovim's keys (one per `nvim_input` key or `nvim_input_mouse` call). The
/// offset is learnt only at a fence answered with nothing sent after it, then
/// checked at each ack and fence. Anything unexplained (another UI's or a
/// plugin's keys, a command-line paste, `<C-c>`, see [`interrupts`]) puts the
/// counts in doubt until the next such fence; an Alt key (see [`uncertain`])
/// does so only from that key on. A key a mapping makes nothing of (`<Nop>`)
/// is counted only with the next key, so a fence ties only if the newest
/// ack's last key is the last sent.
#[derive(Debug, Default)]
pub struct Sync {
    /// Client number = agent count + `offset`.
    offset: Option<i64>,
    doubt: bool,
    /// The first key since the last tie that Neovim may count as more than
    /// one (see [`Sync::uncertain_from`]).
    uncertain: Option<u64>,
    passed: Option<u64>,
    /// Recent inputs by number (`None` for a mouse report).
    recent: VecDeque<(u64, Option<Box<str>>)>,
    pastes: u64,
    /// The earliest fence whose answer can be trusted: after the last paste,
    /// and after the next key if the paste is taken as keys (Neovim takes
    /// them only after answering a fence sent with it) or the last key is
    /// not counted yet.
    barrier: Option<Fenced>,
    /// The newest ack, judged or not.
    pub last: Option<Ack>,
    /// The agent is counting, so a fence sent now can tie the counts.
    pub agent: bool,
}

/// A fence: sent after input `seq` and `pastes` pastes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fenced {
    pub seq: u64,
    pastes: u64,
}

/// What a fence's answer made of the counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// No agent counting.
    NoAgent,
    /// Nothing was sent after it: tied from here.
    Tied,
    /// They were tied, and still agree.
    Checked,
    /// Untrusted: never tied, out of step, the last key not counted yet, or
    /// past a key Neovim may count as two (see [`Sync::passed`]).
    Doubted,
}

impl Sync {
    /// `key` is `None` for a mouse report.
    pub fn sent(&mut self, seq: u64, key: Option<&str>) {
        if self.recent.len() == RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back((seq, key.map(Into::into)));
    }

    /// A paste after input `seq_now`: acks are not matched until a fence
    /// after it is answered, or, if taken as keys (`keys`, as on the command
    /// line), one after the next key, with the counts in doubt.
    pub fn pasted(&mut self, seq_now: u64, keys: bool) {
        self.pastes += 1;
        self.barrier = Some(Fenced {
            seq: seq_now + u64::from(keys),
            pastes: self.pastes,
        });
        if keys {
            self.doubt("a paste Neovim may take as keys");
        }
    }

    /// The counts are in doubt, for `why`.
    pub fn doubt(&mut self, why: &'static str) {
        if !self.doubt {
            tracing::debug!(why, "client: the keys Neovim has taken are in doubt");
        }
        self.doubt = true;
    }

    /// Key `seq` may be counted as more than one (see [`uncertain`]). The
    /// counts are in doubt from the first such key since the last tie; the
    /// keys before that one still hold.
    pub fn uncertain_from(&mut self, seq: u64) {
        if self.uncertain.is_none() && !self.doubt {
            tracing::debug!(
                seq,
                "client: the keys Neovim has taken are in doubt from a key it may count as two"
            );
        }
        self.uncertain.get_or_insert(seq);
    }

    /// The last key before the uncertain one (see [`Sync::uncertain_from`]),
    /// once an ack or fence has counted past it: Neovim has taken every key
    /// up to it. Returned once.
    pub fn passed(&mut self) -> Option<u64> {
        self.passed.take()
    }

    /// A fresh attach brings a fresh agent, counting from 0.
    pub fn attached(&mut self) {
        *self = Self {
            pastes: self.pastes,
            ..Self::default()
        };
    }

    /// Whether the agent's count maps to the client's numbers for every key.
    pub fn synced(&self) -> bool {
        self.offset.is_some() && !self.doubt && self.uncertain.is_none() && self.barrier.is_none()
    }

    /// The first input number the counts are in doubt from: 0 if untied or
    /// in doubt, else that of a key Neovim may count as two, if any. Being
    /// held back by a paste is not doubt.
    pub fn doubted_from(&self) -> Option<u64> {
        if self.offset.is_none() || self.doubt {
            Some(0)
        } else {
            self.uncertain
        }
    }

    /// Whether a fence sent now could tie untied counts.
    pub fn wants_fence(&self, seq_now: u64) -> bool {
        self.agent && !self.synced() && self.barrier.is_none_or(|b| b.seq <= seq_now)
    }

    /// A fence asked once the inputs numbered to `seq` were sent.
    pub fn fenced(&self, seq: u64) -> Fenced {
        Fenced {
            seq,
            pastes: self.pastes,
        }
    }

    /// An ack heard: the client number of its last key, if the counts are
    /// trusted. Neovim cannot have taken more than was sent, and its last
    /// keys must match those sent.
    pub fn heard(&mut self, ack: &Ack, seq_now: u64) -> Option<u64> {
        self.last = Some(ack.clone());
        if self.offset.is_none() || self.doubt || self.barrier.is_some() {
            return None;
        }
        let k = i64::try_from(ack.count)
            .ok()
            .zip(self.offset)
            .and_then(|(count, offset)| count.checked_add(offset))
            .and_then(|k| u64::try_from(k).ok());
        // From a key Neovim may count as two on, the count is not the
        // client's (so it may pass what was sent), but every key before that
        // one was taken.
        if let Some(u) = self.uncertain.filter(|&u| k.is_some_and(|k| k >= u)) {
            self.passed = Some(u.saturating_sub(1));
            return None;
        }
        let Some(k) = k.filter(|&k| k <= seq_now) else {
            self.doubt("more taken than sent");
            return None;
        };
        let taken: Vec<&str> = tokens(&ack.tail).collect();
        for (back, taken) in taken.iter().rev().enumerate() {
            let Some(seq) = k.checked_sub(back as u64) else {
                break;
            };
            let Some((_, sent)) = self.recent.iter().find(|(s, _)| *s == seq) else {
                break;
            };
            if !alike(sent.as_deref(), taken) {
                self.doubt("the last keys taken were not those sent");
                return None;
            }
        }
        Some(k)
    }

    /// Fence `at` answered with `taken` keys counted (-1: no agent). With
    /// nothing sent after it, and past the barrier, it ties the counts;
    /// otherwise the count must lie between the fence and the last input.
    pub fn answered(&mut self, at: Fenced, seq_now: u64, taken: i64) -> Answer {
        if taken < 0 {
            self.agent = false;
            return Answer::NoAgent;
        }
        if self
            .barrier
            .is_some_and(|b| at.seq >= b.seq && at.pastes >= b.pastes)
        {
            self.barrier = None;
        }
        if seq_now == at.seq && self.barrier.is_none() {
            if !self.counted_last(taken) {
                self.doubt("the last key sent not counted yet");
                self.barrier = Some(Fenced {
                    seq: seq_now + 1,
                    pastes: self.pastes,
                });
                return Answer::Doubted;
            }
            let offset = at.seq as i64 - taken;
            // Inputs before an offset change were numbered by the old one, so
            // stop checking them.
            if self.offset != Some(offset) {
                self.recent.clear();
            }
            self.offset = Some(offset);
            self.doubt = false;
            self.uncertain = None;
            return Answer::Tied;
        }
        let Some(offset) = self.offset else {
            return Answer::Doubted;
        };
        // Once a key Neovim may count as two is taken, the count can be
        // neither checked nor mapped to keys; if the counts are otherwise
        // trusted, every key before it was taken.
        if let Some(u) = self
            .uncertain
            .filter(|&u| taken.checked_add(offset).is_some_and(|k| k >= u as i64))
        {
            if !self.doubt && self.barrier.is_none() {
                self.passed = Some(u.saturating_sub(1));
            }
            return Answer::Doubted;
        }
        let range = at.seq as i64..=seq_now as i64;
        if !taken
            .checked_add(offset)
            .is_some_and(|k| range.contains(&k))
        {
            self.doubt("a fence out of step");
        }
        if self.doubt {
            Answer::Doubted
        } else {
            Answer::Checked
        }
    }

    /// The client's number for the agent's `taken`th key, if tied.
    fn number(&self, taken: i64) -> Option<u64> {
        let k = taken.checked_add(self.offset?)?;
        u64::try_from(k).ok()
    }

    /// Whether the agent's last counted key is the last sent, per the newest
    /// ack; true if that ack is for another count or has no keys.
    fn counted_last(&self, taken: i64) -> bool {
        let last = self.last.as_ref();
        let Some(ack) = last.filter(|a| i64::try_from(a.count) == Ok(taken)) else {
            return true;
        };
        let (Some(took), Some((_, sent))) = (tokens(&ack.tail).last(), self.recent.back()) else {
            return true;
        };
        alike(sent.as_deref(), took)
    }
}

/// Neovim's last word: an ack, and the client number of its last key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub seq: u64,
    pub ack: Ack,
}

/// An input sent since Neovim's last word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub seq: u64,
    pub input: Input,
    pub at: Instant,
    /// Its predicted effect is in place (or there is none), so replay skips
    /// it; cleared when dropped.
    pub made: bool,
    /// It may leave Neovim waiting (see [`Typed::pending`]).
    pub pending: bool,
    /// Unreadable when sent (an unfollowed key, a click); it may change what
    /// a window shows without moving cursor or view (a fold, `'wrap'`).
    pub lost: bool,
    /// [`Typed::once`] as last read.
    pub once: bool,
}

/// An input as kept for replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Key(Box<str>),
    Wheel(Turn),
    Click,
    /// A mouse move ([`Typed::hovered`]).
    Move,
}

/// A wheel turn as sent, kept so a replay can remake it the same way; `way`
/// is `down`, `up`, `right` or `left`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub grid: u64,
    pub way: Box<str>,
    pub mods: Box<str>,
}

impl Input {
    pub fn key(&self) -> Option<&str> {
        match self {
            Input::Key(key) => Some(key),
            Input::Wheel(_) | Input::Click | Input::Move => None,
        }
    }
}

/// Most inputs kept since the last ack; more means Neovim is not keeping up
/// or not acking.
const QUEUE: usize = 512;

/// Outcomes of followed motions, judged at each ack (see [`Predictor::judge`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Shadow {
    /// Neovim put the cursor and view where the client would have, or not.
    pub agree: u64,
    pub disagree: u64,
    /// Keys the client could not follow, or had no state to follow from.
    pub unknown: u64,
    /// Acks covering a scroll ahead that their frame did not make.
    pub unmade: u64,
}

impl Shadow {
    /// Logged every [`SHADOW_EVERY`] verdicts and on exit.
    pub fn log(&self) {
        tracing::debug!(
            agree = self.agree,
            disagree = self.disagree,
            unknown = self.unknown,
            unmade = self.unmade,
            "client: the motions followed ahead of Neovim so far"
        );
    }
}

const SHADOW_EVERY: u64 = 100;

/// Consecutive confirmations before a kind drawn wrong in a buffer, or what
/// plugin autocommands may change there ([`Unseen`]), is drawn.
const PROOF: u32 = 8;

/// Of the last `LOOKED_BACK` drawn predictions, at least `FLOOR` tenths must
/// have been right, or none is drawn for the rest of the attach.
const LOOKED_BACK: usize = 64;
const FLOOR: usize = 9;

/// How long a window is left to Neovim after Neovim drew a correct
/// prediction differently, unless its shape is resent.
pub const DISTRUST: Duration = Duration::from_secs(2);

/// Gating for drawn predictions (see `super::App`): a kind drawn wrong in a
/// buffer is computed and judged but hidden until [`PROOF`] confirmations;
/// so, once per attach, is anything plugin autocommands may change
/// ([`Unseen`]); a window where a correct prediction was drawn differently
/// is distrusted for [`DISTRUST`]; and under nine in ten right of the last
/// [`LOOKED_BACK`] turns drawing off for the attach.
#[derive(Debug, Default)]
pub struct Trust {
    /// Kinds proving themselves, by buffer, with their current run.
    proving: HashMap<(i64, Proof), u32>,
    /// Current runs and reached proofs of [`Unseen`] kinds, by buffer.
    runs: HashMap<(i64, Unseen), u32>,
    proven: HashSet<(i64, Unseen)>,
    /// Windows left to Neovim, and until when.
    distrust: HashMap<i64, Instant>,
    /// Whether each recent drawn prediction was right, oldest first.
    recent: VecDeque<bool>,
    off: bool,
    pub tally: Tally,
}

/// The kind of a drawn step. Each typed character is its own kind: an
/// autopair's `(` says nothing about `x`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Proof {
    Move,
    Char(char),
    Rub,
    Enter,
    Leave,
}

impl Proof {
    /// The [`Unseen`] part of this kind, if any.
    fn unseen(self) -> Option<Unseen> {
        match self {
            Proof::Char(_) | Proof::Rub => Some(Unseen::Typed),
            Proof::Enter => Some(Unseen::Entered),
            Proof::Leave => Some(Unseen::Left),
            Proof::Move => None,
        }
    }
}

/// What plugin autocommands in a buffer may change, so it must prove itself
/// there once per attach: typing (`InsertCharPre`, `TextChangedI`,
/// `CursorMovedI`), entering (`InsertEnter`, `ModeChanged`) and leaving
/// (`InsertLeavePre`, `InsertLeave`, `ModeChanged`; e.g. a trailing-blank
/// trimmer that moves the cursor). See `super::typing::Facts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Unseen {
    Typed,
    Entered,
    Left,
}

/// Outcomes of computed predictions, for the log (see [`Trust`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    /// Matched Neovim; `moved` counts motions that moved the cursor.
    pub made: u64,
    pub moved: u64,
    /// Made differently: drawn, or hidden while proving.
    pub wrong: u64,
    pub hidden_wrong: u64,
    /// Dropped unjudged: Neovim's word on them passed, or did not come in time.
    pub unjudged: u64,
}

impl Tally {
    /// Logged every [`TALLY_EVERY`] verdicts and on exit.
    pub fn log(&self) {
        tracing::debug!(
            made = self.made,
            moved = self.moved,
            wrong = self.wrong,
            hidden_wrong = self.hidden_wrong,
            unjudged = self.unjudged,
            "client: the predictions drawn ahead of Neovim so far"
        );
    }

    fn count(&mut self, f: impl FnOnce(&mut Self)) {
        f(self);
        if (self.made + self.wrong + self.hidden_wrong + self.unjudged).is_multiple_of(TALLY_EVERY)
        {
            self.log();
        }
    }
}

const TALLY_EVERY: u64 = 200;

impl Trust {
    /// Whether a prediction in window `win` is to be left to Neovim `now`.
    pub fn refused(&self, win: i64, now: Instant) -> bool {
        self.off || self.distrust.get(&win).is_some_and(|&until| now < until)
    }

    /// Whether `proof` predictions in `buf` are hidden while proving.
    pub fn hidden(&self, buf: i64, proof: Proof) -> bool {
        self.proving.contains_key(&(buf, proof))
    }

    /// Whether `what` has proved itself in `buf` this attach.
    pub fn proven(&self, buf: i64, what: Unseen) -> bool {
        self.proven.contains(&(buf, what))
    }

    /// A prediction Neovim matched; `credited` ones count towards a proof
    /// (see `super::anim::chain::Step::credited`).
    pub fn made(&mut self, buf: i64, proof: Proof, credited: bool, shown: bool) {
        self.tally.count(|t| {
            t.made += 1;
            t.moved += u64::from(credited && proof == Proof::Move);
        });
        if shown {
            self.judged(true);
        }
        if !credited {
            return;
        }
        if let Some(run) = self.proving.get_mut(&(buf, proof)) {
            *run += 1;
            if *run >= PROOF {
                self.proving.remove(&(buf, proof));
            }
        }
        if let Some(what) = proof.unseen() {
            let run = self.runs.entry((buf, what)).or_default();
            *run += 1;
            if *run >= PROOF {
                self.proven.insert((buf, what));
            }
        }
    }

    /// A prediction Neovim made differently: its kind, and its [`Unseen`]
    /// part, prove themselves again from zero.
    pub fn wrong(&mut self, buf: i64, proof: Proof, shown: bool) {
        self.tally.count(|t| {
            if shown {
                t.wrong += 1;
            } else {
                t.hidden_wrong += 1;
            }
        });
        if shown {
            self.judged(false);
        }
        self.again(buf, proof);
    }

    /// An Insert entry or exit Neovim matched that also edited the buffer (a
    /// plugin autocommand did more): not counted wrong, but its kind proves
    /// itself again.
    pub fn edited(&mut self, buf: i64, proof: Proof) {
        self.again(buf, proof);
    }

    fn again(&mut self, buf: i64, proof: Proof) {
        self.proving.insert((buf, proof), 0);
        if let Some(what) = proof.unseen() {
            self.runs.insert((buf, what), 0);
        }
    }

    /// A new plugin autocommand may change `what` in `buf`: its proof restarts.
    pub fn unproven(&mut self, buf: i64, what: Unseen) {
        self.runs.remove(&(buf, what));
        self.proven.remove(&(buf, what));
    }

    /// Steps dropped unjudged: see [`Tally::unjudged`].
    pub fn unjudged(&mut self, n: u64) {
        if n > 0 {
            self.tally.count(|t| t.unjudged += n);
        }
    }

    pub fn distrust(&mut self, win: i64, until: Instant) {
        self.distrust.insert(win, until);
    }

    /// The agent resent `win`'s shape, which may fix what drew it wrong.
    pub fn reshaped(&mut self, win: i64) {
        self.distrust.remove(&win);
    }

    fn judged(&mut self, right: bool) {
        if self.recent.len() == LOOKED_BACK {
            self.recent.pop_front();
        }
        self.recent.push_back(right);
        let rights = self.recent.iter().filter(|r| **r).count();
        if !self.off && self.recent.len() == LOOKED_BACK && rights * 10 < FLOOR * LOOKED_BACK {
            self.off = true;
            tracing::info!(
                rights,
                of = LOOKED_BACK,
                "client: too few predictions drawn ahead of Neovim right; none drawn from here"
            );
        }
    }
}

/// Everything the client keeps for its predictions.
#[derive(Debug, Default)]
pub struct Predictor {
    /// Smoothed round trip and its variation, as TCP does (RFC 6298).
    srtt: Option<Duration>,
    rttvar: Option<Duration>,
    shapes: HashMap<i64, Shape>,
    lines: HashMap<i64, Lines>,
    /// Wanted cursor column per window (see [`Predictor::want`]).
    wants: HashMap<i64, usize>,
    pinged: Option<Instant>,
    pub typed: Typed,
    /// What to predict; motions or typing turn on acks ([`Ack`]).
    pub predictions: Predictions,
    pub sync: Sync,
    /// Neovim's last word and the inputs sent since, oldest first.
    pub base: Option<Base>,
    pub queue: VecDeque<Sent>,
    /// [`Sent::once`] as of the last key Neovim's last word covers.
    based: bool,
    /// Smoothed send-to-ack time and its variation, timed only for keys that
    /// leave nothing pending (the ack for one that does waits on the user).
    lag: Option<(Duration, Duration)>,
    timed: u64,
    pub shadow: Shadow,
    pub trust: Trust,
    /// Each buffer's Insert-mode facts.
    facts: HashMap<i64, typing::Facts>,
    /// Per window, lines in view with virtual text that does not move with
    /// the line end (see [`Predictor::pinned`]).
    pinned: HashMap<i64, Vec<i64>>,
    /// Per window, the lines scanned around the view and the decorated runs
    /// among them (see [`Predictor::decorated`]).
    decorated: HashMap<i64, (std::ops::Range<i64>, Vec<std::ops::Range<i64>>)>,
    /// Per window, the `'scroll'` a counted `<C-d>`/`<C-u>` set and that key's
    /// number; it overrides shapes sent before Neovim took the key.
    scrolls_set: HashMap<i64, (u64, i64)>,
}

/// RFC 6298 smoothing of a time and its variation with sample `r`: the first
/// sample sets `r` and `r / 2`, later ones move them 1/8 and 1/4 of the way.
fn smooth(was: Option<(Duration, Duration)>, r: Duration) -> (Duration, Duration) {
    match was {
        Some((s, v)) => ((s * 7 + r) / 8, (v * 3 + s.abs_diff(r)) / 4),
        None => (r, r / 2),
    }
}

impl Predictor {
    pub fn sample(&mut self, rtt: Duration) {
        let (srtt, rttvar) = smooth(self.srtt.zip(self.rttvar), rtt);
        self.srtt = Some(srtt);
        self.rttvar = Some(rttvar);
    }

    pub fn acks(&self) -> bool {
        self.predictions.acks()
    }

    pub fn typing(&self) -> bool {
        self.predictions.typing
    }

    pub fn set_predictions(&mut self, predictions: Predictions) {
        self.predictions = predictions;
        self.typed.set_predictions(predictions);
    }

    /// With acks, whether the inputs since the last ack can be replayed:
    /// counts tied, an ack to start from, and all inputs followed.
    pub fn known(&self) -> bool {
        self.sync.synced() && self.base.is_some() && self.typed.followed()
    }

    /// Window heights of lines the agent should keep either side of the view:
    /// the pages a held key types in a round trip, plus two for the page in
    /// view and Neovim's own time, within 2..=[`MOST_REACH`].
    pub fn reach(&self) -> i64 {
        let rtt = self.srtt.unwrap_or_default().as_micros();
        let pages = (rtt * PAGES_A_SECOND).div_ceil(1_000_000);
        i64::try_from(pages).map_or(MOST_REACH, |p| (p + 2).clamp(2, MOST_REACH))
    }

    /// When a prediction made `now` is dropped unconfirmed (with acks, once
    /// Neovim has taken its key: see `super::anim::Animator::idle`): two
    /// round trips plus [`GRACE`] (or 4× the variation) plus [`LOSS`]. With
    /// acks, at least the smoothed ack lag plus [`GRACE`] (or 4× its
    /// variation), since keys typed faster than Neovim takes them wait in its
    /// typeahead.
    pub fn deadline(&self, now: Instant) -> Instant {
        let (srtt, rttvar) = (
            self.srtt.unwrap_or_default(),
            self.rttvar.unwrap_or_default(),
        );
        let taking = self
            .lag
            .filter(|_| self.acks())
            .map_or(Duration::ZERO, |(s, v)| s + GRACE.max(v * 4));
        now + (srtt * 2 + GRACE.max(rttvar * 4)).max(taking) + LOSS
    }

    /// Whether to ping: the user typed since the last ping, at least
    /// [`PING_EVERY`] ago.
    pub fn wants_ping(&self, now: Instant, typed: Option<Instant>) -> bool {
        typed.is_some_and(|t| self.pinged.is_none_or(|p| t > p && now >= p + PING_EVERY))
    }

    pub fn pinged(&mut self, now: Instant) {
        self.pinged = Some(now);
    }

    /// Handle a prediction message from the agent; returns whether it was one.
    pub fn said(&mut self, what: &str, v: &[ValueRef<'_>]) -> bool {
        let int = |i: usize| v.get(i).and_then(redraw::int);
        match what {
            "lines" => {
                let (Some(win), Some(buf), Some(tick), Some(first)) =
                    (int(0), int(1), int(2), int(3))
                else {
                    return true;
                };
                let colours = v.get(7).and_then(ValueRef::as_array);
                let text = v
                    .get(4)
                    .and_then(ValueRef::as_array)
                    .map(|a| {
                        a.iter()
                            .enumerate()
                            .map(|(i, t)| {
                                Line::new(lossy(t), spans(colours.and_then(|c| c.get(i))))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let fresh = matches!(v.get(5), Some(ValueRef::Boolean(true)));
                let cap = int(6).map_or(usize::MAX, |c| c.max(0) as usize);
                self.lines
                    .entry(win)
                    .or_default()
                    .take(buf, tick, first, text, cap, fresh);
                true
            }
            "splice" => {
                let Some([win, buf, base, tick, first, last, count]) = (0..7)
                    .map(int)
                    .collect::<Option<Vec<i64>>>()
                    .and_then(|v| <[i64; 7]>::try_from(v).ok())
                else {
                    return true;
                };
                let text = v
                    .get(7)
                    .and_then(ValueRef::as_array)
                    .map(|a| a.iter().map(lossy).collect())
                    .unwrap_or_default();
                let cap = int(8).map_or(usize::MAX, |c| c.max(0) as usize);
                if let Some(lines) = self.lines.get_mut(&win) {
                    lines.splice(Splice {
                        buf,
                        base,
                        tick,
                        first,
                        last,
                        count,
                        text,
                        cap,
                    });
                }
                true
            }
            "keep" => {
                if let (Some(win), Some(buf), Some(tick), Some(from), Some(to)) =
                    (int(0), int(1), int(2), int(3), int(4))
                {
                    if let Some(lines) = self.lines.get_mut(&win) {
                        lines.keep(buf, tick, from, to);
                    }
                }
                true
            }
            "spans" => {
                let (Some(win), Some(buf), Some(tick), Some(first)) =
                    (int(0), int(1), int(2), int(3))
                else {
                    return true;
                };
                let lines = v
                    .get(4)
                    .and_then(ValueRef::as_array)
                    .map(|a| a.iter().map(|l| spans(Some(l))).collect())
                    .unwrap_or_default();
                if let Some(kept) = self.lines.get_mut(&win) {
                    kept.recolour(buf, tick, first, lines);
                }
                true
            }
            "shape" => {
                let shape = v
                    .get(1)
                    .and_then(ValueRef::as_array)
                    .and_then(|a| Shape::read(a));
                if let (Some(win), Some(mut shape)) = (int(0), shape) {
                    if let Some(&(_, scroll)) = self.scrolls_set.get(&win) {
                        shape.scroll = scroll;
                    }
                    self.shapes.insert(win, shape);
                }
                true
            }
            "want" => {
                if let (Some(win), Some(col)) = (int(0), int(1)) {
                    self.wants.insert(win, usize::try_from(col).unwrap_or(0));
                }
                true
            }
            "insert" => {
                if let Some((buf, facts)) = typing::Facts::read(v) {
                    // A plugin autocommand the buffer did not have before:
                    // past confirmations prove nothing about it.
                    let was = self.facts.get(&buf);
                    for (what, now, before) in [
                        (Unseen::Typed, facts.unseen, was.map(|f| f.unseen)),
                        (
                            Unseen::Entered,
                            facts.unseen_entered,
                            was.map(|f| f.unseen_entered),
                        ),
                        (Unseen::Left, facts.unseen_left, was.map(|f| f.unseen_left)),
                    ] {
                        if now && before != Some(true) {
                            self.trust.unproven(buf, what);
                        }
                    }
                    self.facts.insert(buf, facts);
                }
                true
            }
            "pinned" => {
                let lines = v
                    .get(1)
                    .and_then(ValueRef::as_array)
                    .map(|a| a.iter().filter_map(redraw::int).collect());
                if let (Some(win), Some(lines)) = (int(0), lines) {
                    self.pinned.insert(win, lines);
                }
                true
            }
            "decorated" => {
                let runs = v.get(3).and_then(ValueRef::as_array).map(|a| {
                    let ends: Vec<i64> = a.iter().filter_map(redraw::int).collect();
                    ends.as_chunks::<2>().0.iter().map(|&[a, b]| a..b).collect()
                });
                if let (Some(win), Some(from), Some(to), Some(runs)) =
                    (int(0), int(1), int(2), runs)
                {
                    self.decorated.insert(win, (from..to, runs));
                }
                true
            }
            _ => false,
        }
    }

    pub fn closed(&mut self, win: i64) {
        self.shapes.remove(&win);
        self.lines.remove(&win);
        self.wants.remove(&win);
        self.pinned.remove(&win);
        self.decorated.remove(&win);
        self.scrolls_set.remove(&win);
    }

    /// A fresh attach: its agent will resend everything.
    pub fn reset(&mut self) {
        self.shapes.clear();
        self.lines.clear();
        self.wants.clear();
        self.facts.clear();
        self.pinned.clear();
        self.decorated.clear();
        self.scrolls_set.clear();
        self.base = None;
        self.queue.clear();
    }

    /// A fresh attach: pending state unknown, and the new agent counts from 0.
    pub fn attached(&mut self) {
        self.typed.attached();
        self.sync.attached();
        self.trust = Trust::default();
    }

    /// Whether to send a fence: for [`Typed`], or with acks to tie the counts
    /// once an agent is counting; otherwise a fence sent before the agent
    /// existed would leave them untied until the next pause.
    pub fn wants_fence(&self) -> bool {
        self.typed.wants_fence()
            || (self.acks() && self.sync.wants_fence(self.typed.seq()) && !self.typed.fencing())
    }

    /// Input `seq` sent: `key`, or a click. Kept only with acks.
    pub fn sent(&mut self, seq: u64, key: Option<&str>, now: Instant) {
        let input = key.map_or(Input::Click, |k| Input::Key(k.into()));
        self.keep(seq, input, now);
    }

    /// Wheel turn `turn` sent as input `seq` at `now`: see [`Predictor::sent`].
    pub fn turned(&mut self, seq: u64, turn: Turn, now: Instant) {
        self.keep(seq, Input::Wheel(turn), now);
    }

    /// Mouse move `seq` sent while `win` has the cursor: see
    /// [`Predictor::sent`] and [`Predictor::hover`]. Unless mapped, it is
    /// made, with nothing to make.
    pub fn hovered(&mut self, win: Option<i64>, seq: u64, now: Instant) {
        self.hover(win);
        self.keep(seq, Input::Move, now);
        if !self.typed.lost {
            self.made(seq);
        }
    }

    /// A mouse move, read with `win`'s mappings ([`Typed::hovered`]).
    pub fn hover(&mut self, win: Option<i64>) {
        let shape = win.and_then(|w| self.shapes.get(&w));
        self.typed.hovered(mapping(shape, None));
    }

    fn keep(&mut self, seq: u64, input: Input, now: Instant) {
        if !self.acks() {
            return;
        }
        let key = input.key();
        self.sync.sent(seq, key);
        if key.is_some_and(interrupts) {
            self.sync
                .doubt("an interrupt, which may throw away keys sent before it");
        } else if key.is_some_and(uncertain) {
            self.sync.uncertain_from(seq);
        }
        if self.queue.len() == QUEUE {
            self.sync.doubt("too many keys unanswered");
            self.queue.clear();
        }
        let lost = match input {
            Input::Key(_) | Input::Move => self.typed.lost,
            Input::Wheel(_) => false,
            Input::Click => true,
        };
        self.queue.push_back(Sent {
            seq,
            input,
            at: now,
            pending: self.typed.pending(),
            made: false,
            lost,
            once: self.typed.once,
        });
    }

    /// Input `seq` was replayed; record [`Sent::once`] as now read.
    pub fn remade(&mut self, seq: u64) {
        let once = self.typed.once;
        if let Some(s) = self.queue.iter_mut().find(|s| s.seq == seq) {
            s.once = once;
        }
    }

    /// Input `seq` entered Insert mode without its step made.
    pub fn unentered(&mut self, seq: u64) {
        self.typed.unentered();
        self.remade(seq);
    }

    /// Reset [`Typed`] to Neovim's last word, for the caller to replay the
    /// inputs since (see [`Typed::rebase`]); returns whether there is one.
    pub fn rebase(&mut self) -> bool {
        let Some(base) = self.base.as_ref() else {
            return false;
        };
        let rest = base.ack.pos.rest;
        self.typed.rebase(rest);
        self.typed.once = self.based && rest == Rest::Insert;
        true
    }

    /// [`Sent::once`] for input `seq` as last read; false if no longer kept.
    fn once(&self, seq: u64) -> bool {
        self.queue.iter().any(|s| s.seq == seq && s.once)
    }

    /// Whether the inputs after `from` up to `to`, all still kept, leave
    /// window grid `grid` as it was: mouse moves, and wheel turns over other
    /// windows.
    pub fn passes(&self, grid: u64, (from, to): (u64, u64)) -> bool {
        let between = || self.queue.iter().filter(|s| s.seq > from && s.seq <= to);
        between().count() as u64 == to.saturating_sub(from)
            && between().all(|s| match &s.input {
                Input::Move => !s.lost,
                Input::Wheel(t) => t.grid != grid,
                Input::Key(_) | Input::Click => false,
            })
    }

    /// Whether every input up to `k` was readable when sent (see
    /// [`Sent::lost`]) or made since.
    pub fn made_out(&self, k: u64) -> bool {
        self.queue
            .iter()
            .filter(|s| s.seq <= k)
            .all(|s| s.made || !s.lost)
    }

    /// Mark input `seq`'s effect made: see [`Sent::made`].
    pub fn made(&mut self, seq: u64) {
        if let Some(s) = self.queue.iter_mut().find(|s| s.seq == seq) {
            s.made = true;
        }
    }

    /// Steps made ahead for `seqs` were dropped; replay them from the next ack.
    pub fn given_up(&mut self, seqs: &[u64]) {
        for s in self.queue.iter_mut().filter(|s| seqs.contains(&s.seq)) {
            s.made = false;
        }
    }

    /// An ack heard: the client number of its last key, if trusted (see
    /// [`Sync::heard`]). Times that key for [`Predictor::deadline`] unless it
    /// left Neovim waiting (on the user or `'timeoutlen'`), and records the
    /// wanted column.
    pub fn heard(&mut self, ack: &Ack, now: Instant) -> Option<u64> {
        self.wants.insert(ack.pos.win, ack.pos.want.col());
        let k = self.sync.heard(ack, self.typed.seq())?;
        self.taken(k);
        if k > self.timed {
            if let Some(key) = self.queue.iter().find(|s| s.seq == k && !s.pending) {
                self.timed = k;
                self.lag = Some(smooth(self.lag, now.saturating_duration_since(key.at)));
            }
        }
        Some(k)
    }

    /// Fence `at` answered (see [`Sync::answered`]). If it ties the counts and
    /// the last ack is for that count, that ack becomes Neovim's last word.
    /// Not timed: a fence after a waiting key is answered only after the next
    /// key. Returns the last key's number, if trusted.
    pub fn answered(&mut self, at: Fenced, taken: i64) -> Option<u64> {
        let answer = self.sync.answered(at, self.typed.seq(), taken);
        let upto = matches!(answer, Answer::Tied | Answer::Checked)
            .then(|| self.sync.number(taken))
            .flatten();
        if let Some(k) = upto {
            self.taken(k);
        }
        if answer != Answer::Tied {
            return upto;
        }
        let last = self.sync.last.as_ref();
        if let Some(ack) = last.filter(|a| i64::try_from(a.count) == Ok(taken)) {
            self.based = self.once(at.seq);
            self.base = Some(Base {
                seq: at.seq,
                ack: ack.clone(),
            });
            self.rebase();
            self.queue.retain(|s| s.seq > at.seq);
        }
        upto
    }

    /// Whether `win`'s lines are `buf`'s as of `b:changedtick` `tick`. The
    /// agent sends edits ahead of each ack, so they match the ack's.
    pub fn fresh(&self, win: i64, buf: i64, tick: i64) -> bool {
        self.lines
            .get(&win)
            .is_some_and(|l| l.buf == buf && l.tick == tick)
    }

    /// Judge `ack` (covering keys up to `k`): compare where the client would
    /// have put the cursor and view with Neovim's, log the verdict
    /// ([`Shadow`]), and make it the last word. `view` is the window's rows
    /// and columns; `unmade`, whether the frame left a scroll ahead unmade.
    pub fn judge(&mut self, k: u64, ack: Ack, view: Option<(i64, usize)>, unmade: bool) {
        self.shadow.unmade += u64::from(unmade);
        if let Some(base) = self.base.as_ref().filter(|b| b.seq < k) {
            let keys: Option<Vec<&str>> = self
                .queue
                .iter()
                .filter(|s| s.seq > base.seq && s.seq <= k)
                .map(|s| s.input.key())
                .collect();
            let keys = keys.filter(|keys| keys.len() as u64 == k - base.seq);
            let followed = keys
                .as_ref()
                .and_then(|keys| self.followed(&base.ack, keys, &ack, view));
            let shadow = &mut self.shadow;
            match followed {
                None => shadow.unknown += 1,
                Some(to) if to == ack.pos => shadow.agree += 1,
                Some(to) => {
                    shadow.disagree += 1;
                    tracing::info!(
                        keys = ?keys.unwrap_or_default(),
                        from = ?base.ack.pos,
                        followed = ?to,
                        neovim = ?ack.pos,
                        line = ?self.text(ack.pos.win, ack.pos.line),
                        "client: a motion followed ahead of Neovim was not the one it made"
                    );
                }
            }
            let s = self.shadow;
            if (s.agree + s.disagree + s.unknown).is_multiple_of(SHADOW_EVERY) {
                s.log();
            }
        }
        self.based = self.once(k);
        self.base = Some(Base { seq: k, ack });
        self.rebase();
        self.queue.retain(|s| s.seq > k);
    }

    /// Where the client would have put the cursor and view for `keys` from
    /// `from`; `None` if it would not have followed them (a mapped or
    /// unfollowed key, missing or non-[`ordinary`] lines, an edit, or another
    /// window or buffer in `to`).
    fn followed(
        &self,
        from: &Ack,
        keys: &[&str],
        to: &Ack,
        view: Option<(i64, usize)>,
    ) -> Option<Pos> {
        let win = from.pos.win;
        let shape = self.shapes.get(&win)?;
        let (rows, width) = view?;
        let same = to.pos.win == win && to.buf == from.buf && to.tick == from.tick;
        if !same
            || shape.buf != from.buf
            || !shape.predictable
            || !self.fresh(win, from.buf, from.tick)
        {
            return None;
        }
        let motions = moves::parse(keys.iter().copied(), |key| {
            shape.mapped.iter().any(|m| m == key)
        })?;
        let texts = self.ordinary_texts(win, false);
        let text = |l: i64| texts(l);
        let cx = moves::Ctx {
            shape,
            line_count: from.line_count,
            rows,
            width,
            text: &text,
            whichwrap: shape.stepping.as_ref().map(|s| s.whichwrap.as_str()),
            scrolling: shape.stepping.as_ref().map(Stepping::scrolling),
            keyword: shape.stepping.as_ref().map(|s| s.classes.as_slice()),
            lmap: shape.stepping.as_ref().map(|s| s.lmap),
            macros: shape.stepping.as_ref().and_then(Stepping::macros),
        };
        motions
            .into_iter()
            .try_fold(from.pos, |at, (m, count)| moves::moved(m, count, &at, &cx))
    }

    /// The wanted column (`curswant`) in `win`, as last sent: where a scroll
    /// to another line lands the cursor ([`landing`]).
    pub fn want(&self, win: i64) -> Option<usize> {
        self.wants.get(&win).copied()
    }

    pub fn shape(&self, win: i64) -> Option<&Shape> {
        self.shapes.get(&win)
    }

    /// What the agent last said of Insert mode in buffer `buf`.
    pub fn facts(&self, buf: i64) -> Option<&typing::Facts> {
        self.facts.get(&buf)
    }

    /// Whether line `lnum` has (or may have, if unreported) virtual text that
    /// does not move with the line end as text is typed: right-aligned, at a
    /// window column, overlaid, or inline after the end. Misses what a
    /// decoration provider draws, which no extmark shows.
    pub fn pinned(&self, win: i64, lnum: i64) -> bool {
        self.pinned.get(&win).is_none_or(|l| l.contains(&lnum))
    }

    /// Whether Neovim may draw line `lnum`'s text moved or partly hidden by
    /// virtual text (inline, right-aligned, at a window column, or overlaid
    /// on more than blanks), which the client cannot lay out for an
    /// uncovered line (see [`Predictor::row`]). True if unreported.
    pub fn decorated(&self, win: i64, lnum: i64) -> bool {
        self.decorated.get(&win).is_none_or(|(over, runs)| {
            !over.contains(&lnum) || runs.iter().any(|r| r.contains(&lnum))
        })
    }

    /// A key typed while `win` has the cursor, read with its mappings; an
    /// [`Idiom`] mapping reads as the bare key, moving a screen row.
    pub fn key(&mut self, win: Option<i64>, key: &str) -> Option<Command> {
        let shape = win.and_then(|w| self.shapes.get(&w));
        let same = idiomatic(shape, key);
        let command = self.typed.key(key, mapping(shape, same.then_some(key)));
        // An Insert-mode key that starts a mapping there may make Neovim wait
        // for the next, as `inoremap jk <Esc>` does with `j`.
        let facts = shape.and_then(|s| self.facts.get(&s.buf));
        if matches!(command, Some(Command::Type(_))) && facts.is_some_and(|f| f.starts(key)) {
            self.typed.may_wait();
        }
        screened(command, same)
    }

    /// A key replayed from Neovim's last word: see [`Predictor::key`].
    pub fn remake(&mut self, win: Option<i64>, key: &str) -> Option<Command> {
        let shape = win.and_then(|w| self.shapes.get(&w));
        let same = idiomatic(shape, key);
        let command = self.typed.step(key, mapping(shape, same.then_some(key)));
        screened(command, same)
    }

    /// A counted `<C-d>`/`<C-u>` (key `seq`) sets `win`'s `'scroll'`; with
    /// acks this overrides shapes until Neovim takes the key.
    pub fn set_scroll(&mut self, win: i64, scroll: i64, seq: u64) {
        if let Some(s) = self.shapes.get_mut(&win) {
            s.scroll = scroll;
        }
        if self.acks() {
            self.scrolls_set.insert(win, (seq, scroll));
        }
    }

    /// Neovim has taken keys up to `k`, so its shapes now carry their
    /// `'scroll'`.
    fn taken(&mut self, k: u64) {
        self.scrolls_set.retain(|_, (seq, _)| *seq > k);
    }

    /// Neovim's view of window `win` now starts at display column `left`.
    pub fn set_leftcol(&mut self, win: i64, left: usize) {
        if let Some(s) = self.shapes.get_mut(&win) {
            s.leftcol = left;
        }
    }

    /// Line `lnum` of `win`, if the agent has sent it.
    pub fn text(&self, win: i64, lnum: i64) -> Option<&str> {
        let shape = self.shapes.get(&win)?;
        self.lines.get(&win)?.get(shape.buf, lnum)
    }

    /// Text of `win`'s buffer lines a motion may go over: sent whole (see
    /// [`Predictor::cut`]), [`ordinary`], and all ASCII if `ascii`. The window
    /// is looked up once.
    pub fn ordinary_texts<'a>(
        &'a self,
        win: i64,
        ascii: bool,
    ) -> impl Fn(i64) -> Option<&'a str> + 'a {
        let kept = self
            .shapes
            .get(&win)
            .map(|s| s.buf)
            .zip(self.lines.get(&win));
        move |lnum| {
            let (buf, lines) = kept?;
            let line = lines.line(buf, lnum)?;
            let whole = line.text.len() + 3 < lines.cap;
            (whole && line.ordinary && (!ascii || line.ascii)).then_some(line.text.as_str())
        }
    }

    /// Whether the agent may have cut line `lnum` short.
    pub fn cut(&self, win: i64, lnum: i64) -> bool {
        self.shapes
            .get(&win)
            .zip(self.lines.get(&win))
            .is_some_and(|(shape, lines)| lines.cut(shape.buf, lnum))
    }

    /// Line `lnum` with its highlights; `None` if not sent, or if cut short
    /// where `'listchars'` makes that matter ([`Shape::unsure`]).
    pub fn coloured(&self, win: i64, lnum: i64) -> Option<(&str, &[Span])> {
        let shape = self.shapes.get(&win)?;
        let lines = self.lines.get(&win)?;
        let line = lines.line(shape.buf, lnum)?;
        if lines.cut(shape.buf, lnum) && shape.unsure(&line.text) {
            return None;
        }
        Some((line.text.as_str(), line.spans.as_slice()))
    }

    /// Line `lnum` as a row of `win` (see [`render`]), if available.
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
            Some(self.coloured(win, lnum)?)
        };
        render(text, shape, lnum, width, look, cursor)
    }
}

/// Whether a key is mapped in a window of `shape` (in Insert mode if asked).
/// False for unknown windows, and for an [`Idiom`] key `same` in Normal mode.
fn mapping<'a>(
    shape: Option<&'a Shape>,
    same: Option<&'a str>,
) -> impl Fn(bool, &str) -> bool + 'a {
    move |insert, k| {
        (insert || same != Some(k))
            && shape.is_some_and(|s| {
                let list = if insert { &s.imapped } else { &s.mapped };
                list.iter().any(|m| m == k)
            })
    }
}

/// Whether `key`'s mapping moves as the bare key ([`Idiom::same`]).
fn idiomatic(shape: Option<&Shape>, key: &str) -> bool {
    shape
        .and_then(|s| s.stepping.as_ref())
        .is_some_and(|s| s.idioms.iter().any(|i| i.key == key && i.same()))
}

/// `command`, marked as made by a screen-row mapping if `screen`.
fn screened(command: Option<Command>, screen: bool) -> Option<Command> {
    match command {
        Some(Command::Move { motion, count, .. }) => Some(Command::Move {
            motion,
            count,
            screen,
        }),
        other => other,
    }
}

/// A line's text from any bytes: a binary file's line is still drawn.
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

    /// Motions and scrolls, as with `predict_typing = "off"`; and typing too,
    /// the `[client]` default.
    const MOTIONS: Predictions = Predictions {
        scrolls: true,
        motions: true,
        typing: false,
    };
    const TYPING: Predictions = Predictions {
        scrolls: true,
        motions: true,
        typing: true,
    };

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
            lcs: Listchars::default(),
            checks: false,
            scroll: 11,
            page: 0,
            startofline: false,
            mapped: Vec::new(),
            imapped: Vec::new(),
            sidescrolloff: 0,
            sidescroll: 1,
            hor: 6,
            stepping: None,
        }
    }

    fn plain(text: &str) -> Option<(&str, &[Span])> {
        Some((text, &[]))
    }

    fn look() -> Look {
        Look {
            gutter: vec![18; 4],
            text: 0,
            eob: None,
            eob_hl: 18,
            nontext: 19,
            specialkey: 20,
            whitespace: 21,
            rows: 22,
        }
    }

    fn text(row: &[Cell]) -> String {
        let mut b = Vec::new();
        for c in row {
            c.text.push_to(&mut b);
        }
        String::from_utf8(b).unwrap()
    }

    #[test]
    fn spans_colour_what_they_cover() {
        use crate::client::model::syntax_hl;
        let s = Shape {
            textoff: 0,
            number: false,
            ..shape()
        };
        let span = |start, end, group| Span { start, end, group };
        let l = Look { text: 7, ..look() };
        // "if x\t日y": bytes 0-1 `if`, 3 `x`, 4 a tab to column 8, 5-7 `日`,
        // 8 `y`.
        let spans = [
            span(0, 2, 40),
            span(0, 9, 41),
            span(0, 2, 42),
            span(5, usize::MAX, 43),
        ];
        let row = render(Some(("if x\t日y", &spans)), &s, 0, 12, &l, 0).unwrap();
        let hls: Vec<u32> = row.iter().map(|c| c.hl).collect();
        let (kw, word, tail) = (syntax_hl(7, 42), syntax_hl(7, 41), syntax_hl(7, 43));
        assert_eq!(
            hls,
            [kw, kw, word, word, word, word, word, word, tail, tail, tail, 7],
            "if, a blank and x, a tab to 8, 日 twice wide and y, then the rest"
        );
    }

    #[test]
    fn a_row_has_its_number_and_its_text() {
        let row = render(plain("let x = 1;"), &shape(), 22, 20, &look(), 3).unwrap();
        assert_eq!(text(&row), " 23 let x = 1;      ");
        assert!(row[..4].iter().all(|c| c.hl == 18));
        assert!(row[4..].iter().all(|c| c.hl == 0));
    }

    #[test]
    fn relative_numbers_count_from_the_cursor() {
        let s = Shape {
            relativenumber: true,
            ..shape()
        };
        let row = render(plain("x"), &s, 22, 8, &look(), 30).unwrap();
        assert_eq!(text(&row), "  8 x   ");
        let s = Shape {
            number: false,
            relativenumber: false,
            ..shape()
        };
        let row = render(plain("x"), &s, 22, 8, &look(), 30).unwrap();
        assert_eq!(text(&row), "    x   ");
    }

    /// [`shape`] with ack-only fields: number options, `'numberwidth'` `nuw`,
    /// `lines` lines, `'cursorline'` flags `cul`.
    fn numbering((nu, rnu): (bool, bool), nuw: usize, lines: i64, cul: i64) -> Shape {
        Shape {
            number: nu,
            relativenumber: rnu,
            line_count: lines,
            stepping: Some(Stepping {
                whichwrap: "b,s".into(),
                virtualedit: String::new(),
                cursorcolumn: false,
                bound: false,
                langmap: false,
                widths: false,
                numberwidth: nuw,
                number_signs: false,
                cursorline: Cursorline::read(cul),
                idioms: Vec::new(),
                own_highlights: false,
                conceals: false,
                colorcolumn: Vec::new(),
                scrolljump: 1,
                concealing: false,
                classes: Vec::new(),
                lmap: false,
                sections: None,
                paragraphs: None,
                smoothscroll: false,
            }),
            ..shape()
        }
    }

    const IDS: Gutter = Gutter {
        line_nr: 10,
        above: 11,
        below: 12,
        cursor_nr: 13,
        sign: 14,
        cursor_sign: 15,
        fold: 16,
        cursor_fold: 17,
    };

    /// Text and first-cell group of the number column on each of `lines`.
    fn numbered(
        shape: &Shape,
        cursor: i64,
        rows: i64,
        lines: std::ops::Range<i64>,
    ) -> Vec<(String, u32)> {
        lines
            .map(|l| {
                let cells = numbers(l, cursor, shape, rows, &IDS).expect("numbered");
                (text(&cells), cells[0].hl)
            })
            .collect()
    }

    /// Expected values from Neovim 0.12.6.
    #[test]
    fn the_number_column_is_laid_out_as_neovim_lays_it_out() {
        let n = |text: &str, hl: u32| (text.to_string(), hl);
        let nu = numbering((true, false), 4, 30, 0);
        assert_eq!(
            numbered(&nu, 2, 6, 0..4),
            [n("  1 ", 10), n("  2 ", 10), n("  3 ", 10), n("  4 ", 10)]
        );
        let rnu = numbering((false, true), 4, 30, 0);
        assert_eq!(
            numbered(&rnu, 2, 6, 0..4),
            [n("  2 ", 11), n("  1 ", 11), n("  0 ", 10), n("  1 ", 12)]
        );
        let both = numbering((true, true), 4, 30, 0);
        assert_eq!(
            numbered(&both, 3, 6, 2..5),
            [n("  1 ", 11), n("4   ", 10), n("  1 ", 12)]
        );
        let wide = numbering((true, true), 6, 30, 0);
        assert_eq!(
            numbered(&wide, 1, 6, 0..2),
            [n("    1 ", 11), n("2     ", 10)]
        );
        // Width from the last line's number however few are in view, or from
        // the window height with 'relativenumber' alone.
        let many = numbering((true, false), 4, 1000, 0);
        assert_eq!(numbered(&many, 2, 6, 0..1), [n("   1 ", 10)]);
        let many = numbering((false, true), 4, 1000, 0);
        assert_eq!(numbered(&many, 2, 6, 0..1), [n("  2 ", 11)]);
        let few = numbering((true, true), 1, 9, 0);
        assert_eq!(numbered(&few, 2, 6, 1..3), [n("1 ", 11), n("3 ", 10)]);
        let few = numbering((false, true), 1, 9, 0);
        assert_eq!(numbered(&few, 2, 6, 2..3), [n("0 ", 10)]);
        let tall = numbering((false, true), 1, 9, 0);
        assert_eq!(numbered(&tall, 0, 12, 8..9), [n(" 8 ", 12)]);
        // 'cursorline' numbering the cursor's line, with or without
        // 'relativenumber'; the bar alone numbers nothing.
        let cul = numbering((true, true), 4, 30, 1);
        assert_eq!(
            numbered(&cul, 2, 6, 1..4),
            [n("  1 ", 11), n("3   ", 13), n("  1 ", 12)]
        );
        let cul = numbering((true, false), 4, 30, 3);
        assert_eq!(numbered(&cul, 2, 6, 1..3), [n("  2 ", 10), n("  3 ", 13)]);
        let bar = numbering((true, false), 4, 30, 2);
        assert_eq!(numbered(&bar, 2, 6, 2..3), [n("  3 ", 10)]);
        // None without a number column, or with 'numberwidth' unknown.
        let none = numbering((false, false), 4, 30, 0);
        assert_eq!(numbers(0, 2, &none, 6, &IDS), None);
        assert_eq!(
            numbers(
                0,
                2,
                &Shape {
                    stepping: None,
                    ..both
                },
                6,
                &IDS
            ),
            None
        );
    }

    /// Needs the column width, known only with acks.
    #[test]
    fn the_cursors_own_line_is_numbered_from_the_left_with_both_on() {
        let s = numbering((true, true), 4, 100, 0);
        let row = render(plain("x"), &s, 30, 8, &look(), 30).unwrap();
        assert_eq!(text(&row), "31  x   ");
        let row = render(plain("x"), &s, 28, 8, &look(), 30).unwrap();
        assert_eq!(text(&row), "  2 x   ");
        let s = Shape {
            stepping: None,
            ..s
        };
        let row = render(plain("x"), &s, 30, 8, &look(), 30).unwrap();
        assert_eq!(text(&row), " 31 x   ", "as it ever was");
    }

    /// A gutter row of `text`, its cells in runs of the groups in `hls`.
    fn gutter_row(text: &str, hls: &[(u32, usize)]) -> Vec<Cell> {
        let hls = hls.iter().flat_map(|&(hl, n)| std::iter::repeat_n(hl, n));
        text.chars()
            .zip(hls)
            .map(|(c, hl)| Cell {
                text: Text::Char(c),
                hl,
            })
            .collect()
    }

    /// Expected values from Neovim 0.12.6, with a fold column, two sign
    /// columns and four number columns.
    #[test]
    fn a_gutter_is_drawn_again_for_the_cursor_moved() {
        let both = Shape {
            textoff: 7,
            ..numbering((true, true), 4, 30, 0)
        };
        let plain = |n: &str, nr: u32| gutter_row(&format!("   {n}"), &[(16, 1), (14, 2), (nr, 4)]);
        let again =
            |row: &[Cell], lnum: i64, shape: &Shape| regutter(row, lnum, (2, 3), shape, 6, &IDS);
        assert_eq!(again(&plain("  2 ", 11), 0, &both), Some(plain("  3 ", 11)));
        assert_eq!(again(&plain("3   ", 10), 2, &both), Some(plain("  1 ", 11)));
        assert_eq!(again(&plain("  1 ", 12), 3, &both), Some(plain("4   ", 10)));
        // A sign's number keeps its own colours.
        let sign = gutter_row(" >>  1 ", &[(16, 1), (70, 2), (68, 4)]);
        assert_eq!(
            again(&sign, 1, &both),
            Some(gutter_row(" >>  2 ", &[(16, 1), (70, 2), (68, 4)]))
        );
        assert_eq!(
            again(
                &gutter_row(" >>  1 ", &[(16, 1), (70, 2), (68, 4)]),
                3,
                &both
            ),
            Some(gutter_row(" >>4   ", &[(16, 1), (70, 2), (68, 4)]))
        );
        // Not laid out as the client lays it out.
        assert_eq!(again(&plain(" 2  ", 11), 0, &both), None);
        // 'cursorline' numbering: sign and fold columns switch groups on the
        // lines left and entered.
        let cul = Shape {
            textoff: 7,
            ..numbering((true, true), 4, 30, 1)
        };
        let left = gutter_row("   3   ", &[(17, 1), (15, 2), (13, 4)]);
        assert_eq!(
            again(&left, 2, &cul),
            Some(gutter_row("     1 ", &[(16, 1), (14, 2), (11, 4)]))
        );
        assert_eq!(
            again(&plain("  1 ", 12), 3, &cul),
            Some(gutter_row("   4   ", &[(17, 1), (15, 2), (13, 4)]))
        );
        assert_eq!(
            again(&sign, 1, &cul),
            Some(gutter_row(" >>  2 ", &[(16, 1), (70, 2), (68, 4)]))
        );
        let onto = gutter_row(" >>  1 ", &[(16, 1), (70, 2), (12, 4)]);
        assert_eq!(again(&onto, 3, &cul), None, "a sign's colours over its own");
        assert_eq!(
            again(
                &gutter_row(" >>  1 ", &[(16, 1), (14, 2), (68, 4)]),
                3,
                &cul
            ),
            None,
            "a sign's number on it"
        );
        // The same groups on and off the cursor line, as in the default
        // colour scheme: only a sign differs, which the client still refuses.
        let same = Gutter {
            cursor_sign: 14,
            cursor_fold: 16,
            ..IDS
        };
        assert_eq!(
            regutter(&plain("  1 ", 12), 3, (2, 3), &cul, 6, &same),
            Some(gutter_row("   4   ", &[(16, 1), (14, 2), (13, 4)]))
        );
        assert_eq!(regutter(&onto, 3, (2, 3), &cul, 6, &same), None, "a sign");
        assert_eq!(
            regutter(
                &gutter_row(" >>  2 ", &[(16, 1), (70, 2), (12, 4)]),
                4,
                (2, 3),
                &cul,
                6,
                &same
            ),
            Some(gutter_row(" >>  1 ", &[(16, 1), (70, 2), (12, 4)])),
            "a sign on a line neither left nor gone to"
        );
        // 'number' alone: nothing moves.
        let nu = Shape {
            textoff: 7,
            ..numbering((true, false), 4, 30, 0)
        };
        assert_eq!(again(&plain("  1 ", 10), 0, &nu), Some(plain("  1 ", 10)));
    }

    #[test]
    fn a_key_mapped_the_lazyvim_way_is_known_exactly() {
        let idiom = |key: &str, rhs: &str, expr: bool, noremap: bool, alone: bool| Idiom {
            key: key.into(),
            rhs: rhs.into(),
            expr,
            noremap,
            alone,
        };
        let lazy = "v:count == 0 ? 'gj' : 'j'";
        assert!(idiom("j", lazy, true, true, true).same());
        assert!(idiom("<Down>", lazy, true, true, true).same());
        assert!(idiom("k", "v:count == 0 ? 'gk' : 'k'", true, true, true).same());
        assert!(idiom("<Up>", "v:count ? 'k' : 'gk'", true, true, true).same());
        assert!(idiom("j", "v:count ? 'j' : 'gj'", true, true, true).same());
        assert!(idiom("j", "gj", false, true, true).same());
        assert!(idiom("k", "gk", false, true, true).same());
        // A near miss, the other way's keys, or not as an expression.
        assert!(!idiom("j", "v:count==0 ? 'gj' : 'j'", true, true, true).same());
        assert!(!idiom("k", lazy, true, true, true).same());
        assert!(!idiom("j", "gj", true, true, true).same());
        assert!(!idiom("j", lazy, false, true, true).same());
        assert!(!idiom("<C-n>", lazy, true, true, true).same());
        // Mapped again, or waiting for a longer mapping.
        assert!(!idiom("j", lazy, true, false, true).same());
        assert!(!idiom("j", lazy, true, true, false).same());
    }

    /// A shape from an agent sending fewer fields gets Neovim's defaults.
    #[test]
    fn the_gutters_options_come_with_the_shape() {
        let mut p = Predictor::default();
        let mut args = shape_args(10);
        if let Some(Value::Array(v)) = args.get_mut(1) {
            v.resize(25, Value::from(0));
            v.extend([
                "b,s".into(),
                "".into(),
                false.into(),
                false.into(),
                false.into(),
                false.into(),
            ]);
        }
        said(&mut p, "shape", args.clone());
        let s = p
            .shape(1000)
            .and_then(|s| s.stepping.clone())
            .expect("stepping");
        assert_eq!(
            (s.numberwidth, s.number_signs, s.cursorline, s.idioms),
            (4, false, Cursorline::default(), vec![])
        );
        assert!(!s.own_highlights && !s.conceals);
        if let Some(Value::Array(v)) = args.get_mut(1) {
            let lazy = Value::Array(vec![
                "j".into(),
                "v:count == 0 ? 'gj' : 'j'".into(),
                true.into(),
                true.into(),
                true.into(),
            ]);
            v.extend([
                6.into(),
                true.into(),
                5.into(),
                Value::Array(vec![lazy]),
                true.into(),
                true.into(),
            ]);
        }
        said(&mut p, "shape", args.clone());
        let s = p
            .shape(1000)
            .and_then(|s| s.stepping.clone())
            .expect("stepping");
        assert_eq!((s.numberwidth, s.number_signs), (6, true));
        assert_eq!(
            s.cursorline,
            Cursorline {
                number: true,
                line: false,
                screenline: true
            }
        );
        assert!(s.idioms.len() == 1 && s.idioms[0].same());
        assert!(s.own_highlights && s.conceals);
        // A 'numberwidth' wider than Neovim lets it be: the widest.
        if let Some(Value::Array(v)) = args.get_mut(1) {
            v[31] = Value::from(i64::MAX);
        }
        said(&mut p, "shape", args);
        let s = p.shape(1000).and_then(|s| s.stepping.clone());
        assert_eq!(s.map(|s| s.numberwidth), Some(20));
    }

    #[test]
    fn text_is_laid_out_as_neovim_lays_it_out() {
        let s = Shape {
            textoff: 0,
            tabstop: 4,
            ..shape()
        };
        let row = render(plain("a\tb\x01c"), &s, 0, 12, &look(), 0).unwrap();
        assert_eq!(text(&row), "a   b^Ac    ");
        let row = render(plain("日x"), &s, 0, 5, &look(), 0).unwrap();
        assert_eq!(row[0].text, Text::Char('日'));
        assert_eq!(row[1].text, Text::Half);
        assert_eq!(row[2].text, Text::Char('x'));
        let row = render(plain("e\u{301}x"), &s, 0, 3, &look(), 0).unwrap();
        assert_eq!(row[0].text, Text::Cluster("e\u{301}".into()));
        assert_eq!(row[1].text, Text::Char('x'));
        let listed = Shape {
            list: true,
            tab: vec!['>', '-'],
            ..s.clone()
        };
        let row = render(plain("\tx"), &listed, 0, 6, &look(), 0).unwrap();
        assert_eq!(text(&row), ">---x ");
    }

    /// Expected values from Neovim 0.12, whatever the syntax colours.
    #[test]
    fn what_neovim_shows_as_something_else_is_in_specialkey() {
        use crate::client::model::syntax_hl;
        let s = Shape {
            textoff: 0,
            tabstop: 4,
            ..shape()
        };
        let hls = |row: &[Cell]| row.iter().map(|c| c.hl).collect::<Vec<_>>();
        let row = render(plain("a\x01b\x7fc"), &s, 0, 7, &look(), 0).unwrap();
        assert_eq!(text(&row), "a^Ab^?c");
        assert_eq!(hls(&row), [0, 20, 20, 0, 20, 20, 0]);
        let row = render(plain("x\u{80}y"), &s, 0, 6, &look(), 0).unwrap();
        assert_eq!(text(&row), "x<80>y");
        assert_eq!(hls(&row), [0, 20, 20, 20, 20, 0]);
        let listed = Shape {
            list: true,
            ..s.clone()
        };
        let row = render(plain("p\tq"), &listed, 0, 4, &look(), 0).unwrap();
        assert_eq!(text(&row), "p^Iq");
        assert_eq!(hls(&row), [0, 20, 20, 0]);
        let spans = [Span {
            start: 0,
            end: usize::MAX,
            group: 40,
        }];
        let row = render(Some(("a\x01b", &spans)), &s, 0, 4, &look(), 0).unwrap();
        let word = syntax_hl(0, 40);
        assert_eq!(hls(&row), [word, 20, 20, word]);
        let from1 = Shape {
            wrap: false,
            leftcol: 1,
            ..s.clone()
        };
        let row = render(plain("\x01bc"), &from1, 0, 3, &look(), 0).unwrap();
        assert_eq!(text(&row), "Abc");
        assert_eq!(hls(&row), [20, 0, 0]);
        let row = render(plain("\u{80}bc"), &from1, 0, 5, &look(), 0).unwrap();
        assert_eq!(text(&row), "80>bc");
        assert_eq!(hls(&row), [20, 20, 20, 0, 0]);
        let tabbed = Shape {
            list: true,
            tab: vec!['>', '-'],
            ..s
        };
        let row = render(plain("\tx"), &tabbed, 0, 5, &look(), 0).unwrap();
        assert_eq!(text(&row), ">---x");
        assert!(!hls(&row).contains(&20), "Whitespace's, not SpecialKey's");
    }

    /// Expected values from Neovim 0.12 with `tab:>-` and `tab:<->`, whatever
    /// the syntax colours.
    #[test]
    fn a_tab_shown_with_listchars_is_in_whitespace() {
        let two = Shape {
            textoff: 0,
            wrap: false,
            list: true,
            tab: vec!['>', '-'],
            ..shape()
        };
        let three = Shape {
            tab: vec!['<', '-', '>'],
            ..two.clone()
        };
        let cells = |line: &str, s: &Shape, width: usize| {
            let row = render(plain(line), s, 0, width, &look(), 0).unwrap();
            (text(&row), row.iter().map(|c| c.hl).collect::<Vec<_>>())
        };
        let ws = |n: usize| vec![21; n];
        assert_eq!(
            cells("\tx", &two, 9),
            (">-------x".to_string(), [ws(8), vec![0]].concat())
        );
        assert_eq!(
            cells("\tx", &three, 9),
            ("<------>x".to_string(), [ws(8), vec![0]].concat())
        );
        assert_eq!(cells("abcdef\tx", &two, 9).0, "abcdef>-x");
        assert_eq!(cells("abcdef\tx", &three, 9).0, "abcdef<>x");
        assert_eq!(cells("abcdefg\tx", &two, 9).0, "abcdefg>x");
        assert_eq!(
            cells("abcdefg\tx", &three, 9),
            (
                "abcdefg>x".to_string(),
                [vec![0; 7], ws(1), vec![0]].concat()
            ),
            "one cell wide: the third alone"
        );
        let from1 = |s: &Shape| Shape {
            leftcol: 1,
            ..s.clone()
        };
        assert_eq!(
            cells("\tx", &from1(&two), 8),
            ("-------x".to_string(), [ws(7), vec![0]].concat())
        );
        assert_eq!(cells("\tx", &from1(&three), 8).0, "------>x");
        let spans = [Span {
            start: 0,
            end: usize::MAX,
            group: 40,
        }];
        let row = render(Some(("\tx", &spans)), &two, 0, 9, &look(), 0).unwrap();
        let hls: Vec<u32> = row[..8].iter().map(|c| c.hl).collect();
        assert_eq!(hls, ws(8), "coloured by the editor's highlighting");
        let off = Shape { list: false, ..two };
        assert_eq!(cells("\tx", &off, 9), ("        x".to_string(), vec![0; 9]));
    }

    /// A `'list'` window that does not wrap, with `'listchars'` `option`.
    fn listing(option: &str) -> Shape {
        const KEYS: [&str; 10] = [
            "space",
            "multispace",
            "lead",
            "leadmultispace",
            "trail",
            "nbsp",
            "eol",
            "extends",
            "precedes",
            "leadtab",
        ];
        let mut sent = vec![String::new(); KEYS.len()];
        let mut tab = Vec::new();
        for (key, value) in option.split(',').filter_map(|o| o.split_once(':')) {
            match KEYS.iter().position(|k| *k == key) {
                Some(i) => sent[i] = value.into(),
                None => tab = value.chars().collect(),
            }
        }
        Shape {
            textoff: 0,
            wrap: false,
            list: true,
            tab,
            lcs: Listchars::read(&sent),
            ..shape()
        }
    }

    /// A line drawn `width` wide and its highlights per cell (`.` own, `n`
    /// NonText, `s` SpecialKey, `w` Whitespace), trailing blanks dropped.
    fn seen(line: &str, shape: &Shape, width: usize) -> (String, String) {
        let mut row = render(plain(line), shape, 0, width, &look(), 0).unwrap();
        while row
            .last()
            .is_some_and(|c| c.text == Text::Char(' ') && c.hl == 0)
        {
            row.pop();
        }
        let hls = row
            .iter()
            .map(|c| match c.hl {
                0 => '.',
                19 => 'n',
                20 => 's',
                21 => 'w',
                _ => '?',
            })
            .collect();
        (text(&row), hls)
    }

    fn pair(text: &str, hls: &str) -> (String, String) {
        (text.into(), hls.into())
    }

    /// Expected values from Neovim 0.12, 20 columns wide.
    #[test]
    fn blanks_are_drawn_with_listchars_as_neovim_draws_them() {
        let (spaced, nbsp, blank, tabbed, led) = (
            "  a b  c   ",
            "\u{a0}x\u{202f}y",
            "    ",
            "\t a",
            "     x  y",
        );
        let s = listing("tab:> ,trail:-,nbsp:+");
        assert_eq!(seen(spaced, &s, 20), pair("  a b  c---", "........www"));
        assert_eq!(seen(nbsp, &s, 20), pair("+x+y", "w.w."));
        assert_eq!(seen(blank, &s, 20), pair("----", "wwww"));
        assert_eq!(seen(tabbed, &s, 20), pair(">        a", "wwwwwwww.."));
        assert_eq!(seen(led, &s, 20), pair(led, "........."));
        let s = listing("space:.");
        assert_eq!(seen(spaced, &s, 20), pair("..a.b..c...", "ww.w.ww.www"));
        assert_eq!(seen(nbsp, &s, 20), pair(nbsp, "...."), "no nbsp:");
        assert_eq!(seen(blank, &s, 20), pair("....", "wwww"));
        assert_eq!(seen(tabbed, &s, 20), pair("^I.a", "ssw."));
        assert_eq!(seen(led, &s, 20), pair(".....x..y", "wwwww.ww."));
        let s = listing("space:.,lead:_,trail:-");
        assert_eq!(seen(spaced, &s, 20), pair("__a.b..c---", "ww.w.ww.www"));
        assert_eq!(seen(blank, &s, 20), pair("----", "wwww"), "all trailing");
        assert_eq!(seen(tabbed, &s, 20), pair("^I_a", "ssw."));
        assert_eq!(seen(led, &s, 20), pair("_____x..y", "wwwww.ww."));
        let s = listing("multispace:ab");
        assert_eq!(seen(spaced, &s, 20), pair("aba babcaba", "ww...ww.www"));
        assert_eq!(seen(blank, &s, 20), pair("abab", "wwww"));
        assert_eq!(seen(tabbed, &s, 20), pair("^I a", "ss.."));
        assert_eq!(seen(led, &s, 20), pair("ababaxaby", "wwwww.ww."));
        let s = listing("multispace:abc,space:.");
        assert_eq!(seen(spaced, &s, 20), pair("aba.babcabc", "ww.w.ww.www"));
        assert_eq!(seen(blank, &s, 20), pair("abca", "wwww"));
        let s = listing("leadmultispace:12,space:.");
        assert_eq!(seen(spaced, &s, 20), pair("12a.b..c...", "ww.w.ww.www"));
        assert_eq!(seen(blank, &s, 20), pair("....", "wwww"), "none leading");
        assert_eq!(seen(tabbed, &s, 20), pair("^I.a", "ssw."));
        assert_eq!(seen(led, &s, 20), pair("12121x..y", "wwwww.ww."));
        let s = listing("leadmultispace:12,multispace:ab,trail:-");
        assert_eq!(seen(spaced, &s, 20), pair("12a babc---", "ww...ww.www"));
        assert_eq!(seen(blank, &s, 20), pair("----", "wwww"));
        assert_eq!(
            seen(tabbed, &s, 20),
            pair("^I a", "ssw."),
            "leading, and nothing for one alone: itself in Whitespace"
        );
        assert_eq!(seen(led, &s, 20), pair("12121xaby", "wwwww.ww."));
        let s = listing("lead:_");
        assert_eq!(seen(spaced, &s, 20), pair("__a b  c", "ww......"));
        assert_eq!(seen(blank, &s, 20), pair("", ""));
        assert_eq!(seen(tabbed, &s, 20), pair("^I_a", "ssw."));
        assert_eq!(seen(led, &s, 20), pair("_____x  y", "wwwww...."));
        let s = listing("nbsp:+,space:.");
        assert_eq!(seen(nbsp, &s, 20), pair("+x+y", "w.w."));
        let s = listing("leadmultispace:12");
        assert_eq!(seen(" \tx", &s, 20), pair(" ^Ix", "wss."));
        let off = Shape {
            list: false,
            ..listing("space:.,trail:-,nbsp:+")
        };
        assert_eq!(seen(spaced, &off, 20), pair("  a b  c", "........"));
        assert_eq!(seen(nbsp, &off, 20), pair(nbsp, "...."));
    }

    /// The cycle counts from the line's start, not the view's (expected values
    /// from Neovim 0.12); a tab or other character restarts it, and a space
    /// with a combining mark is drawn as is but continues it.
    #[test]
    fn a_run_of_spaces_goes_through_its_listchars_from_the_lines_start() {
        let (lead, run, mixed) = ("      x", "a      b      c", "  \t  x \t ");
        let s = Shape {
            leftcol: 2,
            ..listing("multispace:abc")
        };
        assert_eq!(seen(lead, &s, 20), pair("cabcx", "wwww."));
        assert_eq!(seen(run, &s, 20), pair("bcabcbabcabcc", "wwwww.wwwwww."));
        let s = Shape {
            leftcol: 3,
            ..listing("tab:>-,leadtab:|.,precedes:<,leadmultispace:123,multispace:ab")
        };
        assert_eq!(seen(lead, &s, 20), pair("<23x", "nww."));
        assert_eq!(seen(run, &s, 20), pair("<babbabababc", "nwww.wwwwww."));
        assert_eq!(seen(mixed, &s, 20), pair("<....12x >---", "nwwwwww..wwww"));
        let s = listing("leadmultispace:12");
        assert_eq!(seen(mixed, &s, 20), pair("12^I12x ^I", "wwssww..ss"));
        let marked = "a  \u{301}  b";
        let s = listing("tab:>-,trail:~,lead:_,multispace:ab,space:.");
        assert_eq!(seen(marked, &s, 20), pair("aa \u{301}bab", ".w.ww."));
        let nbsp = "\u{a0}\u{301}x \u{a0}";
        assert_eq!(seen(nbsp, &s, 20), pair("\u{a0}\u{301}x.\u{a0}", "..w."));
        let s = listing("space:.,nbsp:+,multispace:ab");
        assert_eq!(seen(marked, &s, 20), pair("aa \u{301}bab", ".w.ww."));
        assert_eq!(seen(nbsp, &s, 20), pair("\u{a0}\u{301}x.+", "..ww"));
        // Whether the space after a mark is in a run depends on the next.
        let s = listing("multispace:abcde");
        assert_eq!(
            seen("x  \u{301} \u{301}  y", &s, 20),
            pair("xa \u{301} \u{301}bcy", ".w..ww.")
        );
    }

    /// Expected values from Neovim 0.12. With only `leadtab`, leading spaces
    /// are `Whitespace` only when Neovim inspects the line anyway
    /// ([`Shape::checks`], or colours of its own).
    #[test]
    fn a_leading_tab_is_drawn_with_leadtab_as_neovim_draws_it() {
        let (mixed, tabs, one, lead) = ("  \t  x \t ", "\t\tx\ty", " \tx", "      x");
        let s = listing("tab:>-,leadtab:|.-,lead:_,trail:~");
        assert_eq!(
            seen(mixed, &s, 20),
            pair("__|....-__x~>---~", "wwwwwwwwww.wwwwww")
        );
        assert_eq!(
            seen(tabs, &s, 20),
            pair("|......-|......-x>--", "wwwwwwwwwwwwwwww.www")
        );
        assert_eq!(seen(one, &s, 20), pair("_|.....-x", "wwwwwwww."));
        let s = listing("tab:>-,leadtab:|.");
        assert_eq!(
            seen(mixed, &s, 20),
            pair("  |.....  x >---", "..wwwwww....wwww")
        );
        assert_eq!(seen(one, &s, 20), pair(" |......x", ".wwwwwww."));
        assert_eq!(seen(lead, &s, 20), pair(lead, "......."));
        let from3 = Shape {
            leftcol: 3,
            ..s.clone()
        };
        assert_eq!(
            seen(tabs, &from3, 20),
            pair(".....|.......x>-----", "wwwwwwwwwwwww.wwwwww")
        );
        assert_eq!(seen(one, &from3, 20), pair(".....x", "wwwww."));
        let checks = Shape {
            checks: true,
            ..s.clone()
        };
        assert_eq!(
            seen(mixed, &checks, 20),
            pair("  |.....  x >---", "wwwwwwwwww..wwww")
        );
        assert_eq!(seen(lead, &checks, 20), pair(lead, "wwwwww."));
        let spans = [Span {
            start: 6,
            end: 7,
            group: 40,
        }];
        let row = render(Some((lead, &spans)), &s, 0, 7, &look(), 0).unwrap();
        assert!(row[..6].iter().all(|c| c.hl == 21), "colours of its own");
        let trail = listing("tab:>-,leadtab:|.,trail:~");
        assert_eq!(seen(lead, &trail, 20), pair(lead, "wwwwww."));
    }

    /// Expected values from Neovim 0.12, 20 columns wide, all in `NonText`.
    #[test]
    fn a_lines_end_and_edges_are_drawn_as_neovim_draws_them() {
        let x = |n: usize| "x".repeat(n);
        let dots = |n: usize| ".".repeat(n);
        let wide = "日本".repeat(7);
        let (cut_wide, tab, ctrl) = (
            format!("{}日z", x(19)),
            format!("{}\ty", x(16)),
            format!("{}\x01z", x(19)),
        );
        let s = listing("eol:¶,extends:»,precedes:«,tab:>-");
        assert_eq!(
            seen(&x(20), &s, 20),
            (format!("{}»", x(19)), format!("{}n", dots(19))),
            "only its end follows"
        );
        assert_eq!(seen(&cut_wide, &s, 20).0, format!("{}»", x(19)));
        assert_eq!(seen(&wide, &s, 20), ("日本".repeat(5), dots(20)));
        assert_eq!(seen("abc", &s, 20), pair("abc¶", "...n"));
        assert_eq!(
            seen(&tab, &s, 20),
            (format!("{}>--»", x(16)), format!("{}wwwn", dots(16)))
        );
        assert_eq!(seen(&ctrl, &s, 20).0, format!("{}»", x(19)));
        assert_eq!(seen("", &s, 20), pair("¶", "n"));
        let from = |left: usize, s: &Shape| Shape {
            leftcol: left,
            ..s.clone()
        };
        let s1 = from(1, &s);
        assert_eq!(
            seen(&x(20), &s1, 20),
            (format!("«{}¶", x(18)), format!("n{}n", dots(18)))
        );
        assert_eq!(
            seen(&cut_wide, &s1, 20),
            (format!("«{}日", x(17)), format!("n{}", dots(19))),
            "the half of 日 in the last column"
        );
        assert_eq!(
            seen(&wide, &s1, 20),
            (
                format!("«{}»", "本日".repeat(4) + "本"),
                format!("n{}n", dots(18))
            )
        );
        let ab = format!("ab{}", "日本".repeat(5));
        assert_eq!(seen(&ab, &s1, 20).0, format!("«{}日»", "日本".repeat(4)));
        assert_eq!(seen("abc", &s1, 20), pair("«c¶", "n.n"));
        assert_eq!(
            seen(&tab, &s1, 20),
            (format!("«{}>---»", x(14)), format!("n{}wwwwn", dots(14)))
        );
        assert_eq!(
            seen(&ctrl, &s1, 20),
            (format!("«{}^»", x(17)), format!("n{}sn", dots(17)))
        );
        assert_eq!(seen("", &s1, 20), pair("«", "n"));
        let s2 = from(2, &s);
        assert_eq!(
            seen(&wide, &s2, 20),
            (
                format!("«<{}", "日本".repeat(4) + "日"),
                format!("nn{}", dots(18))
            )
        );
        assert_eq!(seen("abc", &s2, 20), pair("«¶", "nn"));
        assert_eq!(
            seen(&cut_wide, &s2, 20),
            (format!("«{}日»", x(16)), format!("n{}..n", dots(16)))
        );
        let eol = from(5, &listing("eol:¶"));
        assert_eq!(seen("abc", &eol, 20), pair("¶", "n"), "scrolled past");
        assert_eq!(seen("abcde", &eol, 20), pair("¶", "n"));
        assert_eq!(seen("", &eol, 20), pair("¶", "n"));
        assert_eq!(
            seen(&wide, &eol, 20),
            (
                format!("<{}>", "本日".repeat(4) + "本"),
                format!("n{}n", dots(18))
            ),
            "cut as ever"
        );
        let precedes = from(5, &listing("precedes:«"));
        assert_eq!(seen("abc", &precedes, 20), pair("", ""), "nothing there");
        assert_eq!(seen(&x(20), &precedes, 20).0, format!("«{}", x(14)));
        assert_eq!(
            seen("abc", &from(6, &listing("eol:¶,precedes:«")), 20),
            pair("«", "n")
        );
        let extends = listing("extends:»");
        assert_eq!(seen(&x(20), &extends, 20).0, x(20), "nothing follows");
        assert_eq!(
            seen(&"y".repeat(30), &extends, 20).0,
            format!("{}»", "y".repeat(19))
        );
        assert_eq!(
            seen(&tab, &extends, 20),
            (format!("{}^Iy", x(16)), format!("{}ss.", dots(16)))
        );
        let tabbed = from(2, &listing("tab:>-,precedes:<,lead:_,trail:-"));
        assert_eq!(
            seen("\tabcdefghijklmnopqrstuvw", &tabbed, 20),
            (
                "<-----abcdefghijklmn".to_string(),
                format!("nwwwww{}", dots(14))
            )
        );
        assert_eq!(seen("  ab  ", &tabbed, 20), pair("<b--", "n.ww"));
        let led = from(1, &listing("lead:_,trail:-,precedes:<"));
        assert_eq!(seen("  ab  ", &led, 20), pair("<ab--", "n..ww"));
    }

    /// As Neovim 0.12 drew it: a line as wide as the window needs two rows.
    #[test]
    fn a_lines_end_takes_a_cell_of_a_window_that_wraps() {
        let s = Shape {
            wrap: true,
            ..listing("eol:$,nbsp:+,space:.")
        };
        let row = render(plain(&"x".repeat(19)), &s, 0, 20, &look(), 0).unwrap();
        assert_eq!(text(&row), format!("{}$", "x".repeat(19)));
        assert_eq!(render(plain(&"x".repeat(20)), &s, 0, 20, &look(), 0), None);
        let row = render(plain("a\u{a0}\u{301}b c"), &s, 0, 20, &look(), 0).unwrap();
        assert_eq!(text(&row[..6]), "a\u{a0}\u{301}b.c$");
        let off = Shape { list: false, ..s };
        assert!(render(plain(&"x".repeat(20)), &off, 0, 20, &look(), 0).is_some());
    }

    #[test]
    fn a_long_line_is_cut_off_or_refused() {
        let s = Shape {
            textoff: 0,
            ..shape()
        };
        assert_eq!(render(plain("abcdefgh"), &s, 0, 6, &look(), 0), None);
        let s = Shape {
            wrap: false,
            leftcol: 2,
            ..s
        };
        let row = render(plain("abcdefgh"), &s, 0, 4, &look(), 0).unwrap();
        assert_eq!(text(&row), "cdef");
    }

    /// Expected values from Neovim 0.12, in `NonText` whatever the
    /// character's colours; never half a character alone.
    #[test]
    fn a_wide_character_the_edge_cuts_is_drawn_as_neovim_draws_it() {
        let s = Shape {
            textoff: 0,
            wrap: false,
            ..shape()
        };
        let mark = |c| Cell {
            text: Text::Char(c),
            hl: 19,
        };
        let row = render(plain("abc日x"), &s, 0, 4, &look(), 0).unwrap();
        assert_eq!(text(&row), "abc>");
        assert_eq!(row[3], mark('>'));
        let from1 = Shape {
            leftcol: 1,
            ..s.clone()
        };
        let row = render(plain("日abcd"), &from1, 0, 4, &look(), 0).unwrap();
        assert_eq!(text(&row), "<abc");
        assert_eq!(row[0], mark('<'));
        // With syntax colours and a combining mark.
        let spans = [Span {
            start: 0,
            end: usize::MAX,
            group: 40,
        }];
        let row = render(Some(("abc日\u{301}x", &spans)), &s, 0, 4, &look(), 0).unwrap();
        assert_eq!(row[3], mark('>'));
        // Past a gutter, in the window's last column and its text's first.
        let gutter = Shape {
            textoff: 4,
            number: true,
            ..s.clone()
        };
        let row = render(plain("abc日x"), &gutter, 6, 8, &look(), 0).unwrap();
        assert_eq!(text(&row), "  7 abc>");
        let gutter = Shape {
            leftcol: 1,
            ..gutter
        };
        let row = render(plain("日abcd"), &gutter, 6, 8, &look(), 0).unwrap();
        assert_eq!(text(&row), "  7 <abc");
        // A one-column text area, cut either way.
        let row = render(plain("日"), &s, 0, 1, &look(), 0).unwrap();
        assert_eq!(row, [mark('>')]);
        let row = render(plain("日"), &from1, 0, 1, &look(), 0).unwrap();
        assert_eq!(row, [mark('<')]);
    }

    #[test]
    fn a_wide_character_that_fits_is_drawn_whole() {
        let s = Shape {
            textoff: 0,
            wrap: false,
            ..shape()
        };
        let whole = [
            Cell {
                text: Text::Char('日'),
                hl: 0,
            },
            Cell {
                text: Text::Half,
                hl: 0,
            },
        ];
        let row = render(plain("ab日x"), &s, 0, 4, &look(), 0).unwrap();
        assert_eq!(text(&row), "ab日");
        assert_eq!(row[2..], whole, "up to the right edge");
        let from1 = Shape {
            leftcol: 1,
            ..s.clone()
        };
        let row = render(plain("a日bc"), &from1, 0, 4, &look(), 0).unwrap();
        assert_eq!(text(&row), "日bc");
        assert_eq!(row[..2], whole, "from the left");
        let wraps = Shape { wrap: true, ..s };
        assert_eq!(render(plain("abc日"), &wraps, 0, 4, &look(), 0), None);
        assert_eq!(
            text(&render(plain("ab日"), &wraps, 0, 4, &look(), 0).unwrap()),
            "ab日"
        );
    }

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

    /// Editor-drawn groups come from the editor if sent, else the text's
    /// highlight (0 for `EndOfBuffer`).
    #[test]
    fn the_colours_are_the_windows_own() {
        let cell = |hl| Cell {
            text: Text::Char('x'),
            hl,
        };
        let row = |g: u32, t: u32| vec![cell(g), cell(g), cell(t), cell(t)];
        let rows = vec![row(5, 0), row(9, 9), row(5, 0), row(5, 0), row(3, 3)];
        let said = |name: &str| match name {
            "EndOfBuffer" => Some(18),
            "NonText" => Some(19),
            "SpecialKey" => Some(20),
            "Whitespace" => Some(21),
            _ => None,
        };
        let l = Look::of(&rows, 4, 2, Some(1), said);
        assert_eq!(l.gutter, vec![5, 5]);
        assert_eq!(l.text, 0);
        assert_eq!(l.eob, Some(row(3, 3)));
        assert_eq!(l.eob_hl, 3);
        assert_eq!((l.nontext, l.specialkey, l.whitespace), (19, 20, 21));
        assert_eq!(Look::of(&rows, 5, 2, None, said).eob_hl, 18);
        let l = Look::of(&[row(5, 7)], 1, 2, None, |_| None);
        assert_eq!(
            (l.eob_hl, l.nontext, l.specialkey, l.whitespace),
            (0, 7, 7, 7),
            "not said"
        );
    }

    /// Recorded from Neovim 0.12 (`h` rows, 100 lines, `'scrolloff'` `so`);
    /// views are 1-based (top, cursor).
    #[test]
    fn scrolls_go_where_neovim_takes_them() {
        use Command::*;
        use Edge::*;
        let half = |down| Half { down, count: None };
        let page = |down, count| Page { down, count };
        let line = |to| Line {
            to,
            first: false,
            line: None,
        };
        let at = |to, first, n| Line {
            to,
            first,
            line: Some(n),
        };
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
            // With a count: that line, the cursor taken there.
            (0, 22, at(Top, false, 45), (40, 50), (45, 45)),
            (0, 22, at(Top, true, 45), (40, 50), (45, 45)),
            (0, 22, at(Middle, true, 45), (40, 50), (35, 45)),
            (0, 22, at(Bottom, true, 45), (40, 50), (24, 45)),
            (0, 22, at(Bottom, false, 45), (40, 50), (24, 45)),
            (5, 22, at(Bottom, false, 55), (40, 50), (39, 55)),
            (0, 22, at(Top, false, 53), (40, 50), (53, 53)),
            (0, 22, at(Top, false, 500), (40, 50), (100, 100)),
            // The Shift/Ctrl wheel in Insert mode: what the window shows.
            (0, 22, Screen(1), (40, 50), (62, 62)),
            (0, 22, Screen(-1), (40, 50), (18, 39)),
            (0, 22, Screen(1), (90, 95), (100, 100)),
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

    #[test]
    fn the_cursor_lands_where_neovim_puts_it() {
        let s = shape();
        assert_eq!(landing("abcdef", &s, 3, false), 3);
        assert_eq!(landing("ab", &s, 3, false), 1);
        assert_eq!(landing("", &s, 3, false), 0);
        assert_eq!(landing("\tx", &s, 3, false), 7, "on a tab, at its end");
        assert_eq!(landing("日本", &s, 3, false), 2, "on the second, wide");
        assert_eq!(
            landing("ab", &s, 30, true),
            2,
            "in insert mode, past the end"
        );
        assert_eq!(landing("\tx", &s, 3, true), 0, "and at a tab's start");
        assert_eq!(first_nonblank("    x", &s), 4);
        assert_eq!(first_nonblank("\t x", &s), 9);
        assert_eq!(first_nonblank("   ", &s), 2, "all blank: the last");
    }

    #[test]
    fn insert_mode_scrolls_are_told_apart() {
        let mut t = Typed::default();
        typed(&mut t, "i");
        let at = t.fenced();
        t.answered(at, Mode::Insert);
        assert!(t.inserting() && t.sure());
        assert_eq!(
            typed(&mut t, "<PageDown><C-x><C-e><C-e><C-y><S-Up>"),
            [
                Command::Page {
                    down: true,
                    count: 1
                },
                Command::Lines(1),
                Command::Lines(1),
                Command::Lines(-1),
                Command::Page {
                    down: false,
                    count: 1
                },
            ]
        );
        assert!(t.sure(), "scrolls, all of them");
        // <C-e> without <C-x> is the character below; text is unpredicted.
        assert!(typed(&mut t, "<C-x>a<C-e>").is_empty());
        assert!(!t.sure() && t.inserting());
        let at = t.fenced();
        t.answered(at, Mode::Insert);
        assert!(t.sure());
        // A key that waits for another: lost.
        assert!(typed(&mut t, "<C-r><PageDown>").is_empty());
        assert!(!t.inserting());
        // An escape: normal mode, as far as the client can tell.
        typed(&mut t, "<Esc>");
        let at = t.fenced();
        t.answered(at, Mode::Normal);
        assert_eq!(typed(&mut t, "<C-e>"), [Command::Lines(1)]);
        // Mapped in insert mode: not followed.
        typed(&mut t, "a");
        let at = t.fenced();
        t.answered(at, Mode::Insert);
        let mapped: Vec<Command> = tokens("<PageDown>")
            .filter_map(|k| t.key(k, |insert, k| insert && k == "<PageDown>"))
            .collect();
        assert!(mapped.is_empty());
    }

    /// Recorded from Neovim 0.12 in an 80-column window that does not wrap,
    /// line `n` (1-based) cut to `[10, 40, 100, 160, 5, 0, 70][n % 7]` columns.
    #[test]
    fn sideways_scrolls_go_where_neovim_takes_them() {
        use Side::*;
        let text: Vec<String> = (0..200)
            .map(|i: usize| {
                let n = i + 1;
                let len = [10, 40, 100, 160, 5, 0, 70][n % 7];
                let s = format!("{}line {n}{}", " ".repeat(n % 4), "x".repeat(200));
                s[..len.min(s.len())].to_string()
            })
            .collect();
        // 'sidescrolloff', the gutter, what is typed or turned, from (line,
        // col, left), to (col, left).
        type Case = (usize, usize, Side, (i64, usize, usize), (usize, usize));
        #[rustfmt::skip]
        let cases: &[Case] = &[
            (0, 0, Cols(1), (52, 0, 0), (1, 1)),
            (0, 0, Cols(1), (52, 10, 0), (10, 1)),
            (0, 0, Cols(5), (52, 10, 0), (10, 5)),
            (0, 0, Cols(-1), (52, 10, 10), (10, 9)),
            (0, 0, Cols(-30), (52, 10, 10), (10, 0)),
            (0, 0, Halves(1), (52, 10, 0), (40, 40)),
            (0, 0, Halves(2), (52, 10, 0), (80, 80)),
            (0, 0, Halves(-1), (52, 100, 60), (99, 20)),
            (0, 0, Start, (52, 100, 21), (100, 100)),
            (0, 0, End, (52, 100, 21), (100, 21)),
            (0, 0, End, (52, 30, 0), (30, 0)),
            (0, 0, Start, (52, 130, 100), (130, 130)),
            (0, 0, Wheel(-6), (52, 10, 0), (10, 0)),
            (0, 0, Wheel(6), (52, 10, 10), (16, 16)),
            (0, 0, Wheel(80), (52, 10, 0), (80, 80)),
            (0, 0, Wheel(-6), (53, 2, 0), (2, 0)),
            (0, 0, Cols(20), (53, 2, 0), (4, 4)),
            (0, 0, Cols(1), (54, 0, 0), (0, 0)),
            (0, 0, Cols(1), (53, 2, 2), (3, 3)),
            (3, 0, Cols(1), (52, 0, 0), (4, 1)),
            (3, 0, Cols(1), (52, 10, 0), (10, 1)),
            (3, 0, Cols(-1), (52, 10, 7), (10, 6)),
            (3, 0, Halves(1), (52, 10, 0), (43, 40)),
            (3, 0, Halves(-1), (52, 100, 60), (96, 20)),
            (3, 0, Start, (52, 100, 24), (100, 97)),
            (3, 0, End, (52, 100, 24), (100, 24)),
            (3, 0, Start, (52, 130, 100), (130, 127)),
            (3, 0, Wheel(6), (52, 10, 7), (16, 13)),
            (3, 0, Wheel(80), (52, 10, 0), (83, 80)),
            (3, 0, Cols(20), (53, 2, 0), (4, 1)),
            (3, 0, Cols(1), (53, 2, 0), (4, 1)),
            (0, 4, Halves(-1), (52, 100, 60), (95, 20)),
            (0, 4, Halves(1), (52, 10, 0), (40, 40)),
            (0, 4, Start, (52, 100, 25), (100, 100)),
            (0, 4, End, (52, 100, 25), (100, 25)),
            (0, 4, Wheel(80), (52, 10, 0), (80, 80)),
        ];
        for &(siso, textoff, side, (line, vcol, left), want) in cases {
            let s = Shape {
                textoff,
                wrap: false,
                sidescrolloff: siso,
                ..shape()
            };
            let at = Across {
                left,
                line: line - 1,
                vcol,
            };
            let got = across(
                side,
                at,
                80,
                &s,
                |l| text.get(l as usize).map(String::as_str),
                39..61,
            )
            .map(|a| (a.vcol, a.left));
            assert_eq!(
                got,
                Some(want),
                "{side:?} from {line}, {vcol}, {left}; siso={siso}, gutter {textoff}"
            );
        }
        // Wrapping, nothing moves.
        let s = Shape {
            wrap: true,
            ..shape()
        };
        let at = Across {
            left: 0,
            line: 51,
            vcol: 10,
        };
        assert_eq!(
            across(
                Cols(5),
                at,
                80,
                &s,
                |l| text.get(l as usize).map(String::as_str),
                39..61
            ),
            Some(at)
        );
    }

    /// Recorded from Neovim 0.12 in an 80-column window that does not wrap:
    /// the view's first column after a scroll left the cursor at `vcol` with
    /// the view from `left`.
    #[test]
    fn a_view_moves_sideways_to_show_the_cursor_as_neovim_moves_it() {
        let long = format!("line 2 {}", "abcdefghij".repeat(20));
        let wide = format!("line 102 {}", "日本".repeat(60));
        // The line, 'sidescrolloff', 'sidescroll', the gutter, (vcol, left),
        // and the column the view is shown from.
        type Case<'a> = (&'a str, usize, usize, usize, (usize, usize), usize);
        #[rustfmt::skip]
        let cases: &[Case] = &[
            // In view, nothing moves.
            (&long, 0, 1, 0, (50, 40), 40),
            // Left of it: by as far, or from half the view away and more,
            // centred, no further than the first column.
            ("line 3", 0, 1, 0, (5, 40), 5),
            ("line 3", 0, 1, 0, (5, 100), 0),
            (&long, 0, 1, 0, (150, 200), 110),
            // Right of it.
            (&long, 0, 1, 0, (100, 5), 21),
            // A character two cells wide that an edge cuts.
            (&wide, 0, 1, 0, (39, 40), 39),
            (&wide, 0, 1, 0, (79, 0), 1),
            // 'sidescrolloff' from the edges.
            (&long, 5, 1, 0, (42, 40), 37),
            // 'sidescroll' columns at least, or, with it 0, centred.
            (&wide, 0, 10, 0, (39, 40), 30),
            ("line 3", 0, 0, 0, (5, 40), 0),
            // Past the end of the line, in Insert mode.
            ("line 21", 0, 1, 0, (7, 30), 7),
            ("line 21", 0, 1, 0, (7, 167), 0),
            // A tab, the cursor on its last column, wholly in view.
            ("\tabc", 0, 1, 0, (7, 4), 0),
            // A gutter four columns wide: 76 of text.
            ("line 3", 0, 1, 4, (5, 40), 5),
            ("line 3", 0, 1, 4, (5, 100), 0),
            (&long, 0, 1, 4, (100, 5), 25),
        ];
        for &(text, siso, ss, textoff, (vcol, left), want) in cases {
            let s = Shape {
                textoff,
                wrap: false,
                sidescrolloff: siso,
                sidescroll: ss,
                ..shape()
            };
            assert_eq!(
                showing(text, &s, vcol, left, 80),
                want,
                "{text:.12?} at {vcol} from {left}; siso={siso}, ss={ss}, gutter {textoff}"
            );
        }
        // Wrapping, nothing moves.
        assert_eq!(showing("line 3", &shape(), 5, 0, 80), 0);
    }

    #[test]
    fn a_column_on_a_lines_last_character_is_at_its_end() {
        let s = shape();
        assert!(!at_end("abc", &s, 1));
        assert!(at_end("abc", &s, 2));
        assert!(at_end("abc", &s, 9));
        assert!(!at_end("a日", &s, 0));
        assert!(at_end("a日", &s, 1), "a character two cells wide");
        assert!(at_end("", &s, 0), "an empty line");
    }

    #[test]
    fn the_wheel_sideways_finds_the_longest_line() {
        let text = ["short", "a longer line", "x", "a longer line", "y"];
        let s = Shape {
            textoff: 0,
            wrap: false,
            ..shape()
        };
        let at = Across {
            left: 0,
            line: 4,
            vcol: 0,
        };
        let to = across(
            Side::Wheel(6),
            at,
            10,
            &s,
            |l| text.get(l as usize).copied(),
            0..5,
        );
        assert_eq!(to.map(|a| (a.line, a.left)), Some((3, 6)));
        assert_eq!(to.map(|a| a.vcol), Some(6), "kept in view");
    }

    /// A cut line serves a sideways scroll only while it reaches past the new
    /// view, and not at all when the wheel seeks the longest line.
    #[test]
    fn lines_cut_short_do_only_as_far_as_they_go() {
        let text = ["a".repeat(30), "b".repeat(12), "c".repeat(3)];
        let s = Shape {
            textoff: 0,
            wrap: false,
            ..shape()
        };
        let line = |l: i64| text.get(l as usize).map(String::as_str);
        let at = Across {
            left: 0,
            line: 0,
            vcol: 0,
        };
        let cut_first = |l: i64| l == 0;
        let to = |left| Across {
            left,
            line: 0,
            vcol: left,
        };
        assert!(
            known(at, to(19), 10, &s, line, cut_first, 0..3),
            "to 29 of 30"
        );
        assert!(
            !known(at, to(20), 10, &s, line, cut_first, 0..3),
            "to its very end"
        );
        assert!(known(at, to(25), 10, &s, line, |_| false, 0..3), "none cut");
        let jumped = Across {
            left: 6,
            line: 1,
            vcol: 6,
        };
        let at = Across { line: 2, ..at };
        assert!(known(at, jumped, 10, &s, line, |_| false, 0..3));
        assert!(
            !known(at, jumped, 10, &s, line, cut_first, 0..3),
            "the longest of lines not all known"
        );

        // The agent sends the cut length; joined runs keep the smaller.
        let mut p = Predictor::default();
        said(&mut p, "shape", shape_args(9));
        let mut run = lines(1000, 5, 0, &["0123456789", "01234"], true);
        run.push(10.into());
        said(&mut p, "lines", run);
        assert!(p.cut(1000, 0));
        assert!(!p.cut(1000, 1));
        let mut run = lines(1000, 5, 2, &["0123456"], false);
        run.push(9.into());
        said(&mut p, "lines", run);
        assert!(p.cut(1000, 2), "within a character of the cap");
        assert!(!p.cut(1000, 1));
        said(&mut p, "lines", lines(1000, 5, 0, &["0123456789"], true));
        assert!(!p.cut(1000, 0), "no cap said: none cut");
    }

    #[test]
    fn rows_moved_sideways_keep_their_colours() {
        let s = Shape {
            textoff: 2,
            wrap: false,
            ..shape()
        };
        let row: Vec<Cell> = "12abcd"
            .chars()
            .enumerate()
            .map(|(i, c)| Cell {
                text: Text::Char(c),
                hl: i as u32 + 10,
            })
            .collect();
        let rows = vec![row.clone(), row];
        let l = Look {
            gutter: vec![18, 18],
            ..look()
        };
        let moved = shifted(&rows, 0, 2, &s, &l, |r| {
            Some((r == 0).then_some(("abcdefgh", &[][..])))
        })
        .unwrap();
        let text_of = |row: &[Cell]| text(row);
        assert_eq!(text_of(&moved[0]), "12cdef");
        let hls: Vec<u32> = moved[0].iter().map(|c| c.hl).collect();
        assert_eq!(
            hls,
            [10, 11, 14, 15, 0, 0],
            "the gutter and what stayed in view, then plain"
        );
        assert_eq!(moved[1], rows[1], "past the end: as it was");
    }

    /// Never Neovim's `<`/`>` at the old edge, nor a lone half.
    #[test]
    fn rows_moved_sideways_cut_wide_characters_as_the_new_view_does() {
        let s = Shape {
            textoff: 0,
            wrap: false,
            ..shape()
        };
        // A row as Neovim drew it, `_` the right half of the character
        // before it, each cell in a highlight of its own.
        let drawn = |s: &str| -> Vec<Cell> {
            s.chars()
                .enumerate()
                .map(|(i, c)| Cell {
                    text: if c == '_' { Text::Half } else { Text::Char(c) },
                    hl: 10 + i as u32,
                })
                .collect()
        };
        let cell = |c, hl| Cell {
            text: Text::Char(c),
            hl,
        };
        let half = |hl| Cell {
            text: Text::Half,
            hl,
        };
        // `日` at columns 1 and 2, `本` at 6 and 7.
        let line = |_: usize| Some(Some(("a日bcd本e", &[][..])));
        let moved = shifted(&[drawn("日_bcd>")], 1, 2, &s, &look(), line).unwrap();
        assert_eq!(
            moved[0],
            [
                cell('<', 19),
                cell('b', 12),
                cell('c', 13),
                cell('d', 14),
                cell('本', 0),
                half(0)
            ]
        );
        let moved = shifted(&[drawn("<bcd本_")], 2, 1, &s, &look(), line).unwrap();
        assert_eq!(
            moved[0],
            [
                cell('日', 0),
                half(0),
                cell('b', 11),
                cell('c', 12),
                cell('d', 13),
                cell('>', 19)
            ]
        );
        let line = |_: usize| Some(Some(("ab日cdefg", &[][..])));
        let moved = shifted(&[drawn("ab日_cd")], 0, 1, &s, &look(), line).unwrap();
        assert_eq!(
            moved[0],
            [
                cell('b', 11),
                cell('日', 12),
                half(13),
                cell('c', 14),
                cell('d', 15),
                cell('e', 0)
            ]
        );
    }

    /// `precedes` and `extends` stay put; an end of line scrolled past is not
    /// kept.
    #[test]
    fn rows_moved_sideways_draw_the_edges_listchars_marks_afresh() {
        let s = listing("eol:¶,extends:»,precedes:«");
        let drawn = |s: &str| -> Vec<Cell> {
            s.chars()
                .enumerate()
                .map(|(i, c)| Cell {
                    text: Text::Char(c),
                    hl: 10 + i as u32,
                })
                .collect()
        };
        let cell = |c, hl| Cell {
            text: Text::Char(c),
            hl,
        };
        let line = |_: usize| Some(Some(("abcdefghij", &[][..])));
        let from1 = [drawn("«cdef»")];
        let moved = shifted(&from1, 1, 2, &s, &look(), line).unwrap();
        assert_eq!(
            moved[0],
            [
                cell('«', 19),
                cell('d', 12),
                cell('e', 13),
                cell('f', 14),
                cell('g', 0),
                cell('»', 19)
            ]
        );
        let moved = shifted(&from1, 1, 0, &s, &look(), line).unwrap();
        assert_eq!(
            moved[0],
            [
                cell('a', 0),
                cell('b', 0),
                cell('c', 11),
                cell('d', 12),
                cell('e', 13),
                cell('»', 19)
            ]
        );
        let short = |_: usize| Some(Some(("ab", &[][..])));
        let moved = shifted(&[drawn("¶     ")], 4, 0, &s, &look(), short).unwrap();
        assert_eq!(text(&moved[0]), "ab¶   ");
        let plain = Shape {
            lcs: Listchars::default(),
            ..s.clone()
        };
        let moved = shifted(&[drawn("bcdefg")], 1, 2, &plain, &look(), line).unwrap();
        assert_eq!(
            moved[0][0],
            cell('c', 11),
            "no listchars at the edges: kept"
        );
    }

    /// Trailing blanks with `trail`, or an all-blank text with lead settings.
    #[test]
    fn lines_cut_short_are_not_drawn_where_their_blanks_turn_on_the_rest() {
        let listed = |lcs: &[&str]| -> Vec<Value> {
            let mut args = shape_args(10);
            if let Some(Value::Array(v)) = args.get_mut(1) {
                v[13] = true.into();
                v.resize(23, Value::from(0));
                v.push(Value::Array(lcs.iter().map(|s| Value::from(*s)).collect()));
            }
            args
        };
        let texts = ["abcdefg   ", "abcdefghij", "          ", "  abcdefgh"];
        let drawn = |args: Vec<Value>| -> Vec<bool> {
            let mut p = Predictor::default();
            said(&mut p, "shape", args);
            let mut run = lines(1000, 5, 0, &texts, true);
            run.push(10.into());
            said(&mut p, "lines", run);
            (0..4)
                .map(|l| {
                    assert_eq!(
                        p.coloured(1000, l).is_some(),
                        p.row(1000, l, 12, &look(), 0).is_some()
                    );
                    p.coloured(1000, l).is_some()
                })
                .collect()
        };
        let trail = ["", "", "", "", "-", "", "", "", "", ""];
        assert_eq!(drawn(listed(&trail)), [false, true, false, true]);
        let lead = ["", "", "_", "", "", "", "", "", "", ""];
        assert_eq!(drawn(listed(&lead)), [true, true, false, true]);
        let space = [".", "", "", "", "", "", "", "", "", ""];
        assert_eq!(
            drawn(listed(&space)),
            [true; 4],
            "nothing turns on the rest"
        );
        assert_eq!(drawn(shape_args(10)), [true; 4], "'list' off");
    }

    #[test]
    fn listchars_come_with_the_shape() {
        let mut p = Predictor::default();
        let mut args = shape_args(10);
        if let Some(Value::Array(v)) = args.get_mut(1) {
            v[13] = true.into();
            v.resize(23, Value::from(0));
            let lcs = [".", "ab", "_", "12", "-", "+", "$", ">", "<", "|."];
            v.push(Value::Array(lcs.iter().map(|s| Value::from(*s)).collect()));
            v.push(true.into());
        }
        said(&mut p, "shape", args);
        let s = p.shape(1000).unwrap();
        assert_eq!(
            s.lcs,
            Listchars {
                space: Some('.'),
                multispace: vec!['a', 'b'],
                lead: Some('_'),
                leadmultispace: vec!['1', '2'],
                leadtab: vec!['|', '.'],
                trail: Some('-'),
                nbsp: Some('+'),
                eol: Some('$'),
                extends: Some('>'),
                precedes: Some('<'),
            }
        );
        assert!(s.checks);
        said(&mut p, "shape", shape_args(10));
        let s = p.shape(1000).unwrap();
        assert_eq!(
            (&s.lcs, s.checks),
            (&Listchars::default(), false),
            "none said"
        );
    }

    #[test]
    fn keys_are_split_as_neovim_writes_them() {
        let all: Vec<&str> = tokens("5<C-e>z<CR>j<lt>日").collect();
        assert_eq!(all, ["5", "<C-e>", "z", "<CR>", "j", "<lt>", "日"]);
    }

    fn typed(t: &mut Typed, keys: &str) -> Vec<Command> {
        tokens(keys)
            .filter_map(|k| t.key(k, |_, _| false))
            .collect()
    }

    #[test]
    fn the_keys_that_scroll_are_told_apart() {
        let mut t = Typed::default();
        assert!(typed(&mut t, "<C-e>").is_empty(), "lost to begin with");
        let at = t.fenced();
        t.answered(at, Mode::Normal);
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
                    first: false,
                    line: None
                },
                Command::Line {
                    to: Edge::Middle,
                    first: true,
                    line: None
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
        t.answered(at, Mode::Normal);
        assert!(t.sure());
        // A key that waits for another: the scroll after it is not one.
        assert!(typed(&mut t, "f<C-e>").is_empty());
        assert!(!t.sure());
        let at = t.fenced();
        t.answered(at, Mode::Normal);
        assert!(t.sure(), "and once Neovim has caught up, nothing pending");
        // One whose next key the client cannot place.
        assert!(typed(&mut t, "ma").is_empty());
        let at = t.fenced();
        t.answered(at, Mode::Normal);
        assert!(!t.sure(), "'a' may yet be waiting for something");
        assert!(typed(&mut t, "<Esc>").is_empty());
        let at = t.fenced();
        t.answered(at, Mode::Normal);
        assert!(t.sure(), "an escape leaves nothing pending");
        // Insert mode, as far as the client can tell, until it is left.
        assert!(typed(&mut t, "i<C-e>").is_empty());
        let at = t.fenced();
        t.answered(at, Mode::Other);
        assert!(!t.sure());
        // A mapped key is one the client cannot follow.
        assert!(typed(&mut t, "<Esc>").is_empty());
        let at = t.fenced();
        t.answered(at, Mode::Normal);
        let mapped: Vec<Command> = tokens("<C-d>j<C-e>")
            .filter_map(|k| t.key(k, |_, k| k == "<C-d>"))
            .collect();
        assert!(mapped.is_empty());
        // A fence answered before the last keys were drawn says nothing of
        // them.
        let at = t.fenced();
        typed(&mut t, "j");
        t.answered(at, Mode::Normal);
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

    #[test]
    fn colours_come_with_the_lines_and_after_them() {
        let mut p = Predictor::default();
        said(&mut p, "shape", shape_args(100));
        let flat = |v: &[i64]| Value::Array(v.iter().map(|n| Value::from(*n)).collect());
        let mut run = lines(1000, 5, 0, &["local a", "b", "c"], true);
        run.push(99.into());
        run.push(Value::Array(vec![
            flat(&[0, 5, 40, 6, -1, 41]),
            flat(&[]),
            flat(&[0, 1, 42]),
        ]));
        said(&mut p, "lines", run);
        let colours = |p: &Predictor, l: i64| p.coloured(1000, l).map(|(_, s)| s.to_vec());
        let span = |start, end, group| Span { start, end, group };
        assert_eq!(
            colours(&p, 0),
            Some(vec![span(0, 5, 40), span(6, usize::MAX, 41)])
        );
        assert_eq!(colours(&p, 1), Some(vec![]));
        assert_eq!(colours(&p, 2), Some(vec![span(0, 1, 42)]));
        // An edit: the line it brings has no colours till they are sent.
        said(
            &mut p,
            "splice",
            vec![
                1000.into(),
                1.into(),
                5.into(),
                6.into(),
                1.into(),
                2.into(),
                1.into(),
                Value::Array(vec!["B".into()]),
                99.into(),
            ],
        );
        assert_eq!(colours(&p, 1), Some(vec![]));
        assert_eq!(colours(&p, 2), Some(vec![span(0, 1, 42)]), "untouched");
        let recolour = |tick: i64, first: i64, spans: Vec<Value>| {
            vec![
                1000.into(),
                1.into(),
                tick.into(),
                first.into(),
                Value::Array(spans),
            ]
        };
        said(
            &mut p,
            "spans",
            recolour(6, 1, vec![flat(&[0, 1, 43]), flat(&[0, 1, 44])]),
        );
        assert_eq!(colours(&p, 1), Some(vec![span(0, 1, 43)]));
        assert_eq!(colours(&p, 2), Some(vec![span(0, 1, 44)]));
        said(&mut p, "spans", recolour(5, 1, vec![flat(&[0, 1, 45])]));
        assert_eq!(
            colours(&p, 1),
            Some(vec![span(0, 1, 43)]),
            "another change's"
        );
    }

    #[test]
    fn edits_are_made_to_the_lines_kept() {
        let mut p = Predictor::default();
        said(&mut p, "shape", shape_args(100));
        said(
            &mut p,
            "lines",
            lines(1000, 5, 10, &["a", "b", "c", "d"], true),
        );
        let splice = |base: i64, tick: i64, first: i64, last: i64, count: i64, text: &[&str]| {
            vec![
                Value::from(1000),
                1.into(),
                base.into(),
                tick.into(),
                first.into(),
                last.into(),
                count.into(),
                Value::Array(text.iter().map(|t| Value::from(*t)).collect()),
                99.into(),
            ]
        };
        let kept = |p: &Predictor| -> Vec<(i64, String)> {
            (0..30)
                .filter_map(|l| p.text(1000, l).map(|t| (l, t.to_string())))
                .collect()
        };
        let at = |first: i64, text: &[&str]| -> Vec<(i64, String)> {
            text.iter()
                .enumerate()
                .map(|(i, t)| (first + i as i64, t.to_string()))
                .collect()
        };
        // "b" and "c" become three lines.
        said(&mut p, "splice", splice(5, 6, 11, 13, 3, &["x", "y", "z"]));
        assert_eq!(kept(&p), at(10, &["a", "x", "y", "z", "d"]));
        // Two lines deleted above: everything moves up.
        said(&mut p, "splice", splice(6, 7, 2, 4, 0, &[]));
        assert_eq!(kept(&p), at(8, &["a", "x", "y", "z", "d"]));
        // After them: nothing to do but the change.
        said(&mut p, "splice", splice(7, 8, 40, 41, 1, &[]));
        assert_eq!(kept(&p), at(8, &["a", "x", "y", "z", "d"]));
        // Straddling the first: what it brings joins what is kept.
        said(&mut p, "splice", splice(8, 9, 6, 9, 2, &["p", "q"]));
        assert_eq!(kept(&p), at(6, &["p", "q", "x", "y", "z", "d"]));
        // Right after the last: joins too.
        said(&mut p, "splice", splice(9, 10, 12, 12, 1, &["e"]));
        assert_eq!(kept(&p), at(6, &["p", "q", "x", "y", "z", "d", "e"]));
        // A tick that changed nothing.
        said(&mut p, "splice", splice(10, 11, 0, 0, 0, &[]));
        assert_eq!(kept(&p).len(), 7);
        // Made to another change than the one kept: nothing to trust.
        said(&mut p, "splice", splice(3, 12, 6, 7, 1, &["r"]));
        assert_eq!(kept(&p), []);
    }

    #[test]
    fn only_the_lines_the_agent_keeps_are_kept() {
        let mut p = Predictor::default();
        said(&mut p, "shape", shape_args(100));
        said(
            &mut p,
            "lines",
            lines(1000, 5, 10, &["a", "b", "c", "d", "e"], true),
        );
        let keep = |tick: i64, from: i64, to: i64| -> Vec<Value> {
            vec![1000.into(), 1.into(), tick.into(), from.into(), to.into()]
        };
        let kept = |p: &Predictor| -> Vec<(i64, String)> {
            (0..30)
                .filter_map(|l| p.text(1000, l).map(|t| (l, t.to_string())))
                .collect()
        };
        let at = |first: i64, text: &[&str]| -> Vec<(i64, String)> {
            text.iter()
                .enumerate()
                .map(|(i, t)| (first + i as i64, t.to_string()))
                .collect()
        };
        // The first two go, and the lines after the last come.
        said(&mut p, "keep", keep(5, 12, 15));
        said(&mut p, "lines", lines(1000, 5, 15, &["f", "g"], false));
        assert_eq!(kept(&p), at(12, &["c", "d", "e", "f", "g"]));
        // The other way: the last go, and lines before the first come.
        said(&mut p, "keep", keep(5, 12, 14));
        said(&mut p, "lines", lines(1000, 5, 11, &["b"], false));
        assert_eq!(kept(&p), at(11, &["b", "c", "d"]));
        // Nothing past what is kept is kept.
        said(&mut p, "keep", keep(5, 0, 99));
        assert_eq!(kept(&p), at(11, &["b", "c", "d"]));
        // Of another change: nothing to trust.
        said(&mut p, "keep", keep(6, 11, 13));
        assert_eq!(kept(&p), []);
    }

    #[test]
    fn lines_are_kept_as_far_as_a_round_trip_pages() {
        let reach = |ms: u64| {
            let mut p = Predictor::default();
            p.sample(Duration::from_millis(ms));
            p.reach()
        };
        assert_eq!(Predictor::default().reach(), 2, "not yet measured");
        assert_eq!(reach(0), 2);
        assert_eq!(reach(10), 3, "half a page");
        assert_eq!(reach(100), 7);
        assert_eq!(reach(200), 12);
        assert_eq!(reach(300), 17);
        assert_eq!(reach(440), MOST_REACH);
        assert_eq!(reach(5000), MOST_REACH);
    }

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

    /// Dropped with the window and on a fresh attach.
    #[test]
    fn the_column_the_cursor_wants_is_kept_for_its_window() {
        let mut p = Predictor::default();
        assert_eq!(p.want(1000), None, "not said");
        said(&mut p, "want", vec![1000.into(), 40.into()]);
        said(&mut p, "want", vec![1001.into(), 3.into()]);
        assert_eq!((p.want(1000), p.want(1001)), (Some(40), Some(3)));
        said(&mut p, "want", vec![1000.into(), 7.into()]);
        assert_eq!(p.want(1000), Some(7), "said again");
        // Past the end of every line, as `$` leaves it.
        said(&mut p, "want", vec![1000.into(), 2_147_483_647.into()]);
        assert_eq!(p.want(1000), Some(2_147_483_647));
        p.closed(1000);
        assert_eq!((p.want(1000), p.want(1001)), (None, Some(3)));
        p.reset();
        assert_eq!(p.want(1001), None);
    }

    #[test]
    fn the_round_trip_is_asked_after_while_at_work() {
        let mut p = Predictor::default();
        let t0 = Instant::now();
        assert!(!p.wants_ping(t0, None), "nobody at work");
        assert!(p.wants_ping(t0, Some(t0)));
        p.pinged(t0);
        assert!(!p.wants_ping(t0 + PING_EVERY, Some(t0)), "nothing since");
        let typed = t0 + Duration::from_millis(10);
        assert!(!p.wants_ping(typed, Some(typed)), "too soon");
        assert!(p.wants_ping(t0 + PING_EVERY, Some(typed)));
    }

    #[test]
    fn the_round_trip_varies_as_tcp_measures_it() {
        let ms = Duration::from_millis;
        let mut p = Predictor::default();
        let seen = |p: &Predictor| (p.srtt.unwrap(), p.rttvar.unwrap());
        p.sample(ms(100));
        assert_eq!(seen(&p), (ms(100), ms(50)));
        p.sample(ms(100));
        assert_eq!(seen(&p), (ms(100), Duration::from_micros(37_500)));
        p.sample(ms(300));
        assert_eq!(seen(&p), (ms(125), Duration::from_micros(78_125)));
    }

    /// Longer when the round-trip variation exceeds a quarter of [`GRACE`].
    #[test]
    fn a_prediction_waits_out_a_lost_segment() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut steady = Predictor::default();
        for _ in 0..20 {
            steady.sample(ms(150));
        }
        let wait = steady.deadline(t0) - t0;
        assert!((ms(750)..ms(760)).contains(&wait), "{wait:?}");
        let mut jumpy = Predictor::default();
        for i in 0..20 {
            jumpy.sample(ms(if i % 2 == 0 { 100 } else { 300 }));
        }
        let (srtt, rttvar) = (jumpy.srtt.unwrap(), jumpy.rttvar.unwrap());
        assert!(rttvar * 4 > GRACE, "varies by more than the grace");
        assert_eq!(jumpy.deadline(t0) - t0, srtt * 2 + rttvar * 4 + LOSS);
    }

    /// An ack for window 1000 in Normal mode at line 0, column 0, after
    /// `count` keys ending in `tail`.
    fn ack(count: u64, tail: &str) -> Ack {
        Ack {
            count,
            tail: tail.into(),
            pos: Pos {
                win: 1000,
                line: 0,
                byte: 0,
                want: Want::Col(0),
                top: 0,
                left: 0,
                rest: Rest::Normal,
            },
            buf: 1,
            tick: 5,
            line_count: 100,
            width: None,
            blanks: Vec::new(),
            insstart: None,
        }
    }

    /// Send `keys` after input `seq`, each numbered on from it.
    fn send(s: &mut Sync, seq: &mut u64, keys: &str) {
        for key in tokens(keys) {
            *seq += 1;
            s.sent(*seq, Some(key));
        }
    }

    /// Counts tied at input `seq` with `taken` counted.
    fn tied(seq: u64, taken: i64) -> Sync {
        let mut s = Sync::default();
        assert_eq!(s.answered(s.fenced(seq), seq, taken), Answer::Tied);
        s
    }

    #[test]
    fn an_ack_is_read_as_the_agent_sends_it() {
        let wire = |want: i64, mode: &str| -> Vec<Value> {
            vec![
                7.into(),
                "jl".into(),
                mode.into(),
                1000.into(),
                1.into(),
                5.into(),
                100.into(),
                3.into(),
                4.into(),
                want.into(),
                1.into(),
                2.into(),
            ]
        };
        let read = |v: Vec<Value>| {
            let refs: Vec<ValueRef<'_>> = v.iter().map(Value::as_ref).collect();
            Ack::read(&refs)
        };
        let want = Ack {
            count: 7,
            tail: "jl".into(),
            pos: Pos {
                win: 1000,
                line: 3,
                byte: 4,
                want: Want::Col(9),
                top: 1,
                left: 2,
                rest: Rest::Normal,
            },
            buf: 1,
            tick: 5,
            line_count: 100,
            width: None,
            blanks: Vec::new(),
            insstart: None,
        };
        assert_eq!(read(wire(9, "n")), Some(want.clone()));
        let end = read(wire(-1, "n")).expect("read");
        assert_eq!((end.pos.want, end.pos.want.col()), (Want::End, usize::MAX));
        for (mode, rest) in [
            ("n", Rest::Normal),
            ("i", Rest::Insert),
            ("no", Rest::Busy),
            ("niI", Rest::Busy),
            ("ic", Rest::Busy),
        ] {
            assert_eq!(
                read(wire(9, mode)).map(|a| a.pos.rest),
                Some(rest),
                "{mode}"
            );
        }
        assert_eq!(read(wire(9, "n")[..11].to_vec()), None, "cut short");
        let mut before = wire(9, "n");
        before[7] = (-1).into();
        assert_eq!(read(before), None, "a line before the first");
        assert_eq!(Rest::drawn("normal"), Rest::Normal);
        assert_eq!(Rest::drawn("insert"), Rest::Insert);
        assert_eq!(Rest::drawn("operator"), Rest::Busy);
    }

    #[test]
    fn an_ack_says_how_wide_neovim_lays_the_line_out() {
        let wire = |width: i64| -> Vec<Value> {
            let mut v: Vec<Value> = vec![7.into(), "jl".into(), "n".into(), 1000.into()];
            v.extend([1, 5, 100, 3, 4, 9, 1, 2, width].map(Value::from));
            v
        };
        let read = |v: Vec<Value>| {
            let refs: Vec<ValueRef<'_>> = v.iter().map(Value::as_ref).collect();
            Ack::read(&refs).map(|a| a.width)
        };
        assert_eq!(read(wire(12)), Some(Some(12)));
        assert_eq!(read(wire(0)), Some(Some(0)), "an empty line");
        assert_eq!(read(wire(-1)), Some(None), "too long to measure");
    }

    #[test]
    fn an_ack_says_how_wide_neovim_lays_lines_ending_in_blanks_out() {
        let mut v: Vec<Value> = vec![7.into(), "jl".into(), "n".into(), 1000.into()];
        v.extend([1, 5, 100, 3, 4, 9, 1, 2, 12].map(Value::from));
        let pair = |l: i64, w: i64| Value::Array(vec![l.into(), w.into()]);
        v.push(Value::Array(vec![pair(5, 9), pair(3, 12), "x".into()]));
        let refs: Vec<ValueRef<'_>> = v.iter().map(Value::as_ref).collect();
        let ack = Ack::read(&refs).expect("read");
        assert_eq!(ack.blanks, [(5, 9), (3, 12)]);
        assert_eq!(
            (ack.width_of(5), ack.width_of(3), ack.width_of(4)),
            (Some(9), Some(12), None)
        );
    }

    #[test]
    fn an_ack_says_where_insert_mode_began() {
        let wire = |line: i64, col: i64| -> Vec<Value> {
            let mut v: Vec<Value> = vec![7.into(), "jl".into(), "i".into(), 1000.into()];
            v.extend([1, 5, 100, 3, 4, 9, 1, 2, 12].map(Value::from));
            v.push(Value::Array(vec![]));
            v.extend([line, col].map(Value::from));
            v
        };
        let read = |v: Vec<Value>| {
            let refs: Vec<ValueRef<'_>> = v.iter().map(Value::as_ref).collect();
            Ack::read(&refs).map(|a| a.insstart)
        };
        let stop = |sticks| {
            Some(Some(Stop {
                line: 3,
                byte: 2,
                sticks,
            }))
        };
        assert_eq!(read(wire(3, 2)), stop(false));
        let mut sticking = wire(3, 2);
        sticking.push(true.into());
        assert_eq!(read(sticking), stop(true));
        assert_eq!(read(wire(-1, -1)), Some(None), "not known");
        assert_eq!(read(wire(3, 2)[..14].to_vec()), Some(None), "not said");
    }

    /// Scrolls are predicted in Visual mode, motions are not.
    #[test]
    fn visual_mode_is_a_rest_of_its_own() {
        for mode in ["v", "V", "\x16"] {
            assert_eq!(Rest::of(mode), Rest::Visual, "{mode:?}");
        }
        assert_eq!(Rest::drawn("visual"), Rest::Visual);
        assert_eq!(Rest::of("vs"), Rest::Busy, "Visual by way of Select mode");
    }

    /// As Neovim 0.12.6's `on_key` counts what `nvim_input` sent.
    #[test]
    fn the_keys_count_as_the_agent_counts_them() {
        for (sent, keytrans) in [
            ("<lt>", "<lt>"),
            ("é", "é"),
            ("e\u{301}", "e\u{301}"),
            ("<C-d>", "<C-D>"),
            (" ", "<Space>"),
            ("\\", "\\"),
            ("|", "|"),
            ("<Tab><CR><BS><Esc>", "<Tab><CR><BS><Esc>"),
            ("<S-Up><PageDown>", "<S-Up><PageDown>"),
        ] {
            let ours: Vec<&str> = tokens(sent).collect();
            let theirs: Vec<&str> = tokens(keytrans).collect();
            assert_eq!(ours.len(), theirs.len(), "{sent:?} and {keytrans:?}");
            assert!(
                ours.iter().zip(&theirs).all(|(a, b)| alike(Some(a), b)),
                "{sent:?} and {keytrans:?}"
            );
        }
        assert!(!alike(Some("j"), "k"));
        assert!(!alike(Some("<Esc>"), "<CR>"));
    }

    #[test]
    fn a_quiet_fence_ties_the_counts() {
        let mut s = Sync::default();
        let mut seq = 0;
        send(&mut s, &mut seq, "abc");
        assert_eq!(s.heard(&ack(1, "c"), seq), None, "not tied yet");
        let at = s.fenced(seq);
        assert_eq!(s.answered(at, seq, 1), Answer::Tied);
        assert!(s.synced());
        send(&mut s, &mut seq, "jl");
        assert_eq!(s.heard(&ack(3, "cjl"), seq), Some(5));
        assert_eq!(s.last, Some(ack(3, "cjl")), "heard, judged or not");
    }

    /// Neovim may take keys sent after a fence before answering it.
    #[test]
    fn a_fence_with_keys_after_it_checks_but_does_not_tie() {
        let mut seq = 0;
        let mut s = tied(0, 0);
        send(&mut s, &mut seq, "jj");
        let at = s.fenced(seq);
        send(&mut s, &mut seq, "k");
        assert_eq!(s.answered(at, seq, 2), Answer::Checked);
        assert_eq!(s.answered(at, seq, 3), Answer::Checked);
        assert!(s.synced());
        let mut untied = Sync::default();
        assert_eq!(untied.answered(at, seq, 2), Answer::Doubted);
        assert!(!untied.synced());
    }

    #[test]
    fn a_fence_out_of_range_puts_the_count_in_doubt() {
        let mut seq = 0;
        let mut s = tied(0, 0);
        send(&mut s, &mut seq, "jj");
        let at = s.fenced(seq);
        send(&mut s, &mut seq, "k");
        assert_eq!(s.answered(at, seq, 1), Answer::Doubted, "a key dropped");
        assert!(!s.synced());
        assert_eq!(s.heard(&ack(3, "jjk"), seq), None);
        let at = s.fenced(seq);
        assert_eq!(s.answered(at, seq, 2), Answer::Tied);
        assert!(s.synced());
    }

    #[test]
    fn an_ack_past_what_was_sent_is_doubted() {
        let mut seq = 0;
        let mut s = tied(0, 0);
        send(&mut s, &mut seq, "j");
        assert_eq!(s.heard(&ack(2, "jj"), seq), None);
        assert!(!s.synced());
    }

    #[test]
    fn a_tail_that_differs_is_doubted() {
        let mut seq = 0;
        let mut s = tied(0, 0);
        send(&mut s, &mut seq, "jk");
        assert_eq!(s.heard(&ack(2, "jj"), seq), None);
        assert!(!s.synced());
    }

    #[test]
    fn a_click_in_the_tail_matches_anything() {
        let mut seq = 0;
        let mut s = tied(0, 0);
        seq += 1;
        s.sent(seq, None);
        send(&mut s, &mut seq, "j");
        assert_eq!(s.heard(&ack(2, "<LeftMouse>j"), seq), Some(2));
    }

    #[test]
    fn modifiers_are_not_compared_by_spelling() {
        let mut seq = 0;
        let mut s = tied(0, 0);
        send(&mut s, &mut seq, "<C-d> <lt><C-i>");
        assert_eq!(s.heard(&ack(4, "<C-D><Space><lt><Tab>"), seq), Some(4));
    }

    /// Also checks which keys [`uncertain`] flags: `<C-c>` and Alt keys.
    #[test]
    fn ctrl_c_puts_the_count_in_doubt_until_a_quiet_fence() {
        let mut p = Predictor {
            predictions: MOTIONS,
            sync: tied(0, 0),
            ..Predictor::default()
        };
        p.sample(Duration::from_millis(100));
        let t0 = Instant::now();
        for key in ["j", "<C-c>"] {
            p.typed.key(key, |_, _| false);
            p.sent(p.typed.seq(), Some(key), t0);
        }
        assert!(!p.sync.synced());
        assert_eq!(p.heard(&ack(3, "j<C-C><C-C>"), t0), None);
        let at = p.sync.fenced(p.typed.seq());
        assert_eq!(p.sync.answered(at, p.typed.seq(), 3), Answer::Tied);
        assert!(p.sync.synced());
        for (key, doubt) in [
            ("<C-c>", true),
            ("<C-C>", true),
            ("<M-x>", true),
            ("<A-j>", true),
            ("<C-M-a>", true),
            ("<M-->", true),
            ("<M-lt>", true),
            ("<C-d>", false),
            ("<S-Up>", false),
            ("<Esc>", false),
            ("<lt>", false),
            ("c", false),
            ("-", false),
        ] {
            assert_eq!(uncertain(key), doubt, "{key}");
        }
    }

    /// Acks for the keys before it still count; later ones map to no key but
    /// raise no doubt, even past what was sent, until a quiet fence. `<C-c>`
    /// doubts them all.
    #[test]
    fn a_key_with_alt_puts_the_counts_in_doubt_from_it_on() {
        let mut p = Predictor {
            predictions: MOTIONS,
            sync: tied(0, 0),
            ..Predictor::default()
        };
        p.sample(Duration::from_millis(100));
        let t0 = Instant::now();
        for key in ["j", "j", "<M-h>", "l"] {
            p.typed.key(key, |_, _| false);
            p.sent(p.typed.seq(), Some(key), t0);
        }
        assert!(!p.sync.synced());
        assert_eq!(p.sync.doubted_from(), Some(3));
        assert_eq!(p.heard(&ack(1, "j"), t0), Some(1));
        assert_eq!(p.heard(&ack(2, "jj"), t0), Some(2));
        assert_eq!(p.sync.passed(), None, "not past it yet");
        // Counted as `<Esc>` and `h`, not the client's `l`; no doubt, but
        // past the keys before it.
        assert_eq!(p.heard(&ack(4, "j<Esc>h"), t0), None);
        assert_eq!(p.sync.doubted_from(), Some(3));
        assert_eq!(p.sync.passed(), Some(2));
        assert_eq!(p.sync.passed(), None, "said once");
        // And one more than was sent.
        assert_eq!(p.heard(&ack(5, "j<Esc>hl"), t0), None);
        assert_eq!(p.sync.doubted_from(), Some(3));
        assert_eq!(p.sync.answered(p.sync.fenced(3), 4, 5), Answer::Doubted);
        assert_eq!(p.sync.passed(), Some(2));
        assert_eq!(p.sync.doubted_from(), Some(3));
        assert_eq!(p.sync.answered(p.sync.fenced(4), 4, 5), Answer::Tied);
        assert!(p.sync.synced());
        assert_eq!(p.sync.doubted_from(), None);
        p.typed.key("<C-c>", |_, _| false);
        p.sent(p.typed.seq(), Some("<C-c>"), t0);
        assert_eq!(p.sync.doubted_from(), Some(0), "every count");
    }

    /// Neovim takes a paste outside the command line as no key.
    #[test]
    fn a_paste_holds_acks_until_a_fence_after_it() {
        let mut seq = 0;
        let mut s = tied(0, 0);
        send(&mut s, &mut seq, "j");
        let before = s.fenced(seq);
        s.pasted(seq, false);
        assert_eq!(s.heard(&ack(1, "j"), seq), None);
        assert_eq!(s.answered(before, seq, 1), Answer::Checked);
        assert!(!s.synced(), "asked before the paste");
        let after = s.fenced(seq);
        assert_eq!(s.answered(after, seq, 1), Answer::Tied);
        assert_eq!(s.heard(&ack(1, "j"), seq), Some(1));
    }

    /// On the command line Neovim takes a paste as typed keys, only after
    /// answering a fence sent with it.
    #[test]
    fn a_paste_taken_as_keys_holds_acks_till_a_fence_after_the_next_key() {
        let mut seq = 0;
        let mut s = tied(0, 0);
        s.agent = true;
        send(&mut s, &mut seq, ":");
        assert_eq!(s.heard(&ack(1, ":"), seq), Some(1));
        s.pasted(seq, true);
        assert!(!s.synced());
        assert_eq!(s.answered(s.fenced(seq), seq, 1), Answer::Doubted);
        assert!(!s.wants_fence(seq), "not till the next key");
        assert_eq!(s.heard(&ack(5, "echo"), seq), None);
        send(&mut s, &mut seq, "<CR>");
        assert!(s.wants_fence(seq));
        assert_eq!(s.heard(&ack(6, "cho<CR>"), seq), None);
        assert_eq!(s.answered(s.fenced(seq), seq, 6), Answer::Tied);
        send(&mut s, &mut seq, "j");
        assert_eq!(s.heard(&ack(7, "ho<CR>j"), seq), Some(3));
    }

    /// A `<Nop>` key reaches the agent only with the next key.
    #[test]
    fn a_key_not_counted_yet_ties_nothing() {
        let mut seq = 0;
        let mut s = tied(0, 0);
        s.agent = true;
        send(&mut s, &mut seq, "j");
        assert_eq!(s.heard(&ack(1, "j"), seq), Some(1));
        send(&mut s, &mut seq, " ");
        assert_eq!(s.answered(s.fenced(seq), seq, 1), Answer::Doubted);
        assert!(!s.synced());
        assert!(!s.wants_fence(seq), "not till the next key");
        send(&mut s, &mut seq, "l");
        assert!(s.wants_fence(seq));
        assert_eq!(s.heard(&ack(3, "j<Space>l"), seq), None);
        assert_eq!(s.answered(s.fenced(seq), seq, 3), Answer::Tied);
        send(&mut s, &mut seq, "j");
        assert_eq!(s.heard(&ack(4, "j<Space>lj"), seq), Some(4));
    }

    /// After `<C-c>` or Alt moves the offset, earlier keys were numbered by
    /// the old one, so the tail is checked only against later keys.
    #[test]
    fn a_tie_that_moves_the_offset_checks_no_key_before_it() {
        for (sent, taken) in [("xk<C-c>", "xk<C-C><C-C>"), ("xk<M-j>", "xk<Esc>j")] {
            let mut seq = 0;
            let mut s = tied(0, 0);
            send(&mut s, &mut seq, sent);
            if sent.ends_with("<C-c>") {
                s.doubt("an interrupt, which may throw away keys sent before it");
            } else {
                s.uncertain_from(seq);
            }
            assert_eq!(s.heard(&ack(4, taken), seq), None);
            assert_eq!(s.answered(s.fenced(seq), seq, 4), Answer::Tied);
            send(&mut s, &mut seq, "l");
            let tail: String = tokens(taken).skip(1).chain(["l"]).collect();
            assert_eq!(s.heard(&ack(5, &tail), seq), Some(4), "{sent}");
            assert!(s.synced());
        }
        let mut seq = 0;
        let mut s = tied(0, 0);
        send(&mut s, &mut seq, "jk");
        assert_eq!(s.answered(s.fenced(seq), seq, 2), Answer::Tied);
        send(&mut s, &mut seq, "l");
        assert_eq!(s.heard(&ack(3, "jjl"), seq), None);
    }

    /// Counts that would overflow when added.
    #[test]
    fn counts_past_reckoning_are_doubted() {
        let mut seq = 5;
        let mut s = tied(5, 0);
        send(&mut s, &mut seq, "j");
        assert_eq!(s.heard(&ack(i64::MAX as u64, "j"), seq), None);
        assert!(!s.synced());
        let mut seq = 5;
        let mut s = tied(5, 0);
        send(&mut s, &mut seq, "j");
        let at = s.fenced(seq);
        send(&mut s, &mut seq, "k");
        assert_eq!(s.answered(at, seq, i64::MAX), Answer::Doubted);
    }

    /// -1: answered before the agent was installed; 0: just after.
    #[test]
    fn minus_one_is_no_agent_and_nought_is_an_agent() {
        let mut s = Sync::default();
        let at = s.fenced(0);
        assert_eq!(s.answered(at, 0, -1), Answer::NoAgent);
        assert!(!s.synced());
        assert_eq!(s.answered(at, 0, 0), Answer::Tied);
        assert!(s.synced());
    }

    /// A [`Typed`] using acks, rested in Normal mode at input 0.
    fn acked() -> Typed {
        let mut t = Typed::default();
        t.set_predictions(MOTIONS);
        t.rebase(Rest::Normal);
        t
    }

    /// A held `<C-e>` stays predicted however far ahead, and wants no fence.
    #[test]
    fn predicted_keys_never_make_the_client_unsure() {
        let mut t = acked();
        for _ in 0..30 {
            assert_eq!(typed(&mut t, "<C-e>"), [Command::Lines(1)]);
            assert!(t.followed());
        }
        assert!(!t.wants_fence());
        let mut p = Predictor::default();
        p.set_predictions(MOTIONS);
        p.sync = tied(0, 0);
        p.base = Some(Base {
            seq: 0,
            ack: ack(0, ""),
        });
        p.typed.rebase(Rest::Normal);
        for _ in 0..30 {
            p.key(Some(1000), "<C-e>");
            assert!(p.known());
        }
        let mut scrolls = Typed::default();
        typed(&mut scrolls, "j");
        assert!(!scrolls.followed(), "lost till a fence, as ever");
    }

    /// A later scroll is read but not predicted until an ack covers the key.
    #[test]
    fn a_key_left_to_neovim_is_caught_up_with_at_its_own_ack() {
        let mut t = acked();
        typed(&mut t, "x");
        assert!(!t.followed(), "blind from `x`");
        assert_eq!(typed(&mut t, "<C-e>"), [Command::Lines(1)]);
        assert!(!t.followed());
        assert!(t.wants_fence(), "a fence after it, for counts and all");
        // An ack covering an input before `x`: replayed, `x` blinds again,
        // and the scroll after it is read.
        t.rebase(Rest::Normal);
        assert!(t.followed());
        assert_eq!(t.step("x", |_, _| false), None);
        assert!(!t.followed());
        assert_eq!(t.step("<C-e>", |_, _| false), Some(Command::Lines(1)));
        // One covering `x` itself.
        t.rebase(Rest::Normal);
        assert!(t.followed());
        assert_eq!(t.step("<C-e>", |_, _| false), Some(Command::Lines(1)));
        assert!(t.followed(), "the scroll followed from there");
        // A replayed key is not resent, so it wants no fence.
        let at = t.fenced();
        t.answered(at, Mode::Normal);
        t.rebase(Rest::Normal);
        t.step("x", |_, _| false);
        assert!(!t.wants_fence());
        // A scroll the client made out but did not make, and a click.
        let mut t = acked();
        typed(&mut t, "<C-e>");
        t.missed();
        assert!(!t.followed(), "a scroll not made");
        t.rebase(Rest::Normal);
        assert!(t.followed());
        t.clicked();
        assert!(!t.followed(), "a click");
    }

    /// A busy rest (operator pending, command line) follows nothing.
    #[test]
    fn the_parser_rests_where_the_ack_says() {
        let rest = |mode: &str| {
            let mut t = acked();
            t.rebase(Rest::of(mode));
            let scroll = t.step("<C-e>", |_, _| false);
            let page = t.step("<PageDown>", |_, _| false);
            (scroll, page, t.followed())
        };
        let page = Command::Page {
            down: true,
            count: 1,
        };
        assert_eq!(rest("n"), (Some(Command::Lines(1)), Some(page), true));
        assert_eq!(rest("V"), (Some(Command::Lines(1)), Some(page), true));
        assert_eq!(rest("i"), (None, Some(page), false), "typed, in Insert");
        assert_eq!(rest("no"), (None, None, false));
        assert_eq!(rest("c"), (None, None, false));
    }

    /// Eight credited matches in a row lift the proof; a wrong one restarts
    /// it. Other buffers are drawn meanwhile.
    #[test]
    fn a_buffer_drawn_wrong_proves_itself() {
        let mut t = Trust::default();
        assert!(!t.hidden(1, Proof::Move));
        t.wrong(1, Proof::Move, true);
        assert!(t.hidden(1, Proof::Move) && !t.hidden(2, Proof::Move));
        for _ in 1..PROOF {
            t.made(1, Proof::Move, true, false);
        }
        t.made(1, Proof::Move, false, false);
        assert!(
            t.hidden(1, Proof::Move),
            "one short, a move that moved nothing aside"
        );
        t.wrong(1, Proof::Move, false);
        for _ in 0..PROOF {
            assert!(t.hidden(1, Proof::Move));
            t.made(1, Proof::Move, true, false);
        }
        assert!(!t.hidden(1, Proof::Move), "proved");
        assert_eq!((t.tally.wrong, t.tally.hidden_wrong), (1, 1));
    }

    /// Per buffer, once per attach. An exit that also edited the buffer
    /// restarts its proof without counting as wrong.
    #[test]
    fn what_plugins_may_do_anything_to_proves_itself_apart() {
        let mut t = Trust::default();
        for _ in 1..PROOF {
            t.made(1, Proof::Leave, true, false);
            t.made(1, Proof::Char('x'), true, false);
            t.made(1, Proof::Enter, false, false);
        }
        assert!(!t.proven(1, Unseen::Left) && !t.proven(1, Unseen::Typed));
        t.edited(1, Proof::Leave);
        assert!(t.hidden(1, Proof::Leave) && !t.hidden(1, Proof::Enter));
        t.made(1, Proof::Rub, true, false);
        assert!(t.proven(1, Unseen::Typed), "a <BS> as good as a character");
        for _ in 1..PROOF {
            t.made(1, Proof::Leave, true, false);
        }
        assert!(!t.proven(1, Unseen::Left), "one short of it again");
        t.made(1, Proof::Leave, true, false);
        assert!(t.proven(1, Unseen::Left) && !t.hidden(1, Proof::Leave));
        assert!(!t.proven(2, Unseen::Left) && !t.proven(1, Unseen::Entered));
        t.wrong(1, Proof::Leave, true);
        assert!(t.proven(1, Unseen::Left), "for the attach");
        assert!(t.hidden(1, Proof::Leave), "but drawn wrong");
        assert_eq!((t.tally.wrong, t.tally.hidden_wrong), (1, 0));
    }

    #[test]
    fn a_window_drawn_elsewhere_is_left_to_neovim_a_while() {
        let now = Instant::now();
        let mut t = Trust::default();
        t.distrust(5, now + DISTRUST);
        assert!(t.refused(5, now) && !t.refused(6, now));
        assert!(!t.refused(5, now + DISTRUST), "the while over");
        t.distrust(5, now + DISTRUST);
        t.reshaped(5);
        assert!(!t.refused(5, now), "shaped again");
    }

    #[test]
    fn too_many_drawn_wrong_and_none_are_drawn() {
        let now = Instant::now();
        let judged = |wrong: usize| {
            let mut t = Trust::default();
            for i in 0..LOOKED_BACK {
                if i < wrong {
                    t.wrong(1, Proof::Move, true);
                } else {
                    t.made(1, Proof::Move, true, true);
                }
            }
            t.refused(5, now)
        };
        assert!(!judged(6), "58 of 64 right");
        assert!(judged(7), "57 of 64");
        let mut t = Trust::default();
        for _ in 0..40 {
            t.wrong(1, Proof::Move, true);
        }
        assert!(
            !t.refused(5, now),
            "not before as many as it looks back over"
        );
    }

    /// Past [`QUEUE`] unacked inputs the counts are in doubt.
    #[test]
    fn a_queue_neovim_never_answers_is_let_go() {
        let t0 = Instant::now();
        let mut p = Predictor {
            predictions: MOTIONS,
            sync: tied(0, 0),
            ..Predictor::default()
        };
        p.sent(1, Some("j"), t0);
        assert_eq!(p.queue.len(), 1, "unmeasured, the link");
        for seq in 2..1 + QUEUE as u64 {
            p.sent(seq, Some("j"), t0);
        }
        assert_eq!(p.queue.len(), QUEUE);
        assert!(p.sync.synced());
        p.sent(1 + QUEUE as u64, Some("j"), t0);
        assert_eq!(p.queue.len(), 1);
        assert!(!p.sync.synced());
        let mut scrolls = Predictor::default();
        scrolls.sample(Duration::from_millis(100));
        scrolls.sent(1, Some("j"), t0);
        assert!(scrolls.queue.is_empty(), "no acks, nothing kept");
    }

    /// Typeahead backed up behind earlier keys; each key is timed once
    /// however often acks repeat it.
    #[test]
    fn a_scroll_waits_as_long_as_neovim_takes_to_answer_keys() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut p = Predictor {
            predictions: MOTIONS,
            sync: tied(0, 0),
            ..Predictor::default()
        };
        for _ in 0..20 {
            p.sample(ms(100));
        }
        let round = p.deadline(t0) - t0;
        p.typed.key("j", |_, _| false);
        p.sent(1, Some("j"), t0);
        assert_eq!(p.heard(&ack(1, "j"), t0 + ms(900)), Some(1));
        assert_eq!(p.lag, Some((ms(900), ms(450))));
        assert_eq!(p.deadline(t0) - t0, ms(900) + ms(450) * 4 + LOSS);
        assert!(p.deadline(t0) - t0 > round);
        p.heard(&ack(1, "j"), t0 + ms(5000));
        assert_eq!(p.lag, Some((ms(900), ms(450))), "the same key, timed once");
        assert_eq!(p.want(1000), Some(0), "and the column the cursor wants");
        p.predictions = Predictions::default();
        assert_eq!(p.deadline(t0) - t0, round, "told not to count any more");
    }

    /// A mapping prefix (held up to `'timeoutlen'`) or `f` waits on the user;
    /// the key ending the wait is timed instead.
    #[test]
    fn a_key_that_waits_for_another_is_not_timed() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut p = Predictor {
            predictions: MOTIONS,
            sync: tied(0, 0),
            ..Predictor::default()
        };
        for _ in 0..20 {
            p.sample(ms(100));
        }
        let round = p.deadline(t0) - t0;
        fn send(p: &mut Predictor, key: &str, mapped: bool, at: Instant) {
            p.typed.key(key, |_, k| mapped && k == key);
            p.sent(p.typed.seq(), Some(key), at);
        }
        send(&mut p, "j", true, t0);
        assert_eq!(p.heard(&ack(1, "j"), t0 + ms(1100)), Some(1));
        assert_eq!(p.lag, None, "a prefix, waited on");
        send(&mut p, "f", false, t0 + ms(2000));
        send(&mut p, "x", false, t0 + ms(4000));
        assert_eq!(p.heard(&ack(3, "jfx"), t0 + ms(4100)), Some(3));
        assert_eq!(p.lag, Some((ms(100), ms(50))), "from the key that ended it");
        assert_eq!(p.deadline(t0) - t0, round);
    }

    /// A character that starts an Insert mapping (`j` of `inoremap jk <Esc>`)
    /// may make Neovim wait. With motions alone, neither applies.
    #[test]
    fn a_character_typed_is_typed_ahead_and_one_a_mapping_starts_may_wait() {
        let mut p = Predictor::default();
        p.set_predictions(TYPING);
        said(&mut p, "shape", shape_args(50));
        let classes: String = (0..256).map(|_| '1').collect();
        let facts: Vec<Value> = vec![
            1.into(),
            true.into(),
            Value::Array(vec!["j".into()]),
            Value::Array(vec![]),
            false.into(),
            0.into(),
            "".into(),
            "".into(),
            false.into(),
            false.into(),
            false.into(),
            "".into(),
            false.into(),
            true.into(),
            true.into(),
            "indent,eol,start".into(),
            false.into(),
            true.into(),
            false.into(),
            "".into(),
            classes.into(),
        ];
        said(&mut p, "insert", facts);
        p.typed.rebase(Rest::Insert);
        let x = Some(Command::Type(typing::Edit::Char('x')));
        assert_eq!(p.key(Some(1000), "x"), x);
        assert!(!p.typed.pending());
        assert_eq!(
            p.key(Some(1000), "j"),
            Some(Command::Type(typing::Edit::Char('j')))
        );
        assert!(p.typed.pending(), "`jk` may follow");
        assert_eq!(
            p.key(Some(1000), "<BS>"),
            Some(Command::Type(typing::Edit::Rub))
        );
        p.set_predictions(MOTIONS);
        p.typed.rebase(Rest::Insert);
        assert_eq!(p.key(Some(1000), "x"), None);
    }

    /// Scripts Neovim 0.12.6 was found to join marks in, and annotation
    /// anchors.
    #[test]
    fn lines_neovim_joins_spacing_marks_on_are_not_ordinary() {
        for text in [
            "a\u{a98f}\u{a9b4}y",
            "\u{1900}\u{1923}",
            "\u{11013}\u{11000}",
            "\u{16f00}\u{16f51}",
            "x\u{2d7f}",
            "a\u{fff9}b",
        ] {
            assert!(!ordinary(text), "{text:?}");
        }
    }

    /// `<Esc>` is not followed after an entry with a count (the text repeats),
    /// by an unfollowed key or without its step made, nor after leaving for a
    /// command.
    #[test]
    fn insert_mode_is_entered_and_left_by_keys_with_no_count() {
        let mut t = acked();
        t.set_predictions(TYPING);
        use typing::Enter;
        let cmds = typed(&mut t, "ifoo<Esc>1A<Esc>I<Esc>");
        assert_eq!(
            cmds,
            [
                Command::Enter(Enter::Before),
                Command::Type(typing::Edit::Char('f')),
                Command::Type(typing::Edit::Char('o')),
                Command::Type(typing::Edit::Char('o')),
                Command::Leave,
                Command::Enter(Enter::End),
                Command::Leave,
                Command::Enter(Enter::First),
                Command::Leave,
            ]
        );
        assert_eq!(typed(&mut t, "3ix<Esc>"), [], "a count: blind");
        t.rebase(Rest::Normal);
        assert_eq!(typed(&mut t, "ox<Esc>"), [], "entered otherwise");
        t.rebase(Rest::Normal);
        assert_eq!(typed(&mut t, "a"), [Command::Enter(Enter::After)]);
        t.unentered();
        assert_eq!(typed(&mut t, "<Esc>"), [], "the `a` not made");
        t.rebase(Rest::Normal);
        assert_eq!(typed(&mut t, "a"), [Command::Enter(Enter::After)]);
        t.rebase(Rest::Insert);
        assert_eq!(typed(&mut t, "<Esc>"), [], "an ack says nothing of it");
        t.rebase(Rest::Normal);
        let cmds = typed(&mut t, "ix<C-o><Esc>");
        assert_eq!(
            cmds.len(),
            2,
            "not after a command from Insert mode: {cmds:?}"
        );
        t.set_predictions(MOTIONS);
        t.rebase(Rest::Normal);
        assert_eq!(typed(&mut t, "i<Esc>"), [], "the motions alone");
    }

    /// A key of a kind set to `"off"` blinds the client like any other
    /// unfollowed key.
    #[test]
    fn each_kind_is_made_by_its_own_setting() {
        let only = |scrolls, motions, typing| {
            let mut t = Typed::default();
            t.set_predictions(Predictions {
                scrolls,
                motions,
                typing,
            });
            t.rebase(Rest::Normal);
            t
        };
        let scroll = Command::Lines(1);
        let down = Command::Move {
            motion: moves::Motion::Down,
            count: None,
            screen: false,
        };
        // The motions alone: no scroll, by its own key, a `z` or in Insert
        // mode.
        let mut t = only(false, true, false);
        assert_eq!(typed(&mut t, "j"), [down]);
        assert!(!t.blind);
        assert_eq!(typed(&mut t, "<C-e>"), []);
        assert!(t.blind, "a scroll left to Neovim");
        t.rebase(Rest::Normal);
        assert_eq!(typed(&mut t, "zt"), []);
        t.rebase(Rest::Insert);
        assert_eq!(typed(&mut t, "<PageDown><C-x><C-e>"), []);
        // The typing alone: what is typed, and the keys in and out of Insert
        // mode, but neither a motion nor a scroll.
        let mut t = only(false, false, true);
        assert_eq!(typed(&mut t, "j"), []);
        t.rebase(Rest::Normal);
        assert_eq!(
            typed(&mut t, "ix<PageDown>"),
            [
                Command::Enter(typing::Enter::Before),
                Command::Type(typing::Edit::Char('x')),
            ]
        );
        // The scrolls and the typing: a scroll still made, a motion not.
        let mut t = only(true, false, true);
        assert_eq!(typed(&mut t, "<C-e>j"), [scroll]);
        // The scrolls and the motions: what is typed left to Neovim.
        let mut t = only(true, true, false);
        assert_eq!(typed(&mut t, "<C-e>j"), [scroll, down]);
        assert_eq!(typed(&mut t, "ix"), []);
    }

    /// Including `ge`/`gE` and the Shift/Ctrl arrows; mapped ones blind the
    /// client.
    #[test]
    fn motions_by_words_are_followed() {
        use crate::client::words::Way;
        let word = |way, big, count| Command::Move {
            motion: moves::Motion::Word { way, big },
            count,
            screen: false,
        };
        let mut t = Typed::default();
        t.set_predictions(MOTIONS);
        t.rebase(Rest::Normal);
        assert_eq!(
            typed(&mut t, "w3bge2gEE<S-Right><C-Left>"),
            [
                word(Way::Next, false, None),
                word(Way::Back, false, Some(3)),
                word(Way::BackEnd, false, None),
                word(Way::BackEnd, true, Some(2)),
                word(Way::End, true, None),
                word(Way::Next, false, None),
                word(Way::Back, true, None),
            ]
        );
        assert!(!t.blind);
        for (keys, mapped) in [("ge", "ge"), ("w", "w"), ("<S-Right>", "<S-Right>")] {
            let mut t = Typed::default();
            t.set_predictions(MOTIONS);
            t.rebase(Rest::Normal);
            let made: Vec<Command> = tokens(keys)
                .filter_map(|k| t.key(k, |_, key| key == mapped))
                .collect();
            assert!(made.is_empty(), "{keys}");
            assert!(t.blind, "{keys}");
        }
    }

    /// Mapped ones blind the client.
    #[test]
    fn motions_to_a_line_are_followed() {
        let goto = |last, count| Command::Move {
            motion: moves::Motion::Goto { last },
            count,
            screen: false,
        };
        let mut t = Typed::default();
        t.set_predictions(MOTIONS);
        t.rebase(Rest::Normal);
        assert_eq!(
            typed(&mut t, "ggG12gg30G"),
            [
                goto(false, None),
                goto(true, None),
                goto(false, Some(12)),
                goto(true, Some(30)),
            ]
        );
        assert!(!t.blind);
        for (keys, mapped) in [("gg", "gg"), ("G", "G")] {
            let mut t = Typed::default();
            t.set_predictions(MOTIONS);
            t.rebase(Rest::Normal);
            let made: Vec<Command> = tokens(keys)
                .filter_map(|k| t.key(k, |_, key| key == mapped))
                .collect();
            assert!(made.is_empty(), "{keys}");
            assert!(t.blind, "{keys}");
        }
    }

    /// Mapped ones blind the client.
    #[test]
    fn motions_by_paragraphs_are_followed() {
        let para = |back, count| Command::Move {
            motion: moves::Motion::Paragraph { back },
            count,
            screen: false,
        };
        let mut t = Typed::default();
        t.set_predictions(MOTIONS);
        t.rebase(Rest::Normal);
        assert_eq!(
            typed(&mut t, "}3{"),
            [para(false, None), para(true, Some(3))]
        );
        assert!(!t.blind);
        let mut t = Typed::default();
        t.set_predictions(MOTIONS);
        t.rebase(Rest::Normal);
        assert_eq!(t.key("}", |_, key| key == "}"), None);
        assert!(t.blind);
    }

    /// The character is followed whatever maps it (Neovim does not look it
    /// up); a mapped motion, or a non-character after it, blinds the client.
    #[test]
    fn motions_to_a_character_are_followed() {
        let find = |c, till, back, count| Command::Move {
            motion: moves::Motion::Find { c, till, back },
            count,
            screen: false,
        };
        let mut t = Typed::default();
        t.set_predictions(MOTIONS);
        t.rebase(Rest::Normal);
        assert_eq!(t.key("3", |_, _| false), None);
        assert_eq!(t.key("f", |_, _| false), None);
        assert!(t.pending(), "waiting for the character");
        assert_eq!(
            t.key("j", |_, key| key == "j"),
            Some(find('j', false, false, Some(3)))
        );
        assert!(!t.pending());
        assert_eq!(
            typed(&mut t, "t<Tab>F<lt>T日"),
            [
                find('\t', true, false, None),
                find('<', false, true, None),
                find('日', true, true, None),
            ]
        );
        assert!(!t.blind);
        for (keys, mapped) in [
            ("fx", "f"),
            ("Tx", "T"),
            ("f<Esc>", ""),
            ("t<C-k>", ""),
            ("F<Left>", ""),
        ] {
            let mut t = Typed::default();
            t.set_predictions(MOTIONS);
            t.rebase(Rest::Normal);
            let made: Vec<Command> = tokens(keys)
                .filter_map(|k| t.key(k, |_, key| key == mapped))
                .collect();
            assert!(made.is_empty(), "{keys}");
            assert!(t.blind, "{keys}");
        }
        // The sought character is not a command: here a `z`, and the `zt`
        // after it is the scroll.
        let mut t = Typed::default();
        t.set_predictions(MOTIONS);
        t.rebase(Rest::Normal);
        assert_eq!(
            typed(&mut t, "fzzt"),
            [
                find('z', false, false, None),
                Command::Line {
                    to: Edge::Top,
                    first: false,
                    line: None
                },
            ]
        );
    }

    /// Checked against Neovim 0.12.6.
    #[test]
    fn only_lines_stepped_over_as_neovim_does_are_ordinary() {
        for text in [
            "hello\tworld",
            "e\u{301}x",
            "a\u{20dd}b",
            "日本語",
            "\u{3000}x",
            "😀x",
            "✔x",
            "a\u{80}b",
            "\u{a0}x",
            "a\u{a0}\u{301}b",
            "a日\u{301}b",
            "ab\u{301}\u{302}c",
        ] {
            assert!(ordinary(text), "{text:?}");
        }
        for text in [
            "\u{915}\u{93e}x",
            "\u{e01}\u{e33}x",
            "\u{ba8}\u{bbf}x",
            "👨\u{200d}👩\u{200d}👧x",
            "🇯🇵x",
            "👍🏽x",
            "❤\u{fe0f}x",
            "a\u{200b}b",
            "a\u{ad}b",
            "a\u{200e}b",
            "a\u{feff}b",
            "a\u{61c}b",
            "\u{301}x",
            " \u{301}x",
            "\t\u{301}b",
            "a\x01\u{301}b",
            "a\x7f\u{301}b",
            "a\u{80}\u{301}b",
        ] {
            assert!(!ordinary(text), "{text:?}");
        }
    }
}
