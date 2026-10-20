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
//!
//! # What Neovim says it made of each key
//!
//! Told to work out more than the scrolls (`[client] predict_keys`), the
//! client has the agent count the keys Neovim takes, as it takes them, and
//! say, each time Neovim goes to wait for a key with every key typed taken
//! and nothing pending, how many it has taken, the last few of them, and
//! where they left the cursor, the view and the mode ([`Ack`]) — just ahead
//! of the frame that draws them, with which the client takes it. The client
//! numbers the keys it sends the same way ([`Typed::seq`]) and ties the two
//! counts where nothing can be in doubt: at a fence answered with nothing
//! sent after it, which Neovim answers only once it has taken every key
//! before it. It checks them at every ack and fence after, and what it
//! cannot account for puts them in doubt till the next such fence
//! ([`Sync`]). So each ack is Neovim's last word on the client's keys up to
//! one of them — what the client would have made of them can be judged by
//! it, key by key — and how long that word takes to come is how long a
//! prediction is waited for ([`Predictor::deadline`]).
//!
//! The scrolls go by it. A key the client predicts no longer leaves it
//! unsure of the next, nor one it cannot follow of every key after it till
//! Neovim has caught up with them all: from each ack it starts again where
//! Neovim is, and makes what it can of the keys sent since — a scroll typed
//! after a key it was blind to is made with that key's ack, a round trip
//! after the key, not after the last typed ([`Typed`]). And a scroll made
//! ahead of Neovim of a key an ack covers, which the frame for it did not
//! make, is given up on with that frame, not at its deadline.
//!
//! Nothing else is drawn of it yet. The client follows the keys that move the
//! cursor in Normal mode a character or a line at a time
//! ([`super::moves`]), works out where those typed since Neovim's last word
//! would have left the cursor and the view, and judges that against where
//! the ack for them says Neovim left them — counting the verdicts for the
//! log ([`Shadow`]), which is to say how often it would have been right
//! before any of it is.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use rmpv::ValueRef;
use unicode_width::UnicodeWidthChar;

use super::grid::{Cell, Text};
use super::moves;
use super::redraw;
use crate::config::PredictKeys;

/// Predictions start once the smoothed round trip reaches this, and stop
/// once it falls back under [`OFF`]: the gap keeps a link that hovers about
/// the one value from turning them on and off.
pub const ON: Duration = Duration::from_millis(30);
pub const OFF: Duration = Duration::from_millis(20);

/// How long past two round trips a prediction waits for Neovim to confirm
/// it — or four times the round trip's variation, where that is longer.
const GRACE: Duration = Duration::from_millis(250);

/// And past that, how long a segment lost on the way takes to come again:
/// TCP sends one again no sooner than its least retransmission timeout,
/// 200 ms on Linux, and everything behind it on the stream waits for it. A
/// prediction given up on just before Neovim's answer arrives is drawn back
/// to where Neovim was and then on to where it is, which on a lossy link
/// would be most of the predictions that waited out a lost segment.
const LOSS: Duration = Duration::from_millis(200);

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
    /// `'list'`, and its `'listchars'` `tab`, empty for none, and the rest
    /// of them.
    pub list: bool,
    pub tab: Vec<char>,
    pub lcs: Listchars,
    /// Whether Neovim looks at every character of a line it draws for some
    /// highlighting of its own — `'linebreak'`, `'spell'`, syntax items —
    /// and so at its blanks, whatever `'listchars'` has: see [`Blanks`].
    pub checks: bool,
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
    /// sidescrolloff, sidescroll, hor, lcs, checks]` — `lcs` as
    /// [`Listchars::read`] has it.
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
        })
    }

    /// Whether `'listchars'` would draw the blanks of a line sent cut short,
    /// whose text the client has is `text`, by what was cut off: by where a
    /// run of blanks at the line's end starts, where its text ends in one,
    /// or where a run at its start ends, where its text is all blanks.
    fn unsure(&self, text: &str) -> bool {
        let l = &self.lcs;
        let blank = |c: char| matches!(c, ' ' | '\t');
        self.list
            && ((l.trail.is_some() && text.ends_with(blank))
                || (l.leads() && text.chars().all(blank)))
    }
}

/// What `'listchars'` shows besides a tab (see [`Shape::tab`]), as the agent
/// last said it — none of it where `'list'` is not set — each as Neovim 0.12
/// was found to draw it: the blanks in `Whitespace`, the rest in `NonText`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listchars {
    /// A space; and one of a run of two or more, through these in turn.
    pub space: Option<char>,
    pub multispace: Vec<char>,
    /// A space before a line's first other character, one of a run of them,
    /// and a tab there, as `tab` draws one.
    pub lead: Option<char>,
    pub leadmultispace: Vec<char>,
    pub leadtab: Vec<char>,
    /// A space after a line's last other character.
    pub trail: Option<char>,
    /// A no-break space, U+00A0 or U+202F.
    pub nbsp: Option<char>,
    /// After a line's text; and, in a view that does not wrap, in its last
    /// column where its line goes on past it, and in its first where it is
    /// scrolled sideways.
    pub eol: Option<char>,
    pub extends: Option<char>,
    pub precedes: Option<char>,
}

impl Listchars {
    /// As the agent sends them: `[space, multispace, lead, leadmultispace,
    /// trail, nbsp, eol, extends, precedes, leadtab]`, empty for none.
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

    /// Whether these draw blanks by where a line's run of them at its start
    /// ends.
    fn leads(&self) -> bool {
        self.lead.is_some() || !self.leadmultispace.is_empty() || !self.leadtab.is_empty()
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
/// in `room`, and its end with it where `'listchars'` shows that.
///
/// A character two cells wide that an edge of the view cuts in two is drawn
/// as Neovim draws it: a `>` in the last column, a `<` in the first, in
/// `NonText`. Its left half on its own would still be drawn two cells wide,
/// by a terminal that knows nothing of the cut, and the rest of its row a
/// cell to the right of where the frame has it (see `super::grid`). A
/// character Neovim shows as something it is not — a control character as
/// `^X`, a tab as `^I` where `'list'` has no `tab:` for it, an unprintable
/// one as `<xx>` — is in `SpecialKey`; and what `'list'` shows with its
/// `'listchars'` as Neovim draws it (see [`Listchars`] and [`Blanks`]): a
/// tab, a space or a no-break space in `Whitespace`, and in `NonText` the
/// line's end — in the first column of a view that does not wrap scrolled
/// past it — and, in such a view, `precedes` in place of whatever is in its
/// first column where it is scrolled sideways, the other half of a
/// character two cells wide there a `<`, and `extends` in place of whatever
/// is in its last where more of the line follows, but for a character two
/// cells wide that ends there. Neovim lays each of these over a window's own
/// `Normal` where it has one, and all but the `>`, `precedes` and `extends`
/// over the character's own colours as well: highlights of its own making,
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
                // `leadtab` in place of `tab` before the line's first other
                // character.
                let tab = if blanks.leading(at) && !lcs.leadtab.is_empty() {
                    &lcs.leadtab
                } else {
                    &shape.tab
                };
                for i in 0..n {
                    // A `tab:` of three ends in its third, a tab one cell
                    // wide in that alone.
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
    // The line's end; in the first column of a view that does not wrap,
    // where that is scrolled past it.
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

/// How `'list'` draws a line's blanks with the `'listchars'` for them, as
/// Neovim's `win_line` does, and as Neovim 0.12 was found to:
///
/// - a space after the line's last character other than a blank as `trail`,
///   where that is set;
/// - one before its first, where `lead`, `leadmultispace` or `leadtab` is: in
///   a run of two or more as `leadmultispace`'s characters in turn, else as
///   `lead`, else as `space`, else as itself — in `Whitespace` all the same;
/// - any other in a run as `multispace`'s in turn, else as `space`;
/// - a no-break space as `nbsp`;
/// - none of them followed by a combining mark;
/// - and a tab before the first character other than a blank as `leadtab`
///   draws it, where that is set, else as `tab` does.
///
/// Neovim looks at a line's blanks only where `'listchars'` has something for
/// them but `leadtab`, or where it looks at every character of the line for
/// highlighting of its own anyway: for the window's (see [`Shape::checks`]),
/// or for decorations on the line, such as tree-sitter's colours. So where
/// `leadtab` is all it has for blanks, a space before a line's first
/// character other than a blank is in `Whitespace` on some lines and not on
/// others. The client takes the colours the agent sent with a line for its
/// decorations: it is wrong on a line whose decorations have none, such as
/// virtual text.
struct Blanks<'a> {
    text: &'a str,
    lcs: &'a Listchars,
    on: bool,
    /// Where the line's first character other than a blank is, in bytes,
    /// where `'listchars'` has something for those before it — none for a
    /// line all blanks; and where the run of blanks at its end starts, where
    /// it has `trail`.
    first: Option<usize>,
    trail: Option<usize>,
    /// How far through `multispace` or `leadmultispace` a run of spaces is.
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

    /// Whether byte `at` is before the line's first character other than a
    /// blank, where `'listchars'` has something for those.
    fn leading(&self, at: usize) -> bool {
        self.first.is_some_and(|f| at < f)
    }

    /// What character `c`, at byte `at`, `prev` before it, is drawn as: `None`
    /// for one `'listchars'` leaves as it is.
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

/// A character of a line as Neovim's cursor steps over it: where it starts,
/// in bytes and in display columns, how wide it is, and what it is — a
/// combining mark goes with the character before it, and is no step of its
/// own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ch {
    pub byte: usize,
    pub vcol: usize,
    pub width: usize,
    pub c: char,
}

impl Ch {
    /// The display column the cursor is drawn in on it in Normal mode, and
    /// which it wants from then on: see [`sits`].
    pub fn at(&self, shape: &Shape) -> usize {
        sits(self.vcol, self.width, self.c, shape, false)
    }
}

/// A line's characters, as a window of `shape` lays them out from its first
/// column: see [`Ch`]. A combining mark at the very start of a line, with no
/// character before it to go with, is left out with it.
pub fn chars(text: &str, shape: &Shape) -> Vec<Ch> {
    let mut out = Vec::new();
    let mut vcol = 0;
    for (byte, c) in text.char_indices() {
        let width = match c {
            '\t' if shape.list && shape.tab.is_empty() => 2,
            '\t' => shape.tabstop - vcol % shape.tabstop,
            '\0'..='\x1f' | '\x7f' => 2,
            _ => match c.width() {
                None => format!("<{:02x}>", c as u32).len(),
                Some(0) => continue,
                Some(w) => w,
            },
        };
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

/// Whether Neovim 0.12 steps over a line's characters as [`chars`] does, as
/// far as the client can tell: not where the line has anything Neovim joins
/// into one character by rules the client does not follow — an emoji
/// sequence (a zero-width joiner, a flag's pair, a skin tone, the selector
/// that draws an emoji two cells wide) or a mark that takes a cell of its
/// own in a script of South or South-East Asia — nor a character of no
/// width that is not a combining mark, which Neovim shows as a cell or as
/// its code (a soft hyphen, `<200b>`), nor a combining mark with nothing
/// before it to go with.
pub fn ordinary(text: &str) -> bool {
    text.chars().enumerate().all(|(i, c)| {
        let joined = matches!(c,
            '\u{900}'..='\u{fff}'           // Devanagari to Tibetan
            | '\u{1000}'..='\u{109f}'       // Myanmar
            | '\u{1780}'..='\u{17ff}'       // Khmer
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
        !joined && (c.width() != Some(0) || (mark && i > 0))
    })
}

/// The display columns of a line's characters, as a window of `shape` lays
/// them out from its first column: where each starts, how wide it is, and
/// what it is. Combining marks take no column and are left out.
fn columns(text: &str, shape: &Shape) -> Vec<(usize, usize, char)> {
    chars(text, shape)
        .into_iter()
        .map(|ch| (ch.vcol, ch.width, ch.c))
        .collect()
}

/// Whether a line of `text` takes no more than a row of a window of `shape`
/// `width` columns wide, its gutter and all: always, in one that does not
/// wrap; in one that does, where its text — and where `'listchars'` shows
/// it, its end — fits beside the gutter.
pub fn fits(text: &str, shape: &Shape, width: usize) -> bool {
    let eol = shape.list && shape.lcs.eol.is_some();
    !shape.wrap
        || end(&columns(text, shape)) + usize::from(eol) <= width.saturating_sub(shape.textoff)
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
/// And so are both views' first and last columns where `'listchars'` may
/// draw something of its own there (see [`line`]). `None` where a line was
/// not sent.
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
                // A cell is kept where the two rows agree on which cells
                // are halves, there and just after it: where they do not, an
                // edge cuts a character in one and not the other.
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
///
/// # Where the agent says what Neovim made of each key
///
/// There is a better word than a fence's to go by (see [`Ack`]), and the
/// client goes by it instead: what it makes of the keys is worked out from
/// Neovim's last word on them, and the keys sent since ([`Typed::rebase`]).
/// A key it predicts leaves it as sure as it was — that is the point of
/// predicting it — and a key it cannot follow, or does not make what it
/// makes of it, leaves it *blind* from that key on: nothing is predicted
/// till an ack covers it. At each ack the client starts again from where it
/// says Neovim is, and makes out the keys after it once more, making what
/// it can of those it could not before (see `super::App`): a scroll typed
/// after a key it was blind to is made once Neovim has said where that key
/// left things, not once Neovim has caught up with every key typed.
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
    /// Every input sent, numbered as the agent counts the keys Neovim takes
    /// (see [`Sync`]): a key of `nvim_input` one, an `nvim_input_mouse` one.
    seq: u64,
    /// Which keys the client works out ahead of Neovim: past the scrolls,
    /// it goes by what the agent says Neovim made of each key, not by
    /// fences.
    keys: PredictKeys,
    /// Where it does, whether an input since Neovim's last word is one it
    /// does not know what Neovim makes of.
    blind: bool,
    /// Whether the input being made out is one the client cannot follow.
    lost: bool,
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
        self.seq += 1;
        let command = self.step(key, mapped);
        if self.lost {
            self.sent += 1;
        }
        command
    }

    /// A key sent before, made out again from where an ack left the client
    /// (see [`Typed::rebase`]) — or the first time, as [`Typed::key`] makes
    /// out a key typed: the scroll it makes, if it makes one the client can
    /// follow. Where it is not one, and the agent says what Neovim made of
    /// each key, the client is blind from it on.
    pub fn step(&mut self, key: &str, mapped: impl Fn(bool, &str) -> bool) -> Option<Command> {
        self.lost = false;
        let command = self.parse(key, mapped);
        if self.lost {
            self.blinded();
        }
        command
    }

    /// What a key typed makes, as far as the client can tell; a key it
    /// cannot follow is [`Typed::lost`].
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
                        self.lost = true;
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
        self.lost = true;
        self.waits = !plain || INSERT_WAITS.contains(&key);
        self.whole = !self.waits;
        self.state = match key {
            "<Esc>" | "<C-c>" if plain => State::Ready(None),
            _ if self.whole => State::Insert { x: false },
            _ => State::Lost,
        };
        None
    }

    /// A mouse report sent: numbered as a key is, and says its number.
    pub fn input(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// The number the last input sent was given, a key or a mouse report.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Whether the keys are being typed in insert mode, as far as the client
    /// can tell.
    pub fn inserting(&self) -> bool {
        matches!(self.state, State::Insert { .. })
    }

    /// A key that moves things the client does not predict.
    fn moved(&mut self, key: &str, mapped: &impl Fn(&str) -> bool) {
        self.lost = true;
        self.waits = INSERT_WAITS.contains(&key);
        self.whole = (WHOLE.contains(&key) || SCROLLS.contains(&key)) && !mapped(key);
        self.state = match self.state {
            _ if self.whole && matches!(key, "<Esc>" | "<C-c>") => State::Ready(None),
            State::Ready(_) if self.whole => State::Ready(None),
            _ => State::Lost,
        };
    }

    /// Something else that moves things — a click, a paste — or a scroll
    /// the client made out but did not predict: the client is blind from
    /// here on, where it goes by acks.
    pub fn missed(&mut self) {
        self.sent += 1;
        self.blinded();
    }

    /// A click or a paste, which may change the mode as well.
    pub fn other(&mut self) {
        self.sent += 1;
        self.clicked();
    }

    /// A click, or one made out again (see [`Typed::step`]): where the
    /// cursor goes, and what mode it leaves, nothing says.
    pub fn clicked(&mut self) {
        self.whole = true;
        self.waits = false;
        self.state = State::Lost;
        self.blinded();
    }

    /// Where the agent says what Neovim made of each key, the client does
    /// not know what Neovim makes of the input made out last, and so of
    /// none after it, till an ack covers it.
    pub fn blinded(&mut self) {
        if self.keys >= PredictKeys::Motions {
            self.blind = true;
        }
    }

    /// Neovim is where an ack says, resting as `rest` says: what the inputs
    /// after the last it covers make is to be made out again from there,
    /// each with [`Typed::step`], blind or not as it now finds them.
    pub fn rebase(&mut self, rest: Rest) {
        self.state = match rest {
            Rest::Normal | Rest::Visual => State::Ready(None),
            Rest::Insert => State::Insert { x: false },
            Rest::Busy => State::Lost,
        };
        self.whole = true;
        self.waits = false;
        self.blind = false;
    }

    /// Whether what the inputs since Neovim's last word make can be followed
    /// from it: none of them one the client does not know what Neovim makes
    /// of, and nothing pending that it cannot make out.
    pub fn followed(&self) -> bool {
        !self.blind && self.state != State::Lost
    }

    /// Work out `keys` ahead of Neovim from here on: see [`Typed::keys`].
    /// Blind or not, the client stays as it is: from the scrolls alone, it
    /// is not; to them, it does not matter; and between the motions and
    /// the typing, what it does not know of a key it still does not.
    pub fn set_keys(&mut self, keys: PredictKeys) {
        self.keys = keys;
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

    /// Whether the last key sent leaves Neovim waiting for another to finish
    /// what it started, as far as the client can tell: a count, `z`, an
    /// operator, a key something maps, or one of Insert mode's that waits —
    /// or one it cannot follow, which may. What Neovim says of such a key
    /// comes only once the user has typed the next, however long they took
    /// to.
    pub fn pending(&self) -> bool {
        !self.whole
            || matches!(
                self.state,
                State::Ready(Some(_)) | State::Z(_) | State::Insert { x: true }
            )
    }

    /// Whether a fence is asked after and not yet answered.
    pub fn fencing(&self) -> bool {
        self.fence.is_some()
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
    /// again at every frame. Where the agent says what Neovim made of each
    /// key, its word settles that instead (see [`Typed::rebase`]): a frame
    /// may be one Neovim drew part way through the keys.
    pub fn settle(&mut self, mode: Mode) {
        if self.keys >= PredictKeys::Motions {
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

/// The display column the cursor wants, as Neovim keeps it (`curswant`):
/// a column, or the end of whatever line it is on, as `$` leaves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    Col(usize),
    End,
}

impl Want {
    /// As [`landing`] takes it: the end of a line is past any column of it.
    pub fn col(self) -> usize {
        match self {
            Want::Col(c) => c,
            Want::End => usize::MAX,
        }
    }
}

/// What Neovim is doing, as far as predictions go: waiting in Normal mode
/// for a command, in Visual mode for one, in Insert mode for what is typed,
/// or anything else — an operator pending, the command line, completion, a
/// key typed after Insert mode's `<C-o>`, Replace — which nothing is
/// predicted from. Visual mode has its scrolls predicted, as Normal mode's
/// are, and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rest {
    Normal,
    Visual,
    Insert,
    Busy,
}

impl Rest {
    /// By the mode `nvim_get_mode()` says, as the agent sends it: `n`, `v`,
    /// `V` or `<C-v>`, `i`, or anything else — `no`, `niI`, `ic` and their
    /// kind.
    pub fn of(mode: &str) -> Self {
        match mode {
            "n" => Rest::Normal,
            "v" | "V" | "\x16" => Rest::Visual,
            "i" => Rest::Insert,
            _ => Rest::Busy,
        }
    }

    /// By the name `mode_change` gives the mode Neovim last drew: which says
    /// less, `<C-o>`'s Normal mode being `normal` and completion `insert`.
    pub fn drawn(name: &str) -> Self {
        match name {
            "normal" => Rest::Normal,
            "visual" => Rest::Visual,
            "insert" => Rest::Insert,
            _ => Rest::Busy,
        }
    }
}

/// Where the cursor is in a window, in the buffer's terms, and the view
/// around it: as Neovim has it, by an ack, or as the client works it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    pub win: i64,
    /// The cursor's line, from nought, and its column in bytes.
    pub line: i64,
    pub byte: usize,
    pub want: Want,
    /// The line at the top of the view, from nought, and the first display
    /// column it shows of a window that does not wrap.
    pub top: i64,
    pub left: usize,
    pub rest: Rest,
}

/// What the agent says each time Neovim goes to wait for a key with every
/// key typed taken and nothing pending (`ack`, see `super::AGENT_LUA`): how
/// many keys it has taken, the last few of them, and where they left the
/// cursor, the view and the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ack {
    pub count: u64,
    /// The last four keys taken, or fewer, as Neovim's `keytrans` writes
    /// them: for the client to check its count by (see [`Sync::heard`]).
    pub tail: Box<str>,
    pub pos: Pos,
    /// The window's buffer, its `b:changedtick` and how many lines it has.
    pub buf: i64,
    pub tick: i64,
    pub line_count: i64,
}

impl Ack {
    /// As the agent sends it: `[count, tail, mode, win, buf, tick,
    /// line_count, lnum, col, want, top, left]`, lines from nought and the
    /// column in bytes, `want` -1 for the end of the line. None of the counts
    /// is below nought, the agent's own being Neovim's.
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
        })
    }
}

/// Whether Neovim may count as more than one a key the client sends as one:
/// `<C-c>`, which interrupts as well and is counted twice, and a key with
/// Alt, which Neovim takes as `<Esc>` and the key unless something maps it
/// as it is. The counts are in doubt after one, until a fence ties them
/// again (see [`Sync`]).
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

/// Whether a key the client sent and one Neovim's `keytrans` wrote are the
/// same key: a mouse report (`None`) is whatever Neovim made of it, and a
/// chord is not compared at all, each spelling its modifiers its own way
/// (`<C-d>` and `<C-D>`, `<C-i>` and `<Tab>`). The rest are compared as
/// Neovim reads them: a name in `<…>` in any case, and `<Space>`, `<lt>`,
/// `<Bar>` and `<Bslash>` as the characters they stand for.
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

/// How many inputs [`Sync`] keeps for checking the agent's count by.
const RECENT: usize = 64;

/// The client's inputs, numbered as its agent counts the keys Neovim takes
/// ([`Typed::seq`]): one a key of `nvim_input`, one an `nvim_input_mouse`,
/// none a paste or a change of focus. The two counts are tied by an offset,
/// learnt only where nothing can be in doubt — a fence answered with nothing
/// sent after it: Neovim answers one only once it has taken every key sent
/// before it, so its count then is theirs — and checked at every ack and
/// fence after, against what was sent and the last keys Neovim says it
/// took. What it cannot account for puts the counts in doubt, and nothing
/// the agent says is taken for a key of the client's till the next fence
/// with nothing after it ties them again: keys of another UI's, or a
/// plugin's typed for it, `<C-c>` and Alt (see [`uncertain`]), a paste on
/// the command line, which Neovim takes as keys typed.
///
/// A key Neovim has taken is not always counted yet, though. One that a
/// mapping makes nothing of — `<Nop>`, an `<expr>` that says `''` — reaches
/// the agent only with the key after it, and Neovim answers a fence asked
/// between the two with the count before it: tied there, the counts would
/// be a key out. So a fence ties them only where the last key the agent
/// says it counted, by the newest ack, is the last sent; where it is not,
/// the next that can tie them is one asked after the next key.
#[derive(Debug, Default)]
pub struct Sync {
    /// The client's number for a key is the agent's count, and this.
    offset: Option<i64>,
    doubt: bool,
    /// The last inputs sent, by number, `None` for a mouse report: those
    /// since the offset last moved, the ones before it numbered by another.
    recent: VecDeque<(u64, Option<Box<str>>)>,
    /// Pastes sent, counted.
    pastes: u64,
    /// How late a fence has to have been asked for its answer to be trusted,
    /// where an earlier one's may not be: after the last paste — Neovim
    /// takes one as no key, mostly, and a fence asked before it may be
    /// answered after — and, where the paste is taken as keys, or the last
    /// key sent is not counted yet, after the next key too: Neovim takes
    /// such a paste's keys only after answering a fence asked with it, and
    /// counts such a key only with the next.
    barrier: Option<Fenced>,
    /// The newest ack heard, judged or not: what a fence that ties the
    /// counts finds Neovim's last word to be.
    pub last: Option<Ack>,
    /// The agent has been left, and counts: a fence asked now finds a count
    /// to tie to — one asked before it was left, as the first after an
    /// attach can be, finds none.
    pub agent: bool,
}

/// A fence, as [`Sync`] needs to know it: asked once the inputs numbered to
/// `seq` had been sent, and so many pastes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fenced {
    pub seq: u64,
    pastes: u64,
}

/// What a fence's answer made of the counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// No agent counted: none left yet, or one that does not.
    NoAgent,
    /// Nothing was sent after it: the counts are tied from here.
    Tied,
    /// They were tied, and still agree.
    Checked,
    /// They are not to be trusted: never tied, out of step, or the last key
    /// sent not counted yet.
    Doubted,
}

impl Sync {
    /// Input `seq` sent: `key`, or a mouse report.
    pub fn sent(&mut self, seq: u64, key: Option<&str>) {
        if self.recent.len() == RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back((seq, key.map(Into::into)));
    }

    /// A paste sent, the last input numbered `seq_now`: nothing the agent
    /// says is taken for a key of the client's till a fence asked after it
    /// has been answered — or, where Neovim may take it as keys typed, as it
    /// does on its command line, one asked after the next key, and with the
    /// counts in doubt till then.
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

    /// A fresh attach, and with it a fresh agent, counting from nought.
    pub fn attached(&mut self) {
        *self = Self {
            pastes: self.pastes,
            ..Self::default()
        };
    }

    /// Whether the agent's count can be taken for a number of the client's.
    pub fn synced(&self) -> bool {
        self.offset.is_some() && !self.doubt && self.barrier.is_none()
    }

    /// Whether a fence asked now, the last input sent numbered `seq_now`,
    /// could tie counts that are not: there is an agent to count, and
    /// nothing waits for a key yet to be sent.
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

    /// An ack heard, the last input sent numbered `seq_now`: the number of
    /// the client's last key it covers, where the counts can be trusted.
    /// Neovim cannot have taken more than was sent; and the keys it says it
    /// took last, newest first, must be those sent under the same numbers,
    /// as far back as the client remembers.
    pub fn heard(&mut self, ack: &Ack, seq_now: u64) -> Option<u64> {
        self.last = Some(ack.clone());
        if !self.synced() {
            return None;
        }
        let k = i64::try_from(ack.count)
            .ok()
            .zip(self.offset)
            .and_then(|(count, offset)| count.checked_add(offset))
            .and_then(|k| u64::try_from(k).ok())
            .filter(|&k| k <= seq_now);
        let Some(k) = k else {
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

    /// Fence `at` answered, the agent having counted `taken` keys — -1 for
    /// no agent — and the last input sent numbered `seq_now`. With nothing
    /// sent after it, and asked late enough (see [`Sync::barrier`]), its
    /// count is the client's: the two are tied there, the last key counted
    /// being the last sent. With keys after it, Neovim may have taken some
    /// of those before it; the count must be somewhere from the fence to
    /// the last.
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
            // Where the offset moves, the inputs before were numbered as it
            // was: no check on the keys Neovim says it took from here.
            if self.offset != Some(offset) {
                self.recent.clear();
            }
            self.offset = Some(offset);
            self.doubt = false;
            return Answer::Tied;
        }
        let Some(offset) = self.offset else {
            return Answer::Doubted;
        };
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

    /// Whether the last key the agent counted, having counted `taken`, is
    /// the last input sent, as far as the newest ack says: where it says
    /// nothing of that count, or of any key, there is nothing to tell by.
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

/// Neovim's last word on the keys: the client's number for the last key an
/// ack covers, and the ack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub seq: u64,
    pub ack: Ack,
}

/// A key or a mouse report sent since Neovim's last word on them, numbered
/// (see [`Typed::seq`]), and when it was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub seq: u64,
    pub input: Input,
    pub at: Instant,
    /// Whether what the client made of it is made: a scroll ahead of
    /// Neovim, or nothing to make — so not to be made again from an ack,
    /// the scroll still ahead or made by Neovim since. A scroll given up on
    /// at an ack, or one not made, is made again from it.
    pub made: bool,
    /// Whether it left Neovim waiting for another key, as far as the client
    /// could tell (see [`Typed::pending`]).
    pub pending: bool,
}

/// An input sent, as the client made it out again from an ack (see
/// [`Typed::rebase`]): a key of Neovim's notation, a turn of the wheel,
/// which a scroll ahead may have been made of, or any other mouse report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Key(Box<str>),
    Wheel,
    Click,
}

impl Input {
    /// The key, if it is one.
    pub fn key(&self) -> Option<&str> {
        match self {
            Input::Key(key) => Some(key),
            Input::Wheel | Input::Click => None,
        }
    }
}

/// The most inputs kept since Neovim's last word on them: more, and Neovim
/// is not keeping up with them, or not saying so.
const QUEUE: usize = 512;

/// How the motions the client followed without drawing them came out, each
/// judged at the ack for its key (see [`Predictor::judge`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Shadow {
    /// Where the client would have put the cursor and the view, Neovim put
    /// them; or did not.
    pub agree: u64,
    pub disagree: u64,
    /// Keys the client could not follow, or nothing to follow them from.
    pub unknown: u64,
    /// Acks that covered a scroll made ahead of Neovim which Neovim's frame
    /// for them did not make: one that, judged by acks, would be given up
    /// on then and not at its deadline.
    pub unmade: u64,
}

impl Shadow {
    /// The counts so far, in the log: every [`SHADOW_EVERY`] verdicts, and
    /// as the client goes.
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

/// How often the counts of [`Shadow`] are logged, in verdicts.
const SHADOW_EVERY: u64 = 100;

/// Everything the client keeps for its predictions.
#[derive(Debug, Default)]
pub struct Predictor {
    /// The round trip to the server, and how much it varies, smoothed as TCP
    /// smooths its own (RFC 6298).
    srtt: Option<Duration>,
    rttvar: Option<Duration>,
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
    /// Which keys it works out ahead of Neovim besides the scrolls: past
    /// those, the agent says what Neovim made of each key ([`Ack`]).
    pub keys: PredictKeys,
    /// The client's numbers for its inputs, tied to the agent's count.
    pub sync: Sync,
    /// Neovim's last word on the keys, and the inputs sent since, oldest
    /// first: what the next ack is judged by.
    pub base: Option<Base>,
    pub queue: VecDeque<Sent>,
    /// How long Neovim takes to say what it made of a key, from the key's
    /// going, and how much that varies, smoothed as the round trip is: a
    /// round trip, and Neovim's own time on the key and those before it —
    /// timed only for a key that leaves nothing pending, Neovim's word on one
    /// that does waiting on the user's next.
    lag: Option<(Duration, Duration)>,
    /// The last key that was timed so.
    timed: u64,
    /// How the motions followed came out: see [`Predictor::judge`].
    pub shadow: Shadow,
}

/// A time smoothed as TCP smooths its round trip (RFC 6298), and how much it
/// varies, after `r`: the first `r` itself and half of it, and each after
/// moving them an eighth and a quarter of the way.
fn smooth(was: Option<(Duration, Duration)>, r: Duration) -> (Duration, Duration) {
    match was {
        Some((s, v)) => ((s * 7 + r) / 8, (v * 3 + s.abs_diff(r)) / 4),
        None => (r, r / 2),
    }
}

impl Predictor {
    /// A round trip, as one request and its answer took.
    pub fn sample(&mut self, rtt: Duration) {
        let (srtt, rttvar) = smooth(self.srtt.zip(self.rttvar), rtt);
        self.srtt = Some(srtt);
        self.rttvar = Some(rttvar);
        self.on = srtt >= if self.on { OFF } else { ON };
    }

    /// Whether the agent says what Neovim made of each key: see
    /// [`Predictor::keys`].
    pub fn acks(&self) -> bool {
        self.keys >= PredictKeys::Motions
    }

    /// Work out `keys` ahead of Neovim from here on, besides the scrolls.
    pub fn set_keys(&mut self, keys: PredictKeys) {
        self.keys = keys;
        self.typed.set_keys(keys);
    }

    /// Whether what the inputs since Neovim's last word make can be worked
    /// out from it, where the agent says what Neovim made of each key: the
    /// counts tied, a word from Neovim to go by, and each input since one
    /// the client follows (see [`Typed::followed`]).
    pub fn known(&self) -> bool {
        self.sync.synced() && self.base.is_some() && self.typed.followed()
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
    /// of it: two round trips, [`GRACE`] or the round trip's variation, and a
    /// segment lost on the way ([`LOSS`]). Or, where the agent says when
    /// Neovim has taken each key, as long as a key has been taking to be
    /// answered so ([`Predictor::heard`]), and the grace or how much that
    /// varies, if that is longer: keys typed faster than Neovim takes them
    /// wait in its typeahead, which no round trip measures. Told since not
    /// to have it say so, the client waits as one never told.
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

    /// A fresh attach, or a link too fast to predict over: its agent will
    /// say everything again — and of the keys, nothing till it does.
    pub fn reset(&mut self) {
        self.shapes.clear();
        self.lines.clear();
        self.wants.clear();
        self.base = None;
        self.queue.clear();
    }

    /// A fresh attach: nothing is known of what is pending, and a fresh
    /// agent counts the keys from nought.
    pub fn attached(&mut self) {
        self.typed.attached();
        self.sync.attached();
    }

    /// Whether to ask after a fence: for [`Typed`], or — where the agent says
    /// what Neovim made of each key — for the counts, untied with an agent
    /// there to tie them to (see [`Sync`]). Without, a fence asked before the
    /// agent was left would leave them untied till the first pause after a
    /// key the client does not predict.
    pub fn wants_fence(&self) -> bool {
        self.typed.wants_fence()
            || (self.acks() && self.sync.wants_fence(self.typed.seq()) && !self.typed.fencing())
    }

    /// Input `seq` sent at `now`: `key`, or a mouse report other than a turn
    /// of the wheel. Where the agent says what Neovim made of each key, kept
    /// for its count to be checked by; and for its acks to be judged by
    /// while it says them, which it does not over a link too fast for
    /// predictions.
    pub fn sent(&mut self, seq: u64, key: Option<&str>, now: Instant) {
        let input = key.map_or(Input::Click, |k| Input::Key(k.into()));
        self.keep(seq, input, now);
    }

    /// Input `seq`, a turn of the wheel, sent at `now`: see
    /// [`Predictor::sent`].
    pub fn turned(&mut self, seq: u64, now: Instant) {
        self.keep(seq, Input::Wheel, now);
    }

    fn keep(&mut self, seq: u64, input: Input, now: Instant) {
        if !self.acks() {
            return;
        }
        let key = input.key();
        self.sync.sent(seq, key);
        if key.is_some_and(uncertain) {
            self.sync.doubt("a key Neovim may count as two");
        }
        if !self.on {
            return;
        }
        if self.queue.len() == QUEUE {
            self.sync.doubt("too many keys unanswered");
            self.queue.clear();
        }
        self.queue.push_back(Sent {
            seq,
            input,
            at: now,
            pending: self.typed.pending(),
            made: false,
        });
    }

    /// What the client made of input `seq` is made: see [`Sent::made`].
    pub fn made(&mut self, seq: u64) {
        if let Some(s) = self.queue.iter_mut().find(|s| s.seq == seq) {
            s.made = true;
        }
    }

    /// The scrolls ahead made of inputs `seqs` are given up on: to be made
    /// again from the next ack, where they can be.
    pub fn given_up(&mut self, seqs: &[u64]) {
        for s in self.queue.iter_mut().filter(|s| seqs.contains(&s.seq)) {
            s.made = false;
        }
    }

    /// An ack heard at `now`: the client's number for the last key it
    /// covers, where the counts can be trusted (see [`Sync::heard`]) — that
    /// key timed, from its going, for [`Predictor::deadline`], where it left
    /// nothing pending: one that waits for the next key, an `f` or a prefix
    /// of `'timeoutlen'`'s, is answered as late as the user, or the timeout,
    /// lets it be, which is no time of Neovim's or the link's. The column
    /// the cursor wants comes with every ack, as it came on its own without
    /// them (see [`Predictor::want`]).
    pub fn heard(&mut self, ack: &Ack, now: Instant) -> Option<u64> {
        self.wants.insert(ack.pos.win, ack.pos.want.col());
        let k = self.sync.heard(ack, self.typed.seq())?;
        if k > self.timed {
            if let Some(key) = self.queue.iter().find(|s| s.seq == k && !s.pending) {
                self.timed = k;
                self.lag = Some(smooth(self.lag, now.saturating_duration_since(key.at)));
            }
        }
        Some(k)
    }

    /// Fence `at` answered, the agent having counted `taken` keys: see
    /// [`Sync::answered`]. Where it ties the counts, Neovim's last word is
    /// the last ack the agent sent, if it sent one after the last key: there
    /// is nothing after it. How long it took is not timed: one asked after
    /// a key that waits for another is answered only once the user has
    /// typed that.
    pub fn answered(&mut self, at: Fenced, taken: i64) {
        if self.sync.answered(at, self.typed.seq(), taken) != Answer::Tied {
            return;
        }
        let last = self.sync.last.as_ref();
        if let Some(ack) = last.filter(|a| i64::try_from(a.count) == Ok(taken)) {
            self.typed.rebase(ack.pos.rest);
            self.base = Some(Base {
                seq: at.seq,
                ack: ack.clone(),
            });
            self.queue.retain(|s| s.seq > at.seq);
        }
    }

    /// Whether the lines kept of window `win` are buffer `buf`'s as it was at
    /// `b:changedtick` `tick`. The agent sends the edits ahead of each ack,
    /// so that they are the ack's.
    pub fn fresh(&self, win: i64, buf: i64, tick: i64) -> bool {
        self.lines
            .get(&win)
            .is_some_and(|l| l.buf == buf && l.tick == tick)
    }

    /// Judge `ack`, which covers the client's keys up to its `k`: where the
    /// client would have put the cursor and the view for the keys since
    /// Neovim's last word, had it drawn them, against where Neovim says it
    /// put them — and take it for Neovim's last word from here. Nothing is
    /// drawn of it yet: the verdicts are counted, and logged ([`Shadow`]).
    /// `view` is the window's rows and columns, its gutter and all, as the
    /// frame for the keys has them; `unmade`, whether that frame left a
    /// scroll ahead of Neovim unmade whose key Neovim has taken.
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
        self.typed.rebase(ack.pos.rest);
        self.base = Some(Base { seq: k, ack });
        self.queue.retain(|s| s.seq > k);
    }

    /// Where the client would have put the cursor and the view for `keys`,
    /// typed from where `from` says they were: `None` where it would not
    /// have followed them — a key it does not, or something maps; lines it
    /// does not have as they are, or whose characters it may not step over
    /// as Neovim does ([`ordinary`]); an edit, or another window or buffer, by
    /// the ack `to` for them.
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
        let text = |l: i64| {
            (!self.cut(win, l))
                .then(|| self.text(win, l))
                .flatten()
                .filter(|t| ordinary(t))
        };
        let cx = moves::Ctx {
            shape,
            line_count: from.line_count,
            rows,
            width,
            text: &text,
            whichwrap: None,
        };
        motions
            .into_iter()
            .try_fold(from.pos, |at, (m, count)| moves::moved(m, count, &at, &cx))
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
        let mapped = mapping(win.and_then(|w| self.shapes.get(&w)));
        self.typed.key(key, mapped)
    }

    /// A key sent while window `win` had the cursor, made out again from
    /// Neovim's last word on the keys before it: see [`Typed::step`].
    pub fn remake(&mut self, win: Option<i64>, key: &str) -> Option<Command> {
        let mapped = mapping(win.and_then(|w| self.shapes.get(&w)));
        self.typed.step(key, mapped)
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
    /// editor's highlighting gives it, as the agent sent them: `None` where
    /// it has not, or where the line was sent cut short and the client cannot
    /// say how `'listchars'` draws the blanks it has (see
    /// [`Shape::unsure`]).
    pub fn coloured(&self, win: i64, lnum: i64) -> Option<(&str, &[Span])> {
        let shape = self.shapes.get(&win)?;
        let lines = self.lines.get(&win)?;
        let line = lines.line(shape.buf, lnum)?;
        if lines.cut(shape.buf, lnum) && shape.unsure(&line.text) {
            return None;
        }
        Some((line.text.as_str(), line.spans.as_slice()))
    }

    /// Buffer line `lnum` of window `win` as a row of it would draw it — see
    /// [`render`] — or `None` where the client cannot say: a line the agent
    /// has not sent, or one [`Predictor::coloured`] has none for. Its text is
    /// in the colours the agent sent with it.
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

/// Whether something maps a key, as a window of `shape` has it, in Insert
/// mode if asked about that (see [`Typed::key`]): nothing, for a window the
/// agent has said nothing of.
fn mapping(shape: Option<&Shape>) -> impl Fn(bool, &str) -> bool + '_ {
    move |insert, k| {
        shape.is_some_and(|s| {
            let list = if insert { &s.imapped } else { &s.mapped };
            list.iter().any(|m| m == k)
        })
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

    /// A window that does not wrap, `'list'` set and `'listchars'` as
    /// Neovim's option has it.
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

    /// A line as a row of a window of `shape` `width` cells wide draws it,
    /// and its colours a letter a cell — `.` the window's own, `n` NonText,
    /// `s` SpecialKey, `w` Whitespace — less the blanks the row ends in.
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

    /// Spaces and no-break spaces as Neovim 0.12 drew them, twenty columns
    /// wide, with each `'listchars'` given: whatever stands for them in
    /// `Whitespace`, and those it leaves alone as they are.
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

    /// A run of spaces goes through `multispace` or `leadmultispace` from the
    /// line's start, whatever the view shows of it, as Neovim 0.12 drew it:
    /// from column 2 with `multispace:abc`, and from 3 with
    /// `leadmultispace:123,multispace:ab` and `precedes:<` — a tab, or any
    /// other character, starting it again. A space with a combining mark on
    /// it is drawn as it is, and keeps the run going; a no-break space with
    /// one is drawn as it is.
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
        // The space after a mark is in the run by the one after it.
        let s = listing("multispace:abcde");
        assert_eq!(
            seen("x  \u{301} \u{301}  y", &s, 20),
            pair("xa \u{301} \u{301}bcy", ".w..ww.")
        );
    }

    /// A tab before a line's first other character is drawn with `leadtab`,
    /// as Neovim 0.12 drew `leadtab:|.-` and `leadtab:|.`, from the line's
    /// start and from column 3. Where that is all `'listchars'` has for
    /// blanks, Neovim draws a space before that character in `Whitespace`
    /// only where it looks at the line's characters for some highlighting of
    /// its own: for the window's, or where the line has colours of its own.
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

    /// The end of a line and the edges of a view that does not wrap, as
    /// Neovim 0.12 drew them twenty columns wide: `eol` after the line, or in
    /// the first column scrolled past it; `extends` in the last column where
    /// more of the line follows — even only its end, or the rest of a tab or
    /// a `^A` — but not on the half of a character two cells wide; and
    /// `precedes` in the first column scrolled sideways, whatever is there,
    /// the other half of a character two cells wide a `<`. All in
    /// `NonText`.
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

    /// A line's end where `'listchars'` shows it takes a cell: in a window
    /// that wraps, a line as wide as the window takes two rows of it, as
    /// Neovim 0.12 drew it.
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

    /// Moved sideways, the first and last columns of both views are drawn
    /// afresh where `'listchars'` draws something of its own at the edges:
    /// `precedes` and `extends` stay where they are, and the end of a line
    /// scrolled past is not kept where it was drawn.
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

    /// A line the agent sent cut short is not drawn where `'listchars'`
    /// draws its blanks by what was cut off: blanks the text it has ends in,
    /// which may be the line's last, where `trail` is set; and a text all
    /// blanks, where what draws those before the first other character is.
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

    /// The agent sends `'listchars'` but its `tab` in a list of its own, and
    /// whether Neovim looks at every character for highlighting of its own.
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

    /// The round trip and how much it varies, as TCP measures its own: the
    /// first sample is the round trip and half of it the variation, and each
    /// after moves them an eighth and a quarter of the way.
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

    /// A prediction waits two round trips and a lost segment, and longer on
    /// a link whose round trip varies by more than [`GRACE`] a quarter.
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

    /// Where the agent says it has the cursor and the view of window 1000,
    /// in Normal mode at the first column of line 0, having taken `count`
    /// keys, the last of them `tail`.
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
        }
    }

    /// Send `keys` after input `seq`, each numbered on from it.
    fn send(s: &mut Sync, seq: &mut u64, keys: &str) {
        for key in tokens(keys) {
            *seq += 1;
            s.sent(*seq, Some(key));
        }
    }

    /// Counts tied by a fence with nothing after it, at input `seq`, the
    /// agent having counted `taken`.
    fn tied(seq: u64, taken: i64) -> Sync {
        let mut s = Sync::default();
        assert_eq!(s.answered(s.fenced(seq), seq, taken), Answer::Tied);
        s
    }

    /// An ack as the agent sends it, every field read: lines from nought,
    /// the column in bytes, and -1 for the end of the line; the modes it
    /// waits in told from those it is busy in.
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

    /// Visual mode, as an ack says it and as the editor draws it, is a rest
    /// of its own: one that keys and the wheel scroll in as predicted, as in
    /// Normal mode, and that no motion is drawn ahead in.
    #[test]
    fn visual_mode_is_a_rest_of_its_own() {
        for mode in ["v", "V", "\x16"] {
            assert_eq!(Rest::of(mode), Rest::Visual, "{mode:?}");
        }
        assert_eq!(Rest::drawn("visual"), Rest::Visual);
        assert_eq!(Rest::of("vs"), Rest::Busy, "Visual by way of Select mode");
    }

    /// Each key the client sends is a key as the agent counts what Neovim's
    /// `keytrans` writes of it, one for one, and the same key by the check
    /// the tail is put to — as Neovim 0.12.6 was found to count them, an
    /// `on_key` counting what `nvim_input` sent it.
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

    /// A fence answered with nothing sent after it ties the counts there:
    /// the keys after it are numbered from the agent's count on.
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

    /// Keys sent after a fence may be taken before Neovim answers it: the
    /// answer is checked, anywhere from the fence to the last key, and ties
    /// nothing.
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

    /// A fence whose count is not between it and the last key sent puts the
    /// counts in doubt, and the next fence with nothing after it ties them
    /// again.
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

    /// Neovim cannot have taken more keys than were sent.
    #[test]
    fn an_ack_past_what_was_sent_is_doubted() {
        let mut seq = 0;
        let mut s = tied(0, 0);
        send(&mut s, &mut seq, "j");
        assert_eq!(s.heard(&ack(2, "jj"), seq), None);
        assert!(!s.synced());
    }

    /// The last keys Neovim says it took must be the last sent under the
    /// same numbers.
    #[test]
    fn a_tail_that_differs_is_doubted() {
        let mut seq = 0;
        let mut s = tied(0, 0);
        send(&mut s, &mut seq, "jk");
        assert_eq!(s.heard(&ack(2, "jj"), seq), None);
        assert!(!s.synced());
    }

    /// A mouse report is whatever Neovim made of it.
    #[test]
    fn a_click_in_the_tail_matches_anything() {
        let mut seq = 0;
        let mut s = tied(0, 0);
        seq += 1;
        s.sent(seq, None);
        send(&mut s, &mut seq, "j");
        assert_eq!(s.heard(&ack(2, "<LeftMouse>j"), seq), Some(2));
    }

    /// A chord is spelt one way by the client and another by `keytrans`,
    /// and the blanks and `<` by their names: none of it is a difference.
    #[test]
    fn modifiers_are_not_compared_by_spelling() {
        let mut seq = 0;
        let mut s = tied(0, 0);
        send(&mut s, &mut seq, "<C-d> <lt><C-i>");
        assert_eq!(s.heard(&ack(4, "<C-D><Space><lt><Tab>"), seq), Some(4));
    }

    /// `<C-c>`, which Neovim counts twice, and a key with Alt, which it may,
    /// put the counts in doubt as they are sent — till a fence with nothing
    /// after it ties them again.
    #[test]
    fn ctrl_c_puts_the_count_in_doubt_until_a_quiet_fence() {
        let mut p = Predictor {
            keys: PredictKeys::Motions,
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

    /// A paste is no key to Neovim in Normal, Visual or Insert mode: what
    /// the agent says after one is not taken for a key of the client's till
    /// a fence asked after the paste has been answered — nor is one asked
    /// before it taken to tie the counts.
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

    /// On the command line Neovim takes a paste as keys typed, and takes
    /// them only after answering a fence asked with the paste: the counts
    /// are in doubt, and tied again by no fence but one asked after the next
    /// key — none of which is asked for till then.
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

    /// A key that a mapping makes nothing of — `<Nop>` — reaches the agent
    /// only with the key after it. A fence answered between the two finds
    /// the count before it, the last key counted not the last sent: it ties
    /// nothing, and none is asked for till a key after, with which the
    /// counts are tied where they were.
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

    /// A tie that moves the offset — after `<C-c>`, counted twice, or a key
    /// with Alt, taken as `<Esc>` and the key — leaves the keys sent before
    /// it numbered as they were, and the last keys Neovim says it took are
    /// checked against those sent after it only. One that leaves the offset
    /// where it was leaves those before it to check by.
    #[test]
    fn a_tie_that_moves_the_offset_checks_no_key_before_it() {
        for (sent, taken) in [("xk<C-c>", "xk<C-C><C-C>"), ("xk<M-j>", "xk<Esc>j")] {
            let mut seq = 0;
            let mut s = tied(0, 0);
            send(&mut s, &mut seq, sent);
            s.doubt("a key Neovim may count as two");
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

    /// Counts past any reckoning — none an agent could send, but the wire
    /// is the wire — put the counts in doubt, where adding them up would
    /// overflow.
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

    /// A fence that finds no count is asked before the agent is there; one
    /// that finds nought, just after.
    #[test]
    fn minus_one_is_no_agent_and_nought_is_an_agent() {
        let mut s = Sync::default();
        let at = s.fenced(0);
        assert_eq!(s.answered(at, 0, -1), Answer::NoAgent);
        assert!(!s.synced());
        assert_eq!(s.answered(at, 0, 0), Answer::Tied);
        assert!(s.synced());
    }

    /// A [`Typed`] that goes by what the agent says Neovim made of each key,
    /// rested in Normal mode by an ack covering input 0.
    fn acked() -> Typed {
        let mut t = Typed::default();
        t.set_keys(PredictKeys::Motions);
        t.rebase(Rest::Normal);
        t
    }

    /// Where the agent says what Neovim made of each key, a key the client
    /// predicts leaves it as sure of the next as it was: a held `<C-e>`
    /// stays predicted, however far ahead of Neovim it gets, and no fence is
    /// wanted for it. Told only to scroll, the client follows the keys as
    /// ever.
    #[test]
    fn predicted_keys_never_make_the_client_unsure() {
        let mut t = acked();
        for _ in 0..30 {
            assert_eq!(typed(&mut t, "<C-e>"), [Command::Lines(1)]);
            assert!(t.followed());
        }
        assert!(!t.wants_fence());
        let mut p = Predictor::default();
        p.set_keys(PredictKeys::Motions);
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

    /// A key the client cannot follow leaves it blind from that key on —
    /// a scroll after it is made out, but nothing is predicted — till an ack
    /// covers it; and then the keys after it are made out again from where
    /// the ack says Neovim is, blind again from the next it cannot follow.
    #[test]
    fn a_key_left_to_neovim_is_caught_up_with_at_its_own_ack() {
        let mut t = acked();
        typed(&mut t, "x");
        assert!(!t.followed(), "blind from `x`");
        assert_eq!(typed(&mut t, "<C-e>"), [Command::Lines(1)]);
        assert!(!t.followed());
        assert!(t.wants_fence(), "a fence after it, for counts and all");
        // An ack covering an input before `x`: made out again, `x` blinds
        // the client again, and the scroll is made out after it.
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
        // Made out again, a key is not sent again: no more fences for it.
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

    /// An ack puts the client where it says Neovim rests: waiting for a
    /// command in Normal or Visual mode, for what is typed in Insert mode,
    /// or busy with something the client cannot make out — an operator
    /// pending, the command line — from which nothing is followed.
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

    /// Inputs are kept for judging the acks by only while acks come — the
    /// link slow enough — and no more of them than Neovim could be behind
    /// on: past that, it is not keeping up, or not saying, and the counts
    /// are in doubt.
    #[test]
    fn a_queue_neovim_never_answers_is_let_go() {
        let t0 = Instant::now();
        let mut p = Predictor {
            keys: PredictKeys::Motions,
            sync: tied(0, 0),
            ..Predictor::default()
        };
        p.sent(1, Some("j"), t0);
        assert!(p.queue.is_empty(), "a link too fast for acks");
        p.sample(Duration::from_millis(100));
        for seq in 2..2 + QUEUE as u64 {
            p.sent(seq, Some("j"), t0);
        }
        assert_eq!(p.queue.len(), QUEUE);
        assert!(p.sync.synced());
        p.sent(2 + QUEUE as u64, Some("j"), t0);
        assert_eq!(p.queue.len(), 1);
        assert!(!p.sync.synced());
        let mut scrolls = Predictor::default();
        scrolls.sample(Duration::from_millis(100));
        scrolls.sent(1, Some("j"), t0);
        assert!(scrolls.queue.is_empty(), "no acks, nothing kept");
    }

    /// A key Neovim takes longer to answer than a round trip, its typeahead
    /// held up behind others, keeps a scroll ahead waiting as long; timed
    /// once, from its going, however often an ack says it again.
    #[test]
    fn a_scroll_waits_as_long_as_neovim_takes_to_answer_keys() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut p = Predictor {
            keys: PredictKeys::Motions,
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
        p.keys = PredictKeys::Scrolls;
        assert_eq!(p.deadline(t0) - t0, round, "told not to count any more");
    }

    /// A key that leaves Neovim waiting for another — a prefix of something
    /// mapped, which it may wait `'timeoutlen'` on, or `f`, which waits for
    /// the user — is answered as late as that lets it be: it is not timed,
    /// and the key that ends the wait is.
    #[test]
    fn a_key_that_waits_for_another_is_not_timed() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut p = Predictor {
            keys: PredictKeys::Motions,
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

    /// The lines whose characters Neovim 0.12.6 was found to step over
    /// otherwise than [`chars`] does, or to show otherwise, are not
    /// ordinary; those it steps over alike are.
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
        ] {
            assert!(!ordinary(text), "{text:?}");
        }
    }
}
