//! Predicts where Normal-mode motions take the cursor, matching Neovim 0.12,
//! from the position Neovim last reported (see [`super::predict::Ack`]).
//!
//! Followed, each with a count: `h` `l` and kin (wrapping lines where
//! `'whichwrap'` allows, see [`wrapped`]); `0` `^` `$` `g_` `|` `<Home>`;
//! `j` `k` and kin, keeping the wanted column (`curswant`); `+` `-` `_`;
//! word motions (see [`super::words`]); `f` `t` `F` `T`; `}` `{` (see
//! [`paragraph`]); `H` `M` `L` (see [`window`]); `gg` `G`. A motion that
//! cannot move leaves the wanted column as it was.
//!
//! After a sideways move the cursor wants the column it is drawn in (a
//! character's first, or a tab's last). Moving past `'scrolloff'` scrolls as
//! Neovim's `update_topline` does: just far enough for a short move, centring
//! the cursor for a long one (see [`keep_in_view`]). Views that do not wrap
//! scroll sideways per [`predict::showing`].
//!
//! The client draws predicted positions at once (see `super::anim::chain`)
//! and checks each against Neovim's (see `super::App` and
//! [`super::predict::Predictor::judge`]).

use super::predict::{self, Ch, Edge, Pos, Rest, Shape, Want};
use super::typing;
use super::words::{self, Way};

/// A Normal-mode motion the client follows; see [`moved`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// `h`, `<Left>`, `<BS>`, `<C-h>`; the `'whichwrap'` letter that lets
    /// each wrap to the line before: `h`, `<`, `b`, `b`.
    Left(char),
    /// `l`, `<Right>`, `<Space>`; `'whichwrap'` letter `l`, `>` or `s`.
    Right(char),
    /// `j`, `<Down>`, `<C-j>`, `<C-n>`; `k`, `<Up>`, `<C-p>`.
    Down,
    Up,
    /// `0`.
    Start,
    /// `^`: first non-blank, or the last character of an all-blank line.
    First,
    /// `$`, `<End>`: the last character, wanting the end of every line; with
    /// a count, on the line count - 1 down.
    End,
    /// `g_`: last non-blank, or the first character of an all-blank line;
    /// with a count, on the line count - 1 down.
    LastNonBlank,
    /// `|`: the character over the counted display column, from 1.
    Column,
    /// `<Home>`: `1|`; Neovim reads any count and drops it.
    Home,
    /// To the first non-blank: `+`, `<CR>` a line down, `-` up, `_` (`here`)
    /// count - 1 down.
    LineFirst {
        down: bool,
        here: bool,
    },
    /// `w`, `<S-Right>`, `b`, `<S-Left>`, `e`, `ge`; with `big`, `W`,
    /// `<C-Right>`, `B`, `<C-Left>`, `E`, `gE` (words split only by blanks).
    Word {
        way: Way,
        big: bool,
    },
    /// `f`, `t`, `F`, `T` and the character typed after: to the counted `c`
    /// along the line, or `back`; `till` stops one character short.
    Find {
        c: char,
        till: bool,
        back: bool,
    },
    /// `}`, or `{` if `back`; see [`paragraph`].
    Paragraph {
        back: bool,
    },
    /// `H`, `M`, `L`, or with a count that many lines in from the top or
    /// bottom; see [`window`].
    Window(Edge),
    /// `gg`, `G`: to the counted line (from 1), else the first or `last`.
    Goto {
        last: bool,
    },
}

/// The motion `key` makes in Normal mode, if followed here. Motions after
/// `g` are [`g_motion`]'s.
pub fn motion(key: &str) -> Option<Motion> {
    Some(match key {
        "h" => Motion::Left('h'),
        "<Left>" => Motion::Left('<'),
        "<BS>" | "<C-h>" => Motion::Left('b'),
        "l" => Motion::Right('l'),
        "<Right>" => Motion::Right('>'),
        " " | "<Space>" => Motion::Right('s'),
        "j" | "<Down>" | "<C-j>" | "<C-n>" => Motion::Down,
        "k" | "<Up>" | "<C-p>" => Motion::Up,
        "0" => Motion::Start,
        "<Home>" => Motion::Home,
        "^" => Motion::First,
        "$" | "<End>" => Motion::End,
        "|" => Motion::Column,
        "+" | "<CR>" => Motion::LineFirst {
            down: true,
            here: false,
        },
        "-" => Motion::LineFirst {
            down: false,
            here: false,
        },
        "_" => Motion::LineFirst {
            down: true,
            here: true,
        },
        "w" | "<S-Right>" | "W" | "<C-Right>" => Motion::Word {
            way: Way::Next,
            big: matches!(key, "W" | "<C-Right>"),
        },
        "b" | "<S-Left>" | "B" | "<C-Left>" => Motion::Word {
            way: Way::Back,
            big: matches!(key, "B" | "<C-Left>"),
        },
        "e" | "E" => Motion::Word {
            way: Way::End,
            big: key == "E",
        },
        "}" | "{" => Motion::Paragraph { back: key == "{" },
        "H" => Motion::Window(Edge::Top),
        "M" => Motion::Window(Edge::Middle),
        "L" => Motion::Window(Edge::Bottom),
        "G" => Motion::Goto { last: true },
        _ => return None,
    })
}

/// The motion `g` then `key` makes: `g_`, `ge`, `gE`, `gg`.
pub fn g_motion(key: &str) -> Option<Motion> {
    Some(match key {
        "_" => Motion::LastNonBlank,
        "g" => Motion::Goto { last: false },
        "e" | "E" => Motion::Word {
            way: Way::BackEnd,
            big: key == "E",
        },
        _ => return None,
    })
}

/// For `f`, `t`, `F`, `T`, which wait for a character: whether it stops
/// `till` the character, and looks `back`.
pub fn finds(key: &str) -> Option<(bool, bool)> {
    Some(match key {
        "f" => (false, false),
        "t" => (true, false),
        "F" => (false, true),
        "T" => (true, true),
        _ => return None,
    })
}

/// The character `key`, typed after `f` and kin, looks for, mappings aside:
/// a plain character other than a control character or combining mark, or
/// `<Space>`, `<lt>`, `<Bar>`, `<Bslash>`, `<Tab>`. `None` for others, which
/// Neovim may treat otherwise (`<C-k>` digraph, `<Esc>` to cancel).
pub fn sought(key: &str) -> Option<char> {
    if key.eq_ignore_ascii_case("<Tab>") {
        return Some('\t');
    }
    match typing::Edit::of(key)? {
        typing::Edit::Char(c) => (!predict::joins(c)).then_some(c),
        _ => None,
    }
}

/// Neovim's largest count; a further digit leaves it unchanged.
const MAX_COUNT: i64 = 999_999_999;

/// The motions, each with its count, made by keys typed in Normal mode with
/// nothing pending. `None` if any key is not a followed motion, `mapped` says
/// a key is mapped or starts a mapping, or the keys end in a count, `g`, or
/// an `f`-like key.
pub fn parse<'a>(
    keys: impl IntoIterator<Item = &'a str>,
    mapped: impl Fn(&str) -> bool,
) -> Option<Vec<(Motion, Option<i64>)>> {
    let mut out = Vec::new();
    let mut count: Option<i64> = None;
    let mut g = false;
    let mut find = None;
    for key in keys {
        // Only `:lmap` mappings can change the character `f` and kin look
        // for (see `Ctx::lmap`).
        if let Some((till, back)) = find.take() {
            let c = sought(key)?;
            out.push((Motion::Find { c, till, back }, count.take()));
            continue;
        }
        // After `g`, look up `g{key}` whole: Neovim's own `g…` mappings make
        // `g` alone a prefix.
        if std::mem::take(&mut g) {
            if mapped(&format!("g{key}")) {
                return None;
            }
            out.push((g_motion(key)?, count.take()));
            continue;
        }
        if key == "g" {
            g = true;
            continue;
        }
        if mapped(key) {
            return None;
        }
        if let Some(f) = finds(key) {
            find = Some(f);
            continue;
        }
        let digit = key
            .parse::<i64>()
            .ok()
            .filter(|d| key.len() == 1 && (*d > 0 || count.is_some()));
        if let Some(d) = digit {
            let n = count.unwrap_or(0);
            count = Some(if n >= MAX_COUNT / 10 {
                MAX_COUNT
            } else {
                n * 10 + d
            });
            continue;
        }
        out.push((motion(key)?, count.take()));
    }
    (count.is_none() && !g && find.is_none()).then_some(out)
}

/// A window and its lines, for working out a motion.
pub struct Ctx<'a> {
    pub shape: &'a Shape,
    pub line_count: i64,
    /// The window's text rows, and its columns, gutter included.
    pub rows: i64,
    pub width: usize,
    /// A line's text (0-based), where the client has it whole.
    pub text: &'a dyn Fn(i64) -> Option<&'a str>,
    /// `'whichwrap'`, if known (the agent reports it only along with what
    /// Neovim did with each key). Unknown, a sideways move past the line's
    /// end is not followed.
    pub whichwrap: Option<&'a str>,
    /// `'scrolljump'` and `'smoothscroll'`; unknown, a motion that moves the
    /// view is not followed.
    pub scrolling: Option<Scrolling>,
    /// `'iskeyword'`'s class of each character below 256 (see
    /// [`words::class`]); unknown, word motions are not followed.
    pub keyword: Option<&'a [u8]>,
    /// Whether `:lmap` mappings may change the character after `f` and kin
    /// (`'iminsert'` 1). If they may, or this is unknown, finds are not
    /// followed.
    pub lmap: Option<bool>,
    /// The macros `'sections'` and `'paragraphs'` name; unknown, paragraph
    /// motions are not followed.
    pub macros: Option<Macros<'a>>,
}

impl Ctx<'_> {
    /// The top line of a view at `top` once the cursor goes to `line`:
    /// unchanged while `'scrolloff'` holds (buffer edges aside), else
    /// scrolled as Neovim does (see [`keep_in_view`]), with one line per row.
    /// `None` if the view moves and scrolling options are unknown, or a line
    /// shown or consulted is too long for a row or unknown to the client.
    fn view(&self, top: i64, line: i64) -> Option<i64> {
        let (h, so) = (self.rows, self.shape.scrolloff as i64);
        let one_row =
            |l: i64| (self.text)(l).is_some_and(|t| predict::fits(t, self.shape, self.width));
        let how = self.scrolling.unwrap_or_default();
        let to = keep_in_view((top, line), (h, self.line_count - 1), so, how, &one_row)?;
        if to != top && self.scrolling.is_none() {
            return None;
        }
        let mut shown = to..to.saturating_add(h).min(self.line_count);
        (shown.contains(&line) && shown.all(one_row)).then_some(to)
    }

    /// The left column of a non-wrapping view of `text` once the cursor is on
    /// `ch`, previously shown from `left`; see [`predict::showing`].
    fn left(&self, text: &str, ch: Option<&Ch>, left: usize) -> usize {
        let vcol = ch.map_or(0, |ch| ch.at(self.shape));
        predict::showing(text, self.shape, vcol, left, self.width)
    }

    /// Whether `'whichwrap'` has the key's letter `ww`, letting a sideways
    /// move cross the line's end; `None` if unknown.
    fn wraps(&self, ww: char) -> Option<bool> {
        Some(self.whichwrap?.split(',').any(|f| f.starts_with(ww)))
    }
}

/// Where `m` with `count` takes the cursor from `at` in `cx`, as Neovim 0.12
/// does, including any vertical scroll (see [`Ctx::view`]). `None` where the
/// client cannot say.
pub fn moved(m: Motion, count: Option<i64>, at: &Pos, cx: &Ctx) -> Option<Pos> {
    if at.rest != Rest::Normal || cx.view(at.top, at.line) != Some(at.top) {
        return None;
    }
    let shape = cx.shape;
    let last = (cx.line_count - 1).max(0);
    let n = count.unwrap_or(1).max(1);
    let text = (cx.text)(at.line)?;
    let cs = predict::chars(text, shape);
    // The cursor's character: the one its byte is in.
    let i = cs.iter().rposition(|ch| ch.byte <= at.byte).unwrap_or(0);
    // To character `j` of this line, wanting its drawn column or `want`.
    let sideways = |j: Option<usize>, want: Option<Want>| {
        let ch = j.and_then(|j| cs.get(j));
        Pos {
            byte: ch.map_or(0, |ch| ch.byte),
            want: want.unwrap_or_else(|| Want::Col(ch.map_or(0, |ch| ch.at(shape)))),
            left: cx.left(text, ch, at.left),
            ..*at
        }
    };
    // To the character `pick` finds on `line`, wanting its drawn column or
    // `want`, scrolling the view.
    let down = |line: i64, pick: &dyn Fn(&[Ch]) -> Option<usize>, want: Option<Want>| {
        put(line, cx.view(at.top, line)?, pick, want, at, cx)
    };
    // Neovim's `beginline(BL_SOL | BL_FIX)` for `H`, `G` and kin: the first
    // non-blank with `'startofline'`, else the wanted column as `j` uses.
    let begin = |line: i64| {
        if shape.startofline {
            down(line, &first_non_blank, None)
        } else {
            let want = at.want.col();
            down(line, &|cs| over(cs, want), Some(at.want))
        }
    };
    // `g_`: last non-blank, or the first of an all-blank line.
    let last_non_blank = |cs: &[Ch]| {
        cs.iter()
            .rposition(|ch| !matches!(ch.c, ' ' | '\t'))
            .or((!cs.is_empty()).then_some(0))
    };
    match m {
        Motion::Left(ww) | Motion::Right(ww) => {
            let left = matches!(m, Motion::Left(_));
            let room = if left {
                i
            } else {
                cs.len().saturating_sub(i + 1)
            };
            let n = usize::try_from(n).unwrap_or(usize::MAX);
            if n > room && cx.wraps(ww)? {
                return wrapped(left, n, at, cx);
            }
            match n.min(room) {
                // Nowhere to go: the wanted column is unchanged.
                0 => Some(*at),
                n => Some(sideways(Some(if left { i - n } else { i + n }), None)),
            }
        }
        Motion::Down | Motion::Up => {
            let line = if m == Motion::Down {
                at.line.saturating_add(n)
            } else {
                at.line.saturating_sub(n)
            }
            .clamp(0, last);
            if line == at.line {
                return Some(*at);
            }
            let want = at.want.col();
            down(line, &|cs| over(cs, want), Some(at.want))
        }
        Motion::Start | Motion::First if count.is_some() => None,
        Motion::Start => Some(sideways(Some(0), None)),
        Motion::First => Some(sideways(first_non_blank(&cs), None)),
        // With a count, on the line count - 1 down, or the last. From the
        // last line the cursor stays put but still wants the line end, as
        // Neovim sets that before finding it cannot go down.
        Motion::End | Motion::LastNonBlank if n > 1 => {
            if at.line == last {
                return Some(Pos {
                    want: Want::End,
                    ..*at
                });
            }
            let line = at.line.saturating_add(n - 1).min(last);
            match m {
                Motion::End => down(line, &|cs| cs.len().checked_sub(1), Some(Want::End)),
                _ => down(line, &last_non_blank, None),
            }
        }
        Motion::End => Some(sideways(cs.len().checked_sub(1), Some(Want::End))),
        Motion::LastNonBlank => Some(sideways(last_non_blank(&cs), None)),
        Motion::Column | Motion::Home => {
            let vcol = match m {
                Motion::Home => 0,
                _ => usize::try_from(n - 1).unwrap_or(usize::MAX),
            };
            Some(sideways(over(&cs, vcol), Some(Want::Col(vcol))))
        }
        Motion::LineFirst {
            down: forward,
            here,
        } => {
            let by = if here { n - 1 } else { n };
            let line = if forward {
                at.line.saturating_add(by)
            } else {
                at.line.saturating_sub(by)
            }
            .clamp(0, last);
            // Asked to go and going nowhere: Neovim moves nothing.
            if by > 0 && line == at.line {
                return Some(*at);
            }
            down(line, &first_non_blank, None)
        }
        Motion::Word { way, big } => {
            let lines = words::Lines {
                text: cx.text,
                count: cx.line_count,
                keyword: cx.keyword?,
            };
            let (line, byte) = words::moved(way, big, n, (at.line, at.byte), &lines)?;
            let on = |cs: &[Ch]| cs.iter().position(|ch| ch.byte == byte);
            if line == at.line {
                return Some(sideways(on(&cs), None));
            }
            down(line, &on, None)
        }
        Motion::Find { c, till, back } => {
            if cx.lmap? {
                return None;
            }
            let n = usize::try_from(n).unwrap_or(usize::MAX);
            match find(text, at.byte, c, (till, back), n) {
                // Fewer matches than counted: no move, wanted column kept.
                None => Some(*at),
                Some(byte) => Some(sideways(cs.iter().position(|ch| ch.byte == byte), None)),
            }
        }
        Motion::Paragraph { back } => {
            let macros = cx.macros?;
            let found = paragraph(back, n, at.line, cx.line_count, cx.text, macros)?;
            // Hit the buffer's edge before the last count: no move, but the
            // cursor wants its drawn column, as `nv_findpar` sets that first.
            let Some((line, byte)) = found else {
                return Some(sideways(Some(i), None));
            };
            let on = |cs: &[Ch]| Some(cs.iter().position(|ch| ch.byte == byte).unwrap_or(0));
            if line == at.line {
                return Some(sideways(on(&cs), None));
            }
            down(line, &on, None)
        }
        Motion::Window(edge) => {
            let so = shape.scrolloff as i64;
            begin(window(edge, n, at.top, (cx.rows, so), cx.line_count))
        }
        Motion::Goto { last: to_last } => begin(match count {
            Some(n) => (n - 1).clamp(0, last),
            None if to_last => last,
            None => 0,
        }),
    }
}

/// The line `H`, `M` or `L` (`edge`) with count `n` takes the cursor to, in
/// a view from `top`, `rows` tall at a line per row, over `count` lines, as
/// Neovim's `nv_scroll` then `cursor_correct` do: `H` the n-th line from the
/// top, `L` from the bottom (the first line if counted past it), `M` halfway
/// down the lines shown; then `'scrolloff'` (`so`) in from the view's edges,
/// except the buffer's own. The view never moves.
pub fn window(edge: Edge, n: i64, top: i64, (rows, so): (i64, i64), count: i64) -> i64 {
    let n = n.max(1);
    // One past the last line shown.
    let shown = (top + rows).min(count);
    let line = match edge {
        Edge::Top => top.saturating_add(n - 1).min(count - 1),
        Edge::Bottom if n > shown => 0,
        Edge::Bottom => shown - n,
        Edge::Middle => {
            let empty = (rows - (count - top)).max(0);
            let half = (rows - empty + 1) / 2;
            let (mut k, mut used) = (0, 0);
            while top + 1 + k < count {
                used += 1;
                if used >= half {
                    break;
                }
                k += 1;
            }
            top + k
        }
    };
    // Neovim's `cursor_correct`: no `'scrolloff'` at the buffer's start or
    // end when shown, and then at most half the view on the other side.
    let last_shown = top + rows >= count;
    let mut above_wanted = if top == 0 { 0 } else { so };
    let mut below_wanted = if top == 0 { so.min(rows / 2) } else { so };
    if last_shown {
        below_wanted = 0;
        above_wanted = above_wanted.min((rows - 1) / 2);
    }
    if line >= top + above_wanted && line < shown - below_wanted {
        return line;
    }
    let (mut t, mut b) = (top, shown - 1);
    let (mut above, mut below) = (0, 0);
    while (above < above_wanted || below < below_wanted) && t < b {
        if below < below_wanted && (below <= above || above >= above_wanted) {
            below += 1;
            b -= 1;
        } else {
            above += 1;
            t += 1;
        }
    }
    if t == b || b < 0 {
        t
    } else if t > b {
        b
    } else if line < t && top > 0 {
        t
    } else if line > b && !last_shown {
        b
    } else {
        line
    }
}

/// Where `f` (`t` if `till`, `F`/`T` if `back`) for the n-th `c` takes the
/// cursor from `byte` of `text`, as Neovim's `searchc` does: the landing
/// character's first byte, or `None` if fewer are found. A combining mark
/// goes with its base character.
pub fn find(
    text: &str,
    byte: usize,
    c: char,
    (till, back): (bool, bool),
    n: usize,
) -> Option<usize> {
    let cs: Vec<(usize, char)> = text
        .char_indices()
        .filter(|&(_, c)| !predict::joins(c))
        .collect();
    let i = cs.iter().rposition(|&(b, _)| b <= byte).unwrap_or(0);
    let n = n.max(1);
    let j = if back {
        (0..i).rev().filter(|&j| cs[j].1 == c).nth(n - 1)
    } else {
        (i + 1..cs.len()).filter(|&j| cs[j].1 == c).nth(n - 1)
    }?;
    let j = match (till, back) {
        (false, _) => j,
        (true, false) => j - 1,
        // To the byte after `c`: the next character's, or a combining mark's
        // on `c`, which Neovim moves back to `c`'s first byte.
        (true, true) if cs[j].0 + c.len_utf8() < cs[j + 1].0 => j,
        (true, true) => j + 1,
    };
    Some(cs[j].0)
}

/// The nroff macros `'sections'` and `'paragraphs'` name, which start a
/// section or paragraph on a line beginning with `.` and the macro.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Macros<'a> {
    pub sections: &'a str,
    pub paragraphs: &'a str,
}

/// Where `}` (`{` if `back`) with count `n` takes the cursor from `line` of
/// `count`, as Neovim's `findpar` does: past `n` paragraphs, each ended by
/// the first starting line (see [`starts`]) after a non-empty one, or to the
/// buffer's first or last line if the last count reaches it. Lands on the
/// first character, or going forward on the buffer's last line, its last.
/// `Some(None)` if an earlier count reaches the edge (no move); `None` if a
/// line passed is not known to the client.
pub fn paragraph<'a>(
    back: bool,
    n: i64,
    line: i64,
    count: i64,
    text: &'a dyn Fn(i64) -> Option<&'a str>,
    macros: Macros,
) -> Option<Option<(i64, usize)>> {
    let step = if back { -1 } else { 1 };
    let mut at = line;
    for left in (0..n.max(1)).rev() {
        let mut skipped = false;
        let mut first = true;
        loop {
            let t = text(at)?;
            skipped |= !t.is_empty();
            if !first && skipped && starts(t, macros)? {
                break;
            }
            first = false;
            at += step;
            if !(0..count).contains(&at) {
                if left > 0 {
                    return Some(None);
                }
                at -= step;
                break;
            }
        }
    }
    // The last character's first byte, combining marks going with their
    // base.
    let byte = if back || at < count - 1 {
        0
    } else {
        text(at)?
            .char_indices()
            .rfind(|&(_, c)| !predict::joins(c))
            .map_or(0, |(b, _)| b)
    };
    Some(Some((at, byte)))
}

/// Whether a line starts a paragraph or section, per Neovim's `startPS` for
/// `{` and `}`: empty, starting with a form feed, or `.` and a macro `macros`
/// names. `None` where Neovim reads past the line's end to say.
fn starts(text: &str, macros: Macros) -> Option<bool> {
    match text.as_bytes() {
        [] | [b'\x0c', ..] => Some(true),
        [b'.', rest @ ..] => Some(named(macros.sections, rest)? || named(macros.paragraphs, rest)?),
        _ => Some(false),
    }
}

/// Whether `s` (a line after its `.`) starts with a macro `opt` names, per
/// Neovim's `inmacro`: two characters each, a blank in `opt` matching a blank
/// or the line's end. `None` where Neovim reads past the end of a lone `.`.
fn named(opt: &str, s: &[u8]) -> Option<bool> {
    let (s0, s1) = (
        s.first().copied().unwrap_or(0),
        s.get(1).copied().unwrap_or(0),
    );
    for pair in opt.as_bytes().chunks(2) {
        let (m0, m1) = (pair[0], pair.get(1).copied().unwrap_or(0));
        if !(m0 == s0 || (m0 == b' ' && (s0 == 0 || s0 == b' '))) {
            continue;
        }
        let ends = m1 == 0 || m1 == b' ';
        if s0 == 0 && !ends {
            return None;
        }
        if m1 == s1 || (ends && (s0 == 0 || s1 == 0 || s1 == b' ')) {
            return Some(true);
        }
    }
    Some(false)
}

/// How a scroll lands the cursor on its new line; see [`onto`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landing {
    /// On the character over the wanted column, as `j` and `k` land: most
    /// scrolls.
    Want,
    /// On the first non-blank, wanting its drawn column: a page or half page
    /// with `'startofline'`, and `z<CR>`, `z.`, `z-`.
    First,
    /// On the character at the old byte, or the line's last before it,
    /// wanting as before: counted `zt`, `zz`, `zb`, as `check_cursor_col`.
    Byte,
}

/// The cursor after a scroll from `at` that puts the view at `top` and the
/// cursor on `line`, landing per `how`, as Neovim 0.12.6 does. `None` if the
/// line is not known whole.
pub fn onto(line: i64, how: Landing, top: i64, at: &Pos, cx: &Ctx) -> Option<Pos> {
    match how {
        Landing::Want => {
            let want = at.want.col();
            put(line, top, &|cs| over(cs, want), Some(at.want), at, cx)
        }
        Landing::First => put(line, top, &first_non_blank, None, at, cx),
        Landing::Byte => {
            let byte = at.byte;
            let pick = |cs: &[Ch]| cs.iter().rposition(|ch| ch.byte <= byte);
            put(line, top, &pick, Some(at.want), at, cx)
        }
    }
}

/// The cursor from `at` on `line` of a view from `top`, on the character
/// `pick` finds, wanting its drawn column or `want`; a view that does not
/// wrap scrolls sideways to show it.
fn put(
    line: i64,
    top: i64,
    pick: &dyn Fn(&[Ch]) -> Option<usize>,
    want: Option<Want>,
    at: &Pos,
    cx: &Ctx,
) -> Option<Pos> {
    let text = (cx.text)(line)?;
    let cs = predict::chars(text, cx.shape);
    let ch = pick(&cs).and_then(|j| cs.get(j));
    Some(Pos {
        line,
        byte: ch.map_or(0, |ch| ch.byte),
        want: want.unwrap_or_else(|| Want::Col(ch.map_or(0, |ch| ch.at(cx.shape)))),
        top,
        left: cx.left(text, ch, at.left),
        ..*at
    })
}

/// The character over display column `vcol`, or the last.
fn over(cs: &[Ch], vcol: usize) -> Option<usize> {
    cs.iter()
        .position(|ch| vcol < ch.vcol + ch.width)
        .or(cs.len().checked_sub(1))
}

/// The first non-blank, or the last character of an all-blank line.
fn first_non_blank(cs: &[Ch]) -> Option<usize> {
    cs.iter()
        .position(|ch| !matches!(ch.c, ' ' | '\t'))
        .or(cs.len().checked_sub(1))
}

/// Where a sideways move of `n` characters (back if `left`) takes the cursor
/// when `'whichwrap'` lets it cross line ends, as Neovim's `nv_left` and
/// `nv_right` do: crossing to the adjacent line is one step, an empty line is
/// a stop of its own, and the buffer's first and last lines are the limit.
/// The cursor then wants its drawn column, or keeps its want if it did not
/// move. `None` if it would pass a line not known whole, or leave the lines
/// where the view stays (see [`Ctx::view`]), which also bounds the walk.
fn wrapped(left: bool, n: usize, at: &Pos, cx: &Ctx) -> Option<Pos> {
    let last = (cx.line_count - 1).max(0);
    let mut line = at.line;
    let mut cs = predict::chars((cx.text)(line)?, cx.shape);
    let mut i = cs.iter().rposition(|ch| ch.byte <= at.byte).unwrap_or(0);
    let mut steps = n;
    loop {
        let room = if left {
            i
        } else {
            cs.len().saturating_sub(i + 1)
        };
        let go = steps.min(room);
        i = if left { i - go } else { i + go };
        steps -= go;
        if steps == 0 || line == if left { 0 } else { last } {
            break;
        }
        line = if left { line - 1 } else { line + 1 };
        if cx.view(at.top, line) != Some(at.top) {
            return None;
        }
        cs = predict::chars((cx.text)(line)?, cx.shape);
        i = if left { cs.len().saturating_sub(1) } else { 0 };
        steps -= 1;
    }
    let ch = cs.get(i);
    let byte = ch.map_or(0, |ch| ch.byte);
    if (line, byte) == (at.line, at.byte) {
        return Some(*at);
    }
    Some(Pos {
        line,
        byte,
        want: Want::Col(ch.map_or(0, |ch| ch.at(cx.shape))),
        left: cx.left((cx.text)(line)?, ch, at.left),
        ..*at
    })
}

/// How Neovim scrolls to keep the cursor in view, besides `'scrolloff'`; see
/// [`keep_in_view`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scrolling {
    /// `'scrolljump'`: the least a motion past `'scrolloff'` scrolls, in
    /// lines or, if negative, a percentage of the window.
    pub jump: i64,
    /// `'smoothscroll'` in a wrapping window: centre by rows, not lines.
    pub smooth: bool,
}

impl Default for Scrolling {
    /// Neovim's defaults.
    fn default() -> Self {
        Self {
            jump: 1,
            smooth: false,
        }
    }
}

/// The top line Neovim 0.12.6's `update_topline` gives a view of `h` rows at
/// `top` once a motion puts the cursor on `cursor` (buffer's last line
/// `last`, `'scrolloff'` `so`, scrolling per `how`). The view stays if the
/// cursor is at least `so` lines from its edges, or at the buffer's own; for a
/// short move it scrolls just far enough, by at least `'scrolljump'`
/// (`scroll_cursor_top`/`scroll_cursor_bot`); for a longer one it centres the
/// cursor (`scroll_cursor_halfway`). Lines must take one row each: `None` if
/// `one_row` says a consulted line does not, or if `so` exceeds half the
/// window (Neovim then keeps the cursor centred by other rules). Checked
/// against Neovim for windows of 4 to 40 rows and every `'scrolloff'` up to
/// half (`tests/client.rs`'s `views_go_where_neovim_takes_them`).
pub fn keep_in_view(
    (top, cursor): (i64, i64),
    (h, last): (i64, i64),
    so: i64,
    how: Scrolling,
    one_row: &dyn Fn(i64) -> bool,
) -> Option<i64> {
    if so > (h - 1) / 2 || h < 1 || last < 0 {
        return None;
    }
    let jump = if how.jump >= 0 {
        how.jump
    } else {
        h.saturating_mul(how.jump.saturating_neg()) / 100
    };
    let view = Topline {
        h,
        n: last.checked_add(1)?,
        so,
        jump,
        smooth: how.smooth,
        one_row,
    };
    Some(view.update(top.checked_add(1)?, cursor.checked_add(1)?)? - 1)
}

/// A window as `update_topline` and its helpers see it: `h` rows over `n`
/// lines numbered from 1, a row each (see [`keep_in_view`]); `'scrolloff'`
/// `so`, and at least `jump` lines per scroll.
struct Topline<'a> {
    h: i64,
    n: i64,
    so: i64,
    jump: i64,
    smooth: bool,
    one_row: &'a dyn Fn(i64) -> bool,
}

impl Topline<'_> {
    /// `Some` if line `l` takes exactly one row.
    fn row(&self, l: i64) -> Option<()> {
        (self.one_row)(l - 1).then_some(())
    }

    /// `w_botline`: the line after the last one shown from `t`.
    fn botline(&self, t: i64) -> Option<i64> {
        let b = t.saturating_add(self.h).min(self.n + 1);
        (t..b).try_for_each(|l| self.row(l))?;
        Some(b)
    }

    /// `update_topline`: the view from `t` once the cursor is on `c`.
    fn update(&self, t: i64, c: i64) -> Option<i64> {
        let (h, so) = (self.h, self.so);
        let mut t = t;
        // Too near the top, or above it: `check_top_offset`, counting the
        // lines in view above the cursor.
        let mut check_topline = false;
        if t > 1 {
            if c < t {
                check_topline = true;
            } else if c < t.saturating_add(so) {
                let (mut k, mut l) = (0, c);
                while k < so {
                    l -= 1;
                    if l < t {
                        break;
                    }
                    self.row(l)?;
                    k += 1;
                }
                check_topline = k < so;
            }
        }
        let mut check_botline = true;
        if check_topline {
            let halfheight = (h / 2 - 1).max(2);
            if t.saturating_add(so) - c >= halfheight {
                t = self.halfway(c, false)?;
                check_botline = false;
            } else {
                t = self.top(t, c)?;
            }
        }
        // Too near the bottom, or below it: fewer than `'scrolloff'` lines
        // shown below the cursor.
        if check_botline {
            let b = self.botline(t)?;
            if b <= self.n {
                let short = if c < b {
                    c >= b - so && {
                        let (mut k, mut l, mut height) = (0, c, 0);
                        while l < b {
                            k += height;
                            if k >= so {
                                break;
                            }
                            l += 1;
                            if l < b {
                                self.row(l)?;
                            }
                            height = 1;
                        }
                        k < so
                    }
                } else {
                    true
                };
                if short {
                    t = if c - b + 1 + so <= h + 1 {
                        self.bot(t, c)?
                    } else {
                        self.halfway(c, false)?
                    };
                }
            }
        }
        Some(t)
    }

    /// `scroll_cursor_top`: scroll up for the cursor on `c`, just enough to
    /// show `'scrolloff'` lines above it (and by at least `'scrolljump'`), or
    /// centre it if the lines wanted around it do not fit.
    fn top(&self, t: i64, c: i64) -> Option<i64> {
        let (h, so, n) = (self.h, self.so, self.n);
        self.row(c)?;
        let mut used = 1;
        let mut scrolled = if c < t { used } else { 0 };
        let (mut top, mut bot) = (c - 1, c + 1);
        let mut new_topline = top + 1;
        let mut extra = 0;
        while top > 0 {
            self.row(top)?;
            if top < t {
                scrolled += 1;
            }
            if (new_topline >= t || scrolled > self.jump) && extra >= so {
                break;
            }
            used += 1;
            if extra < so && bot < n {
                self.row(bot)?;
                used += 1;
            }
            if used > h {
                break;
            }
            extra += 1;
            new_topline = top;
            top -= 1;
            bot += 1;
        }
        if used > h {
            return self.halfway(c, false);
        }
        Some(t.min(new_topline).min(c))
    }

    /// `scroll_cursor_bot`: scroll down for the cursor on `c`, just enough to
    /// show `'scrolloff'` lines below it (and by at least `'scrolljump'`), or
    /// centre it once the lines from the view's bottom to it, and as many
    /// below, would fill the window.
    fn bot(&self, t: i64, c: i64) -> Option<i64> {
        let (h, so, n) = (self.h, self.so, self.n);
        let b = self.botline(t)?;
        // `w_empty_rows`: the rows under the buffer's last line, where the
        // view shows it.
        let empty = if b > n { h - (n - t + 1) } else { 0 };
        self.row(c)?;
        let mut used = 1;
        let mut scrolled = 0;
        if c >= b {
            scrolled = used;
            if c == b {
                scrolled -= empty;
            }
        }
        let (mut above, mut below) = (c, c);
        let mut extra = 0;
        while above > 1 {
            let enough = (scrolled <= 0 || scrolled >= self.jump) && extra >= so;
            if (enough || below + 1 > n) && above <= b {
                break;
            }
            let was = above;
            above -= 1;
            self.row(above)?;
            used += 1;
            if used > h {
                break;
            }
            if above >= b {
                scrolled += 1;
                if above == b && was > b {
                    scrolled -= empty;
                }
            }
            if below < n {
                let was = below;
                below += 1;
                self.row(below)?;
                used += 1;
                if used > h {
                    break;
                }
                if extra < so || scrolled < self.jump {
                    extra += 1;
                    if below >= b {
                        scrolled += 1;
                        if was < b {
                            scrolled -= empty;
                        }
                    }
                }
            }
        }
        // Lines to scroll: those holding the rows past the bottom, or, past
        // them all, Neovim's 9999 for more than it can count.
        let lines = if scrolled <= 0 {
            0
        } else if used > h {
            used
        } else {
            let (mut lines, mut l) = (0, t - 1);
            while lines < scrolled && l < b {
                l += 1;
                self.row(l)?;
                lines += 1;
            }
            if lines < scrolled {
                9999
            } else {
                lines
            }
        };
        if lines >= h && lines > self.jump {
            return self.halfway(c, true);
        }
        // With `'smoothscroll'`, by rows past the bottom rather than lines:
        // equal for one-row lines, except where the count overran the window.
        let by = if self.smooth { scrolled } else { lines };
        // A `'scrolljump'` larger than the window can scroll past the cursor;
        // Neovim's `scrollup` then moves it to the top line and carries on,
        // which is not followed.
        let to = t.saturating_add(by.max(0)).min(n);
        (to <= c).then_some(to)
    }

    /// `scroll_cursor_halfway`: centre the cursor on `c`, adding a line below
    /// and above in turn (above first if `prefer_above`) until the window is
    /// full or the buffer ends; with `'smoothscroll'`, both at once.
    fn halfway(&self, c: i64, prefer_above: bool) -> Option<i64> {
        let (h, n) = (self.h, self.n);
        self.row(c)?;
        let mut used = 1;
        let mut topline = c;
        let mut below = c;
        if self.smooth {
            while topline > 1 {
                let above = topline - 1;
                self.row(above)?;
                used += 1;
                if below < n {
                    below += 1;
                    self.row(below)?;
                    used += 1;
                }
                // Past the window: a line only partly shown, which a one-row
                // line never is.
                if used > h {
                    break;
                }
                topline = above;
            }
            return Some(topline);
        }
        while topline > 1 {
            let (mut up, mut down) = (0, 0);
            for round in 1..=2 {
                let add_below = if prefer_above {
                    round == 2 && down < up
                } else {
                    round == 1 && down <= up
                };
                if add_below {
                    if below < n {
                        below += 1;
                        self.row(below)?;
                        used += 1;
                        if used > h {
                            return Some(topline);
                        }
                    }
                    down += 1;
                }
                let add_above = if prefer_above {
                    round == 1 && down >= up
                } else {
                    round == 1 && down > up
                };
                if add_above {
                    self.row(topline - 1)?;
                    used += 1;
                    if used > h {
                        return Some(topline);
                    }
                    up += 1;
                    topline -= 1;
                }
            }
        }
        Some(topline)
    }
}

/// The display column of the Normal-mode cursor at `byte` of `text`: its
/// character's first, or a tab's last (see [`Ch::at`]); 0 on an empty line.
pub fn drawn(text: &str, shape: &Shape, byte: usize) -> usize {
    let cs = predict::chars(text, shape);
    cs.iter()
        .rfind(|ch| ch.byte <= byte)
        .map_or(0, |ch| ch.at(shape))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::grid::Text;
    use crate::client::predict::Listchars;

    fn shape() -> Shape {
        Shape {
            buf: 1,
            line_count: 13,
            textoff: 0,
            tabstop: 8,
            number: false,
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

    /// The buffer the motions were probed on in Neovim 0.12.6: each fed with
    /// `nvim_feedkeys`, with `winsaveview()` read before and after.
    const LINES: &[&str] = &[
        "hello world",
        "ab",
        "",
        "\tx",
        "a\tb",
        "日本語",
        "e\u{301}x",
        "   ",
        "  indented line",
        "アイウエオキクケコ",
        "the quick brown fox jumps",
        "\t  tab then blanks",
        "last",
    ];

    /// `(line, byte, curswant or -1 for line end, topline, leftcol)`, lines
    /// 0-based, as the probe prints a state.
    type State = (i64, usize, i64, i64, usize);

    fn pos((line, byte, want, top, left): State) -> Pos {
        Pos {
            win: 1000,
            line,
            byte,
            want: match want {
                -1 => Want::End,
                n => Want::Col(n as usize),
            },
            top,
            left,
            rest: Rest::Normal,
        }
    }

    /// Runs `keys` from `from` in a 22x80 window over [`LINES`].
    fn run(keys: &str, from: State, shape: &Shape, ww: Option<&str>) -> Option<Pos> {
        let text = |l: i64| usize::try_from(l).ok().and_then(|l| LINES.get(l)).copied();
        let keyword = words::defaults();
        let cx = Ctx {
            shape,
            line_count: LINES.len() as i64,
            rows: 22,
            width: 80,
            text: &text,
            whichwrap: ww,
            scrolling: Some(Scrolling::default()),
            keyword: Some(&keyword),
            lmap: Some(false),
            macros: Some(MACROS),
        };
        let motions = parse(predict::tokens(keys), |_| false)?;
        motions
            .into_iter()
            .try_fold(pos(from), |at, (m, count)| moved(m, count, &at, &cx))
    }

    /// Neovim's before and after for each motion, with the default
    /// `'whichwrap'` (`b,s`), tabstop 8, 22 rows, unscrolled.
    #[test]
    fn motions_go_where_neovim_takes_them() {
        #[rustfmt::skip]
        let cases: &[(State, &str, State)] = &[
            ((2, 0, 0, 0, 0), "0", (2, 0, 0, 0, 0)),
            ((2, 0, -1, 0, 0), "0", (2, 0, 0, 0, 0)),
            // On a tab, the cursor is drawn in and wants its last column.
            ((3, 0, 7, 0, 0), "0", (3, 0, 7, 0, 0)),
            ((4, 0, 0, 0, 0), "l", (4, 1, 7, 0, 0)),
            ((4, 0, 0, 0, 0), "2l", (4, 2, 8, 0, 0)),
            // A combining mark goes with its character; a wide one is one step.
            ((6, 0, 0, 0, 0), "l", (6, 3, 1, 0, 0)),
            ((5, 0, 0, 0, 0), "l", (5, 3, 2, 0, 0)),
            ((0, 0, 0, 0, 0), "100l", (0, 10, 10, 0, 0)),
            // No move keeps the wanted column.
            ((1, 1, -1, 0, 0), "l", (1, 1, -1, 0, 0)),
            ((1, 1, 4, 0, 0), "l", (1, 1, 4, 0, 0)),
            ((1, 1, 4, 0, 0), "h", (1, 0, 0, 0, 0)),
            ((0, 10, -1, 0, 0), "h", (0, 9, 9, 0, 0)),
            ((7, 0, 0, 0, 0), "^", (7, 2, 2, 0, 0)),
            ((3, 0, 7, 0, 0), "5|", (3, 0, 4, 0, 0)),
            ((0, 0, 0, 0, 0), "20|j", (1, 1, 19, 0, 0)),
            ((2, 0, 0, 0, 0), "$", (2, 0, -1, 0, 0)),
            ((0, 10, 10, 0, 0), "j", (1, 1, 10, 0, 0)),
            ((1, 1, 10, 0, 0), "jjjj", (5, 6, 10, 0, 0)),
            ((0, 10, 10, 0, 0), "9j", (9, 15, 10, 0, 0)),
            ((0, 10, 10, 0, 0), "jj", (2, 0, 10, 0, 0)),
            ((0, 10, 10, 0, 0), "jjjjjjjj", (8, 10, 10, 0, 0)),
            ((0, 10, -1, 0, 0), "jjjjjjj", (7, 2, -1, 0, 0)),
            ((0, 10, 10, 0, 0), "<C-j>", (1, 1, 10, 0, 0)),
            ((0, 10, 10, 0, 0), "2<C-n>", (2, 0, 10, 0, 0)),
            ((1, 1, 10, 0, 0), "<C-p>", (0, 10, 10, 0, 0)),
            ((0, 10, -1, 0, 0), "3<C-n>", (3, 1, -1, 0, 0)),
            ((8, 0, 0, 0, 0), "5j", (12, 0, 0, 0, 0)),
            ((10, 0, 0, 0, 0), "5j", (12, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "h", (0, 0, 0, 0, 0)),
            ((0, 5, 5, 0, 0), "h", (0, 4, 4, 0, 0)),
            ((0, 5, 5, 0, 0), "3h", (0, 2, 2, 0, 0)),
            ((0, 5, 5, 0, 0), "9h", (0, 0, 0, 0, 0)),
            ((0, 5, 5, 0, 0), "<Left>", (0, 4, 4, 0, 0)),
            ((0, 5, 5, 0, 0), "<C-h>", (0, 4, 4, 0, 0)),
            ((0, 5, 5, 0, 0), "<BS>", (0, 4, 4, 0, 0)),
            ((0, 0, 0, 0, 0), "<Right>", (0, 1, 1, 0, 0)),
            ((0, 0, 0, 0, 0), " ", (0, 1, 1, 0, 0)),
            ((0, 0, 0, 0, 0), "3 ", (0, 3, 3, 0, 0)),
            ((0, 10, -1, 0, 0), "l", (0, 10, -1, 0, 0)),
            ((0, 0, 0, 0, 0), "<Home>", (0, 0, 0, 0, 0)),
            ((0, 5, 5, 0, 0), "<Home>", (0, 0, 0, 0, 0)),
            // `<Home>` is `1|`: on a tab it wants the first column, not the
            // last it is drawn in; a count is ignored.
            ((3, 1, -1, 0, 0), "<Home>", (3, 0, 0, 0, 0)),
            ((3, 1, -1, 0, 0), "3<Home>", (3, 0, 0, 0, 0)),
            ((11, 17, -1, 0, 0), "<Home>", (11, 0, 0, 0, 0)),
            ((3, 1, -1, 0, 0), "<Home>j", (4, 0, 0, 0, 0)),
            ((0, 5, -1, 0, 0), "3<Home>", (0, 0, 0, 0, 0)),
            ((2, 0, -1, 0, 0), "<Home>", (2, 0, 0, 0, 0)),
            ((0, 5, 5, 0, 0), "<End>", (0, 10, -1, 0, 0)),
            ((8, 0, 0, 0, 0), "^", (8, 2, 2, 0, 0)),
            ((11, 0, 0, 0, 0), "^", (11, 3, 10, 0, 0)),
            ((11, 17, -1, 0, 0), "0", (11, 0, 7, 0, 0)),
            ((11, 17, -1, 0, 0), "^", (11, 3, 10, 0, 0)),
            ((3, 0, 0, 0, 0), "^", (3, 1, 8, 0, 0)),
            ((0, 0, 0, 0, 0), "3|", (0, 2, 2, 0, 0)),
            ((1, 0, 0, 0, 0), "9|", (1, 1, 8, 0, 0)),
            ((3, 0, 0, 0, 0), "3|", (3, 0, 2, 0, 0)),
            ((4, 0, 0, 0, 0), "2|", (4, 1, 1, 0, 0)),
            ((4, 0, 0, 0, 0), "3|", (4, 1, 2, 0, 0)),
            ((5, 0, 0, 0, 0), "4|", (5, 3, 3, 0, 0)),
            ((11, 0, 0, 0, 0), "5|", (11, 0, 4, 0, 0)),
            ((11, 0, 0, 0, 0), "9|", (11, 1, 8, 0, 0)),
            ((11, 0, 0, 0, 0), "10|", (11, 2, 9, 0, 0)),
            ((11, 0, 7, 0, 0), "l", (11, 1, 8, 0, 0)),
            ((11, 0, 7, 0, 0), "h", (11, 0, 7, 0, 0)),
            ((0, 0, 0, 0, 0), "j", (1, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "<Down>", (1, 0, 0, 0, 0)),
            ((3, 0, 0, 0, 0), "k", (2, 0, 0, 0, 0)),
            ((3, 0, 0, 0, 0), "<Up>", (2, 0, 0, 0, 0)),
            ((0, 10, -1, 0, 0), "j", (1, 1, -1, 0, 0)),
            ((0, 10, -1, 0, 0), "jj", (2, 0, -1, 0, 0)),
            ((0, 10, -1, 0, 0), "jjjj", (4, 2, -1, 0, 0)),
            ((0, 8, 8, 0, 0), "jjj", (3, 1, 8, 0, 0)),
            ((0, 8, 8, 0, 0), "jjjj", (4, 2, 8, 0, 0)),
            ((3, 0, 7, 0, 0), "j", (4, 1, 7, 0, 0)),
            ((0, 0, 0, 0, 0), "k", (0, 0, 0, 0, 0)),
            ((8, 0, 0, 0, 0), "j", (9, 0, 0, 0, 0)),
            ((9, 24, -1, 0, 0), "h", (9, 21, 14, 0, 0)),
            ((9, 24, -1, 0, 0), "k", (8, 14, -1, 0, 0)),
            ((0, 0, 0, 0, 0), "+", (1, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "<CR>", (1, 0, 0, 0, 0)),
            ((7, 0, 0, 0, 0), "-", (6, 0, 0, 0, 0)),
            ((7, 0, 0, 0, 0), "_", (7, 2, 2, 0, 0)),
            ((7, 0, 0, 0, 0), "3_", (9, 0, 0, 0, 0)),
            ((7, 0, 0, 0, 0), "2+", (9, 0, 0, 0, 0)),
            ((7, 0, 0, 0, 0), "9+", (12, 0, 0, 0, 0)),
            ((7, 0, 0, 0, 0), "3-", (4, 0, 0, 0, 0)),
            ((7, 0, 0, 0, 0), "20-", (0, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "-", (0, 0, 0, 0, 0)),
            ((12, 0, 0, 0, 0), "+", (12, 0, 0, 0, 0)),
            ((12, 0, 0, 0, 0), "_", (12, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "9_", (8, 2, 2, 0, 0)),
            ((0, 0, 0, 0, 0), "20_", (12, 0, 0, 0, 0)),
            ((12, 2, 2, 0, 0), "+", (12, 2, 2, 0, 0)),
            ((0, 4, 4, 0, 0), "-", (0, 4, 4, 0, 0)),
            ((10, 3, 3, 0, 0), "5+", (12, 0, 0, 0, 0)),
            ((12, 2, 2, 0, 0), "2_", (12, 2, 2, 0, 0)),
            ((12, 2, 2, 0, 0), "_", (12, 0, 0, 0, 0)),
            ((2, 0, 0, 0, 0), "h", (2, 0, 0, 0, 0)),
            ((2, 0, 0, 0, 0), "l", (2, 0, 0, 0, 0)),
            ((6, 0, 0, 0, 0), "l$", (6, 3, -1, 0, 0)),
            ((5, 3, 2, 0, 0), "h", (5, 0, 0, 0, 0)),
            ((6, 3, 1, 0, 0), "h", (6, 0, 0, 0, 0)),
            ((4, 2, -1, 0, 0), "h", (4, 1, 7, 0, 0)),
            ((4, 2, -1, 0, 0), "2h", (4, 0, 0, 0, 0)),
            ((10, 0, 0, 0, 0), "7l", (10, 7, 7, 0, 0)),
            ((10, 7, 7, 0, 0), "jk", (10, 7, 7, 0, 0)),
            ((10, 0, 0, 0, 0), "30l", (10, 24, 24, 0, 0)),
            ((0, 0, 0, 0, 0), "$", (0, 10, -1, 0, 0)),
            ((5, 0, 0, 0, 0), "$", (5, 6, -1, 0, 0)),
            ((6, 0, 0, 0, 0), "$", (6, 3, -1, 0, 0)),
            ((6, 3, -1, 0, 0), "h", (6, 0, 0, 0, 0)),
            ((9, 0, 0, 0, 0), "$", (9, 24, -1, 0, 0)),
            ((11, 0, 0, 0, 0), "$", (11, 17, -1, 0, 0)),
            // `g_`: last non-blank (a tab's last column wanted); the first
            // of an all-blank line.
            ((0, 0, 0, 0, 0), "g_", (0, 10, 10, 0, 0)),
            ((1, 0, 0, 0, 0), "g_", (1, 1, 1, 0, 0)),
            ((2, 0, 0, 0, 0), "g_", (2, 0, 0, 0, 0)),
            ((3, 0, 0, 0, 0), "g_", (3, 1, 8, 0, 0)),
            ((4, 0, 0, 0, 0), "g_", (4, 2, 8, 0, 0)),
            ((5, 0, 0, 0, 0), "g_", (5, 6, 4, 0, 0)),
            ((6, 0, 0, 0, 0), "g_", (6, 3, 1, 0, 0)),
            ((7, 0, 0, 0, 0), "g_", (7, 0, 0, 0, 0)),
            ((8, 0, 0, 0, 0), "g_", (8, 14, 14, 0, 0)),
            ((9, 0, 0, 0, 0), "g_", (9, 24, 16, 0, 0)),
            ((11, 0, 0, 0, 0), "g_", (11, 17, 24, 0, 0)),
            ((0, 3, 3, 0, 0), "g_", (0, 10, 10, 0, 0)),
            ((0, 0, 0, 0, 0), "1g_", (0, 10, 10, 0, 0)),
            ((0, 4, 4, 0, 0), "g_j", (1, 1, 10, 0, 0)),
            // `<Space>` and `<BS>` wrap lines: the line end is a step, an
            // empty line a stop, the buffer's ends the limit.
            ((0, 10, -1, 0, 0), " ", (1, 0, 0, 0, 0)),
            ((0, 10, -1, 0, 0), "2 ", (1, 1, 1, 0, 0)),
            ((0, 10, -1, 0, 0), "3 ", (2, 0, 0, 0, 0)),
            ((1, 1, -1, 0, 0), "3 ", (3, 1, 8, 0, 0)),
            ((2, 0, 0, 0, 0), "10 ", (6, 3, 1, 0, 0)),
            ((2, 0, 0, 0, 0), " ", (3, 0, 7, 0, 0)),
            ((1, 0, 0, 0, 0), "<BS>", (0, 10, 10, 0, 0)),
            ((2, 0, 0, 0, 0), "<BS>", (1, 1, 1, 0, 0)),
            ((3, 0, 0, 0, 0), "<BS>", (2, 0, 0, 0, 0)),
            ((3, 0, 0, 0, 0), "2<BS>", (1, 1, 1, 0, 0)),
            ((4, 0, 0, 0, 0), "<BS>", (3, 1, 8, 0, 0)),
            ((4, 0, 0, 0, 0), "3<BS>", (2, 0, 0, 0, 0)),
            ((4, 0, 0, 0, 0), "<C-h>", (3, 1, 8, 0, 0)),
            ((12, 3, -1, 0, 0), " ", (12, 3, -1, 0, 0)),
            ((12, 3, -1, 0, 0), "9 ", (12, 3, -1, 0, 0)),
            ((0, 0, 0, 0, 0), "<BS>", (0, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "5<BS>", (0, 0, 0, 0, 0)),
            ((10, 24, -1, 0, 0), "40 ", (12, 3, 3, 0, 0)),
            ((7, 0, 0, 0, 0), "<BS>", (6, 3, 1, 0, 0)),
            ((6, 3, -1, 0, 0), " ", (7, 0, 0, 0, 0)),
            ((8, 0, 0, 0, 0), "<BS>", (7, 2, 2, 0, 0)),
            ((11, 0, 0, 0, 0), "<BS>", (10, 24, 24, 0, 0)),
            ((12, 0, 0, 0, 0), "<BS>", (11, 17, 24, 0, 0)),
            ((2, 0, 0, 0, 0), "2<BS>", (1, 0, 0, 0, 0)),
            // Counted `$` and `g_`: on the line count - 1 down, or the last;
            // from the last, no move but wanting the line end.
            ((0, 0, 0, 0, 0), "2$", (1, 1, -1, 0, 0)),
            ((0, 0, 0, 0, 0), "3$", (2, 0, -1, 0, 0)),
            ((0, 0, 0, 0, 0), "4$", (3, 1, -1, 0, 0)),
            ((0, 5, 5, 0, 0), "2<End>", (1, 1, -1, 0, 0)),
            ((11, 0, 0, 0, 0), "2$", (12, 3, -1, 0, 0)),
            ((12, 0, 0, 0, 0), "2$", (12, 0, -1, 0, 0)),
            ((11, 0, 0, 0, 0), "5$", (12, 3, -1, 0, 0)),
            ((12, 2, 2, 0, 0), "2$", (12, 2, -1, 0, 0)),
            ((0, 0, 0, 0, 0), "1$", (0, 10, -1, 0, 0)),
            ((1, 1, -1, 0, 0), "j", (2, 0, -1, 0, 0)),
            ((0, 0, 0, 0, 0), "13$", (12, 3, -1, 0, 0)),
            ((0, 0, 0, 0, 0), "14$", (12, 3, -1, 0, 0)),
            ((0, 0, 0, 0, 0), "2g_", (1, 1, 1, 0, 0)),
            ((0, 0, 0, 0, 0), "3g_", (2, 0, 0, 0, 0)),
            ((11, 0, 0, 0, 0), "2g_", (12, 3, 3, 0, 0)),
            ((12, 0, 0, 0, 0), "2g_", (12, 0, -1, 0, 0)),
            ((12, 2, 2, 0, 0), "2g_", (12, 2, -1, 0, 0)),
            ((11, 0, 0, 0, 0), "5g_", (12, 3, 3, 0, 0)),
            // Vertical moves over tabs, wide characters, short and empty
            // lines keep the wanted column (the line end after `$`); `<CR>`,
            // `_` and `-` want the first non-blank's column.
            ((0, 4, 4, 0, 0), "jjj", (3, 0, 4, 0, 0)),
            ((0, 4, 4, 0, 0), "jjjj", (4, 1, 4, 0, 0)),
            ((0, 1, 1, 0, 0), "jjjjj", (5, 0, 1, 0, 0)),
            ((0, 3, 3, 0, 0), "jjjjj", (5, 3, 3, 0, 0)),
            ((0, 9, 9, 0, 0), "jjj", (3, 1, 9, 0, 0)),
            ((0, 9, 9, 0, 0), "jjjj", (4, 2, 9, 0, 0)),
            ((0, 2, 2, 0, 0), "jjjjj", (5, 3, 2, 0, 0)),
            ((5, 6, -1, 0, 0), "k", (4, 2, -1, 0, 0)),
            ((5, 6, -1, 0, 0), "j", (6, 3, -1, 0, 0)),
            ((5, 0, 1, 0, 0), "j", (6, 3, 1, 0, 0)),
            ((5, 0, 1, 0, 0), "k", (4, 1, 1, 0, 0)),
            ((5, 3, 3, 0, 0), "jjjj", (9, 3, 3, 0, 0)),
            ((9, 24, -1, 0, 0), "jj", (11, 17, -1, 0, 0)),
            ((9, 6, 5, 0, 0), "k", (8, 5, 5, 0, 0)),
            ((9, 6, 5, 0, 0), "kkk", (6, 3, 5, 0, 0)),
            ((9, 0, 1, 0, 0), "4k", (5, 0, 1, 0, 0)),
            ((1, 0, 0, 0, 0), "j", (2, 0, 0, 0, 0)),
            ((8, 2, 2, 0, 0), "jj", (10, 2, 2, 0, 0)),
            ((11, 3, 10, 0, 0), "k", (10, 10, 10, 0, 0)),
            ((11, 3, 10, 0, 0), "kk", (9, 15, 10, 0, 0)),
            ((11, 0, 7, 0, 0), "k", (10, 7, 7, 0, 0)),
            ((11, 0, 7, 0, 0), "j", (12, 3, 7, 0, 0)),
            ((0, 0, 0, 0, 0), "5j", (5, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "12j", (12, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "20j", (12, 0, 0, 0, 0)),
            ((12, 0, 0, 0, 0), "20k", (0, 0, 0, 0, 0)),
            ((12, 3, -1, 0, 0), "3k", (9, 24, -1, 0, 0)),
            ((8, 0, 0, 0, 0), "_j", (9, 3, 2, 0, 0)),
            ((6, 0, 0, 0, 0), "-k", (4, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "<CR>j", (2, 0, 0, 0, 0)),
            ((7, 0, 0, 0, 0), "<CR>jj", (10, 2, 2, 0, 0)),
        ];
        let s = shape();
        for &(from, keys, to) in cases {
            assert_eq!(
                run(keys, from, &s, Some("b,s")),
                Some(pos(to)),
                "{keys:?} from {from:?}"
            );
        }
    }

    /// With `h,l,<,>` in `'whichwrap'`, those keys wrap as `<BS>` and
    /// `<Space>` do by default, and these, left out, stop.
    #[test]
    fn moves_past_a_lines_end_go_on_where_whichwrap_says() {
        #[rustfmt::skip]
        let cases: &[(State, &str, State)] = &[
            ((1, 0, 0, 0, 0), "h", (0, 10, 10, 0, 0)),
            ((1, 0, 0, 0, 0), "3h", (0, 8, 8, 0, 0)),
            ((0, 10, -1, 0, 0), "l", (1, 0, 0, 0, 0)),
            ((0, 10, -1, 0, 0), "2l", (1, 1, 1, 0, 0)),
            ((2, 0, 0, 0, 0), "l", (3, 0, 7, 0, 0)),
            ((2, 0, 0, 0, 0), "h", (1, 1, 1, 0, 0)),
            ((1, 0, 0, 0, 0), "<Left>", (0, 10, 10, 0, 0)),
            ((0, 10, -1, 0, 0), "<Right>", (1, 0, 0, 0, 0)),
            ((0, 10, -1, 0, 0), " ", (0, 10, -1, 0, 0)),
            ((1, 0, 0, 0, 0), "<BS>", (1, 0, 0, 0, 0)),
            ((12, 3, -1, 0, 0), "l", (12, 3, -1, 0, 0)),
            ((0, 0, 0, 0, 0), "h", (0, 0, 0, 0, 0)),
        ];
        let s = shape();
        for &(from, keys, to) in cases {
            assert_eq!(
                run(keys, from, &s, Some("h,l,<,>")),
                Some(pos(to)),
                "{keys:?} from {from:?}"
            );
        }
    }

    /// Not followed: a wrap with `'whichwrap'` unknown, keys Neovim treats
    /// otherwise, and moves that scroll an unknown way.
    #[test]
    fn what_cannot_be_followed_is_not() {
        let s = shape();
        let not = |keys: &str, from: State, ww: Option<&str>| {
            assert_eq!(run(keys, from, &s, ww), None, "{keys:?} from {from:?}");
        };
        not("100l", (0, 0, 0, 0, 0), None);
        not(" ", (0, 10, -1, 0, 0), None);
        // A count Neovim treats otherwise, and a pending count.
        not("3^", (0, 5, 5, 0, 0), Some("b,s"));
        not("3", (0, 5, 5, 0, 0), Some("b,s"));
        // Not a motion, or mapped.
        not("x", (0, 0, 0, 0, 0), Some("b,s"));
        assert_eq!(parse(["j"], |k| k == "j"), None);
        // Not idle in Normal mode.
        let text = |l: i64| usize::try_from(l).ok().and_then(|l| LINES.get(l)).copied();
        let cx = Ctx {
            shape: &s,
            line_count: LINES.len() as i64,
            rows: 22,
            width: 80,
            text: &text,
            whichwrap: Some("b,s"),
            scrolling: Some(Scrolling::default()),
            keyword: None,
            lmap: None,
            macros: None,
        };
        let busy = Pos {
            rest: Rest::Busy,
            ..pos((0, 0, 0, 0, 0))
        };
        assert_eq!(moved(Motion::Down, None, &busy, &cx), None);
        // Past the edge of a four-row view with scrolling unknown; to its
        // last row is fine.
        let short = Ctx { rows: 4, ..cx };
        let from = pos((2, 0, 0, 0, 0));
        let blind = Ctx {
            scrolling: None,
            ..short
        };
        assert!(moved(Motion::Down, None, &from, &blind).is_some());
        assert_eq!(moved(Motion::Down, Some(2), &from, &blind), None);
        let lower = pos((4, 0, 0, 4, 0));
        assert_eq!(moved(Motion::Up, Some(1), &lower, &blind), None);
        // Wrapping out of the view; a huge count walks no further than the
        // view.
        assert_eq!(moved(Motion::Right('s'), Some(10), &from, &short), None);
        assert_eq!(moved(Motion::Left('b'), Some(10), &lower, &short), None);
        assert_eq!(
            moved(Motion::Right('s'), Some(999_999_999), &from, &cx),
            Some(pos((12, 3, 3, 0, 0))),
            "to the buffer's last line, in view"
        );
        // A counted `$` out of view with scrolling unknown.
        assert_eq!(moved(Motion::End, Some(4), &from, &blind), None);
        // A line too long for a wrapping window breaks line-per-row.
        let narrow = Ctx { width: 5, ..cx };
        assert_eq!(moved(Motion::Down, None, &from, &narrow), None);
        // An absurd topline (no agent sends one) must not overflow.
        let far = pos((0, 0, 0, i64::MAX - 1, 0));
        assert_eq!(moved(Motion::Down, None, &far, &cx), None);
        let so = Shape {
            scrolloff: 3,
            ..s.clone()
        };
        let kept = Ctx { shape: &so, ..cx };
        assert_eq!(moved(Motion::Down, None, &far, &kept), None);
    }

    /// `g_` past trailing blanks with a tab among them, onto a wide
    /// character, and on a lone tab onto the tab, wanting its last column.
    #[test]
    fn the_last_non_blank_is_found_as_neovim_finds_it() {
        let lines = ["trail  \t ", "x\t", "\t", "ab日 "];
        let text = |l: i64| usize::try_from(l).ok().and_then(|l| lines.get(l)).copied();
        let s = shape();
        let cx = Ctx {
            shape: &s,
            line_count: 4,
            rows: 22,
            width: 80,
            text: &text,
            whichwrap: Some("b,s"),
            scrolling: Some(Scrolling::default()),
            keyword: None,
            lmap: None,
            macros: None,
        };
        for (line, (byte, want, vcol)) in [(4, 4, 4), (0, 0, 0), (0, 7, 7), (2, 2, 2)]
            .into_iter()
            .enumerate()
        {
            let from = pos((line as i64, 0, 0, 0, 0));
            let to = moved(Motion::LastNonBlank, None, &from, &cx).expect("followed");
            assert_eq!(
                (to.byte, to.want),
                (byte, Want::Col(want)),
                "{:?}",
                lines[line]
            );
            assert_eq!(drawn(lines[line], &s, to.byte), vcol, "{:?}", lines[line]);
        }
        // With a count, on the line count - 1 down.
        let from = pos((0, 0, 0, 0, 0));
        let to = moved(Motion::LastNonBlank, Some(2), &from, &cx).expect("followed");
        assert_eq!((to.line, to.byte, to.want), (1, 0, Want::Col(0)));
        assert_eq!(parse(["g", "_"], |k| k == "g_"), None, "something maps it");
        assert_eq!(parse(["g", "j"], |_| false), None, "not `g_`");
        assert_eq!(parse(["g"], |_| false), None, "pending");
    }

    /// Digits before a motion, `0` among them but not first.
    #[test]
    fn counts_are_read_as_neovim_reads_them() {
        assert_eq!(
            parse(predict::tokens("10j0"), |_| false),
            Some(vec![(Motion::Down, Some(10)), (Motion::Start, None)])
        );
        let word = |way, big| Motion::Word { way, big };
        assert_eq!(
            parse(predict::tokens("3w2gEb<C-Left>"), |_| false),
            Some(vec![
                (word(Way::Next, false), Some(3)),
                (word(Way::BackEnd, true), Some(2)),
                (word(Way::Back, false), None),
                (word(Way::Back, true), None),
            ])
        );
        assert_eq!(parse(predict::tokens("ge"), |k| k == "ge"), None, "mapped");
        let find = |c, till, back| Motion::Find { c, till, back };
        assert_eq!(
            parse(predict::tokens("3fx2T<Tab>t0F f<lt>"), |_| false),
            Some(vec![
                (find('x', false, false), Some(3)),
                (find('\t', true, true), Some(2)),
                (find('0', true, false), None),
                (find(' ', false, true), None),
                (find('<', false, false), None),
            ])
        );
        // The character after `f` is not looked up as a mapping.
        assert_eq!(
            parse(predict::tokens("fj"), |k| k == "j"),
            Some(vec![(find('j', false, false), None)])
        );
        assert_eq!(parse(predict::tokens("fj"), |k| k == "f"), None, "mapped");
        assert_eq!(parse(["f"], |_| false), None, "pending");
        for keys in ["f<Esc>", "f<C-k>", "f<Left>", "f\u{301}", "f\u{7}"] {
            assert_eq!(parse(predict::tokens(keys), |_| false), None, "{keys:?}");
        }
        let huge = "9".repeat(20);
        assert_eq!(
            parse(predict::tokens(&format!("{huge}l")), |_| false),
            Some(vec![(Motion::Right('l'), Some(MAX_COUNT))])
        );
    }

    /// Default `'iskeyword'`; the cursor then wants its drawn column (a
    /// tab's last).
    #[test]
    fn motions_by_words_go_where_neovim_takes_them() {
        #[rustfmt::skip]
        let cases: &[(State, &str, State)] = &[
            ((0, 0, 0, 0, 0), "w", (0, 6, 6, 0, 0)),
            ((0, 0, 0, 0, 0), "2w", (1, 0, 0, 0, 0)),
            ((0, 6, 6, 0, 0), "w", (1, 0, 0, 0, 0)),
            ((0, 6, 6, 0, 0), "3w", (3, 1, 8, 0, 0)),
            ((1, 0, 0, 0, 0), "w", (2, 0, 0, 0, 0)),
            ((2, 0, 0, 0, 0), "w", (3, 1, 8, 0, 0)),
            ((3, 0, 7, 0, 0), "w", (3, 1, 8, 0, 0)),
            ((3, 1, 8, 0, 0), "w", (4, 0, 0, 0, 0)),
            ((4, 0, 0, 0, 0), "w", (4, 2, 8, 0, 0)),
            ((4, 0, 0, 0, 0), "e", (4, 2, 8, 0, 0)),
            ((0, 0, 0, 0, 0), "e", (0, 4, 4, 0, 0)),
            ((0, 4, 4, 0, 0), "e", (0, 10, 10, 0, 0)),
            ((0, 10, 10, 0, 0), "e", (1, 1, 1, 0, 0)),
            ((1, 1, 1, 0, 0), "e", (3, 1, 8, 0, 0)),
            ((3, 1, 8, 0, 0), "b", (2, 0, 0, 0, 0)),
            ((2, 0, 0, 0, 0), "b", (1, 0, 0, 0, 0)),
            ((1, 0, 0, 0, 0), "b", (0, 6, 6, 0, 0)),
            ((0, 6, 6, 0, 0), "b", (0, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "b", (0, 0, 0, 0, 0)),
            ((12, 3, 3, 0, 0), "w", (12, 3, 3, 0, 0)),
            ((12, 0, 0, 0, 0), "w", (12, 3, 3, 0, 0)),
            ((12, 0, 0, 0, 0), "e", (12, 3, 3, 0, 0)),
            ((12, 3, 3, 0, 0), "e", (12, 3, 3, 0, 0)),
            ((9, 0, 0, 0, 0), "w", (10, 0, 0, 0, 0)),
            ((5, 0, 0, 0, 0), "w", (6, 0, 0, 0, 0)),
            ((5, 0, 0, 0, 0), "e", (5, 6, 4, 0, 0)),
            ((6, 0, 0, 0, 0), "w", (8, 2, 2, 0, 0)),
            ((6, 0, 0, 0, 0), "e", (6, 3, 1, 0, 0)),
            ((7, 0, 0, 0, 0), "w", (8, 2, 2, 0, 0)),
            ((7, 1, 1, 0, 0), "b", (6, 0, 0, 0, 0)),
            ((8, 0, 0, 0, 0), "w", (8, 2, 2, 0, 0)),
            ((8, 2, 2, 0, 0), "ge", (6, 3, 1, 0, 0)),
            ((4, 2, 8, 0, 0), "ge", (4, 0, 0, 0, 0)),
            ((2, 0, 0, 0, 0), "ge", (1, 1, 1, 0, 0)),
            ((0, 6, 6, 0, 0), "ge", (0, 4, 4, 0, 0)),
            ((0, 0, 0, 0, 0), "ge", (0, 0, 0, 0, 0)),
            ((10, 4, 4, 0, 0), "W", (10, 10, 10, 0, 0)),
            ((10, 4, 4, 0, 0), "B", (10, 0, 0, 0, 0)),
            ((10, 4, 4, 0, 0), "E", (10, 8, 8, 0, 0)),
            ((10, 4, 4, 0, 0), "gE", (10, 2, 2, 0, 0)),
            ((10, 0, 0, 0, 0), "10w", (12, 3, 3, 0, 0)),
            ((11, 3, 10, 0, 0), "b", (10, 20, 20, 0, 0)),
            ((11, 0, 7, 0, 0), "w", (11, 3, 10, 0, 0)),
            ((0, 0, 0, 0, 0), "50w", (12, 3, 3, 0, 0)),
            ((12, 0, 0, 0, 0), "50b", (0, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "<S-Right>", (0, 6, 6, 0, 0)),
            ((0, 6, 6, 0, 0), "<S-Left>", (0, 0, 0, 0, 0)),
            ((10, 4, 4, 0, 0), "<C-Right>", (10, 10, 10, 0, 0)),
            ((10, 4, 4, 0, 0), "<C-Left>", (10, 0, 0, 0, 0)),
            ((0, 6, -1, 0, 0), "w", (1, 0, 0, 0, 0)),
        ];
        let s = shape();
        for &(from, keys, to) in cases {
            assert_eq!(
                run(keys, from, &s, Some("b,s")),
                Some(pos(to)),
                "{keys} from {from:?}"
            );
        }
        // With `'iskeyword'` unknown, word motions are not followed.
        let text = |l: i64| usize::try_from(l).ok().and_then(|l| LINES.get(l)).copied();
        let cx = Ctx {
            shape: &s,
            line_count: LINES.len() as i64,
            rows: 22,
            width: 80,
            text: &text,
            whichwrap: Some("b,s"),
            scrolling: Some(Scrolling::default()),
            keyword: None,
            lmap: None,
            macros: None,
        };
        let w = Motion::Word {
            way: Way::Next,
            big: false,
        };
        assert_eq!(moved(w, None, &pos((0, 0, 0, 0, 0)), &cx), None);
    }

    /// `T` stops on a character with a combining mark, as Neovim does. The
    /// cursor then wants its drawn column, even if it stays; too few matches
    /// is no move and keeps the want.
    #[test]
    fn motions_to_a_character_go_where_neovim_takes_them() {
        #[rustfmt::skip]
        let cases: &[(State, &str, State)] = &[
            ((0, 0, 0, 0, 0), "fo", (0, 4, 4, 0, 0)),
            ((0, 0, 0, 0, 0), "2fo", (0, 7, 7, 0, 0)),
            ((0, 0, 0, 0, 0), "3fo", (0, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "to", (0, 3, 3, 0, 0)),
            ((0, 0, 0, 0, 0), "2to", (0, 6, 6, 0, 0)),
            ((0, 10, 10, 0, 0), "Fo", (0, 7, 7, 0, 0)),
            ((0, 10, 10, 0, 0), "To", (0, 8, 8, 0, 0)),
            ((0, 10, 10, 0, 0), "2Fo", (0, 4, 4, 0, 0)),
            ((0, 10, 10, 0, 0), "2To", (0, 5, 5, 0, 0)),
            ((0, 1, -1, 0, 0), "tl", (0, 1, 1, 0, 0)),
            ((0, 1, -1, 0, 0), "fz", (0, 1, -1, 0, 0)),
            ((0, 3, 3, 0, 0), "Tl", (0, 3, 3, 0, 0)),
            ((0, 0, 0, 0, 0), "f ", (0, 5, 5, 0, 0)),
            ((0, 0, 0, 0, 0), "f<Space>", (0, 5, 5, 0, 0)),
            ((0, 0, 0, 0, 0), "fw", (0, 6, 6, 0, 0)),
            ((0, 0, 0, 0, 0), "100fl", (0, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "3fl", (0, 9, 9, 0, 0)),
            ((1, 0, 0, 0, 0), "fb", (1, 1, 1, 0, 0)),
            ((1, 0, 0, 0, 0), "tb", (1, 0, 0, 0, 0)),
            ((2, 0, 0, 0, 0), "fx", (2, 0, 0, 0, 0)),
            ((2, 0, 0, 0, 0), "Fx", (2, 0, 0, 0, 0)),
            ((3, 0, 7, 0, 0), "fx", (3, 1, 8, 0, 0)),
            ((3, 1, 8, 0, 0), "F<Tab>", (3, 0, 7, 0, 0)),
            ((3, 1, 8, 0, 0), "T<Tab>", (3, 1, 8, 0, 0)),
            ((4, 0, 0, 0, 0), "f<Tab>", (4, 1, 7, 0, 0)),
            ((4, 0, 0, 0, 0), "t<Tab>", (4, 0, 0, 0, 0)),
            ((4, 0, 0, 0, 0), "tb", (4, 1, 7, 0, 0)),
            ((4, 2, 8, 0, 0), "Ta", (4, 1, 7, 0, 0)),
            ((5, 0, 0, 0, 0), "f語", (5, 6, 4, 0, 0)),
            ((5, 0, 0, 0, 0), "t語", (5, 3, 2, 0, 0)),
            ((5, 6, 4, 0, 0), "T日", (5, 3, 2, 0, 0)),
            ((5, 6, 4, 0, 0), "F本", (5, 3, 2, 0, 0)),
            ((6, 0, 0, 0, 0), "fx", (6, 3, 1, 0, 0)),
            ((6, 0, 0, 0, 0), "tx", (6, 0, 0, 0, 0)),
            ((6, 3, 1, 0, 0), "Fe", (6, 0, 0, 0, 0)),
            ((6, 3, 1, 0, 0), "Te", (6, 0, 0, 0, 0)),
            ((6, 3, 1, 0, 0), "Fé", (6, 3, 1, 0, 0)),
            ((9, 0, 0, 0, 0), "fコ", (9, 24, 16, 0, 0)),
            ((9, 24, 16, 0, 0), "Tア", (9, 3, 2, 0, 0)),
            ((10, 0, 0, 0, 0), "2fo", (10, 17, 17, 0, 0)),
            ((10, 24, 24, 0, 0), "3F ", (10, 9, 9, 0, 0)),
            ((10, 24, 24, 0, 0), "3T ", (10, 10, 10, 0, 0)),
            ((11, 0, 7, 0, 0), "ft", (11, 3, 10, 0, 0)),
            ((11, 3, 10, 0, 0), "F<Tab>", (11, 0, 7, 0, 0)),
            ((11, 3, 10, 0, 0), "T<Tab>", (11, 1, 8, 0, 0)),
            ((12, 0, 0, 0, 0), "fs", (12, 2, 2, 0, 0)),
            ((12, 3, 3, 0, 0), "Fl", (12, 0, 0, 0, 0)),
            ((8, 0, 0, 0, 0), "f<lt>", (8, 0, 0, 0, 0)),
            ((10, 0, -1, 0, 0), "fq", (10, 4, 4, 0, 0)),
            // From on a sought character, to the next.
            ((0, 4, 4, 0, 0), "fo", (0, 7, 7, 0, 0)),
            ((0, 3, 3, 0, 0), "tl", (0, 8, 8, 0, 0)),
            ((0, 2, 2, 0, 0), "2fl", (0, 9, 9, 0, 0)),
            ((0, 2, 2, 0, 0), "tl", (0, 2, 2, 0, 0)),
        ];
        let s = shape();
        for &(from, keys, to) in cases {
            assert_eq!(
                run(keys, from, &s, None),
                Some(pos(to)),
                "{keys} from {from:?}"
            );
        }
        // Not followed if `:lmap` may change the character, or if unknown.
        let text = |l: i64| usize::try_from(l).ok().and_then(|l| LINES.get(l)).copied();
        for lmap in [Some(true), None] {
            let cx = Ctx {
                shape: &s,
                line_count: LINES.len() as i64,
                rows: 22,
                width: 80,
                text: &text,
                whichwrap: None,
                scrolling: Some(Scrolling::default()),
                keyword: None,
                lmap,
                macros: None,
            };
            let f = Motion::Find {
                c: 'o',
                till: false,
                back: false,
            };
            assert_eq!(moved(f, None, &pos((0, 0, 0, 0, 0)), &cx), None, "{lmap:?}");
        }
    }

    /// Neovim's default `'sections'` and `'paragraphs'`.
    const MACROS: Macros = Macros {
        sections: "SHNHH HUnhsh",
        paragraphs: "IPLPPPQPP TPHPLIPpLpItpplpipbp",
    };

    /// To the empty (not all-blank) line ending a paragraph. A count that
    /// hits the edge early is no move but still sets the wanted column.
    #[test]
    fn motions_by_paragraphs_go_where_neovim_takes_them() {
        #[rustfmt::skip]
        let cases: &[(State, &str, State)] = &[
            ((0, 0, 0, 0, 0), "}", (2, 0, 0, 0, 0)),
            ((0, 6, 6, 0, 0), "}", (2, 0, 0, 0, 0)),
            ((0, 0, -1, 0, 0), "}", (2, 0, 0, 0, 0)),
            ((2, 0, 0, 0, 0), "}", (12, 3, 3, 0, 0)),
            ((0, 0, 0, 0, 0), "2}", (12, 3, 3, 0, 0)),
            ((0, 0, 0, 0, 0), "3}", (0, 0, 0, 0, 0)),
            ((12, 0, 0, 0, 0), "{", (2, 0, 0, 0, 0)),
            ((12, 3, 3, 0, 0), "{", (2, 0, 0, 0, 0)),
            ((2, 0, 0, 0, 0), "{", (0, 0, 0, 0, 0)),
            ((12, 0, 0, 0, 0), "2{", (0, 0, 0, 0, 0)),
            ((12, 0, 0, 0, 0), "3{", (12, 0, 0, 0, 0)),
            ((1, 1, 1, 0, 0), "{", (0, 0, 0, 0, 0)),
            ((12, 0, 0, 0, 0), "}", (12, 3, 3, 0, 0)),
            ((0, 0, 0, 0, 0), "{", (0, 0, 0, 0, 0)),
            ((7, 1, 1, 0, 0), "}", (12, 3, 3, 0, 0)),
            ((7, 1, 1, 0, 0), "{", (2, 0, 0, 0, 0)),
            ((3, 1, 8, 0, 0), "{", (2, 0, 0, 0, 0)),
            ((0, 6, -1, 0, 0), "3}", (0, 6, 6, 0, 0)),
            ((12, 3, -1, 0, 0), "3{", (12, 3, 3, 0, 0)),
            ((3, 1, 3, 0, 0), "9{", (3, 1, 8, 0, 0)),
        ];
        let s = shape();
        for &(from, keys, to) in cases {
            assert_eq!(
                run(keys, from, &s, None),
                Some(pos(to)),
                "{keys} from {from:?}"
            );
        }
    }

    /// `M` is halfway down the lines shown, not the window. Landing is on the
    /// wanted column, or the first non-blank with `'startofline'`.
    #[test]
    fn motions_to_the_window_edges_go_where_neovim_takes_them() {
        #[rustfmt::skip]
        let wanted: &[(State, &str, State)] = &[
            ((5, 3, 2, 0, 0), "H", (0, 2, 2, 0, 0)),
            ((5, 3, 2, 0, 0), "M", (6, 3, 2, 0, 0)),
            ((5, 3, 2, 0, 0), "L", (12, 2, 2, 0, 0)),
            ((0, 6, 6, 0, 0), "L", (12, 3, 6, 0, 0)),
            ((0, 6, -1, 0, 0), "L", (12, 3, -1, 0, 0)),
            ((12, 0, 0, 0, 0), "3H", (2, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "3L", (10, 0, 0, 0, 0)),
            ((0, 10, 10, 0, 0), "4H", (3, 1, 10, 0, 0)),
            ((0, 10, 10, 0, 0), "M", (6, 3, 10, 0, 0)),
            ((3, 1, 8, 0, 0), "9H", (8, 8, 8, 0, 0)),
            ((2, 0, 0, 0, 0), "50H", (12, 0, 0, 0, 0)),
            ((2, 0, 0, 0, 0), "50L", (0, 0, 0, 0, 0)),
        ];
        #[rustfmt::skip]
        let sol: &[(State, &str, State)] = &[
            ((5, 3, 2, 0, 0), "H", (0, 0, 0, 0, 0)),
            ((5, 3, 2, 0, 0), "M", (6, 0, 0, 0, 0)),
            ((5, 3, 2, 0, 0), "L", (12, 0, 0, 0, 0)),
            ((0, 6, 6, 0, 0), "L", (12, 0, 0, 0, 0)),
            ((0, 6, -1, 0, 0), "L", (12, 0, 0, 0, 0)),
            ((12, 0, 0, 0, 0), "3H", (2, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "3L", (10, 0, 0, 0, 0)),
            ((0, 10, 10, 0, 0), "4H", (3, 1, 8, 0, 0)),
            ((0, 10, 10, 0, 0), "M", (6, 0, 0, 0, 0)),
            ((3, 1, 8, 0, 0), "9H", (8, 2, 2, 0, 0)),
            ((2, 0, 0, 0, 0), "50H", (12, 0, 0, 0, 0)),
            ((2, 0, 0, 0, 0), "50L", (0, 0, 0, 0, 0)),
        ];
        let tables = [
            (shape(), wanted),
            (
                Shape {
                    startofline: true,
                    ..shape()
                },
                sol,
            ),
        ];
        for (s, cases) in &tables {
            for &(from, keys, to) in *cases {
                assert_eq!(
                    run(keys, from, s, None),
                    Some(pos(to)),
                    "{keys} from {from:?}, 'startofline' {}",
                    s.startofline
                );
            }
        }
    }

    /// Counted past the end, to the last line. Landing is on the wanted
    /// column, or the first non-blank with `'startofline'`.
    #[test]
    fn motions_to_a_line_go_where_neovim_takes_them() {
        #[rustfmt::skip]
        let wanted: &[(State, &str, State)] = &[
            ((5, 3, 2, 0, 0), "gg", (0, 2, 2, 0, 0)),
            ((5, 3, 2, 0, 0), "G", (12, 2, 2, 0, 0)),
            ((0, 6, 6, 0, 0), "G", (12, 3, 6, 0, 0)),
            ((0, 6, -1, 0, 0), "G", (12, 3, -1, 0, 0)),
            ((12, 0, 0, 0, 0), "3gg", (2, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "3G", (2, 0, 0, 0, 0)),
            ((0, 10, 10, 0, 0), "4G", (3, 1, 10, 0, 0)),
            ((0, 10, 10, 0, 0), "9gg", (8, 10, 10, 0, 0)),
            ((3, 1, 8, 0, 0), "9G", (8, 8, 8, 0, 0)),
            ((2, 0, 0, 0, 0), "50G", (12, 0, 0, 0, 0)),
            ((12, 3, 3, 0, 0), "50gg", (12, 3, 3, 0, 0)),
            ((0, 4, 4, 0, 0), "gg", (0, 4, 4, 0, 0)),
            ((0, 10, -1, 0, 0), "1G", (0, 10, -1, 0, 0)),
        ];
        #[rustfmt::skip]
        let sol: &[(State, &str, State)] = &[
            ((5, 3, 2, 0, 0), "gg", (0, 0, 0, 0, 0)),
            ((5, 3, 2, 0, 0), "G", (12, 0, 0, 0, 0)),
            ((0, 6, 6, 0, 0), "G", (12, 0, 0, 0, 0)),
            ((0, 6, -1, 0, 0), "G", (12, 0, 0, 0, 0)),
            ((12, 0, 0, 0, 0), "3gg", (2, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "3G", (2, 0, 0, 0, 0)),
            ((0, 10, 10, 0, 0), "4G", (3, 1, 8, 0, 0)),
            ((0, 10, 10, 0, 0), "9gg", (8, 2, 2, 0, 0)),
            ((3, 1, 8, 0, 0), "9G", (8, 2, 2, 0, 0)),
            ((2, 0, 0, 0, 0), "50G", (12, 0, 0, 0, 0)),
            ((12, 3, 3, 0, 0), "50gg", (12, 0, 0, 0, 0)),
            ((0, 4, 4, 0, 0), "gg", (0, 0, 0, 0, 0)),
            ((0, 10, -1, 0, 0), "1G", (0, 0, 0, 0, 0)),
        ];
        let tables = [
            (shape(), wanted),
            (
                Shape {
                    startofline: true,
                    ..shape()
                },
                sol,
            ),
        ];
        for (s, cases) in &tables {
            for &(from, keys, to) in *cases {
                assert_eq!(
                    run(keys, from, s, None),
                    Some(pos(to)),
                    "{keys} from {from:?}, 'startofline' {}",
                    s.startofline
                );
            }
        }
    }

    /// With the default macros, `.PP`, `.P` and a form feed start one, even
    /// right after an empty line; `.` alone or before a CSS selector does
    /// not. Unknown macros: not followed.
    #[test]
    fn paragraphs_start_where_neovim_starts_them() {
        let lines = [
            "\tfirst",
            "x",
            "",
            ".PP",
            "y",
            "\x0cff",
            "z",
            ".",
            ".P",
            ".container {",
            "end",
        ];
        let text = |l: i64| usize::try_from(l).ok().and_then(|l| lines.get(l)).copied();
        let s = shape();
        let cx = |macros| Ctx {
            shape: &s,
            line_count: lines.len() as i64,
            rows: 22,
            width: 80,
            text: &text,
            whichwrap: None,
            scrolling: Some(Scrolling::default()),
            keyword: None,
            lmap: None,
            macros,
        };
        #[rustfmt::skip]
        let cases: &[(State, &str, State)] = &[
            ((1, 0, 0, 0, 0), "{", (0, 0, 7, 0, 0)),
            ((0, 0, 0, 0, 0), "}", (2, 0, 0, 0, 0)),
            ((2, 0, 0, 0, 0), "}", (3, 0, 0, 0, 0)),
            ((3, 0, 0, 0, 0), "}", (5, 0, 0, 0, 0)),
            ((4, 0, 0, 0, 0), "}", (5, 0, 0, 0, 0)),
            ((5, 0, 0, 0, 0), "}", (8, 0, 0, 0, 0)),
            ((6, 0, 0, 0, 0), "}", (8, 0, 0, 0, 0)),
            ((7, 0, 0, 0, 0), "}", (8, 0, 0, 0, 0)),
            ((8, 0, 0, 0, 0), "}", (10, 2, 2, 0, 0)),
            ((9, 0, 0, 0, 0), "}", (10, 2, 2, 0, 0)),
            ((10, 0, 0, 0, 0), "{", (8, 0, 0, 0, 0)),
            ((9, 0, 0, 0, 0), "{", (8, 0, 0, 0, 0)),
            ((6, 0, 0, 0, 0), "{", (5, 0, 0, 0, 0)),
            ((4, 0, 0, 0, 0), "{", (3, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "3}", (5, 0, 0, 0, 0)),
            ((10, 0, 0, 0, 0), "4{", (2, 0, 0, 0, 0)),
        ];
        for &(from, keys, to) in cases {
            let got = parse(predict::tokens(keys), |_| false)
                .expect("motions")
                .into_iter()
                .try_fold(pos(from), |at, (m, count)| {
                    moved(m, count, &at, &cx(Some(MACROS)))
                });
            assert_eq!(got, Some(pos(to)), "{keys} from {from:?}");
        }
        let down = Motion::Paragraph { back: false };
        assert_eq!(moved(down, None, &pos((0, 0, 0, 0, 0)), &cx(None)), None);
    }

    /// A run of empty lines ends one paragraph: from one of them, `}` and `{`
    /// skip the rest to the next paragraph end.
    #[test]
    fn empty_lines_in_a_run_end_one_paragraph() {
        let lines = ["a", "", "", "b", "", "c"];
        let text = |l: i64| usize::try_from(l).ok().and_then(|l| lines.get(l)).copied();
        let s = shape();
        let cx = Ctx {
            shape: &s,
            line_count: lines.len() as i64,
            rows: 22,
            width: 80,
            text: &text,
            whichwrap: None,
            scrolling: Some(Scrolling::default()),
            keyword: None,
            lmap: None,
            macros: Some(MACROS),
        };
        #[rustfmt::skip]
        let cases: &[(State, &str, State)] = &[
            ((1, 0, 0, 0, 0), "}", (4, 0, 0, 0, 0)),
            ((2, 0, 0, 0, 0), "}", (4, 0, 0, 0, 0)),
            ((4, 0, 0, 0, 0), "{", (2, 0, 0, 0, 0)),
            ((3, 0, 0, 0, 0), "{", (2, 0, 0, 0, 0)),
            ((0, 0, 0, 0, 0), "2}", (4, 0, 0, 0, 0)),
            ((5, 0, 0, 0, 0), "2{", (2, 0, 0, 0, 0)),
        ];
        for &(from, keys, to) in cases {
            let got = parse(predict::tokens(keys), |_| false)
                .expect("motions")
                .into_iter()
                .try_fold(pos(from), |at, (m, count)| moved(m, count, &at, &cx));
            assert_eq!(got, Some(pos(to)), "{keys} from {from:?}");
        }
    }

    /// Found by the letter; `T` lands on the letter, the byte after it being
    /// the mark's.
    #[test]
    fn a_letter_with_a_combining_mark_is_one_character_to_look_for() {
        let text = "ae\u{301}xe\u{301}";
        for (byte, c, way, n, to) in [
            (0, 'x', (true, false), 1, Some(1)),
            (0, 'x', (false, false), 1, Some(4)),
            (0, 'e', (false, false), 2, Some(5)),
            (4, 'e', (false, true), 1, Some(1)),
            (4, 'e', (true, true), 1, Some(1)),
            (4, 'a', (true, true), 1, Some(1)),
            (4, '\u{301}', (false, true), 1, None),
        ] {
            assert_eq!(
                find(text, byte, c, way, n),
                to,
                "{c:?} {way:?} {n} from {byte}"
            );
        }
    }

    /// Just enough, or centring the cursor if it is half a view or more out.
    #[test]
    fn a_view_that_does_not_wrap_follows_the_cursor_sideways() {
        let s = Shape {
            wrap: false,
            ..shape()
        };
        let lines = ["0123456789abcdefghij", "short"];
        let text = |l: i64| usize::try_from(l).ok().and_then(|l| lines.get(l)).copied();
        let cx = Ctx {
            shape: &s,
            line_count: 2,
            rows: 4,
            width: 10,
            text: &text,
            whichwrap: Some("b,s"),
            scrolling: Some(Scrolling::default()),
            keyword: None,
            lmap: Some(false),
            macros: None,
        };
        #[rustfmt::skip]
        let cases: &[(State, &str, State)] = &[
            ((0, 0, 0, 0, 0), "$", (0, 19, -1, 0, 14)),
            ((0, 0, 0, 0, 0), "9l", (0, 9, 9, 0, 0)),
            ((0, 0, 0, 0, 0), "10l", (0, 10, 10, 0, 1)),
            ((0, 0, 0, 0, 0), "12l", (0, 12, 12, 0, 3)),
            ((0, 19, -1, 0, 14), "0", (0, 0, 0, 0, 0)),
            ((0, 19, -1, 0, 14), "j", (1, 4, -1, 0, 0)),
            ((0, 19, -1, 0, 14), "h", (0, 18, 18, 0, 14)),
            ((0, 15, 15, 0, 10), "j", (1, 4, 15, 0, 0)),
            ((0, 0, 0, 0, 0), "fj", (0, 19, 19, 0, 14)),
            ((0, 0, 0, 0, 0), "fc", (0, 12, 12, 0, 3)),
            ((0, 0, 0, 0, 0), "tc", (0, 11, 11, 0, 2)),
            ((0, 19, -1, 0, 14), "F0", (0, 0, 0, 0, 0)),
            ((0, 19, -1, 0, 14), "Fe", (0, 14, 14, 0, 14)),
        ];
        for &(from, keys, to) in cases {
            let motions = parse(predict::tokens(keys), |_| false).expect("motions");
            let got = motions
                .into_iter()
                .try_fold(pos(from), |at, (m, count)| moved(m, count, &at, &cx));
            assert_eq!(got, Some(pos(to)), "{keys:?} from {from:?}");
        }
    }

    /// A 5-row view scrolls just enough, or, where Neovim centres the cursor,
    /// as far as the buffer's ends allow.
    #[test]
    fn motions_past_the_edges_move_the_view_with_them() {
        #[rustfmt::skip]
        let cases: &[(usize, State, &str, State)] = &[
            (0, (4, 0, 0, 0, 0), "j", (5, 0, 0, 1, 0)),
            (0, (4, 0, 0, 0, 0), "2j", (6, 0, 0, 2, 0)),
            (0, (4, 0, 0, 0, 0), "3j", (7, 0, 0, 3, 0)),
            (0, (4, 0, 0, 0, 0), "8j", (12, 0, 0, 8, 0)),
            (0, (3, 0, 7, 0, 0), "jj", (5, 6, 7, 1, 0)),
            (1, (3, 0, 7, 0, 0), "j", (4, 1, 7, 1, 0)),
            (1, (3, 0, 7, 0, 0), "2j", (5, 6, 7, 2, 0)),
            (0, (2, 0, 0, 2, 0), "k", (1, 0, 0, 1, 0)),
            (0, (2, 0, 0, 2, 0), "2k", (0, 0, 0, 0, 0)),
            (1, (9, 0, 0, 8, 0), "8k", (1, 0, 0, 0, 0)),
            (1, (5, 0, 0, 4, 0), "k", (4, 0, 0, 3, 0)),
            (1, (5, 0, 0, 4, 0), "-", (4, 0, 0, 3, 0)),
            (1, (5, 0, 0, 4, 0), "kj", (5, 0, 0, 3, 0)),
            (0, (1, 1, -1, 1, 0), "k", (0, 10, -1, 0, 0)),
            (0, (4, 0, 0, 0, 0), "jk", (4, 0, 0, 1, 0)),
            (0, (4, 0, 0, 0, 0), "jjjj", (8, 0, 0, 4, 0)),
            (0, (4, 0, 0, 0, 0), "+", (5, 0, 0, 1, 0)),
            (0, (4, 0, 0, 0, 0), "<CR>", (5, 0, 0, 1, 0)),
            (0, (4, 0, 0, 0, 0), "2_", (5, 0, 0, 1, 0)),
            (0, (4, 0, 0, 0, 0), "2$", (5, 6, -1, 1, 0)),
            (0, (9, 24, -1, 5, 0), "jj", (11, 17, -1, 7, 0)),
            (0, (9, 6, 5, 8, 0), "kk", (7, 2, 5, 7, 0)),
            // The buffer's last line in view: nothing to scroll.
            (0, (11, 3, 10, 8, 0), "j", (12, 3, 10, 8, 0)),
            (2, (10, 0, 0, 8, 0), "j", (11, 0, 0, 8, 0)),
            (2, (10, 0, 0, 8, 0), "jj", (12, 0, 0, 8, 0)),
            (0, (12, 0, 0, 8, 0), "j", (12, 0, 0, 8, 0)),
        ];
        for &(so, from, keys, to) in cases {
            let s = Shape {
                scrolloff: so,
                ..shape()
            };
            let text = |l: i64| usize::try_from(l).ok().and_then(|l| LINES.get(l)).copied();
            let cx = Ctx {
                shape: &s,
                line_count: LINES.len() as i64,
                rows: 5,
                width: 80,
                text: &text,
                whichwrap: Some("b,s"),
                scrolling: Some(Scrolling::default()),
                keyword: None,
                lmap: None,
                macros: None,
            };
            let got = parse(predict::tokens(keys), |_| false)
                .expect("motions")
                .into_iter()
                .try_fold(pos(from), |at, (m, count)| moved(m, count, &at, &cx));
            assert_eq!(
                got,
                Some(pos(to)),
                "{keys:?} from {from:?}, 'scrolloff' {so}"
            );
        }
    }

    /// [`keep_in_view`] with one-row lines and default scrolling.
    fn kept(top: i64, cursor: i64, h: i64, so: i64, last: i64) -> Option<i64> {
        keep_in_view((top, cursor), (h, last), so, Scrolling::default(), &|_| {
            true
        })
    }

    /// [`keep_in_view`] scrolls minimally exactly where [`TOPLINE`] says
    /// Neovim does, from the row nearest the edge `'scrolloff'` allows.
    /// Upward that is counts below half the window less one (at least 2),
    /// whatever `'scrolloff'`; beyond, it centres: 9 up in 21 rows onto the
    /// middle row, 10 up in 22 onto the upper middle one.
    #[test]
    fn motions_past_the_edges_scroll_as_neovim_does() {
        let (top, last) = (400, 999);
        let mut followed = 0;
        for &(h, runs) in TOPLINE {
            assert_eq!(runs[0], runs[1], "`gj` as `j`, {h} rows");
            assert_eq!(runs[2], runs[3], "`gk` as `k`, {h} rows");
            for (k, run) in runs.iter().enumerate() {
                let up = k >= 2;
                for (so, &just) in run.iter().enumerate() {
                    let so = so as i64;
                    let from = if up { top + so } else { top + h - 1 - so };
                    let short = (h / 2 - 1).max(2);
                    for n in 1..=h {
                        let cursor = if up { from - n } else { from + n };
                        let minimal = if up {
                            cursor - so
                        } else {
                            cursor + so - (h - 1)
                        };
                        let got = kept(top, cursor, h, so, last);
                        let at = format!(
                            "{h} rows, 'scrolloff' {so}, {n} {}",
                            ["j", "gj", "k", "gk"][k]
                        );
                        if so > (h - 1) / 2 {
                            assert_eq!(got, None, "{at}");
                            continue;
                        }
                        assert!(got.is_some(), "{at}");
                        assert_eq!(got == Some(minimal), n <= just, "{at}: {got:?}");
                        if n < short {
                            assert_eq!(got, Some(minimal), "{at}");
                        }
                        followed += 1;
                    }
                }
            }
        }
        assert!(followed > 10_000, "{followed}");
        let up = |h: i64, n: i64| kept(400, 400 - n, h, 0, 999);
        assert_eq!(up(21, 8), Some(392));
        assert_eq!(up(21, 9), Some(381));
        assert_eq!(up(22, 9), Some(391));
        assert_eq!(up(22, 10), Some(380));
    }

    /// A `'scrolloff'` over half the window keeps the cursor centred by
    /// Neovim's own rules, so it is refused even where the view would not
    /// move. Half the window less a row (10 of 22) still scrolls minimally.
    #[test]
    fn so_past_half_the_window_is_refused() {
        for so in [11, 999] {
            assert_eq!(kept(400, 412, 22, so, 999), None, "{so} down");
            assert_eq!(kept(400, 410, 22, so, 999), None, "{so} up");
            assert_eq!(kept(400, 411, 22, so, 999), None, "{so} in the middle");
        }
        assert_eq!(kept(400, 412, 22, 10, 999), Some(401));
        assert_eq!(kept(400, 409, 22, 10, 999), Some(399));
        assert_eq!(kept(400, 411, 22, 10, 999), Some(400));
    }

    /// Generated by `scripts/sweep-topline.lua` with Neovim 0.12.6. Per window
    /// height, for `j`, `gj`, `k`, `gk`, and each 'scrolloff' from 0 to half
    /// the height: the largest count for which Neovim scrolls no further than
    /// it must (the height if every count does).
    #[rustfmt::skip]
    const TOPLINE: &[(i64, [&[i64]; 4])] = &[
        (4, [
            &[2, 4, 0],
            &[2, 4, 0],
            &[1, 4, 0],
            &[1, 4, 0],
        ]),
        (5, [
            &[3, 4, 5],
            &[3, 4, 5],
            &[1, 1, 5],
            &[1, 1, 5],
        ]),
        (6, [
            &[3, 4, 6, 0],
            &[3, 4, 6, 0],
            &[1, 1, 6, 0],
            &[1, 1, 6, 0],
        ]),
        (7, [
            &[4, 5, 6, 7],
            &[4, 5, 6, 7],
            &[1, 1, 1, 7],
            &[1, 1, 1, 7],
        ]),
        (8, [
            &[4, 5, 6, 8, 0],
            &[4, 5, 6, 8, 0],
            &[2, 2, 2, 8, 0],
            &[2, 2, 2, 8, 0],
        ]),
        (9, [
            &[5, 6, 7, 8, 9],
            &[5, 6, 7, 8, 9],
            &[2, 2, 2, 2, 9],
            &[2, 2, 2, 2, 9],
        ]),
        (10, [
            &[5, 6, 7, 8, 10, 0],
            &[5, 6, 7, 8, 10, 0],
            &[3, 3, 3, 3, 10, 0],
            &[3, 3, 3, 3, 10, 0],
        ]),
        (11, [
            &[6, 7, 8, 9, 10, 11],
            &[6, 7, 8, 9, 10, 11],
            &[3, 3, 3, 3, 3, 11],
            &[3, 3, 3, 3, 3, 11],
        ]),
        (12, [
            &[6, 7, 8, 9, 10, 12, 0],
            &[6, 7, 8, 9, 10, 12, 0],
            &[4, 4, 4, 4, 4, 12, 0],
            &[4, 4, 4, 4, 4, 12, 0],
        ]),
        (13, [
            &[7, 8, 9, 10, 11, 12, 13],
            &[7, 8, 9, 10, 11, 12, 13],
            &[4, 4, 4, 4, 4, 4, 13],
            &[4, 4, 4, 4, 4, 4, 13],
        ]),
        (14, [
            &[7, 8, 9, 10, 11, 12, 14, 0],
            &[7, 8, 9, 10, 11, 12, 14, 0],
            &[5, 5, 5, 5, 5, 5, 14, 0],
            &[5, 5, 5, 5, 5, 5, 14, 0],
        ]),
        (15, [
            &[8, 9, 10, 11, 12, 13, 14, 15],
            &[8, 9, 10, 11, 12, 13, 14, 15],
            &[5, 5, 5, 5, 5, 5, 5, 15],
            &[5, 5, 5, 5, 5, 5, 5, 15],
        ]),
        (16, [
            &[8, 9, 10, 11, 12, 13, 14, 16, 0],
            &[8, 9, 10, 11, 12, 13, 14, 16, 0],
            &[6, 6, 6, 6, 6, 6, 6, 16, 0],
            &[6, 6, 6, 6, 6, 6, 6, 16, 0],
        ]),
        (17, [
            &[9, 10, 11, 12, 13, 14, 15, 16, 17],
            &[9, 10, 11, 12, 13, 14, 15, 16, 17],
            &[6, 6, 6, 6, 6, 6, 6, 6, 17],
            &[6, 6, 6, 6, 6, 6, 6, 6, 17],
        ]),
        (18, [
            &[9, 10, 11, 12, 13, 14, 15, 16, 18, 0],
            &[9, 10, 11, 12, 13, 14, 15, 16, 18, 0],
            &[7, 7, 7, 7, 7, 7, 7, 7, 18, 0],
            &[7, 7, 7, 7, 7, 7, 7, 7, 18, 0],
        ]),
        (19, [
            &[10, 11, 12, 13, 14, 15, 16, 17, 18, 19],
            &[10, 11, 12, 13, 14, 15, 16, 17, 18, 19],
            &[7, 7, 7, 7, 7, 7, 7, 7, 7, 19],
            &[7, 7, 7, 7, 7, 7, 7, 7, 7, 19],
        ]),
        (20, [
            &[10, 11, 12, 13, 14, 15, 16, 17, 18, 20, 0],
            &[10, 11, 12, 13, 14, 15, 16, 17, 18, 20, 0],
            &[8, 8, 8, 8, 8, 8, 8, 8, 8, 20, 0],
            &[8, 8, 8, 8, 8, 8, 8, 8, 8, 20, 0],
        ]),
        (21, [
            &[11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21],
            &[11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21],
            &[8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 21],
            &[8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 21],
        ]),
        (22, [
            &[11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 22, 0],
            &[11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 22, 0],
            &[9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 22, 0],
            &[9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 22, 0],
        ]),
        (23, [
            &[12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23],
            &[12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23],
            &[9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 23],
            &[9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 23],
        ]),
        (24, [
            &[12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 24, 0],
            &[12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 24, 0],
            &[10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 24, 0],
            &[10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 24, 0],
        ]),
        (25, [
            &[13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25],
            &[13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25],
            &[10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 25],
            &[10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 25],
        ]),
        (26, [
            &[13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 26, 0],
            &[13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 26, 0],
            &[11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 26, 0],
            &[11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 26, 0],
        ]),
        (27, [
            &[14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27],
            &[14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27],
            &[11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 27],
            &[11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 27],
        ]),
        (28, [
            &[14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 28, 0],
            &[14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 28, 0],
            &[12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 28, 0],
            &[12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 28, 0],
        ]),
        (29, [
            &[15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29],
            &[15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29],
            &[12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 29],
            &[12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 29],
        ]),
        (30, [
            &[15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 30, 0],
            &[15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 30, 0],
            &[13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 30, 0],
            &[13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 30, 0],
        ]),
        (31, [
            &[16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31],
            &[16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31],
            &[13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 31],
            &[13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 31],
        ]),
        (32, [
            &[16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 32, 0],
            &[16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 32, 0],
            &[14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 32, 0],
            &[14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 32, 0],
        ]),
        (33, [
            &[17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33],
            &[17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33],
            &[14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 33],
            &[14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 33],
        ]),
        (34, [
            &[17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 34, 0],
            &[17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 34, 0],
            &[15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 34, 0],
            &[15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 34, 0],
        ]),
        (35, [
            &[18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35],
            &[18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35],
            &[15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 35],
            &[15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 35],
        ]),
        (36, [
            &[18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 36, 0],
            &[18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 36, 0],
            &[16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 36, 0],
            &[16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 36, 0],
        ]),
        (37, [
            &[19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37],
            &[19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37],
            &[16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 37],
            &[16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 37],
        ]),
        (38, [
            &[19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 38, 0],
            &[19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 38, 0],
            &[17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 38, 0],
            &[17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 38, 0],
        ]),
        (39, [
            &[20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39],
            &[20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39],
            &[17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 39],
            &[17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 39],
        ]),
        (40, [
            &[20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 40, 0],
            &[20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 40, 0],
            &[18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 40, 0],
            &[18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 40, 0],
        ]),
        (41, [
            &[21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41],
            &[21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41],
            &[18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 41],
            &[18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 41],
        ]),
        (42, [
            &[21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 42, 0],
            &[21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 42, 0],
            &[19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 42, 0],
            &[19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 42, 0],
        ]),
        (43, [
            &[22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43],
            &[22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43],
            &[19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 43],
            &[19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 43],
        ]),
        (44, [
            &[22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 44, 0],
            &[22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 44, 0],
            &[20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 44, 0],
            &[20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 44, 0],
        ]),
        (45, [
            &[23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45],
            &[23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45],
            &[20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 45],
            &[20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 45],
        ]),
        (46, [
            &[23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 46, 0],
            &[23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 46, 0],
            &[21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 46, 0],
            &[21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 46, 0],
        ]),
        (47, [
            &[24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47],
            &[24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47],
            &[21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 47],
            &[21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 47],
        ]),
        (48, [
            &[24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 48, 0],
            &[24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 48, 0],
            &[22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 48, 0],
            &[22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 48, 0],
        ]),
        (49, [
            &[25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49],
            &[25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49],
            &[22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 49],
            &[22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 49],
        ]),
        (50, [
            &[25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 50, 0],
            &[25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 50, 0],
            &[23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 50, 0],
            &[23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 50, 0],
        ]),
        (51, [
            &[26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51],
            &[26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51],
            &[23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 51],
            &[23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 51],
        ]),
        (52, [
            &[26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 52, 0],
            &[26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 52, 0],
            &[24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 52, 0],
            &[24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 52, 0],
        ]),
        (53, [
            &[27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53],
            &[27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53],
            &[24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 53],
            &[24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 53],
        ]),
        (54, [
            &[27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 54, 0],
            &[27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 54, 0],
            &[25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 54, 0],
            &[25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 54, 0],
        ]),
        (55, [
            &[28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55],
            &[28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55],
            &[25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 55],
            &[25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 55],
        ]),
        (56, [
            &[28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 56, 0],
            &[28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 56, 0],
            &[26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 56, 0],
            &[26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 56, 0],
        ]),
        (57, [
            &[29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57],
            &[29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57],
            &[26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 57],
            &[26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 57],
        ]),
        (58, [
            &[29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 58, 0],
            &[29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 58, 0],
            &[27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 58, 0],
            &[27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 58, 0],
        ]),
        (59, [
            &[30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59],
            &[30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59],
            &[27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 59],
            &[27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 59],
        ]),
        (60, [
            &[30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 60, 0],
            &[30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 60, 0],
            &[28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 60, 0],
            &[28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 60, 0],
        ]),
    ];
}
