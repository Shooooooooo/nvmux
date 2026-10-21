//! Where the keys that move the cursor in Normal mode take it, as Neovim
//! 0.12 was found to take it — worked out by the client for the keys it
//! sends, from where Neovim last said the cursor was (see
//! [`super::predict::Ack`]).
//!
//! These are the motions that stay on a line, go a line at a time, or go
//! by words, each with a count:
//!
//! - `h`, `<Left>`, `<BS>` and `<C-h>` a character back, `l`, `<Right>` and
//!   `<Space>` one on — a character with combining marks one, a character two
//!   cells wide one — stopping at the line's first or last, or going on past
//!   it to the next line, or the one before, where `'whichwrap'` says: the
//!   line's end a step, an empty line a place to step onto (see
//!   [`wrapped`]); one that cannot go at all leaves the column the cursor
//!   wants as it was;
//! - `0` to the line's first character, `^` to its first that is not a
//!   blank, `$` and `<End>` to its last, `g_` to its last that is not a
//!   blank — or its first, for a line all blanks — those two with a count
//!   on the line one fewer than counted down, `|` to the character over a
//!   display column, and `<Home>` to the one over the first, as `1|` takes
//!   it, the cursor wanting that column rather than the one it is drawn in,
//!   a tab's last;
//! - `j`, `k` and their kind a line down or up in the column the cursor
//!   wants (Neovim's `curswant`), no further than the buffer goes: onto the
//!   character over that column, a tab or a character two cells wide that
//!   covers it, or a shorter line's last — the end of every line, after
//!   `$` — the column wanted kept as it was;
//! - `+`, `<CR>`, `-` and `_` a line down or up, or none for `_`, to its
//!   first character that is not a blank;
//! - `w`, `b`, `e` and `ge`, and `W`, `B`, `E` and `gE`, and the arrows
//!   with Shift or Ctrl that make some of them, by words, from line to line
//!   where they go on past one's end (see [`super::words`]);
//! - `f`, `t`, `F` and `T` along the line to the character typed after
//!   them — the one counted of those, on or back — or till it, stopping on
//!   the character before; one that finds too few goes nowhere, leaving
//!   the column the cursor wants as it was;
//! - and `}` and `{` by paragraphs, to the empty line after the next or
//!   before the last, or the line `'paragraphs'` and `'sections'` start one
//!   with (see [`paragraph`]);
//!
//! — and after a move sideways the cursor wants the column it is drawn in:
//! a character's first, or a tab's last. A motion that takes the cursor up
//! or down past `'scrolloff'` moves the view as Neovim's `update_topline`
//! moves it for a short move, just far enough to keep `'scrolloff'` — and
//! is not followed where it goes far enough for Neovim to centre the cursor
//! instead (see [`keep_in_view`]); one that would move a view that does not
//! wrap sideways moves it as Neovim does ([`predict::showing`]).
//!
//! The client follows the keys it sends by these, draws where they take the
//! cursor at once (see `super::anim::chain`), and judges each against where
//! Neovim says it put the cursor (see `super::App`; and, for the log, every
//! motion followed, drawn or not, [`super::predict::Predictor::judge`]).

use super::predict::{self, Ch, Pos, Rest, Shape, Want};
use super::typing;
use super::words::{self, Way};

/// A motion in Normal mode, as the client follows it: see [`moved`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// `h`, `<Left>`, `<BS>`, `<C-h>`: back a character. Each lets the
    /// cursor go on to the line before where `'whichwrap'` has its letter:
    /// `h`, `<`, `b`, `b`.
    Left(char),
    /// `l`, `<Right>`, `<Space>`: on a character, `'whichwrap'`'s `l`, `>`
    /// or `s` letting it go on to the next line.
    Right(char),
    /// `j`, `<Down>`, `<C-j>`, `<C-n>`; `k`, `<Up>`, `<C-p>`.
    Down,
    Up,
    /// `0`: the line's first character.
    Start,
    /// `^`: its first that is not a blank — its last, for a line all blank.
    First,
    /// `$`, `<End>`: its last, the cursor wanting the end of every line
    /// after it — with a count, the last of the line one fewer down.
    End,
    /// `g_`: its last that is not a blank — its first, for a line all
    /// blanks — with a count, of the line one fewer down.
    LastNonBlank,
    /// `|`: the character over the display column counted, from one.
    Column,
    /// `<Home>`: `1|`, whatever the count, which Neovim reads and lets go.
    Home,
    /// `+`, `<CR>` down a line, `-` up one, `_` none — or one fewer than
    /// counted, `here` — to its first character that is not a blank.
    LineFirst {
        down: bool,
        here: bool,
    },
    /// `w`, `<S-Right>`, `b`, `<S-Left>`, `e` and `ge` by words; and `W`,
    /// `<C-Right>`, `B`, `<C-Left>`, `E` and `gE` by words only blanks part,
    /// `big`.
    Word {
        way: Way,
        big: bool,
    },
    /// `f`, `t`, `F` and `T` and the character typed after them: on along
    /// the line to the character counted that is `c`, or `back` to it —
    /// or `till` it, onto the character before, or after.
    Find {
        c: char,
        till: bool,
        back: bool,
    },
    /// `}` on past a paragraph, `{` `back` past one: see [`paragraph`].
    Paragraph {
        back: bool,
    },
}

/// The motion a key typed in Normal mode makes, if it is one followed here:
/// `g_`, which takes two, is made out by whoever has the `g`.
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
        _ => return None,
    })
}

/// The motion `g` and then `key` make, if it is one followed here: `g_`, and
/// `ge` and `gE` back to a word's end.
pub fn g_motion(key: &str) -> Option<Motion> {
    Some(match key {
        "_" => Motion::LastNonBlank,
        "e" | "E" => Motion::Word {
            way: Way::BackEnd,
            big: key == "E",
        },
        _ => return None,
    })
}

/// Whether `key` is `f`, `t`, `F` or `T`, which wait for a character to
/// look for: whether it stops `till` the character, and looks `back`.
pub fn finds(key: &str) -> Option<(bool, bool)> {
    Some(match key {
        "f" => (false, false),
        "t" => (true, false),
        "F" => (false, true),
        "T" => (true, true),
        _ => return None,
    })
}

/// The character a key typed after `f`, `t`, `F` or `T` has them look for,
/// as Neovim takes it, mappings aside: a character on its own other than a
/// control character or a combining mark — or `<Space>`, `<lt>`, `<Bar>`,
/// `<Bslash>` or `<Tab>` for one. `None` for any other, which Neovim may
/// take otherwise: `<C-k>` for a digraph, `<Esc>` to give up.
pub fn sought(key: &str) -> Option<char> {
    if key.eq_ignore_ascii_case("<Tab>") {
        return Some('\t');
    }
    match typing::Edit::of(key)? {
        typing::Edit::Char(c) => (!predict::joins(c)).then_some(c),
        typing::Edit::Rub => None,
    }
}

/// The largest count Neovim keeps: a digit more leaves it as it is.
const MAX_COUNT: i64 = 999_999_999;

/// The motions a run of keys typed in Normal mode with nothing pending
/// makes, each with the count typed before it: `None` where any key of them
/// is not a motion followed here, or `mapped` says something maps it — or
/// starts a mapping with it — or the run ends in a count, a `g` or an `f`
/// or its kind.
pub fn parse<'a>(
    keys: impl IntoIterator<Item = &'a str>,
    mapped: impl Fn(&str) -> bool,
) -> Option<Vec<(Motion, Option<i64>)>> {
    let mut out = Vec::new();
    let mut count: Option<i64> = None;
    let mut g = false;
    let mut find = None;
    for key in keys {
        // The character `f` and its kind look for: no mapping but `:lmap`'s
        // takes it for another (see [`Ctx::lmap`]).
        if let Some((till, back)) = find.take() {
            let c = sought(key)?;
            out.push((Motion::Find { c, till, back }, count.take()));
            continue;
        }
        // `g`, and `_` or `e` after it, `g_` or `ge` as a whole looked up:
        // Neovim's own mappings of `g…` make a prefix of `g` alone.
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

/// What a window and its lines are, for working out a motion in it.
pub struct Ctx<'a> {
    pub shape: &'a Shape,
    pub line_count: i64,
    /// The window's text rows, and its columns, its gutter and all.
    pub rows: i64,
    pub width: usize,
    /// The text of a line, from nought, where the client has it whole.
    pub text: &'a dyn Fn(i64) -> Option<&'a str>,
    /// `'whichwrap'`, where the client knows it — the agent says it only
    /// where it says what Neovim made of each key: not knowing it, a move
    /// sideways that would go past the end of its line is not followed,
    /// wherever it would have gone.
    pub whichwrap: Option<&'a str>,
    /// `'scrolljump'`, where the client knows it, as `'whichwrap'`: not
    /// knowing it, or it other than one — the view moved further than it
    /// must, by a count of lines or a share of the window — a motion that
    /// would move the view is not followed.
    pub scrolljump: Option<i64>,
    /// What `'iskeyword'` makes of each character under 256 in the window's
    /// buffer, as `'whichwrap'`: not knowing it, a motion by words is not
    /// followed (see [`words::class`]).
    pub keyword: Option<&'a [u8]>,
    /// Whether `:lmap` mappings may take the character typed after `f` and
    /// its kind for another — the buffer's `'iminsert'` 1 — as
    /// `'whichwrap'`: where they may, or the client does not know, a motion
    /// to a character is not followed.
    pub lmap: Option<bool>,
    /// The macros `'sections'` and `'paragraphs'` name, as `'whichwrap'`:
    /// not knowing them, a motion by paragraphs is not followed.
    pub macros: Option<Macros<'a>>,
}

impl Ctx<'_> {
    /// The line a view from line `top` shows first once the cursor goes to
    /// line `line`: that line, where `'scrolloff'` is kept between the
    /// cursor and its edges — but for the buffer's own — or one just far
    /// enough to keep it, where Neovim scrolls no further (see
    /// [`keep_in_view`]); where the window shows its lines one to a row.
    /// `None` where the client cannot say: the view moved further, or by
    /// a `'scrolljump'` other than one, or a line in it too long for a
    /// row.
    fn view(&self, top: i64, line: i64) -> Option<i64> {
        let (h, so) = (self.rows, self.shape.scrolloff as i64);
        let to = keep_in_view(top, line, h, so, self.line_count - 1)?;
        if to != top && self.scrolljump != Some(1) {
            return None;
        }
        let shown = to..to.saturating_add(h).min(self.line_count);
        let one_to_one = shown
            .clone()
            .all(|l| (self.text)(l).is_some_and(|t| predict::fits(t, self.shape, self.width)));
        (shown.contains(&line) && one_to_one).then_some(to)
    }

    /// Where a view that does not wrap shows a line from once the cursor is
    /// drawn on `ch` of it, shown from `left` till then: see
    /// [`predict::showing`].
    fn left(&self, text: &str, ch: Option<&Ch>, left: usize) -> usize {
        let vcol = ch.map_or(0, |ch| ch.at(self.shape));
        predict::showing(text, self.shape, vcol, left, self.width)
    }

    /// Whether `'whichwrap'` lets a move sideways go on past the end of its
    /// line, for the letter `ww` of the key that makes it: `None` where the
    /// client does not know.
    fn wraps(&self, ww: char) -> Option<bool> {
        Some(self.whichwrap?.split(',').any(|f| f.starts_with(ww)))
    }
}

/// Where `m`, counted `count`, takes the cursor from `at`, in the window
/// and over the lines `cx` says, as Neovim 0.12 was found to take it, and
/// where the view goes with it, up or down (see [`Ctx::view`]): `None`
/// where the client cannot say.
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
    // To character `j` of the cursor's line, wanting what the cursor is
    // drawn in there, or `want`.
    let sideways = |j: Option<usize>, want: Option<Want>| {
        let ch = j.and_then(|j| cs.get(j));
        Pos {
            byte: ch.map_or(0, |ch| ch.byte),
            want: want.unwrap_or_else(|| Want::Col(ch.map_or(0, |ch| ch.at(shape)))),
            left: cx.left(text, ch, at.left),
            ..*at
        }
    };
    // To line `line`, onto the character `pick` finds there, wanting what
    // the cursor is drawn in there, or `want`; the view moved with it.
    let down = |line: i64, pick: &dyn Fn(&[Ch]) -> Option<usize>, want: Option<Want>| {
        put(line, cx.view(at.top, line)?, pick, want, at, cx)
    };
    // `g_`'s: the last that is not a blank, or the first of a line all
    // blanks.
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
                // Nowhere to go: the column wanted as it was.
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
        // With a count, on the line one fewer than counted down, or the
        // buffer's last — but from that, nowhere: the cursor stays on its
        // character, wanting the end of the line all the same, as Neovim
        // sets that before it finds it cannot go down.
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
                // Not found as often as counted: nowhere, the column wanted
                // as it was.
                None => Some(*at),
                Some(byte) => Some(sideways(cs.iter().position(|ch| ch.byte == byte), None)),
            }
        }
        Motion::Paragraph { back } => {
            let macros = cx.macros?;
            let found = paragraph(back, n, at.line, cx.line_count, cx.text, macros)?;
            // Run into the buffer's edge before the last count: nowhere —
            // but the cursor wants the column it is drawn in all the same,
            // as Neovim's `nv_findpar` has it want before it looks.
            let Some((line, byte)) = found else {
                return Some(sideways(Some(i), None));
            };
            let on = |cs: &[Ch]| Some(cs.iter().position(|ch| ch.byte == byte).unwrap_or(0));
            if line == at.line {
                return Some(sideways(on(&cs), None));
            }
            down(line, &on, None)
        }
    }
}

/// Where `f` — `t`, `till` the character, and `F` and `T`, `back` —
/// looking for `c`, counted `n`, takes the cursor from byte `byte` of a
/// line of `text`, as Neovim's `searchc` takes it: the first byte of the
/// character it lands on, or `None` where it finds fewer than counted. A
/// character at a time, a combining mark with the one it goes with, each
/// whose first is `c` is found.
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
        // To the byte after `c`: the next character's first — or, where a
        // combining mark goes with `c`, the mark's, which Neovim takes back
        // to the first of the character it is in.
        (true, true) if cs[j].0 + c.len_utf8() < cs[j + 1].0 => j,
        (true, true) => j + 1,
    };
    Some(cs[j].0)
}

/// What `'sections'` and `'paragraphs'` name: the nroff macros that start a
/// section or a paragraph, on a line that starts with `.` and one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Macros<'a> {
    pub sections: &'a str,
    pub paragraphs: &'a str,
}

/// Where `}` — `{`, `back` — counted `n` takes the cursor from line `line`
/// of `count` lines, `text` giving each, as Neovim's `findpar` takes it:
/// past as many paragraphs, each ended by the first line after one that is
/// not empty that starts one (see [`starts`]), to the line that ends the
/// last — or to the buffer's last line or its first, where the last count
/// runs into it — onto its first character, or the last of the buffer's
/// last line, going on: `(line, byte)`. `Some(None)` where a count before
/// the last runs into it, which goes nowhere; `None` where it passes a line
/// the client does not have.
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
    // The last character's first byte, a combining mark with the one it
    // goes with.
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

/// Whether a line of `text` starts a paragraph or a section, as Neovim's
/// `startPS` has it for `{` and `}`: an empty line, one that starts with a
/// form feed, or with `.` and a macro `macros` names. `None` where Neovim
/// reads past the line's end to say.
fn starts(text: &str, macros: Macros) -> Option<bool> {
    match text.as_bytes() {
        [] | [b'\x0c', ..] => Some(true),
        [b'.', rest @ ..] => Some(named(macros.sections, rest)? || named(macros.paragraphs, rest)?),
        _ => Some(false),
    }
}

/// Whether `s`, a line after its `.`, starts with a macro `opt` names, as
/// Neovim's `inmacro` has it: two characters at a time, a blank in `opt`
/// matching a blank or the line's end. `None` where Neovim reads past the
/// end of a line of `.` alone to say.
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

/// Where a scroll lands the cursor on the line it takes it to: see [`onto`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landing {
    /// Onto the character over the column it wants, as a motion down or up
    /// lands it: what most scrolls do.
    Want,
    /// Onto the line's first character that is not a blank, wanting the
    /// column it is drawn in: a page or half of one with `'startofline'`,
    /// and `z<CR>`, `z.` and `z-`.
    First,
    /// Onto the character at the byte it was at, or the line's last before
    /// it, wanting what it did: `zt`, `zz` and `zb` counted, which take it
    /// to the line counted as `check_cursor_col` does.
    Byte,
}

/// Where a scroll takes the cursor from `at`, the view going to line `top`
/// and the cursor to line `line`, landing as `how` says — as Neovim 0.12.6
/// was found to. `None` where the client does not have the line whole.
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

/// The cursor from `at` on line `line` of a view from line `top`, on the
/// character `pick` finds there, wanting what it is drawn in there, or
/// `want`; the view moved sideways to show it, where it does not wrap.
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

/// The first character that is not a blank — the last, of a line all
/// blanks.
fn first_non_blank(cs: &[Ch]) -> Option<usize> {
    cs.iter()
        .position(|ch| !matches!(ch.c, ' ' | '\t'))
        .or(cs.len().checked_sub(1))
}

/// Where a move sideways of `n` characters — back if `left` — takes the
/// cursor from `at` where `'whichwrap'` lets it go on past the ends of its
/// line, as Neovim's `nv_left` and `nv_right` take it: from a line's last
/// character on to the next line's first, or from its first back to the
/// line before's last, is a step as a character is, and an empty line a
/// place of its own to step onto; the buffer's first line or last is as far
/// as it goes. The cursor then wants the column it is drawn in — or, having
/// gone nowhere at all, what it wanted. `None` where it would pass a line
/// the client does not have whole, or out of the view's band — the lines
/// `'scrolloff'` leaves the cursor, where the view stays (see
/// [`Ctx::view`]) — which a count as long as the buffer would: so many
/// lines walked are no more than the view has.
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

/// The line Neovim puts at the top of a view of `h` rows from line `top`
/// once a motion takes the cursor to line `cursor`, of a buffer whose last
/// is `last`, `'scrolloff'` `so`: where it was, if the cursor is `so` lines
/// or more from its edges, or from the buffer's own; else just far enough
/// to have it so, as Neovim's `scroll_cursor_top` and `scroll_cursor_bot`
/// scroll for a short move — and `None` from as far on as `update_topline`
/// would have `scroll_cursor_halfway` centre the cursor instead: up, from
/// `halfheight`, half the window less one and never under two; down, from
/// further than that, which is kept to the same. A `so` past half the
/// window, which keeps the cursor in its middle by rules of its own, is
/// `None` whatever the move. As Neovim 0.12.6 was found to, over every
/// window from 4 rows to 60 (see `scripts/sweep-topline.lua`).
pub fn keep_in_view(top: i64, cursor: i64, h: i64, so: i64, last: i64) -> Option<i64> {
    if so > (h - 1) / 2 {
        return None;
    }
    let to = if top > 0 && cursor < top + so {
        (cursor - so).max(0)
    } else if top.saturating_add(h - 1) < last && cursor > top + h - 1 - so {
        ((cursor + so).min(last) - (h - 1)).max(top)
    } else {
        top
    };
    (to.abs_diff(top) < (h / 2 - 1).max(2).unsigned_abs()).then_some(to)
}

/// The display column the cursor is drawn in, in Normal mode, at byte
/// `byte` of a line of `text`: its character's first, or a tab's last (see
/// [`Ch::at`]); nought on an empty line.
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

    /// The lines the motions were tried on in Neovim 0.12.6: a buffer of
    /// these, each motion fed with `nvim_feedkeys` from a start a few keys
    /// put the cursor at, and `winsaveview()` read before and after.
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

    /// `(line, byte, curswant or -1 for the end of the line, topline,
    /// leftcol)`, lines from nought, as the probe prints a state.
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

    /// The keys from `from`, in a window of `shape` 22 rows by 80 columns
    /// over [`LINES`], `'whichwrap'` as `ww` says.
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
            scrolljump: Some(1),
            keyword: Some(&keyword),
            lmap: Some(false),
            macros: Some(MACROS),
        };
        let motions = parse(predict::tokens(keys), |_| false)?;
        motions
            .into_iter()
            .try_fold(pos(from), |at, (m, count)| moved(m, count, &at, &cx))
    }

    /// Each motion from where Neovim had the cursor to where it took it,
    /// over [`LINES`] — `'whichwrap'` at its default, `b,s`, 8 columns to a
    /// tab, 22 rows and none scrolled.
    #[test]
    fn motions_go_where_neovim_takes_them() {
        #[rustfmt::skip]
        let cases: &[(State, &str, State)] = &[
            ((2, 0, 0, 0, 0), "0", (2, 0, 0, 0, 0)),
            ((2, 0, -1, 0, 0), "0", (2, 0, 0, 0, 0)),
            // On a tab, the cursor is drawn in its last column, and wants it.
            ((3, 0, 7, 0, 0), "0", (3, 0, 7, 0, 0)),
            ((4, 0, 0, 0, 0), "l", (4, 1, 7, 0, 0)),
            ((4, 0, 0, 0, 0), "2l", (4, 2, 8, 0, 0)),
            // A combining mark goes with its character; a wide one is one.
            ((6, 0, 0, 0, 0), "l", (6, 3, 1, 0, 0)),
            ((5, 0, 0, 0, 0), "l", (5, 3, 2, 0, 0)),
            ((0, 0, 0, 0, 0), "100l", (0, 10, 10, 0, 0)),
            // Going nowhere, the column wanted is kept.
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
            // `<Home>` is `1|`: on a tab, the cursor wants its first column,
            // not the last it is drawn in; and a count is no matter.
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
            // `g_`: the last that is not a blank, a tab's last column
            // wanted; the first of a line all blanks.
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
            // Past a line's end, `<Space>` and `<BS>` go on to the next line,
            // and to the one before: the end a step, an empty line a place;
            // no further than the buffer's first line and its last.
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
            // `$` and `g_` counted: on the line one fewer down, or the last;
            // from the last, nowhere, wanting the end of the line.
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
            // Down and up onto a tab, a character two cells wide, shorter
            // lines and empty ones, the column wanted kept; the end of the
            // line kept to after `$`; and after `<CR>`, `_` and `-`, the
            // first non-blank's column wanted.
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

    /// With `'whichwrap'` letting `h`, `l` and the arrows go on past a
    /// line's ends too, each does as `<BS>` and `<Space>` do — and those,
    /// left out of it, stop there — as Neovim 0.12.6 was found to.
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

    /// Where the client does not know whether `'whichwrap'` would take the
    /// cursor on to another line, the move is not followed; and neither is
    /// one Neovim takes some other way than these, or that the view would
    /// move for.
    #[test]
    fn what_cannot_be_followed_is_not() {
        let s = shape();
        let not = |keys: &str, from: State, ww: Option<&str>| {
            assert_eq!(run(keys, from, &s, ww), None, "{keys:?} from {from:?}");
        };
        not("100l", (0, 0, 0, 0, 0), None);
        not(" ", (0, 10, -1, 0, 0), None);
        // Counted where Neovim takes a count otherwise, and pending.
        not("3^", (0, 5, 5, 0, 0), Some("b,s"));
        not("3", (0, 5, 5, 0, 0), Some("b,s"));
        // A key that is not a motion, or that something maps.
        not("x", (0, 0, 0, 0, 0), Some("b,s"));
        assert_eq!(parse(["j"], |k| k == "j"), None);
        // Not in Normal mode with nothing pending.
        let text = |l: i64| usize::try_from(l).ok().and_then(|l| LINES.get(l)).copied();
        let cx = Ctx {
            shape: &s,
            line_count: LINES.len() as i64,
            rows: 22,
            width: 80,
            text: &text,
            whichwrap: Some("b,s"),
            scrolljump: Some(1),
            keyword: None,
            lmap: None,
            macros: None,
        };
        let busy = Pos {
            rest: Rest::Busy,
            ..pos((0, 0, 0, 0, 0))
        };
        assert_eq!(moved(Motion::Down, None, &busy, &cx), None);
        // Down past the bottom of a view of four rows, or within
        // 'scrolloff' of it, and up past the top, as far as Neovim centres
        // the cursor rather than scroll just far enough: two rows of four.
        let short = Ctx { rows: 4, ..cx };
        let from = pos((2, 0, 0, 0, 0));
        assert!(moved(Motion::Down, None, &from, &short).is_some());
        assert_eq!(moved(Motion::Down, Some(3), &from, &short), None);
        let so = Shape {
            scrolloff: 1,
            ..s.clone()
        };
        let kept = Ctx {
            shape: &so,
            ..short
        };
        assert_eq!(moved(Motion::Down, Some(2), &from, &kept), None);
        let lower = pos((4, 0, 0, 4, 0));
        assert_eq!(moved(Motion::Up, Some(2), &lower, &short), None);
        // Nor a row, by a 'scrolljump' the client does not know, or one
        // that moves the view further than it must.
        for jump in [None, Some(5), Some(-50)] {
            let jumps = Ctx {
                scrolljump: jump,
                ..short
            };
            assert_eq!(moved(Motion::Down, Some(2), &from, &jumps), None);
            assert!(moved(Motion::Down, None, &from, &jumps).is_some());
        }
        // On past a line's end, out of the view: a count as long as the
        // buffer walks no further than the view.
        assert_eq!(moved(Motion::Right('s'), Some(10), &from, &short), None);
        assert_eq!(moved(Motion::Left('b'), Some(10), &lower, &short), None);
        assert_eq!(
            moved(Motion::Right('s'), Some(999_999_999), &from, &cx),
            Some(pos((12, 3, 3, 0, 0))),
            "to the buffer's last line, in view"
        );
        // A counted `$` to a line as far out of it.
        assert_eq!(moved(Motion::End, Some(4), &from, &short), None);
        // A line too long for a window that wraps puts the rows out of step
        // with the lines.
        let narrow = Ctx { width: 5, ..cx };
        assert_eq!(moved(Motion::Down, None, &from, &narrow), None);
        // A view from a line past reckoning, which no agent sends: no view
        // of these lines, and no overflow working that out.
        let far = pos((0, 0, 0, i64::MAX - 1, 0));
        assert_eq!(moved(Motion::Down, None, &far, &cx), None);
    }

    /// `g_` past blanks at the end of a line, a tab among them, and onto a
    /// character two cells wide; on a line of one tab, onto the tab, drawn
    /// in its first column but wanting its last — as Neovim 0.12.6 was
    /// found to.
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
            scrolljump: Some(1),
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
        // With a count, on the line one fewer down.
        let from = pos((0, 0, 0, 0, 0));
        let to = moved(Motion::LastNonBlank, Some(2), &from, &cx).expect("followed");
        assert_eq!((to.line, to.byte, to.want), (1, 0, Want::Col(0)));
        assert_eq!(parse(["g", "_"], |k| k == "g_"), None, "something maps it");
        assert_eq!(parse(["g", "j"], |_| false), None, "not `g_`");
        assert_eq!(parse(["g"], |_| false), None, "pending");
    }

    /// A count is digits before a motion, `0` among them but not first.
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
        // What `f` is followed by is no key a mapping is looked up for.
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

    /// Each motion by words from where Neovim had the cursor to where it
    /// took it, over [`LINES`] — `'iskeyword'` at its default, 8 columns to a
    /// tab, 22 rows and none scrolled: along a line and on to the next, over
    /// tabs, scripts of their own, a combining mark, blanks and an empty
    /// line, into the buffer's start and its end, the cursor wanting the
    /// column it is drawn in after each — a tab's last.
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
        // Not knowing what `'iskeyword'` makes of the characters, a motion by
        // words is not followed.
        let text = |l: i64| usize::try_from(l).ok().and_then(|l| LINES.get(l)).copied();
        let cx = Ctx {
            shape: &s,
            line_count: LINES.len() as i64,
            rows: 22,
            width: 80,
            text: &text,
            whichwrap: Some("b,s"),
            scrolljump: Some(1),
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

    /// Each of `f`, `t`, `F` and `T` from where Neovim had the cursor to
    /// where it took it, over [`LINES`], 8 columns to a tab, 22 rows and none
    /// scrolled: to the character counted, or till it, over tabs, a blank,
    /// characters two cells wide and one with a combining mark — which `T`
    /// stops on, as Neovim does, rather than after — the cursor wanting the
    /// column it is drawn in after each, even where it stays; but where
    /// too few are found, going nowhere and wanting what it did.
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
            // From on a character looked for, the next.
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
        // Where `:lmap` mappings may take the character typed for another,
        // or the client does not know, it is not followed.
        let text = |l: i64| usize::try_from(l).ok().and_then(|l| LINES.get(l)).copied();
        for lmap in [Some(true), None] {
            let cx = Ctx {
                shape: &s,
                line_count: LINES.len() as i64,
                rows: 22,
                width: 80,
                text: &text,
                whichwrap: None,
                scrolljump: Some(1),
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

    /// Neovim's own `'sections'` and `'paragraphs'`.
    const MACROS: Macros = Macros {
        sections: "SHNHH HUnhsh",
        paragraphs: "IPLPPPQPP TPHPLIPpLpItpplpipbp",
    };

    /// Each `}` and `{` from where Neovim had the cursor to where it took
    /// it, over [`LINES`], 22 rows and none scrolled: to the empty line that
    /// ends a paragraph — not one of blanks — or on, past the last, to the
    /// buffer's last character, or back to its first line; with counts, and
    /// with a count that runs into the buffer's edge before it is done,
    /// nowhere — the cursor wanting the column it is drawn in, though, as it
    /// does after every other.
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

    /// Paragraphs and sections that start as Neovim 0.12.6 was found to
    /// start them with its own `'sections'` and `'paragraphs'`: at a line
    /// that starts with `.` and a macro they name — `PP`, or `P` alone —
    /// even right after an empty line, or with a form feed; not at a `.`
    /// with none, alone or before a stylesheet's selector. The cursor on
    /// the buffer's first line, on a tab, wants the tab's last column; and
    /// not knowing the macros, the motion is not followed.
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
            scrolljump: Some(1),
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

    /// Empty lines in a run end one paragraph between them, as Neovim
    /// 0.12.6 was found to have them: from one of them, `}` and `{` go on
    /// past the rest to the next that ends one.
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
            scrolljump: Some(1),
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

    /// A letter with a combining mark is one character to `f` and its kind,
    /// found by the letter, as Neovim 0.12.6 was found to: on, back, and
    /// till it — `T` onto it, the byte after the letter being the mark's.
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

    /// In a window that does not wrap, ten columns wide, a move out of view
    /// moves the view sideways as Neovim 0.12.6 was found to: as far as it
    /// takes, or with the cursor half a view or more out, to centre it.
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
            scrolljump: Some(1),
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

    /// Past the edges of a view five rows tall, over [`LINES`]: a line or
    /// two, `'scrolloff'` as each says, by `j`, `k` and their kind, the
    /// view moved just far enough with the cursor — and, as far as Neovim
    /// centres the cursor instead, not followed — as Neovim 0.12.6 was found
    /// to move them.
    #[test]
    fn motions_past_the_edges_move_the_view_with_them() {
        #[rustfmt::skip]
        let cases: &[(usize, State, &str, Option<State>)] = &[
            (0, (4, 0, 0, 0, 0), "j", Some((5, 0, 0, 1, 0))),
            (0, (4, 0, 0, 0, 0), "2j", None),
            (0, (4, 0, 0, 0, 0), "3j", None),
            (0, (3, 0, 7, 0, 0), "jj", Some((5, 6, 7, 1, 0))),
            (1, (3, 0, 7, 0, 0), "j", Some((4, 1, 7, 1, 0))),
            (1, (3, 0, 7, 0, 0), "2j", None),
            (0, (2, 0, 0, 2, 0), "k", Some((1, 0, 0, 1, 0))),
            (0, (2, 0, 0, 2, 0), "2k", None),
            (1, (5, 0, 0, 4, 0), "k", Some((4, 0, 0, 3, 0))),
            (1, (5, 0, 0, 4, 0), "-", Some((4, 0, 0, 3, 0))),
            (1, (5, 0, 0, 4, 0), "kj", Some((5, 0, 0, 3, 0))),
            (0, (1, 1, -1, 1, 0), "k", Some((0, 10, -1, 0, 0))),
            (0, (4, 0, 0, 0, 0), "jk", Some((4, 0, 0, 1, 0))),
            (0, (4, 0, 0, 0, 0), "jjjj", Some((8, 0, 0, 4, 0))),
            (0, (4, 0, 0, 0, 0), "+", Some((5, 0, 0, 1, 0))),
            (0, (4, 0, 0, 0, 0), "<CR>", Some((5, 0, 0, 1, 0))),
            (0, (4, 0, 0, 0, 0), "2_", Some((5, 0, 0, 1, 0))),
            (0, (4, 0, 0, 0, 0), "2$", Some((5, 6, -1, 1, 0))),
            (0, (9, 24, -1, 5, 0), "jj", Some((11, 17, -1, 7, 0))),
            (0, (9, 6, 5, 8, 0), "kk", Some((7, 2, 5, 7, 0))),
            // The buffer's last line in view: nothing to scroll.
            (0, (11, 3, 10, 8, 0), "j", Some((12, 3, 10, 8, 0))),
            (2, (10, 0, 0, 8, 0), "j", Some((11, 0, 0, 8, 0))),
            (2, (10, 0, 0, 8, 0), "jj", Some((12, 0, 0, 8, 0))),
            (0, (12, 0, 0, 8, 0), "j", Some((12, 0, 0, 8, 0))),
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
                scrolljump: Some(1),
                keyword: None,
                lmap: None,
                macros: None,
            };
            let got = parse(predict::tokens(keys), |_| false)
                .expect("motions")
                .into_iter()
                .try_fold(pos(from), |at, (m, count)| moved(m, count, &at, &cx));
            assert_eq!(got, to.map(pos), "{keys:?} from {from:?}, 'scrolloff' {so}");
        }
    }

    /// Every top [`keep_in_view`] gives is the one Neovim gives, over every
    /// window from 4 rows to 60, every `'scrolloff'` to half of it, and a
    /// count of up to as many lines, down and up from the row nearest the
    /// edge `'scrolloff'` lets the cursor be on: a short move, or none
    /// (see [`TOPLINE`]). Up, Neovim scrolls just far enough for a count of
    /// up to half the window less one, or two, whatever `'scrolloff'` is —
    /// two rows, of four; nine, of 21; ten, of 22 and 23 — and down further;
    /// `gj` and `gk` as `j` and `k`. Every move short of that is followed.
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
                        let got = keep_in_view(top, cursor, h, so, last);
                        let at = format!(
                            "{h} rows, 'scrolloff' {so}, {n} {}",
                            ["j", "gj", "k", "gk"][k]
                        );
                        assert!(
                            got.is_none() || (n <= just && got == Some(minimal)),
                            "{at}: {got:?}"
                        );
                        if so <= (h - 1) / 2 && n < short {
                            assert_eq!(got, Some(minimal), "{at}");
                            followed += 1;
                        }
                    }
                }
            }
        }
        assert!(followed > 10_000, "{followed}");
        let up = |h: i64, n: i64| keep_in_view(400, 400 - n, h, 0, 999);
        assert_eq!(up(21, 8), Some(392));
        assert_eq!(up(21, 9), None);
        assert_eq!(up(22, 9), Some(391));
        assert_eq!(up(22, 10), None);
    }

    /// A `'scrolloff'` past half the window keeps the cursor in its middle
    /// by rules of its own, which Neovim follows for every move: the view
    /// it leaves is not worked out, however short the move — nor where it
    /// would not move at all. Half the window less a row — ten of 22, the
    /// cursor kept to the two rows between — scrolls just far enough, as
    /// any less does.
    #[test]
    fn so_past_half_the_window_is_refused() {
        for so in [11, 999] {
            assert_eq!(keep_in_view(400, 412, 22, so, 999), None, "{so} down");
            assert_eq!(keep_in_view(400, 410, 22, so, 999), None, "{so} up");
            assert_eq!(
                keep_in_view(400, 411, 22, so, 999),
                None,
                "{so} in the middle"
            );
        }
        assert_eq!(keep_in_view(400, 412, 22, 10, 999), Some(401));
        assert_eq!(keep_in_view(400, 409, 22, 10, 999), Some(399));
        assert_eq!(keep_in_view(400, 411, 22, 10, 999), Some(400));
    }

    /// Generated by `scripts/sweep-topline.lua` with Neovim 0.12.6: for
    /// each window height, and for `j`, `gj`, `k` and `gk` in turn, how far a
    /// count goes, for each 'scrolloff' from nought to half the height, with
    /// Neovim scrolling the view no further than it must — the count before the
    /// first it scrolls further for, or the height where none is.
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
