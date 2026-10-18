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
//! - `zt`, `zz`, `zb`, `z<CR>`, `z.` and `z-`, the cursor's line — or with
//!   a count, that line — to the top, the middle or the bottom;
//! - in Insert mode, `<PageDown>`, `<PageUp>`, `<S-Down>`, `<S-Up>`,
//!   `<C-x><C-e>`, `<C-x><C-y>` and the wheel, the cursor free to sit past
//!   the end of its line;
//! - and in a window that does not wrap, sideways: `zl`, `zh`, `zL`, `zH`,
//!   `zs`, `ze` and the wheel left and right ([`across`]), the cursor kept
//!   `'sidescrolloff'` from the edges, and the wheel past the end of the
//!   cursor's line sending it to the longest line in view;
//!
//! — with `'scrolloff'` keeping the cursor in from the edges, and a count
//! where Neovim takes one; one after another, as fast as they are typed —
//! as far as thirty-two windows ahead of Neovim, as soon as the lines they
//! show have come (see `super::anim::scroll::HOLD`). A scroll that takes the
//! cursor to another line lands it in the column it wants, which Neovim
//! keeps through moves up and down (`curswant`) and the agent says
//! ([`Predictor::want`]), not the one it is in: further left, at the end of
//! a line too short for it, or on a character two cells wide. Where that is
//! out of view, or too near its edge, Neovim moves the view sideways to show
//! it ([`showing`]), and a scroll alone ahead of Neovim is drawn moved so;
//! one behind others is left to Neovim, and those after it with it.
//!
//! Most of the rows a scroll shows are rows the window already shows, moved.
//! What the client does not have is the lines it uncovers. So the agent the
//! client leaves in the editor (see `super::AGENT_LUA`) sends it the text of
//! the lines around each window's view — as far each way as a key held
//! down pages in a round trip, which is as far ahead of Neovim's view as the
//! client gets before the lines around Neovim's next view come
//! ([`Predictor::reach`]), and no more than a few thousand lines in all,
//! those furthest behind let go; as the view moves, only the lines it has
//! not sent, and as the buffer changes, only the lines that did, which the
//! client splices into what it has; of a long line, no more than the window
//! could show ([`known`]) — with what it takes to draw them as the window
//! would ([`Shape`]), and the colours the editor's own highlighting gives
//! them — its tree-sitter captures, and the highlights put on the buffer,
//! such as a language server's semantic tokens — sent again where an edit or
//! a new parse changes them. A row drawn from that is the line's text and
//! its number, in those colours over the window's own, and nothing more: no
//! signs, no virtual text, no `:syntax` colours, which Neovim cannot say for
//! lines it has not drawn. It is on the screen for the round trip it takes
//! Neovim's own row to arrive.
//!
//! What a line costs is Neovim's time more than the link's: its colours,
//! found by walking its tree-sitter captures, take some 60 µs a line, all of
//! it holding up Neovim's own redraw and the keys typed meanwhile. So no
//! more is sent than the round trip needs, and a buffer that comes into a
//! window is sent two windows each way at first, the rest a moment later, a
//! few hundred lines at a time. Pages typed in that moment can outrun the
//! lines, as can pages typed faster than keys repeat over the slowest links.
//! A scroll that uncovers a line yet to come is made all the same, and the
//! next from where it goes, but it is drawn only once the line has come.
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
//! A key is a scroll only in the mode it scrolls in with nothing pending —
//! Normal or Visual, or Insert for its own — and the client cannot see
//! Neovim's pending state, only its own keys: so it follows them ([`Typed`]),
//! and predicts nothing from where it loses track, nor while a key before
//! has yet to be drawn.
//!
//! Neovim always has the last word. What it scrolls is taken off what the
//! client is ahead by; a prediction it has not confirmed in two round trips
//! and a little more is let go, and the view goes back to Neovim's (see
//! `super::anim::scroll`). Neovim behind on the keys typed — on a busy
//! machine, or a buffer slow to scroll — confirms nothing till it has taken
//! the last of them, though: it sends no frame before. So the agent says when
//! Neovim moves a view working through them, and the predictions waiting for
//! it are given as long again from then as one made then would be.

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

/// How many pages a second a key held down types, set to repeat as fast as
/// people set keys to: what the agent keeps lines ahead for (see
/// [`Predictor::reach`]).
const PAGES_A_SECOND: u128 = 50;

/// The most windows either side of a view the agent keeps lines for: a round
/// trip of over 400 ms at [`PAGES_A_SECOND`]. Short of how far a view may be
/// scrolled ahead at all (`super::anim::scroll::HOLD`), so that pages typed
/// past the lines on a slower link still are, held until their lines come.
const MOST_REACH: i64 = 24;

/// The most lines kept for a window. The agent has the client keep fewer —
/// no more than 4000, and no splice of more than 2000 past that — and this
/// is what keeps a client from growing should it ever not.
const MAX_LINES: usize = 8192;

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
    /// Which of the keys the client follows ([`keys`]) something maps in
    /// normal or visual mode, in the window's buffer or everywhere — or
    /// starts a mapping with: Neovim does with them what the mapping says.
    pub mapped: Vec<String>,
    /// The same of the keys it follows in insert mode ([`insert_keys`]).
    pub imapped: Vec<String>,
    /// `'sidescrolloff'`, `'sidescroll'`, and `'mousescroll'`'s `hor`: how
    /// near the sides Neovim lets the cursor be, how far the view moves to
    /// keep it, and how many columns a turn of the wheel sideways scrolls.
    pub sidescrolloff: usize,
    pub sidescroll: usize,
    pub hor: i64,
}

impl Shape {
    /// As the agent sends it: `[buf, line_count, textoff, tabstop, number,
    /// relativenumber, plain_gutter, wrap, leftcol, predictable, scrolloff,
    /// step, eob, list, tab, scroll, page, startofline, mapped, imapped,
    /// sidescrolloff, sidescroll, hor]`.
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
            mapped: strings(v.get(18)),
            imapped: strings(v.get(19)),
            sidescrolloff: size(20).unwrap_or(0),
            sidescroll: size(21).unwrap_or(1),
            hor: int(22).unwrap_or(6),
        })
    }
}

/// An array of strings, as the agent sends one: none for anything else.
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

/// A run of bytes of a line in an editor's highlight group, the end
/// `usize::MAX` for one that runs to the end of the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub group: u32,
}

/// A line the agent sent: its text, and the colours the editor's own
/// highlighting gives it, later spans over earlier ones.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Line {
    text: String,
    spans: Vec<Span>,
}

/// The spans of a line as the agent sends them: start, end and group, flat,
/// an end of -1 running to the end of the line.
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

/// The lines the agent has sent of a window's buffer: a run of them from
/// `first`, as they were at `tick`, none longer than `cap` bytes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Lines {
    buf: i64,
    tick: i64,
    first: i64,
    text: Vec<Line>,
    /// The agent sends no more of a line than the window could show, and a
    /// little over; a line this long, give or take a character cut in two,
    /// may be longer in the buffer.
    cap: usize,
}

impl Lines {
    fn end(&self) -> i64 {
        self.first + self.text.len() as i64
    }

    /// Take a run of lines the agent sent. The agent keeps the same account
    /// of what the client has and follows the same rule: a run of the same
    /// buffer as it was at the same change, touching what is here, joins it;
    /// anything else — or a run it says is to start afresh — replaces it.
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

    /// Take an edit the agent says the buffer has had since it sent the
    /// lines: by the same rule the agent keeps its account by, so that the
    /// two agree on what the client has. One made to lines the client does
    /// not have as they were leaves it none to trust.
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
            text.extend(s.text.into_iter().map(|text| Line {
                text,
                spans: Vec::new(),
            }));
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

    /// Keep only lines `from` to `to`, the agent says, of a buffer as it was
    /// at `tick`: as it keeps its own account. Kept as they were at another
    /// change, or of another buffer, the lines are not the ones it means,
    /// and none are to be trusted.
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

    /// Take the colours the agent says lines from `first` have now, the
    /// buffer at `tick`: the editor's highlighting changed, or the lines
    /// did. Lines not kept, or kept as they were at another change, are
    /// passed over.
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

    /// Whether line `lnum` may have been cut short. A character cut in two
    /// comes as one replacement character, of three bytes.
    fn cut(&self, buf: i64, lnum: i64) -> bool {
        self.get(buf, lnum).is_some_and(|t| t.len() + 3 >= self.cap)
    }
}

/// An edit to a buffer, as the agent says it: lines `first..last` of the
/// buffer as it was at `base` are `count` lines at `tick` — `text`, where
/// that touches the lines the client has — each no longer than `cap` bytes.
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
    /// What a character two cells wide cut in two at the edge of a window
    /// that does not wrap is drawn in, as `>` or `<` (see [`line`]):
    /// `NonText`, or the text's highlight where the editor has not said.
    pub nontext: u32,
    /// What a character shown as something it is not — `^X`, `<xx>` — is
    /// drawn in (see [`line`]): `SpecialKey`, or the text's highlight where
    /// the editor has not said.
    pub specialkey: u32,
    /// What a tab `'list'` shows with its `'listchars'` is drawn in (see
    /// [`line`]): `Whitespace`, or the text's highlight where the editor
    /// has not said.
    pub whitespace: u32,
}

impl Look {
    /// Read the colours off `rows`, a window's rows from its top, of which the
    /// first `lines` show the buffer's lines and the rest are past its end —
    /// leaving out the row at `skip`, the cursor's, which `'cursorline'`
    /// draws differently. `group` says the highlight of a group the editor
    /// draws itself, `hl_group_set`'s, if it has said: `EndOfBuffer`, for a
    /// window that shows no row past the end, and `NonText`, `SpecialKey`
    /// and `Whitespace` for what is drawn in them.
    pub fn of(
        rows: &[Vec<Cell>],
        lines: usize,
        textoff: usize,
        skip: Option<usize>,
        group: impl Fn(&str) -> Option<u32>,
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
            .map_or_else(|| group("EndOfBuffer").unwrap_or(0), |c| c.hl);
        Self {
            gutter,
            text,
            eob,
            eob_hl,
            nontext: group("NonText").unwrap_or(text),
            specialkey: group("SpecialKey").unwrap_or(text),
            whitespace: group("Whitespace").unwrap_or(text),
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

/// Append the cells of a line's text in the window's text highlight — or
/// where `spans` colour it, their groups laid over it — from display column
/// `shape.leftcol`, `room` of them at most. Says whether the whole line fits
/// in `room`.
///
/// A character two cells wide that an edge of the view cuts in two is drawn
/// as Neovim draws it: a `>` in the last column, a `<` in the first, in
/// `NonText`. Its left half on its own would still be drawn two cells wide,
/// by a terminal that knows nothing of the cut, and the rest of its row a
/// cell to the right of where the frame has it (see `super::grid`). A
/// character Neovim shows as something it is not — a control character as
/// `^X`, a tab as `^I` where `'list'` has no `tab:` for it, an unprintable
/// one as `<xx>` — is in `SpecialKey`, and a tab `'list'` shows with its
/// `tab:` in `Whitespace`, as Neovim draws them. Neovim lays each of these
/// over a window's own `Normal` where it has one, and all but the `>` over
/// the character's own colours as well: highlights of its own making,
/// which the client has no id for, and are left out.
fn line(
    text: &str,
    spans: &[Span],
    shape: &Shape,
    look: &Look,
    room: usize,
    row: &mut Vec<Cell>,
) -> bool {
    let from = shape.leftcol;
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
        match c {
            '\t' if shape.list && shape.tab.is_empty() => {
                special(row, vcol, '^');
                special(row, vcol + 1, 'I');
                vcol += 2;
            }
            '\t' => {
                let n = shape.tabstop - vcol % shape.tabstop;
                for i in 0..n {
                    // A `tab:` of three ends in its third, a tab one cell
                    // wide in that alone.
                    let ch = match (shape.list, shape.tab.as_slice()) {
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
            // The C0 controls and DEL as `^X`, the C1 as `<xx>`, as Neovim
            // shows them.
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
                    let (left, right) = (shown(vcol), shown(vcol + w - 1));
                    if left && !right {
                        // Its combining marks go with it.
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
    vcol <= room
}

/// A scroll as Neovim will make it, from a key and a count and the window's
/// options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// `<C-e>`, `<C-y>`, a turn of the wheel: lines, down the buffer above
    /// nought. The cursor stays on its line, if `'scrolloff'` lets it.
    Lines(i64),
    /// The wheel with Shift or Ctrl in insert mode: as many lines as the
    /// window shows, down the buffer for 1 and up it for -1.
    Screen(i64),
    /// `<C-d>`, `<C-u>`: `'scroll'` lines, or as many as a count says, which
    /// is `'scroll'` from then on. The cursor goes as far.
    Half { down: bool, count: Option<i64> },
    /// `<C-f>`, `<C-b>`, `<PageDown>`, `<PageUp>`, `<S-Down>`, `<S-Up>`, the
    /// wheel with Shift or Ctrl: pages. The cursor goes to the top of the
    /// view, or the bottom.
    Page { down: bool, count: i64 },
    /// `zt`, `zz`, `zb`: the cursor's line to the top of the view, the
    /// middle, the bottom — or, with a count, that line's, the cursor taken
    /// there first; `first` — `z<CR>`, `z.`, `z-` — takes the cursor to the
    /// line's first non-blank as well.
    Line {
        to: Edge,
        first: bool,
        line: Option<i64>,
    },
    /// A scroll sideways, in a window that does not wrap: see [`across`].
    Side(Side),
}

/// A scroll sideways, as Neovim makes it in a window that does not wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// `zl`, `zh`: columns, the view moving right over the text above
    /// nought.
    Cols(i64),
    /// `zL`, `zH`: half the window's width, as many times as counted.
    Halves(i64),
    /// `zs`, `ze`: the cursor's character at the left of the view, or the
    /// right, `'sidescrolloff'` from it.
    Start,
    End,
    /// The wheel sideways: `'mousescroll'`'s `hor` columns, or with Shift or
    /// Ctrl the window's width; one that leaves the cursor's line out of
    /// sight takes the cursor to the longest line in view.
    Wheel(i64),
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
        Command::Side(_) => return None,
        Command::Line { to, line, .. } => {
            // A 'scrolloff' as tall as half the window keeps the cursor in
            // the middle by rules of its own.
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
/// character's first — and always its first in insert mode.
fn sits(start: usize, width: usize, c: char, shape: &Shape, insert: bool) -> usize {
    if c == '\t' && !shape.list && !insert {
        start + width - 1
    } else {
        start
    }
}

/// Where on a line the cursor lands, in display columns, wanting to be at
/// `want`: on the character there, or on the last of a line too short — or,
/// in insert mode, just past it.
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

/// The display column of the character at byte `byte` of a line — Neovim's
/// `curcol` — or of its last, for a byte past its end.
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

/// Where a line's first non-blank character is, in display columns — or
/// its last character, for a line all blank.
pub fn first_nonblank(text: &str, shape: &Shape) -> usize {
    let cols = columns(text, shape);
    let at = cols
        .iter()
        .find(|(_, _, c)| !matches!(c, ' ' | '\t'))
        .or(cols.last());
    at.map_or(0, |&(start, w, c)| sits(start, w, c, shape, false))
}

/// Where a view is sideways, as far as a prediction is concerned: its first
/// display column, and the cursor's line and display column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Across {
    pub left: usize,
    pub line: i64,
    pub vcol: usize,
}

/// The span, in display columns, of the character at display column `vcol`
/// of a line, or of its last if the line is shorter; `(0, 0)` for an empty
/// one.
fn span(cols: &[(usize, usize, char)], vcol: usize) -> (usize, usize) {
    cols.iter()
        .find(|(start, w, _)| vcol < start + w)
        .or(cols.last())
        .map_or((0, 0), |&(start, w, _)| (start, start + w - 1))
}

/// Where `side` takes a view at `at` in a window `width` columns wide —
/// its gutter and all — and of `shape`, as Neovim's `set_leftcol`, `zs`, `ze`
/// and `do_mousescroll_horiz` take it, and as Neovim 0.12 was found to:
/// `text` is the text of a line, and `shown` the lines in view, for the
/// wheel to find the longest of. `None` where the client cannot say.
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
        // These set the column outright, and nothing more.
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
    // The wheel, past the end of the cursor's line: the cursor goes to the
    // longest line in view, the nearest of those as long, and its start.
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
    // `set_leftcol`: the cursor kept in view, `'sidescrolloff'` from its
    // edges, on a character wholly in view where there is one.
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
    // And then as the cursor is drawn: a view the cursor is too near the
    // edge of moves `'sidescroll'` columns at least — or, with that nought,
    // or that far, centres it, which is not predicted.
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

/// Where Neovim shows a view that does not wrap from — its first display
/// column — once it draws the cursor on display column `vcol` of a line of
/// `text`, the view `width` columns wide, its gutter and all, and shown from
/// column `left` till then: as its `curs_columns` does, and as Neovim 0.12
/// was found to. A view the cursor is not wholly in, `'sidescrolloff'` from
/// its edges — out of it, or on a character two cells wide that an edge
/// cuts — moves `'sidescroll'` columns at least to show it; or, with that
/// nought or the cursor half the view or more away, is centred on it, as
/// near as the first column lets it be. `left` where it need not move.
pub fn showing(text: &str, shape: &Shape, vcol: usize, left: usize, width: usize) -> usize {
    let tw = width.saturating_sub(shape.textoff) as i64;
    if shape.wrap || tw == 0 {
        return left;
    }
    let cols = columns(text, shape);
    // Past the end of the line, as in Insert mode, the cursor is a cell of
    // nothing.
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

/// Whether display column `vcol` of a line of `text` is on its last
/// character, or past it: on a line sent cut short, where the line goes on
/// past what the client has.
pub fn at_end(text: &str, shape: &Shape, vcol: usize) -> bool {
    columns(text, shape)
        .last()
        .is_none_or(|&(start, ..)| vcol >= start)
}

/// Whether the client knows enough of the lines to scroll a view sideways
/// from `at` to `to` (see [`across`]), where some were sent cut short
/// (`cut`): each of those, in view or the cursor's, must reach past the new
/// view's right edge — so that it draws, and nothing turns on where it
/// ends — and the wheel must not have sent the cursor to the longest line.
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

/// The rows of a window's text, `rows` as Neovim drew them from display
/// column `from`, moved to show the text from column `to`: what was in view
/// and still is keeps its colours, and what comes into view is drawn from
/// `text` — the row's line, `None` past the buffer's end — in the window's
/// own colours. So is a character two cells wide that the edge of one view
/// cuts in two and not the other's, whole or cut as the new one has it (see
/// [`line`]): Neovim's `<` or `>` for an edge that has moved, or a half whose
/// other half the new view's edge leaves out, is not what Neovim will draw.
/// `None` where a line was not sent.
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
            let mut out = row.clone();
            for c in shape.textoff..width {
                let v = to + (c - shape.textoff);
                // A cell is kept where the two rows agree on which cells
                // are halves, there and just after it: where they do not, an
                // edge cuts a character in one and not the other.
                let kept = (from..from + tw)
                    .contains(&v)
                    .then(|| shape.textoff + v - from)
                    .filter(|&o| {
                        half(row, o) == half(&fresh, c) && half(row, o + 1) == half(&fresh, c + 1)
                    });
                out[c] = match kept {
                    Some(o) => row[o].clone(),
                    None => fresh[c].clone(),
                };
            }
            Some(out)
        })
        .collect()
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

/// The `z` commands that scroll, after their `z`; and those that scroll
/// sideways.
const Z: &[&str] = &["t", "<CR>", "z", ".", "b", "-"];
const Z_SIDE: &[&str] = &["l", "h", "L", "H", "s", "e", "<Right>", "<Left>"];

/// The keys that scroll in insert mode: by themselves, and after `<C-x>`.
const INSERT_SCROLLS: &[&str] = &["<PageDown>", "<PageUp>", "<S-Down>", "<S-Up>"];
const CTRL_X: &[&str] = &["<C-e>", "<C-y>"];

/// Keys that, in insert mode, wait for another, or leave it for a command:
/// the client cannot follow them.
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

/// The wheel, as a mapping names it.
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

/// Every key, and `z` command, a mapping of which changes what the client
/// makes of it: what the agent is asked to look up (see [`Shape::mapped`]).
pub fn keys() -> Vec<String> {
    let digits = (0..10).map(|d| d.to_string());
    let z = Z.iter().chain(Z_SIDE).map(|k| format!("z{k}"));
    WHOLE
        .iter()
        .chain(SCROLLS)
        .chain(WHEEL)
        .map(|k| k.to_string())
        .chain(z)
        .chain(digits)
        .collect()
}

/// The keys the client follows in insert mode, a mapping of which changes
/// what it makes of them (see [`Shape::imapped`]).
pub fn insert_keys() -> Vec<String> {
    INSERT_SCROLLS
        .iter()
        .chain(CTRL_X)
        .chain(&["<C-x>", "<Esc>", "<C-c>"])
        .map(|k| k.to_string())
        .collect()
}

/// The name a mapping of the wheel turned `way` — `Down`, `Up`, `Right`,
/// `Left` — with modifiers `mods` has, as in [`WHEEL`].
pub fn wheel(way: &str, mods: &str) -> String {
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

/// The mode Neovim says it is in, as far as the predictions care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Normal or visual mode.
    Normal,
    /// Insert or replace mode.
    Insert,
    Other,
}

impl Mode {
    /// A mode by the name `mode_change` gives it.
    pub fn of(name: &str) -> Self {
        match name {
            "normal" | "visual" => Mode::Normal,
            "insert" | "replace" => Mode::Insert,
            _ => Mode::Other,
        }
    }
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
    /// The last of them leaves nothing pending in normal mode; and it is one
    /// that, in insert mode, waits for another.
    whole: bool,
    waits: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    /// Waiting for a command, with the count typed so far.
    Ready(Option<i64>),
    /// A `z`, and the count before it.
    Z(Option<i64>),
    /// Insert or replace mode, after a `<C-x>` if `x`.
    Insert { x: bool },
    #[default]
    Lost,
}

impl Typed {
    /// A key typed — `mapped` says whether something maps it, in insert mode
    /// if asked about it — as Neovim will take it: the scroll it makes, if it
    /// makes one the client can follow.
    pub fn key(&mut self, key: &str, mapped: impl Fn(bool, &str) -> bool) -> Option<Command> {
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
                if let Some(side) = side.filter(|_| !mapped(&format!("z{key}"))) {
                    self.state = State::Ready(None);
                    self.whole = true;
                    return Some(Command::Side(side));
                }
                let command = Z
                    .iter()
                    .position(|z| *z == key)
                    .filter(|_| !mapped(&format!("z{key}")));
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
                        self.sent += 1;
                        self.whole = false;
                        self.state = State::Lost;
                        None
                    }
                }
            }
            State::Lost | State::Insert { .. } => {
                self.moved(key, &mapped);
                None
            }
        }
    }

    /// A key typed in insert mode, after a `<C-x>` if `x`.
    fn insert_key(&mut self, key: &str, x: bool, mapped: impl Fn(&str) -> bool) -> Option<Command> {
        let plain = !mapped(key);
        if x && plain {
            match key {
                "<C-e>" => return Some(Command::Lines(1)),
                "<C-y>" => return Some(Command::Lines(-1)),
                _ => {}
            }
        }
        if plain {
            if let Some(command) = INSERT_SCROLLS
                .contains(&key)
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
        // Text, or a key that moves the cursor, which the client does not
        // predict; or one that waits for another, which it cannot follow.
        self.sent += 1;
        self.waits = !plain || INSERT_WAITS.contains(&key);
        self.whole = !self.waits;
        self.state = match key {
            "<Esc>" | "<C-c>" if plain => State::Ready(None),
            _ if self.whole => State::Insert { x: false },
            _ => State::Lost,
        };
        None
    }

    /// Whether the keys are being typed in insert mode, as far as the client
    /// can tell.
    pub fn inserting(&self) -> bool {
        matches!(self.state, State::Insert { .. })
    }

    /// A key that moves things the client does not predict.
    fn moved(&mut self, key: &str, mapped: &impl Fn(&str) -> bool) {
        self.sent += 1;
        self.waits = INSERT_WAITS.contains(&key);
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
        self.waits = false;
        self.state = State::Lost;
    }

    /// A fresh attach: nothing is known of what is pending.
    pub fn attached(&mut self) {
        self.sent += 1;
        self.whole = true;
        self.waits = false;
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

    /// The fence for the inputs counted `at` is answered, Neovim in `mode`.
    pub fn answered(&mut self, at: u64, mode: Mode) {
        self.fence = None;
        self.drawn = self.drawn.max(at);
        self.settle(mode);
    }

    /// Neovim has drawn, and is in `mode`: with every input drawn and the
    /// last leaving nothing pending, it is waiting for a fresh command, or
    /// for what is typed in insert mode. A fence can be answered before the
    /// frame that says which mode Neovim is in, which is why this is asked
    /// again at every frame.
    pub fn settle(&mut self, mode: Mode) {
        if self.drawn == self.sent && self.state == State::Lost {
            self.state = match mode {
                Mode::Normal if self.whole => State::Ready(None),
                Mode::Insert if !self.waits => State::Insert { x: false },
                _ => State::Lost,
            };
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
    /// The display column the cursor wants in each window, as the agent
    /// last said: see [`Predictor::want`].
    wants: HashMap<i64, usize>,
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

    /// How many times as far as it is tall the agent is to keep the lines
    /// either side of a window's view, for the round trip as it is: as many
    /// pages as a key held down types in a round trip, which is how far the
    /// client gets ahead of Neovim's view before the lines around Neovim's
    /// next one come, and two over, for the page in view and the time Neovim
    /// and the link take besides; no fewer than two, and no more than
    /// [`MOST_REACH`].
    pub fn reach(&self) -> i64 {
        let rtt = self.srtt.unwrap_or_default().as_micros();
        let pages = (rtt * PAGES_A_SECOND).div_ceil(1_000_000);
        i64::try_from(pages).map_or(MOST_REACH, |p| (p + 2).clamp(2, MOST_REACH))
    }

    /// How far the agent is to keep lines either side of a view: as far as
    /// [`Predictor::reach`] says while the link is slow enough to predict
    /// over, and not at all while it is not — no lines, colours or shapes
    /// sent for predictions none are made from (see [`ON`]).
    pub fn agent_reach(&self) -> i64 {
        if self.on {
            self.reach()
        } else {
            0
        }
    }

    /// When a prediction made `now` is given up on, unconfirmed — and one
    /// made before, Neovim heard `now` still at work on the keys typed ahead
    /// of it.
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

    /// What the agent says that is the predictions': `lines` and what goes
    /// with them, `shape` and `want`. Says whether it was.
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
                            .map(|(i, t)| Line {
                                text: lossy(t),
                                spans: spans(colours.and_then(|c| c.get(i))),
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
                if let (Some(win), Some(shape)) = (int(0), shape) {
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
            _ => false,
        }
    }

    /// A window closed.
    pub fn closed(&mut self, win: i64) {
        self.shapes.remove(&win);
        self.lines.remove(&win);
        self.wants.remove(&win);
    }

    /// A fresh attach: its agent will say everything again.
    pub fn reset(&mut self) {
        self.shapes.clear();
        self.lines.clear();
        self.wants.clear();
    }

    /// The display column the cursor wants in window `win`, as Neovim keeps
    /// it (`curswant`) and the agent last said: where a scroll that takes
    /// the cursor to another line lands it ([`landing`]), wherever on its
    /// own line it is now — further left at the end of a line too short for
    /// it, or on a character two cells wide that starts before it.
    pub fn want(&self, win: i64) -> Option<usize> {
        self.wants.get(&win).copied()
    }

    pub fn shape(&self, win: i64) -> Option<&Shape> {
        self.shapes.get(&win)
    }

    /// A key typed while window `win` has the cursor, as [`Typed::key`]
    /// takes it, with that window's mappings.
    pub fn key(&mut self, win: Option<i64>, key: &str) -> Option<Command> {
        let shape = win.and_then(|w| self.shapes.get(&w));
        let mapped = |insert: bool, k: &str| {
            shape.is_some_and(|s| {
                let list = if insert { &s.imapped } else { &s.mapped };
                list.iter().any(|m| m == k)
            })
        };
        self.typed.key(key, mapped)
    }

    /// A count given `<C-d>` or `<C-u>` is window `win`'s `'scroll'` from
    /// then on.
    pub fn set_scroll(&mut self, win: i64, scroll: i64) {
        if let Some(s) = self.shapes.get_mut(&win) {
            s.scroll = scroll;
        }
    }

    /// Neovim's view of window `win` now starts at display column `left`.
    pub fn set_leftcol(&mut self, win: i64, left: usize) {
        if let Some(s) = self.shapes.get_mut(&win) {
            s.leftcol = left;
        }
    }

    /// The text of buffer line `lnum` of window `win`, if the agent has sent
    /// it.
    pub fn text(&self, win: i64, lnum: i64) -> Option<&str> {
        let shape = self.shapes.get(&win)?;
        self.lines.get(&win)?.get(shape.buf, lnum)
    }

    /// Whether the agent may have sent buffer line `lnum` of window `win`
    /// cut short: see [`known`].
    pub fn cut(&self, win: i64, lnum: i64) -> bool {
        self.shapes
            .get(&win)
            .zip(self.lines.get(&win))
            .is_some_and(|(shape, lines)| lines.cut(shape.buf, lnum))
    }

    /// The text of buffer line `lnum` of window `win`, and the colours the
    /// editor's highlighting gives it, as the agent sent them.
    pub fn coloured(&self, win: i64, lnum: i64) -> Option<(&str, &[Span])> {
        let shape = self.shapes.get(&win)?;
        let line = self.lines.get(&win)?.line(shape.buf, lnum)?;
        Some((line.text.as_str(), line.spans.as_slice()))
    }

    /// Buffer line `lnum` of window `win` as a row of it would draw it — see
    /// [`render`] — or `None` where the client cannot say: a line the agent
    /// has not sent. Its text is in the colours the agent sent with it.
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
            let line = self.lines.get(&win)?.line(shape.buf, lnum)?;
            Some((line.text.as_str(), line.spans.as_slice()))
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
            imapped: Vec::new(),
            sidescrolloff: 0,
            sidescroll: 1,
            hor: 6,
        }
    }

    /// A line's text with no colours of its own.
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
        }
    }

    fn text(row: &[Cell]) -> String {
        let mut b = Vec::new();
        for c in row {
            c.text.push_to(&mut b);
        }
        String::from_utf8(b).unwrap()
    }

    /// Spans colour the bytes they cover, a group laid over the window's own
    /// colour; one later in the list over one before it; one with no end to
    /// the end of the line — whatever the characters are as wide as.
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

    /// The number right-aligned in the gutter, a blank after it, as Neovim
    /// draws `'number'`; the text in the window's own colour.
    #[test]
    fn a_row_has_its_number_and_its_text() {
        let row = render(plain("let x = 1;"), &shape(), 22, 20, &look(), 3).unwrap();
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

    /// Tabs to the next stop, controls as `^X`, a character two cells wide as
    /// two, and a combining mark with the character before it.
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

    /// What Neovim shows as something it is not is in `SpecialKey`, every
    /// cell of it, as Neovim 0.12 drew it: `a^Ab^?c`, `x<80>y`, a tab as
    /// `^I` under `'list'` with no `tab:` for it — whatever colours the
    /// editor's highlighting gives the character — and, from `'leftcol'` 1,
    /// what is left of one: the `A` of `^A`, the `80>` of `<80>`. A tab
    /// drawn with `'listchars'` is not one: Neovim has `Whitespace` for it.
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

    /// A tab `'list'` shows with its `tab:` is in `Whitespace`, every cell
    /// of it, as Neovim 0.12 drew it with `tab:>-` and `tab:<->`: eight
    /// cells wide `>-------` and `<------>`, two `>-` and `<>`, one `>`
    /// either way — a `tab:` of three ends in its third — and from
    /// `'leftcol'` 1, what is left of one; whatever colours the editor's
    /// highlighting gives the tab. With `'list'` off, blanks as ever.
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

    /// A window that wraps takes no line longer than it is wide; one that
    /// does not cuts it off, from where it is scrolled to sideways.
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

    /// A character two cells wide that the edge of a window that does not
    /// wrap cuts in two is drawn as Neovim 0.12 drew one, in `NonText`
    /// whatever the character's own colours: twelve columns wide,
    /// `abcdefghijk日x` was `abcdefghijk>`, and from `leftcol` 1,
    /// `日abcdefghijklmn` was `<abcdefghijk`. Never half of it on its own.
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
        // Coloured by the editor's highlighting, and with a combining mark,
        // which goes with the character.
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
        // A column of text and no more, cut either way.
        let row = render(plain("日"), &s, 0, 1, &look(), 0).unwrap();
        assert_eq!(row, [mark('>')]);
        let row = render(plain("日"), &from1, 0, 1, &look(), 0).unwrap();
        assert_eq!(row, [mark('<')]);
    }

    /// A character two cells wide that fits is drawn whole, as ever: up to a
    /// window's right edge, or from its left; and a window that wraps still
    /// takes no line too long for it.
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
    /// past the end left out of both. The groups the editor draws itself
    /// are its to say, and where it has not, the text's own — or for a row
    /// past the end, the default.
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
            // The wheel with Shift or Ctrl in insert mode: what the window shows.
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

    /// Where the cursor lands on a line: on the character there, a tab's last
    /// column, the last character of a line too short; the first non-blank.
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

    /// In insert mode, the keys that scroll there — the pages, and
    /// `<C-e>` and `<C-y>` after `<C-x>`, for as long as they follow it — and
    /// no others; insert mode found once Neovim has caught up, and left by an
    /// escape.
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

    /// What Neovim 0.12 did sideways in a window 80 columns wide that does
    /// not wrap, over lines `n` as long as the n-th of 10, 40, 100, 160, 5,
    /// 0 and 70 — line 52 is 160 long, 53 5 and 54 empty — from the cursor's
    /// line, its column and the view's first column, to the cursor's column
    /// and the view's first.
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

    /// What Neovim 0.12 did with a view 80 columns wide that does not wrap,
    /// once a scroll had left the cursor on display column `vcol` of a line
    /// with the view from column `left`: the column it then showed the view
    /// from. Over `line 3`; a line of `abcdefghij` 207 long; `line 21` in
    /// Insert mode, the cursor past its end; `line 102 ` and `日本` on, a
    /// character two cells wide starting on each odd column; and a tab.
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
            // 'sidescroll' columns at least, or with it nought, centred.
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

    /// The last character of a line, and past it, are its end.
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

    /// The wheel past the end of the cursor's line takes the cursor to the
    /// longest line in view, the nearest of those as long.
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

    /// A line sent cut short does for a scroll sideways only while it reaches
    /// past the new view; and where the wheel looks for the longest line,
    /// no line cut short does at all.
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

        // The agent says how much of a line it sends; runs joined keep the
        // least of what they were sent with.
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

    /// Moved sideways, what was in view keeps its colours; what comes into
    /// view is the line's text in the window's own; the gutter stays put.
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

    /// Moved sideways, a character two cells wide is drawn as the new view
    /// cuts it: whole where only the old view's edge cut it, marked where
    /// only the new one's does — never Neovim's `<` or `>` where its edge
    /// was, nor a half on its own. One whole in both keeps its colours.
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

    /// Scrolls are known by their keys, with their counts and their `z`s;
    /// the keys between them are followed, and a key that cannot be loses
    /// the client until Neovim is known to have caught up.
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

    /// The colours the agent sends come with the lines, go with the lines an
    /// edit replaces until it sends theirs, and are taken anew for lines kept
    /// as they are at the change it says.
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

    /// An edit the agent says the buffer has had is made to the lines kept:
    /// within them, its text in their place; before them, they move; after
    /// them, nothing — and one made to lines as they were at another change
    /// leaves none.
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

    /// The agent says which of the lines kept to keep, as it lets go of
    /// those furthest from where the view is going: the rest go, and what
    /// it sends next joins what is left. Said of the buffer at another
    /// change than the one kept, it leaves none.
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

    /// The lines kept reach as far either side as a key held down pages in
    /// a round trip, and two windows more; no fewer than two, nor more than
    /// [`MOST_REACH`].
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

    /// The column the agent says the cursor wants is its window's, till it
    /// says another; it goes with the window, and with an attach afresh,
    /// whose agent says it again.
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
