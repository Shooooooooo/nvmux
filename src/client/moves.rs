//! Where the keys that move the cursor in Normal mode take it, as Neovim
//! 0.12 was found to take it — worked out by the client for the keys it
//! sends, from where Neovim last said the cursor was (see
//! [`super::predict::Ack`]).
//!
//! These are the motions that stay on a line or go a line at a time, each
//! with a count:
//!
//! - `h`, `<Left>`, `<BS>` and `<C-h>` a character back, `l`, `<Right>` and
//!   `<Space>` one on — a character with combining marks one, a character two
//!   cells wide one — stopping at the line's first or last, or going on past
//!   it to the next line where `'whichwrap'` says, which the client does not
//!   follow; one that cannot go at all leaves the column the cursor wants as
//!   it was;
//! - `0` to the line's first character, `^` to its first that is not a
//!   blank, `$` and `<End>` to its last, `g_` to its last that is not a
//!   blank — or its first, for a line all blanks — `|` to the character
//!   over a display column, and `<Home>` to the one over the first, as `1|`
//!   takes it, the cursor wanting that column rather than the one it is
//!   drawn in, a tab's last;
//! - `j`, `k` and their kind a line down or up in the column the cursor
//!   wants (Neovim's `curswant`), no further than the buffer goes;
//! - `+`, `<CR>`, `-` and `_` a line down or up, or none for `_`, to its
//!   first character that is not a blank;
//!
//! — and after a move sideways the cursor wants the column it is drawn in:
//! a character's first, or a tab's last. A motion that would move the view,
//! up or down past `'scrolloff'`, is not followed; one that would move a view
//! that does not wrap sideways moves it as Neovim does ([`predict::showing`]).
//!
//! The client follows the keys it sends by these, and judges where it put
//! the cursor against where Neovim says it did (see `super::App`): those
//! that keep the cursor on its line it draws at once (see
//! `super::anim::chain`); the rest it judges without drawing, for the log
//! (see [`super::predict::Predictor::judge`]).

use super::predict::{self, Ch, Pos, Rest, Shape, Want};

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
    /// after it.
    End,
    /// `g_`: its last that is not a blank — its first, for a line all
    /// blanks.
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
        _ => return None,
    })
}

/// The largest count Neovim keeps: a digit more leaves it as it is.
const MAX_COUNT: i64 = 999_999_999;

/// The motions a run of keys typed in Normal mode with nothing pending
/// makes, each with the count typed before it: `None` where any key of them
/// is not a motion followed here, or `mapped` says something maps it — or
/// starts a mapping with it — or the run ends in a count or a `g`.
pub fn parse<'a>(
    keys: impl IntoIterator<Item = &'a str>,
    mapped: impl Fn(&str) -> bool,
) -> Option<Vec<(Motion, Option<i64>)>> {
    let mut out = Vec::new();
    let mut count: Option<i64> = None;
    let mut g = false;
    for key in keys {
        // `g`, and `_` after it, `g_` as a whole looked up: Neovim's own
        // mappings of `g…` make a prefix of `g` alone.
        if std::mem::take(&mut g) {
            if key != "_" || mapped("g_") {
                return None;
            }
            out.push((Motion::LastNonBlank, count.take()));
            continue;
        }
        if key == "g" {
            g = true;
            continue;
        }
        if mapped(key) {
            return None;
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
    (count.is_none() && !g).then_some(out)
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
    /// whatever it would have done.
    pub whichwrap: Option<&'a str>,
}

impl Ctx<'_> {
    /// Whether the view from line `top` would still be there with the
    /// cursor on line `line`: the window shows its lines one to a row, and
    /// `'scrolloff'` is kept between the cursor and its edges but for the
    /// buffer's own. A `'scrolloff'` as tall as half the window keeps the
    /// cursor in its middle by rules of its own, which are not followed.
    fn holds(&self, top: i64, line: i64) -> bool {
        let (h, so) = (self.rows, self.shape.scrolloff as i64);
        let bottom = top.saturating_add(h);
        let shown = top..bottom.min(self.line_count);
        2 * so < h - 1
            && (line - top >= so || top == 0)
            && (bottom - 1 - line >= so || bottom >= self.line_count)
            && shown.contains(&line)
            && shown
                .clone()
                .all(|l| (self.text)(l).is_some_and(|t| predict::fits(t, self.shape, self.width)))
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
/// and over the lines `cx` says, as Neovim 0.12 was found to take it: `None`
/// where the client cannot say — or where the view would move up or down to
/// show it, which is not followed.
pub fn moved(m: Motion, count: Option<i64>, at: &Pos, cx: &Ctx) -> Option<Pos> {
    if at.rest != Rest::Normal || !cx.holds(at.top, at.line) {
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
    // the cursor is drawn in there, or `want`.
    let down = |line: i64, pick: &dyn Fn(&[Ch]) -> Option<usize>, want: Option<Want>| {
        if !cx.holds(at.top, line) {
            return None;
        }
        let text = (cx.text)(line)?;
        let cs = predict::chars(text, shape);
        let ch = pick(&cs).and_then(|j| cs.get(j));
        Some(Pos {
            line,
            byte: ch.map_or(0, |ch| ch.byte),
            want: want.unwrap_or_else(|| Want::Col(ch.map_or(0, |ch| ch.at(shape)))),
            left: cx.left(text, ch, at.left),
            ..*at
        })
    };
    // The character over display column `vcol`, or the last.
    let over = |cs: &[Ch], vcol: usize| {
        cs.iter()
            .position(|ch| vcol < ch.vcol + ch.width)
            .or(cs.len().checked_sub(1))
    };
    let first = |cs: &[Ch]| {
        cs.iter()
            .position(|ch| !matches!(ch.c, ' ' | '\t'))
            .or(cs.len().checked_sub(1))
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
                return None;
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
        Motion::First => Some(sideways(first(&cs), None)),
        Motion::End if n > 1 => None,
        Motion::End => Some(sideways(cs.len().checked_sub(1), Some(Want::End))),
        // With a count, lines down — or none, wanting the end of the line,
        // where there are not as many.
        Motion::LastNonBlank if n > 1 => None,
        Motion::LastNonBlank => {
            let last = cs.iter().rposition(|ch| !matches!(ch.c, ' ' | '\t'));
            Some(sideways(last.or((!cs.is_empty()).then_some(0)), None))
        }
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
            down(line, &first, None)
        }
    }
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
        let cx = Ctx {
            shape,
            line_count: LINES.len() as i64,
            rows: 22,
            width: 80,
            text: &text,
            whichwrap: ww,
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

    /// Where `'whichwrap'` would take the cursor on to another line, or the
    /// client does not know whether it would, the move is not followed; and
    /// neither is one Neovim takes some other way than these, or that the
    /// view would move for.
    #[test]
    fn what_cannot_be_followed_is_not() {
        let s = shape();
        let not = |keys: &str, from: State, ww: Option<&str>| {
            assert_eq!(run(keys, from, &s, ww), None, "{keys:?} from {from:?}");
        };
        // Past the end of a line, `<Space>` and `<BS>` go on by default.
        not(" ", (0, 10, -1, 0, 0), Some("b,s"));
        not("<BS>", (1, 0, 0, 0, 0), Some("b,s"));
        not("h", (0, 0, 0, 0, 0), Some("h,l"));
        not("100l", (0, 0, 0, 0, 0), None);
        // Counted where Neovim takes a count otherwise, and pending.
        not("3$", (0, 5, 5, 0, 0), Some("b,s"));
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
        };
        let busy = Pos {
            rest: Rest::Busy,
            ..pos((0, 0, 0, 0, 0))
        };
        assert_eq!(moved(Motion::Down, None, &busy, &cx), None);
        // Down past the bottom of a view of four rows, or within
        // 'scrolloff' of it; up past the top.
        let short = Ctx { rows: 4, ..cx };
        let from = pos((2, 0, 0, 0, 0));
        assert!(moved(Motion::Down, None, &from, &short).is_some());
        assert_eq!(moved(Motion::Down, Some(2), &from, &short), None);
        let so = Shape {
            scrolloff: 1,
            ..s.clone()
        };
        let kept = Ctx {
            shape: &so,
            ..short
        };
        assert_eq!(moved(Motion::Down, None, &from, &kept), None);
        let lower = pos((4, 0, 0, 4, 0));
        assert_eq!(moved(Motion::Up, None, &lower, &short), None);
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
        // With a count, lines down, which is not followed here.
        let from = pos((0, 0, 0, 0, 0));
        assert_eq!(moved(Motion::LastNonBlank, Some(2), &from, &cx), None);
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
        let huge = "9".repeat(20);
        assert_eq!(
            parse(predict::tokens(&format!("{huge}l")), |_| false),
            Some(vec![(Motion::Right('l'), Some(MAX_COUNT))])
        );
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
        ];
        for &(from, keys, to) in cases {
            let motions = parse(predict::tokens(keys), |_| false).expect("motions");
            let got = motions
                .into_iter()
                .try_fold(pos(from), |at, (m, count)| moved(m, count, &at, &cx));
            assert_eq!(got, Some(pos(to)), "{keys:?} from {from:?}");
        }
    }
}
