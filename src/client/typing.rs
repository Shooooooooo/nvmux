//! Typing ahead of a slow link: what a key typed in Insert mode does to the
//! cursor's line, worked out by the client before Neovim has heard of it
//! (see `super::predict`).
//!
//! Most keys typed in Insert mode do one plain thing: a character goes in
//! where the cursor is, and the cursor goes on past it; `<BS>` takes out the
//! character before the cursor, and the cursor goes back over it. A key does
//! more, or something else, only where something says so — a mapping that
//! starts with it, an abbreviation it ends, a re-indent it sets off, a line
//! broken at `'textwidth'`, a bracket shown matched, a softtabstop's worth of
//! blanks taken out at once — and the agent says, of each buffer, what there
//! is of that ([`Facts`]). Where none of it bears on a key ([`refused`]),
//! the client draws what the key does at once: Neovim's own row for the line,
//! laid out afresh around the text the key leaves ([`relayout`]).
//!
//! What the agent cannot say is what a plugin's autocommand does as a key is
//! typed — `InsertCharPre`, or `TextChangedI` and `CursorMovedI` of its own —
//! or as Insert mode is entered or left: `InsertEnter`, `InsertLeavePre`,
//! `InsertLeave` and `ModeChanged`. It says only that there is one, and the
//! client works out what it types in such a buffer, or the keys into Insert
//! mode or out of it, without drawing them, till enough of them have been
//! judged right (see `super::predict::Trust`).

use rmpv::ValueRef;
use unicode_width::UnicodeWidthChar;

use super::grid::{Cell, Text};
use super::predict::{self, Look, Shape};
use super::redraw;

/// What the agent says of Insert mode in a buffer, as it last said it: what
/// would make a key typed there do other than what typing it plainly does —
/// each of Neovim's own, looked up as Neovim 0.12 was found to act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// Said as the buffer was last entered in Insert mode, with the
    /// mappings a plugin loaded on the way in has made by then; not merely
    /// as the buffer came into a window, or an option changed, before the
    /// first time it was.
    pub from_insert: bool,
    /// The keys a mapping in Insert mode starts with — or a language
    /// mapping, where `'iminsert'` turns those on — in the buffer or
    /// everywhere: each as Neovim's `keytrans` writes it.
    pub firsts: Vec<Box<str>>,
    /// What each abbreviation of Insert mode is typed as.
    pub abbrevs: Vec<Box<str>>,
    /// An `InsertCharPre` autocommand, or one for `TextChangedI` or
    /// `CursorMovedI`, of something other than Neovim's own matchparen and
    /// nvmux's agents: what it does to a key typed, nothing says (see the
    /// module docs).
    pub unseen: bool,
    /// The same of entering Insert mode, an autocommand of `InsertEnter` or
    /// `ModeChanged`; and of leaving it, one of `InsertLeavePre`,
    /// `InsertLeave` or `ModeChanged`: what it does — move the cursor, edit
    /// the buffer — nothing says.
    pub unseen_entered: bool,
    pub unseen_left: bool,
    /// The column past which a character typed breaks the line, nought for
    /// none: `'textwidth'`, or the window's width less `'wrapmargin'` and
    /// its gutter, as Neovim's `comp_textwidth` works it out — or narrower.
    pub tw: usize,
    /// `'formatoptions'`.
    pub fo: Box<str>,
    /// The keys that have Neovim indent the line again: `'indentkeys'`
    /// where `'indentexpr'` is set, `'cinkeys'` where `'cindent'` is, none
    /// otherwise.
    pub indentkeys: Indentkeys,
    /// `'smartindent'`, `'lisp'`, `'showmatch'` and `'matchpairs'`, the
    /// pairs as their opening and closing characters.
    pub smartindent: bool,
    pub lisp: bool,
    pub showmatch: bool,
    pub matchpairs: Vec<(char, char)>,
    /// `'autocomplete'`, which opens a completion menu as text is typed.
    pub autocomplete: bool,
    /// Whether text can go in as typed: the buffer `'modifiable'`, and not
    /// `'readonly'` and unchanged, which warns and waits at the first
    /// change, nor a prompt buffer, whose text goes in after its prompt
    /// wherever Insert mode is entered there.
    pub editable: bool,
    /// None of `'revins'`, `'rightleft'`, `'paste'`, an input method
    /// (`'iminsert'` 2) or `'digraph'`, which type otherwise — a character,
    /// `<BS>` and another a digraph, with the last.
    pub plain: bool,
    /// `'backspace'`.
    pub bs: Box<str>,
    /// Whether `<BS>` takes out blanks a softtabstop's worth at a time:
    /// `'softtabstop'` or `'varsofttabstop'` set; and before a line's first
    /// character other than a blank, a `'shiftwidth'`'s with `'smarttab'`.
    pub softtabs: bool,
    pub smarttab: bool,
    /// `'delcombine'`: `<BS>` takes out a character's last combining mark
    /// alone.
    pub delcombine: bool,
    /// The modes `'concealcursor'` hides text on the cursor's line in, by
    /// their letters, where `'conceallevel'` hides any: `i`, the line typed
    /// into drawn otherwise than its text; `n` and not `i`, or `i` and not
    /// `n`, the line drawn otherwise as Insert mode is entered or left.
    pub conceals: Box<str>,
    /// What `'iskeyword'` makes of each character under 256, by its code:
    /// see [`Facts::keyword`].
    pub classes: Vec<u8>,
}

impl Facts {
    /// As the agent sends them: `[buf, from_insert, firsts, abbrevs, unseen,
    /// tw, fo, indentkeys, smartindent, lisp, showmatch, matchpairs,
    /// autocomplete, editable, plain, bs, softtabs, smarttab, delcombine,
    /// conceals, classes, unseen_entered, unseen_left]` — `matchpairs` and
    /// `conceals` as the options have them, and `classes` a character a
    /// code, `0` for a blank, `1` for one of punctuation and `2` for one of
    /// a word. Says the buffer, and them.
    pub fn read(v: &[ValueRef<'_>]) -> Option<(i64, Self)> {
        let int = |i: usize| v.get(i).and_then(redraw::int);
        let flag = |i: usize| matches!(v.get(i), Some(ValueRef::Boolean(true)));
        let text = |i: usize| v.get(i).and_then(redraw::as_str);
        let strings = |i: usize| -> Option<Vec<Box<str>>> {
            v.get(i)?
                .as_array()?
                .iter()
                .map(|s| redraw::as_str(s).map(Into::into))
                .collect()
        };
        let facts = Self {
            from_insert: flag(1),
            firsts: strings(2)?,
            abbrevs: strings(3)?,
            unseen: flag(4),
            tw: usize::try_from(int(5)?).unwrap_or(0),
            fo: text(6)?.into(),
            indentkeys: Indentkeys::parse(text(7)?),
            smartindent: flag(8),
            lisp: flag(9),
            showmatch: flag(10),
            matchpairs: pairs(text(11)?),
            autocomplete: flag(12),
            editable: flag(13),
            plain: flag(14),
            bs: text(15)?.into(),
            softtabs: flag(16),
            smarttab: flag(17),
            delcombine: flag(18),
            conceals: text(19)?.into(),
            classes: text(20)?.bytes().map(|b| b.saturating_sub(b'0')).collect(),
            unseen_entered: flag(21),
            unseen_left: flag(22),
        };
        Some((int(0)?, facts))
    }

    /// Whether a mapping of Insert mode starts with `key`, as Neovim writes
    /// keys.
    pub fn starts(&self, key: &str) -> bool {
        self.firsts.iter().any(|k| same_key(k, key))
    }

    /// Whether `c` is a character of a word, as `'iskeyword'` has it — or,
    /// past the 256 it says, as Neovim's own classes have most of them; but
    /// not every one, so `None` for those.
    pub fn keyword(&self, c: char) -> Option<bool> {
        let code = usize::try_from(u32::from(c)).ok()?;
        self.classes.get(code).map(|class| *class >= 2)
    }
}

/// `'matchpairs'`, as its opening and closing characters.
fn pairs(option: &str) -> Vec<(char, char)> {
    option
        .split(',')
        .filter_map(|pair| {
            let mut cs = pair.chars();
            match (cs.next(), cs.next(), cs.next(), cs.next()) {
                (Some(open), Some(':'), Some(close), None) => Some((open, close)),
                _ => None,
            }
        })
        .collect()
}

/// `'indentkeys'` or `'cinkeys'`, as Neovim's `in_cinkeys` reads them: those
/// that a character typed can set off. Each `*` or `!` before one — indent
/// before the key goes in, or instead — is passed over: either way the key
/// does more than go in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Indentkeys(Vec<Key>);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Key {
    /// A character, anywhere — or, `first`, only with nothing but blanks
    /// before the cursor (`0x`). In `<…>`, a few of the characters the
    /// notation would otherwise take, and those Neovim has names for.
    Char { c: char, first: bool },
    /// `=word`, `=~word` (`icase`) or `0=word` (`first`): the word's last
    /// character typed completing it, with no character of a word before
    /// it — or, `first`, with only blanks.
    Word {
        word: String,
        icase: bool,
        first: bool,
    },
    /// `e`: `else` completed with only blanks before it.
    Else,
    /// `:`, which Neovim acts on at the end of a label or a `case`, or after
    /// a `::` it was in: as good as always.
    Colon,
}

impl Indentkeys {
    /// The option's value, entry by entry as `in_cinkeys` walks it: past
    /// each, a comma and the blanks after it, if one comes next; the keys
    /// that are not characters — `^X`, `o`, `O`, `<CR>` — left out.
    pub fn parse(option: &str) -> Self {
        let s: Vec<char> = option.chars().collect();
        let mut keys = Vec::new();
        let mut i = 0;
        while i < s.len() {
            if matches!(s[i], '*' | '!') {
                i += 1;
            }
            let first = s.get(i) == Some(&'0');
            if first {
                i += 1;
            }
            match s.get(i).copied() {
                None => break,
                Some('^') if s.get(i + 1).is_some_and(|c| ('?'..='_').contains(c)) => i += 2,
                Some('o' | 'O') => i += 1,
                Some('e') => {
                    keys.push(Key::Else);
                    i += 1;
                }
                Some(':') => {
                    keys.push(Key::Colon);
                    i += 1;
                }
                Some('<') => {
                    let name: String = s[i + 1..].iter().take_while(|c| **c != '>').collect();
                    if let Some(&c) = s.get(i + 1).filter(|c| "<>!*oOe0:".contains(**c)) {
                        keys.push(Key::Char { c, first });
                    }
                    let named = match name.to_ascii_lowercase().as_str() {
                        "space" => Some(' '),
                        "lt" => Some('<'),
                        "bar" => Some('|'),
                        "bslash" => Some('\\'),
                        _ => None,
                    };
                    if let Some(c) = named {
                        keys.push(Key::Char { c, first });
                    }
                    i += 1 + name.chars().count();
                    while s.get(i) == Some(&'>') {
                        i += 1;
                    }
                }
                Some('=') if s.get(i + 1).is_some_and(|c| *c != ',') => {
                    i += 1;
                    let icase = s.get(i) == Some(&'~');
                    if icase {
                        i += 1;
                    }
                    let word: String = s[i..].iter().take_while(|c| **c != ',').collect();
                    i += word.chars().count();
                    if !word.is_empty() {
                        keys.push(Key::Word { word, icase, first });
                    }
                }
                Some(c) => {
                    keys.push(Key::Char { c, first });
                    i += 1;
                }
            }
            if s.get(i) == Some(&',') {
                i += 1;
            }
            while s.get(i) == Some(&' ') {
                i += 1;
            }
        }
        Self(keys)
    }

    /// Whether `c`, typed with `before` the line's text before the cursor,
    /// has Neovim indent the line again: as `in_cinkeys` has it, over the
    /// line as it is once `c` has gone in.
    fn indents(&self, c: char, before: &str, facts: &Facts) -> bool {
        let blank = |c: char| matches!(c, ' ' | '\t');
        let in_indent = before.chars().all(blank);
        self.0.iter().any(|key| match key {
            Key::Char { c: k, first } => *k == c && (!first || in_indent),
            Key::Colon => c == ':',
            Key::Else => c == 'e' && before.trim_start_matches(blank) == "els",
            Key::Word { word, icase, first } => {
                let typed = format!("{before}{c}");
                let n = word.len();
                let Some(start) = typed.len().checked_sub(n) else {
                    return false;
                };
                let Some(tail) = typed.get(start..) else {
                    return false;
                };
                let same = if *icase {
                    tail.eq_ignore_ascii_case(word)
                } else {
                    tail == word
                };
                // A word of its own: at the line's start, or after what is
                // not a word's character — the byte before it, as Neovim
                // looks at it.
                let alone = start == 0
                    || facts.keyword(char::from(typed.as_bytes()[start - 1])) != Some(true);
                same && alone && (!first || typed[..start].chars().all(blank))
            }
        })
    }
}

/// What a key typed in Insert mode does, where it does only what typing it
/// plainly does: a character goes in, or the one before the cursor comes out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    Char(char),
    Rub,
}

impl Edit {
    /// The edit a key Neovim's notation writes as `key` makes: a character
    /// on its own, other than a control character — or `<Space>`, `<lt>`,
    /// `<Bar>` or `<Bslash>` for one — or `<BS>` and `<C-h>`. `None` for any
    /// other.
    pub fn of(key: &str) -> Option<Self> {
        let mut cs = key.chars();
        if let (Some(c), None) = (cs.next(), cs.next()) {
            return (!c.is_control()).then_some(Edit::Char(c));
        }
        match key.to_ascii_lowercase().as_str() {
            "<space>" => Some(Edit::Char(' ')),
            "<lt>" => Some(Edit::Char('<')),
            "<bar>" => Some(Edit::Char('|')),
            "<bslash>" => Some(Edit::Char('\\')),
            "<bs>" | "<c-h>" => Some(Edit::Rub),
            _ => None,
        }
    }
}

/// How a key typed in Normal mode enters Insert mode, where it does nothing
/// more: `i` before the cursor's character, `a` after it, `A` at the line's
/// end, `I` before its first character other than a blank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enter {
    Before,
    After,
    End,
    First,
}

impl Enter {
    /// The way of entering Insert mode a key Neovim's notation writes as
    /// `key` takes, if it is one of those.
    pub fn of(key: &str) -> Option<Self> {
        Some(match key {
            "i" => Enter::Before,
            "a" => Enter::After,
            "A" => Enter::End,
            "I" => Enter::First,
            _ => return None,
        })
    }
}

/// Where Insert mode entered as `e` puts the cursor, from byte `byte` of a
/// line of `text` in Normal mode, in a window of `shape`: its byte, and the
/// display column it is drawn in and wants (see [`sits`]) — as Neovim 0.12.6
/// was found to put it. `a` goes past the cursor's character and its
/// combining marks, a tab or a character two cells wide whole, and nowhere
/// on an empty line; `I` to the line's end on a line all blanks.
pub fn entered(e: Enter, text: &str, shape: &Shape, byte: usize) -> (usize, usize) {
    let cs = predict::chars(text, shape);
    let at = match e {
        Enter::Before => byte,
        Enter::After => cs
            .iter()
            .find(|ch| ch.byte > byte)
            .map_or(text.len(), |ch| ch.byte),
        Enter::End => text.len(),
        Enter::First => cs
            .iter()
            .find(|ch| !matches!(ch.c, ' ' | '\t'))
            .map_or(text.len(), |ch| ch.byte),
    };
    (at, sits(text, shape, at))
}

/// Where `<Esc>` puts the cursor, leaving Insert mode at byte `byte` of a
/// line of `text`, in a window of `shape`: back on the character before it
/// — where the cursor is not at the line's start — its byte, and the
/// display column it is drawn in there in Normal mode, and wants: a tab's
/// last, as Neovim 0.12.6 was found to put it.
pub fn left(text: &str, shape: &Shape, byte: usize) -> (usize, usize) {
    let cs = predict::chars(text, shape);
    let at = cs
        .iter()
        .rfind(|ch| ch.byte < byte)
        .or(cs.first())
        .map_or(0, |ch| ch.byte);
    (at, super::moves::drawn(text, shape, at))
}

/// Why `<Esc>`, leaving Insert mode at byte `byte` of a line of `text`,
/// might do more than put the cursor back a character, as the buffer's
/// `facts` say: `None` where it will not. An abbreviation before the cursor
/// is expanded; the blanks of an indent nothing was typed after, which
/// `'autoindent'` and an untouched comment leader leave, are taken out —
/// which the client cannot tell from blanks typed, and so takes for those.
pub fn unleft(text: &str, byte: usize, facts: &Facts) -> Option<&'static str> {
    let Some(before) = text.get(..byte) else {
        return Some("the cursor inside a character");
    };
    let blank = |c: char| matches!(c, ' ' | '\t');
    if (!text.is_empty() && text.chars().all(blank)) || before.ends_with(blank) {
        return Some("blanks before the cursor it may take out");
    }
    if facts.abbrevs.iter().any(|a| before.ends_with(&**a)) {
        return Some("it ends an abbreviation");
    }
    None
}

/// A key as a mapping's first is compared with it: a character as itself,
/// whatever Neovim's notation writes it as; a name in `<…>` in any case.
fn same_key(a: &str, b: &str) -> bool {
    let norm = |k: &str| -> String {
        match Edit::of(k) {
            Some(Edit::Char(c)) => c.to_string(),
            _ => k.to_ascii_lowercase(),
        }
    };
    norm(a) == norm(b)
}

/// Why `key`, making `e` at byte `byte` of a line of `text` — the cursor on
/// display column `vcol` — in a window of `shape`, might not do there what
/// typing it plainly does, as the buffer's `facts` say: `None` where it
/// will. Each as Neovim 0.12 was found to do.
pub fn refused(
    key: &str,
    e: Edit,
    (text, byte, vcol): (&str, usize, usize),
    facts: &Facts,
    shape: &Shape,
) -> Option<&'static str> {
    let f = facts;
    if !f.from_insert {
        return Some("the buffer not yet entered in Insert mode");
    }
    if !(f.editable && f.plain) || f.lisp || f.autocomplete || f.conceals.contains('i') {
        return Some("text typed otherwise in the buffer");
    }
    if f.starts(key) {
        return Some("a mapping starts with it");
    }
    let Some(before) = text.get(..byte) else {
        return Some("the cursor inside a character");
    };
    let blank = |c: char| matches!(c, ' ' | '\t');
    match e {
        Edit::Char(c) => {
            // One or two cells wide to Neovim as to the client: no
            // character but ASCII where Neovim may take one for another
            // width than the client does, and none of the emoji, and the
            // symbols drawn as emoji or not by what comes after them.
            let w = c.width().unwrap_or(0);
            let widths = shape.stepping.as_ref().is_none_or(|s| s.widths);
            let emoji = matches!(c,
                '\u{1f000}'..='\u{1faff}' | '\u{2600}'..='\u{27bf}' | '\u{2b00}'..='\u{2bff}');
            if !(1..=2).contains(&w) || c.is_control() || (!c.is_ascii() && (widths || emoji)) {
                return Some("a character Neovim may not draw as the client does");
            }
            // An abbreviation is expanded by a character not of a word
            // typed after it — whatever comes before it, as far as the
            // client goes.
            if f.keyword(c) != Some(true) && f.abbrevs.iter().any(|a| before.ends_with(&**a)) {
                return Some("it ends an abbreviation");
            }
            // A character other than a blank that would end past
            // `'textwidth'` breaks the line, where `'formatoptions'` has it
            // broken — in a comment only, for `c`, which the client does
            // not tell apart; and `a` formats the paragraph at every key.
            // Neovim measures the cursor's column without `'list'`, a tab
            // drawn `^I` as wide as a tab, which the client does not.
            let formats = f.tw > 0 && f.fo.contains(['t', 'c']) && !blank(c);
            let unlisted = shape.list && shape.tab.is_empty() && before.contains('\t');
            if f.fo.contains('a') || (formats && (unlisted || vcol + w > f.tw)) {
                return Some("it would break the line at 'textwidth'");
            }
            let si = f.smartindent && matches!(c, '{' | '}' | '#') && before.chars().all(blank);
            if si || f.indentkeys.indents(c, before, f) {
                return Some("it has the line indented again");
            }
            if f.showmatch && f.matchpairs.iter().any(|(_, close)| *close == c) {
                return Some("it shows the bracket it closes");
            }
            None
        }
        Edit::Rub => {
            let Some(gone) = before.chars().next_back() else {
                return Some("it would join the line to the one before");
            };
            let bs = &f.bs;
            if !(bs.contains("start") || bs.contains("nostop") || matches!(&**bs, "2" | "3")) {
                return Some("'backspace' may not take out what was there before");
            }
            if blank(gone) {
                if f.softtabs || (f.smarttab && before.chars().all(blank)) {
                    return Some("a softtabstop's worth of blanks may come out");
                }
            } else if f.delcombine && gone.width() == Some(0) {
                return Some("a combining mark comes out alone");
            }
            None
        }
    }
}

/// Where the cursor goes in Insert mode once `e` is made at byte `byte` of a
/// line of `text`, and the line's text then: past a character typed, or
/// back over the one taken out — a character with its combining marks, as
/// Neovim takes it out without `'delcombine'`. `None` where there is no
/// character before the cursor to take out.
pub fn edited(e: Edit, text: &str, byte: usize) -> Option<(String, usize)> {
    let (before, after) = (text.get(..byte)?, text.get(byte..)?);
    match e {
        Edit::Char(c) => Some((format!("{before}{c}{after}"), byte + c.len_utf8())),
        Edit::Rub => {
            // Back over the marks, and the character they go with.
            let at = before
                .char_indices()
                .rev()
                .find(|(_, c)| c.width() != Some(0))
                .map(|(i, _)| i)?;
            Some((format!("{}{after}", &before[..at]), at))
        }
    }
}

/// Where the cursor is drawn in Insert mode at byte `byte` of a line of
/// `text`, in display columns: on its character's first column — a tab's
/// first, unlike Normal mode — or just past the line's end.
pub fn sits(text: &str, shape: &Shape, byte: usize) -> usize {
    predict::chars(text, shape)
        .iter()
        .find(|ch| ch.byte >= byte)
        .map_or_else(|| predict::width(text, shape), |ch| ch.vcol)
}

/// Neovim's row for a line whose text was `old`, `row` its cells past the
/// gutter as Neovim drew them, as it is once the text is `new`, `put` bytes
/// taking the place of the bytes `from..from + gone` of `old` — with the
/// cursor on display column `cursor` — in a window of `shape`: the glyphs as
/// the client lays `new` out (see `predict::render`), `'listchars'` and tabs
/// as wide as they are there; the colours of the characters the two have
/// alike carried over from Neovim's cells for them, kind for kind — a
/// character drawn as itself in both, or as the same kind of glyph of
/// `'list'`'s — those of a glyph that is new in the colours of the row's own
/// of its kind, or `look`'s, and those of text that is new as [`colour`] has
/// them; and past the end, what Neovim drew there as [`Past`] tells it
/// apart: what moves with the line's end — virtual text after it — moved
/// with it, and the rest — the row's fill, a bar of `'cursorline'`, the
/// columns `'colorcolumn'` colours — left in the window's columns, where
/// Neovim draws it whatever the text.
///
/// `None` where `old` is not what the row shows, as the client lays it out,
/// or where `new` would not be drawn on the row as `old` was: in a window
/// that wraps, a line going on to the next row, or the cursor going there
/// at the end of a line that fills its row; in one that does not, a view
/// Neovim would move sideways to keep the cursor in it, or one it has. And
/// where the client cannot say what Neovim draws of it: a column of
/// `'colorcolumn'` among the characters the edit moves or makes, whose
/// colours there are the column's and the character's at once; past the
/// end, a column of it drawn other than blank (see [`Past::of`]), what moves
/// with the end going over such a column or from one, or going back to show
/// more of itself than the row had room for.
pub fn relayout(
    row: &[Cell],
    (old, new): (&str, &str),
    (from, gone, put): (usize, usize, usize),
    cursor: usize,
    (shape, look, facts): (&Shape, &Look, &Facts),
) -> Option<Vec<Cell>> {
    let room = row.len();
    if shape.leftcol > 0
        || (shape.wrap && (predict::reach(new, shape) > room || cursor >= room))
        || predict::showing(new, shape, cursor, 0, room + shape.textoff) != 0
    {
        return None;
    }
    // Laid out with an id of their own for each kind of glyph — text, and
    // what `'list'` draws, and Neovim's own forms of what it shows as
    // something else — so as to tell each cell's kind.
    let kinds = Look {
        text: TEXT,
        whitespace: WHITESPACE,
        nontext: NONTEXT,
        specialkey: SPECIALKEY,
        ..Look::default()
    };
    let lay = |text: &str| {
        let mut cells = Vec::with_capacity(room);
        predict::line(text, &[], shape, &kinds, room, &mut cells);
        cells
    };
    let (was, mut out) = (lay(old), lay(new));
    let (old_end, new_end) = (was.len(), out.len());
    if row.len() < old_end || row.iter().zip(&was).any(|(a, b)| a.text != b.text) {
        return None;
    }
    let cc = colorcolumn(shape);
    let edited = sits(old, shape, from);
    if cc
        .iter()
        .any(|c| (edited..old_end.max(new_end)).contains(c))
    {
        return None;
    }
    let (old_w, new_w) = (predict::width(old, shape), predict::width(new, shape));
    let past = Past::of(row, old_end, cc)?;
    let mut colours: Vec<Option<u32>> = vec![None; new_end];
    // A character of `new` before the edit, or after it, is the one of
    // `old`'s there: its cells, kind for kind, as Neovim drew them — a tab
    // grown or shrunk as its first and last.
    let old_cs = predict::chars(old, shape);
    for ch in predict::chars(new, shape) {
        let then = match ch.byte {
            b if b < from => b,
            b if b >= from + put => b - put + gone,
            _ => continue,
        };
        let Some(o) = old_cs.iter().find(|o| o.byte == then) else {
            continue;
        };
        for i in 0..ch.width {
            let (n, k) = (ch.vcol + i, o.vcol + i.min(o.width.saturating_sub(1)));
            if let (Some(cell), Some(drawn)) = (out.get(n), was.get(k)) {
                if cell.hl == drawn.hl {
                    colours[n] = Some(row[k].hl);
                }
            }
        }
    }
    // The line's end, where `'listchars'` draws one, is its own as well.
    if new_w < new_end && old_w < old_end {
        colours[new_w] = Some(row[old_w].hl);
    }
    // Past the end, what Neovim drew there: what moves with the end moved
    // with it, a character two cells wide cut by the row's end not one to
    // draw half of — and not where it was cut, should it go back, nor over
    // a column of `'colorcolumn'`'s, or from one; and the rest of the row in
    // its fill, but for the columns `'colorcolumn'` colours, in theirs.
    let (run, to) = (past.run.clone(), new_end);
    if (run.end == room && to < run.start)
        || cc
            .iter()
            .any(|c| run.contains(c) || (to..to + run.len()).contains(c))
    {
        return None;
    }
    let coloured = cc
        .iter()
        .filter(|c| **c >= old_end && !run.contains(c))
        .find_map(|c| row.get(*c))
        .map(|c| c.hl);
    let blank = |hl: Option<u32>| {
        hl.map(|hl| Cell {
            text: Text::Char(' '),
            hl,
        })
    };
    for n in new_end..room {
        out.push(if (to..to + run.len()).contains(&n) {
            let cell = &row[run.start + n - to];
            let cut = (n == to && cell.text == Text::Half)
                || (n + 1 == room
                    && row
                        .get(run.start + n - to + 1)
                        .is_some_and(|c| c.text == Text::Half));
            if cut {
                blank(Some(cell.hl))?
            } else {
                cell.clone()
            }
        } else if cc.contains(&n) {
            blank(coloured)?
        } else {
            blank(past.fill)?
        });
    }
    // What has no colour carried over: a glyph in the colours of the row's
    // own of its kind, or the window's; text by its neighbours, left to
    // right, the right half of a character two cells wide as its left. Not
    // by a cell of a column of `'colorcolumn'`'s, whose colours are the
    // column's as well.
    let own = |kind: u32| -> u32 {
        let window = match kind {
            WHITESPACE => look.whitespace,
            NONTEXT => look.nontext,
            _ => look.specialkey,
        };
        (0..old_end)
            .find(|k| was[*k].hl == kind && !cc.contains(k))
            .map_or(window, |k| row[k].hl)
    };
    let kind: Vec<u32> = out[..new_end].iter().map(|c| c.hl).collect();
    let mut unset = vec![false; new_end];
    for n in 0..new_end {
        match (colours[n], kind[n]) {
            (Some(hl), _) => out[n].hl = hl,
            (None, TEXT) => unset[n] = true,
            (None, k) => out[n].hl = own(k),
        }
    }
    let new_cs = predict::chars(new, shape);
    let fill = past.fill.unwrap_or(look.text);
    for n in 0..new_end {
        if !unset[n] {
            continue;
        }
        let hl = if out[n].text == Text::Half && n > 0 {
            out[n - 1].hl
        } else {
            let at = new_cs.iter().rposition(|ch| ch.vcol <= n);
            let c = at.map_or(' ', |i| new_cs[i].c);
            let left_char = at.and_then(|i| i.checked_sub(1)).map(|i| new_cs[i].c);
            // A neighbour to go by: text, not a glyph of `'list'`'s — a
            // blank, a tab, which take the window's colours for them — nor
            // a cell of a column of `'colorcolumn'`'s, nor text yet to be
            // coloured.
            let by = |k: usize| {
                let glyph = kind.get(k).is_some_and(|k| *k != TEXT);
                let later = unset.get(k).copied().unwrap_or(false);
                (!glyph && !later && !cc.contains(&k)).then(|| &out[k])
            };
            let left = n.checked_sub(1).and_then(by);
            let right = (n + 1..out.len())
                .find(|&r| out[r].text != Text::Half)
                .and_then(by);
            colour((left, c, right), left_char, fill, facts)
        };
        out[n].hl = hl;
        unset[n] = false;
    }
    Some(out)
}

/// What Neovim drew on a line's row past the line's end, as far as the
/// client tells it apart, and as Neovim 0.12.6 was found to draw it: the
/// row's fill, in the colours of most of its blanks — the bar of
/// `'cursorline'` across the cursor's line among them — and, blank, the
/// columns `'colorcolumn'` colours, both of them the window's whatever the
/// text; and from the line's end to the last of anything else, what moves
/// with the end — virtual text after it, its own blanks and the fill between
/// them and all — but for virtual text drawn in a column of the window's,
/// which the agent says of instead (see `super::predict::Predictor::pinned`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Past {
    /// The fill's colours: `None` where the row has no blank past the end
    /// but in a column of `'colorcolumn'`'s.
    fill: Option<u32>,
    /// The columns of what moves with the line's end.
    run: std::ops::Range<usize>,
}

impl Past {
    /// What Neovim drew on `row` from column `end`, the first past the
    /// line's text and its `eol` glyph, on, in a window whose
    /// `'colorcolumn'` colours columns `cc`: `None` where something not
    /// blank is drawn in one of those columns.
    fn of(row: &[Cell], end: usize, cc: &[usize]) -> Option<Self> {
        let blank = |c: &Cell| c.text == Text::Char(' ');
        let mut counts: Vec<(u32, usize)> = Vec::new();
        for (n, c) in row.iter().enumerate().skip(end) {
            if blank(c) && !cc.contains(&n) {
                match counts.iter_mut().find(|(hl, _)| *hl == c.hl) {
                    Some((_, k)) => *k += 1,
                    None => counts.push((c.hl, 1)),
                }
            }
        }
        let fill = counts.iter().max_by_key(|(_, k)| *k).map(|(hl, _)| *hl);
        let moves = |n: usize| {
            !cc.contains(&n) && row.get(n).is_some_and(|c| !blank(c) || Some(c.hl) != fill)
        };
        let stop = (end..row.len())
            .rev()
            .find(|n| moves(*n))
            .map_or(end, |n| n + 1);
        (stop..row.len())
            .all(|n| !cc.contains(&n) || blank(&row[n]))
            .then_some(Self {
                fill,
                run: end.min(stop)..stop,
            })
    }
}

/// The display columns `'colorcolumn'` colours in a window of `shape`, as
/// the agent says them.
fn colorcolumn(shape: &Shape) -> &[usize] {
    shape
        .stepping
        .as_ref()
        .map_or(&[], |s| s.colorcolumn.as_slice())
}

/// The ids [`relayout`] lays a line out with, for the kinds of its glyphs:
/// none an id the client gives a highlight of Neovim's, which count up from
/// one and come nowhere near.
const TEXT: u32 = u32::MAX - 3;
const WHITESPACE: u32 = u32::MAX - 2;
const NONTEXT: u32 = u32::MAX - 1;
const SPECIALKEY: u32 = u32::MAX;

/// The colour a character `c` typed is drawn in till Neovim draws it — a
/// guess, and never judged: its neighbours' where the two have the same,
/// inside a string or a comment; the one's before it where both are
/// characters of a word, carrying on a name; else the colour Neovim fills
/// the row with past the line's end, `fill`.
fn colour(
    (left, c, right): (Option<&Cell>, char, Option<&Cell>),
    left_char: Option<char>,
    fill: u32,
    facts: &Facts,
) -> u32 {
    let word = |c: char| facts.keyword(c).unwrap_or(true);
    match (left, right) {
        (Some(l), Some(r)) if l.hl == r.hl => l.hl,
        (Some(l), _) if word(c) && left_char.is_some_and(word) => l.hl,
        _ => fill,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::predict::{Cursorline, Listchars, Stepping};

    /// A window 20 columns wide with no gutter, `'wrap'` as `wrap` says, a
    /// tab 8 wide, nothing that takes a character for another width.
    fn shape(wrap: bool) -> Shape {
        Shape {
            buf: 1,
            line_count: 10,
            textoff: 0,
            tabstop: 8,
            number: false,
            relativenumber: false,
            plain_gutter: true,
            wrap,
            leftcol: 0,
            predictable: true,
            scrolloff: 0,
            step: 3,
            eob: Text::Char('~'),
            list: false,
            tab: Vec::new(),
            lcs: Listchars::default(),
            checks: false,
            scroll: 5,
            page: 0,
            startofline: false,
            mapped: Vec::new(),
            imapped: Vec::new(),
            sidescrolloff: 0,
            sidescroll: 1,
            hor: 6,
            stepping: Some(Stepping {
                whichwrap: "b,s".into(),
                virtualedit: String::new(),
                cursorcolumn: false,
                bound: false,
                langmap: false,
                widths: false,
                numberwidth: 4,
                number_signs: false,
                cursorline: Cursorline::default(),
                idioms: Vec::new(),
                own_highlights: false,
                conceals: false,
                colorcolumn: Vec::new(),
                scrolljump: 1,
                concealing: false,
                classes: Vec::new(),
            }),
        }
    }

    /// [`shape`], with `'list'` and `'listchars'` as `tab`, `trail`, `space`
    /// and `eol` say.
    fn listing(tab: &str, trail: Option<char>, space: Option<char>, eol: Option<char>) -> Shape {
        Shape {
            list: true,
            tab: tab.chars().collect(),
            lcs: Listchars {
                trail,
                space,
                eol,
                ..Listchars::default()
            },
            ..shape(true)
        }
    }

    /// Neovim's defaults: nothing that types otherwise, `'iskeyword'` as it
    /// comes.
    fn facts() -> Facts {
        Facts {
            from_insert: true,
            firsts: Vec::new(),
            abbrevs: Vec::new(),
            unseen: false,
            unseen_entered: false,
            unseen_left: false,
            tw: 0,
            fo: "tcqj".into(),
            indentkeys: Indentkeys::default(),
            smartindent: false,
            lisp: false,
            showmatch: false,
            matchpairs: pairs("(:),{:},[:]"),
            autocomplete: false,
            editable: true,
            plain: true,
            bs: "indent,eol,start".into(),
            softtabs: false,
            smarttab: true,
            delcombine: false,
            conceals: "".into(),
            classes: (0u8..=255)
                .map(|b| match b {
                    0 | b'\t' | b' ' | 0xa0 => 0,
                    b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'_' | 192..=255 => 2,
                    _ => 1,
                })
                .collect(),
        }
    }

    /// The window's colours: the text in 1, `NonText` 2, `Whitespace` 3.
    fn look() -> Look {
        Look {
            text: 1,
            nontext: 2,
            whitespace: 3,
            specialkey: 4,
            ..Look::default()
        }
    }

    /// A row as Neovim draws it, 20 cells: `text`, a character a cell or
    /// two — `_` for the right half of a character two cells wide — each
    /// in the highlight `hls` has for it, a digit, and 1 past them.
    fn row(text: &str, hls: &str) -> Vec<Cell> {
        let mut hls = hls.chars().map(|h| h.to_digit(10).unwrap_or(1));
        let mut cells: Vec<Cell> = text
            .chars()
            .map(|c| Cell {
                text: if c == '_' { Text::Half } else { Text::Char(c) },
                hl: hls.next().unwrap_or(1),
            })
            .collect();
        cells.resize(
            20,
            Cell {
                text: Text::Char(' '),
                hl: 1,
            },
        );
        cells
    }

    /// The text of a row, `_` for the right half of a character two cells
    /// wide, and its highlights as digits, past the blanks at its end in
    /// the window's.
    fn drawn(cells: &[Cell]) -> (String, String) {
        let text: String = cells
            .iter()
            .map(|c| match &c.text {
                Text::Half => '_',
                Text::Char(c) => *c,
                Text::Cluster(s) => s.chars().next().unwrap_or(' '),
            })
            .collect();
        let end = text.trim_end().chars().count();
        let hls: String = cells[..end]
            .iter()
            .map(|c| char::from_digit(c.hl, 10).unwrap_or('?'))
            .collect();
        (text.trim_end().to_string(), hls)
    }

    /// `keys` typed at byte `byte` of a line of `text`, in a window of
    /// `shape` whose row Neovim drew as `cells`: each key's row, laid out
    /// from the one before, and `None` from the first that is not drawn on
    /// it as the line was.
    fn typed(
        shape: &Shape,
        (text, byte): (&str, usize),
        cells: Vec<Cell>,
        keys: &[&str],
    ) -> Vec<Option<(String, String)>> {
        let (mut text, mut byte, mut cells) = (text.to_string(), byte, Some(cells));
        let f = facts();
        keys.iter()
            .map(|key| {
                let e = Edit::of(key)?;
                let (new, to) = edited(e, &text, byte)?;
                let (at, gone, put) = match e {
                    Edit::Char(c) => (byte, 0, c.len_utf8()),
                    Edit::Rub => (to, byte - to, 0),
                };
                let cursor = sits(&new, shape, to);
                let laid = relayout(
                    cells.as_ref()?,
                    (&text, &new),
                    (at, gone, put),
                    cursor,
                    (shape, &look(), &f),
                );
                cells = laid.clone();
                (text, byte) = (new, to);
                laid.map(|cells| drawn(&cells))
            })
            .collect()
    }

    fn seen(text: &str, hls: &str) -> Option<(String, String)> {
        Some((text.into(), hls.into()))
    }

    /// Text typed before a tab takes it in: the tab grows narrower, and what
    /// comes after it stays where it was — `b` at the tab stop — in the
    /// colours Neovim drew it in, as Neovim 0.12.6 was found to draw it.
    #[test]
    fn a_tab_after_the_cursor_takes_in_what_is_typed() {
        let rows = typed(
            &shape(true),
            ("a\tb", 1),
            row("a       b", "511111115"),
            &["x", "y"],
        );
        assert_eq!(
            rows,
            [
                seen("ax      b", "551111115"),
                seen("axy     b", "555111115")
            ]
        );
    }

    /// With `'list'`, a blank typed at a line's end is drawn as `trail`
    /// says, in `Whitespace`, before `eol`; and one typed after it turns the
    /// blanks before it back into blanks. `<BS>` brings the `trail` back.
    #[test]
    fn listchars_are_drawn_as_neovim_draws_them_as_text_is_typed() {
        let shape = listing(">-", Some('-'), None, Some('$'));
        let rows = typed(
            &shape,
            ("ab", 2),
            row("ab$", "112"),
            &[" ", " ", "z", "<BS>", "<BS>"],
        );
        assert_eq!(
            rows,
            [
                seen("ab-$", "1132"),
                seen("ab--$", "11332"),
                seen("ab  z$", "111112"),
                seen("ab--$", "11332"),
                seen("ab-$", "1132"),
            ]
        );
        // A tab drawn as `tab`, narrower as text goes in before it, and a
        // blank as `space`.
        let shape = listing(">-", None, Some('.'), None);
        let rows = typed(
            &shape,
            ("a\tb", 1),
            row("a>------b", "133333331"),
            &["x", "y", " "],
        );
        assert_eq!(
            rows,
            [
                seen("ax>-----b", "113333331"),
                seen("axy>----b", "111333331"),
                seen("axy.>---b", "111333331"),
            ]
        );
    }

    /// A character two cells wide goes in as both its halves, and comes
    /// out whole.
    #[test]
    fn a_character_two_cells_wide_goes_in_and_out_whole() {
        let rows = typed(&shape(true), ("ab", 1), row("ab", "11"), &["日", "<BS>"]);
        assert_eq!(rows, [seen("a日_b", "1111"), seen("ab", "11")]);
    }

    /// Virtual text Neovim draws past a line's end goes with the end, in its
    /// own colours.
    #[test]
    fn what_is_drawn_past_the_end_moves_with_it() {
        let rows = typed(
            &shape(true),
            ("abc", 3),
            row("abc <-E", "1111666"),
            &["x", "y", "<BS>"],
        );
        assert_eq!(
            rows,
            [
                seen("abcx <-E", "11111666"),
                seen("abcxy <-E", "111111666"),
                seen("abcx <-E", "11111666"),
            ]
        );
    }

    /// `key` typed at byte `byte` of a line of `text`, in a window of `shape`
    /// whose row Neovim drew as `cells`: the row laid out, every cell's
    /// highlight a digit, or `None` where it is not drawn ahead.
    fn laid(
        shape: &Shape,
        (text, byte): (&str, usize),
        cells: &[Cell],
        key: &str,
    ) -> Option<String> {
        let e = Edit::of(key)?;
        let (new, to) = edited(e, text, byte)?;
        let (at, gone, put) = match e {
            Edit::Char(c) => (byte, 0, c.len_utf8()),
            Edit::Rub => (to, byte - to, 0),
        };
        let cursor = sits(&new, shape, to);
        let cells = relayout(
            cells,
            (text, &new),
            (at, gone, put),
            cursor,
            (shape, &look(), &facts()),
        )?;
        Some(
            cells
                .iter()
                .map(|c| char::from_digit(c.hl, 10).unwrap_or('?'))
                .collect(),
        )
    }

    /// [`shape`], with `'colorcolumn'` colouring display columns `cc`.
    fn coloured(cc: &[usize]) -> Shape {
        let mut s = shape(true);
        if let Some(stepping) = s.stepping.as_mut() {
            stepping.colorcolumn = cc.to_vec();
        }
        s
    }

    /// `'colorcolumn'` colours columns of the window, not characters: past
    /// the line's end, the column stays where it is, in Neovim's colours for
    /// it, however much is typed or taken out before it — with virtual text
    /// at the end moving short of it; and an edit that would move a
    /// character into the column, or out of it, or the virtual text over
    /// it, is not drawn ahead, as Neovim 0.12.6 was found to draw them.
    #[test]
    fn colorcolumn_stays_in_its_columns() {
        let cc = coloured(&[12]);
        let drawn = row("foobar       ", "1111111111117");
        for key in ["x", "<BS>"] {
            assert_eq!(
                laid(&cc, ("foobar", 6), &drawn, key).as_deref(),
                Some("11111111111171111111"),
                "{key}"
            );
        }
        // Virtual text at the end goes with it, short of the column; not
        // over it.
        let vt = row("foo <-E      ", "1111666111117");
        assert_eq!(
            laid(&cc, ("foo", 3), &vt, "x").as_deref(),
            Some("11111666111171111111")
        );
        let vt = |cc: usize| {
            let mut hls = "1111111666111111".to_string();
            hls.replace_range(cc..=cc, "7");
            (
                coloured(&[cc]),
                row(&format!("foobar <-E{}", " ".repeat(cc - 9)), &hls),
            )
        };
        let (cc, drawn) = vt(11);
        assert_eq!(
            laid(&cc, ("foobar", 6), &drawn, "x").as_deref(),
            Some("11111111666711111111")
        );
        let (cc, drawn) = vt(10);
        assert_eq!(laid(&cc, ("foobar", 6), &drawn, "x"), None);
        // Inside the text, a column after the edit is moved over; one before
        // it is not.
        let cc = coloured(&[8]);
        let drawn = row("foobarbazqux", "111111117111");
        assert_eq!(laid(&cc, ("foobarbazqux", 1), &drawn, "x"), None);
        assert_eq!(
            laid(&cc, ("foobarbazqux", 12), &drawn, "x").as_deref(),
            Some("11111111711111111111")
        );
        // A character typed next to the column does not take its colours.
        let cc = coloured(&[2]);
        let drawn = row("abcd", "1171");
        assert_eq!(
            laid(&cc, ("abcd", 4), &drawn, "e").as_deref(),
            Some("11711111111111111111")
        );
    }

    /// What Neovim draws past a line's end moves with the end, from it to
    /// the last of it, the fill between and all — a diagnostic's virtual
    /// text, its spacing the fill's, as Neovim 0.12.6 draws it — and the
    /// rest of the row stays the fill; but virtual text cut by the row's end
    /// is not drawn going back over what it did not show, nor is anything
    /// drawn in a column of `'colorcolumn'`'s past the end, which is not
    /// the column's.
    #[test]
    fn what_is_drawn_past_the_end_moves_with_the_end_as_a_whole() {
        let plain = shape(true);
        let diagnostic = row("abc     E msg  W", "1111111166666116");
        assert_eq!(
            laid(&plain, ("abc", 3), &diagnostic, "x").as_deref(),
            Some("11111111166666116111")
        );
        assert_eq!(
            laid(&plain, ("abc", 3), &diagnostic, "<BS>").as_deref(),
            Some("11111116666611611111")
        );
        let cut = row("abc 0123456789abcdef", "11116666666666666666");
        assert!(laid(&plain, ("abc", 3), &cut, "x").is_some());
        assert_eq!(laid(&plain, ("abc", 3), &cut, "<BS>"), None);
        let over = row("abc      |", "1111111116");
        assert_eq!(laid(&coloured(&[9]), ("abc", 3), &over, "x"), None);
    }

    /// Text typed between glyphs of `'list'`'s takes the colours of the
    /// row's fill, not the glyphs' `Whitespace`; and one typed before the
    /// `eol` glyph, the fill past it, not its `NonText`.
    #[test]
    fn text_typed_between_glyphs_is_not_coloured_as_them() {
        let shape = listing(">-", None, Some('.'), None);
        assert_eq!(
            laid(&shape, (" \tb", 1), &row(".>------b", "333333331"), "q").as_deref(),
            Some("31333333111111111111")
        );
        let shape = listing("", None, None, Some('$'));
        assert_eq!(
            laid(&shape, ("a.", 2), &row("a.$", "152"), ",").as_deref(),
            Some("15121111111111111111")
        );
    }

    /// In a window that wraps, a character that fills the row's last column
    /// at the line's end sends the cursor to the next row, as Neovim draws
    /// it, and is not drawn ahead — unless the line's end, drawn, fills it;
    /// in one that does not, one that would move the view sideways is not
    /// either, `'sidescrolloff'` from its edge.
    #[test]
    fn a_key_that_would_wrap_or_move_the_view_is_not_drawn() {
        let full = "0123456789abcdef";
        let rows = typed(
            &shape(true),
            (full, 16),
            row(full, ""),
            &["g", "h", "i", "j"],
        );
        assert!(rows[2].is_some() && rows[3].is_none(), "{rows:?}");
        let eol = listing("", None, None, Some('$'));
        let drawn_eol = row("0123456789abcdef$", "11111111111111112");
        let rows = typed(&eol, (full, 16), drawn_eol, &["g", "h", "i", "j"]);
        assert_eq!(
            rows[2],
            seen("0123456789abcdefghi$", "11111111111111111112")
        );
        assert!(rows[3].is_none(), "the next goes on to the next row");
        let rows = typed(
            &shape(false),
            (full, 16),
            row(full, ""),
            &["g", "h", "i", "j"],
        );
        assert!(rows[2].is_some() && rows[3].is_none(), "{rows:?}");
        let siso = Shape {
            sidescrolloff: 3,
            ..shape(false)
        };
        let rows = typed(
            &siso,
            ("0123456789ab", 12),
            row("0123456789ab", ""),
            &["c", "d", "e", "f", "g"],
        );
        assert!(rows[3].is_some() && rows[4].is_none(), "{rows:?}");
    }

    /// Whether `key` typed at the end of `before`, the line's text up to
    /// the cursor, is refused, as the buffer's `f` and a window of
    /// [`shape`] have it.
    fn refuses(f: &Facts, key: &str, before: &str) -> bool {
        let shape = shape(true);
        let e = Edit::of(key).expect("an edit");
        let text = format!("{before}rest");
        let vcol = sits(&text, &shape, before.len());
        refused(key, e, (&text, before.len(), vcol), f, &shape).is_some()
    }

    /// The keys that have Neovim indent the line again, as `in_cinkeys`
    /// finds them in `'cinkeys'` and `'indentkeys'`, and as Neovim 0.12.6
    /// was found to: C's `}`, `#` and `{` as a line's first character but
    /// not after another, `else` completed and `:`; Lua's `end` completed
    /// with only blanks before it; Python's `elif` completed after anything
    /// but a character of a word.
    #[test]
    fn keys_that_indent_the_line_again_are_left_to_neovim() {
        let c = Facts {
            indentkeys: Indentkeys::parse("0{,0},0),0],:,0#,!^F,o,O,e"),
            ..facts()
        };
        for (key, before, refused) in [
            ("}", "        ", true),
            ("}", "        x", false),
            ("#", "    ", true),
            ("#", "    x", false),
            ("{", "", true),
            ("e", "            els", true),
            ("e", "    x els", false),
            ("e", "    tel", false),
            (":", "    case 1", true),
            (":", "    a", true),
            ("x", "    ", false),
        ] {
            assert_eq!(
                refuses(&c, key, before),
                refused,
                "C: {key:?} after {before:?}"
            );
        }
        let lua = Facts {
            indentkeys: Indentkeys::parse("0{,0},0),0],:,0#,!^F,o,O,e,0=end,0=until"),
            ..facts()
        };
        for (key, before, refused) in [
            ("d", "  en", true),
            ("d", "  x en", false),
            ("d", "  xen", false),
            ("l", "  unti", true),
            ("n", "  e", false),
        ] {
            assert_eq!(
                refuses(&lua, key, before),
                refused,
                "Lua: {key:?} after {before:?}"
            );
        }
        let python = Facts {
            indentkeys: Indentkeys::parse("0{,0},0),0],:,!^F,o,O,e,<:>,=elif,=except"),
            ..facts()
        };
        for (key, before, refused) in [
            ("f", "        eli", true),
            ("f", "        z = eli", true),
            ("f", "        zeli", false),
            ("t", "    excep", true),
            (":", "x", true),
        ] {
            assert_eq!(
                refuses(&python, key, before),
                refused,
                "Python: {key:?} after {before:?}"
            );
        }
        let named = Indentkeys::parse("<Space>,0<lt>,<>>,=~END");
        let f = Facts {
            indentkeys: named,
            ..facts()
        };
        assert!(refuses(&f, " ", "x") && refuses(&f, "<lt>", " ") && !refuses(&f, "<lt>", "x"));
        assert!(refuses(&f, ">", "x") && refuses(&f, "D", "x en") && !refuses(&f, "d", "xen"));
    }

    /// A character not of a word typed after an abbreviation — whatever is
    /// before it — waits for Neovim, which may expand it; one of a word
    /// does not.
    #[test]
    fn a_key_after_an_abbreviation_is_left_to_neovim() {
        let f = Facts {
            abbrevs: vec!["teh".into(), "#i".into(), "def#".into()],
            ..facts()
        };
        for (key, before, refused) in [
            (" ", "teh", true),
            (".", ".teh", true),
            (" ", "xteh", true),
            ("x", "teh", false),
            (" ", "#i", true),
            (" ", "def#", true),
            ("x", "def#", false),
            (" ", "tex", false),
        ] {
            assert_eq!(
                refuses(&f, key, before),
                refused,
                "{key:?} after {before:?}"
            );
        }
    }

    /// A character other than a blank that would end past `'textwidth'`,
    /// where `'formatoptions'` breaks lines there, waits for Neovim; one
    /// that ends on it does not, nor a blank, as Neovim 0.12.6 was found
    /// to break them. `a` in `'formatoptions'` formats at every key.
    #[test]
    fn a_key_that_would_break_the_line_is_left_to_neovim() {
        let f = Facts {
            tw: 20,
            fo: "tcq".into(),
            ..facts()
        };
        assert!(!refuses(&f, "f", "one two three four "), "ends on the 20th");
        assert!(refuses(&f, "i", "one two three four f"), "the 21st");
        assert!(!refuses(&f, " ", "one two three four f"), "a blank");
        assert!(
            refuses(&f, "日", "one two three four "),
            "two cells to the 21st"
        );
        let comments = Facts {
            fo: "cq".into(),
            ..f.clone()
        };
        assert!(refuses(&comments, "i", "one two three four f"));
        let not = Facts {
            fo: "q".into(),
            ..f.clone()
        };
        assert!(!refuses(&not, "i", "one two three four f"));
        let auto = Facts {
            tw: 0,
            fo: "tcqa".into(),
            ..facts()
        };
        assert!(refuses(&auto, "x", ""));
    }

    /// With `'showmatch'`, a closing bracket of `'matchpairs'` sends the
    /// cursor to the one it closes for a while: left to Neovim, as is every
    /// key where text is typed otherwise in the buffer, or a mapping starts
    /// with it — compared as Neovim writes keys, a character as itself.
    #[test]
    fn keys_neovim_does_more_with_are_left_to_it() {
        let f = Facts {
            showmatch: true,
            ..facts()
        };
        assert!(refuses(&f, ")", "f(x") && refuses(&f, "]", "") && !refuses(&f, "(", "f"));
        for unlike in [
            Facts {
                from_insert: false,
                ..facts()
            },
            Facts {
                editable: false,
                ..facts()
            },
            Facts {
                plain: false,
                ..facts()
            },
            Facts {
                autocomplete: true,
                ..facts()
            },
            Facts {
                conceals: "ni".into(),
                ..facts()
            },
        ] {
            assert!(refuses(&unlike, "x", "a"), "{unlike:?}");
        }
        let mapped = Facts {
            firsts: vec!["(".into(), "<Space>".into(), "<lt>".into(), "<C-H>".into()],
            ..facts()
        };
        for (key, refused) in [
            ("(", true),
            (" ", true),
            ("<Space>", true),
            ("<lt>", true),
            ("<C-h>", true),
            ("<BS>", false),
            ("x", false),
        ] {
            assert_eq!(refuses(&mapped, key, "ab"), refused, "{key:?}");
        }
    }

    /// Characters Neovim may lay out otherwise than the client does are left
    /// to it: emoji, and any but ASCII where it may take a character for
    /// another width. The rest of those one or two cells wide are not.
    #[test]
    fn characters_of_widths_neovim_may_differ_on_are_left_to_it() {
        let f = facts();
        assert!(!refuses(&f, "ö", "a") && !refuses(&f, "日", "a"));
        assert!(refuses(&f, "😀", "a") && refuses(&f, "☺", "a"));
        let mut shape = shape(true);
        if let Some(s) = shape.stepping.as_mut() {
            s.widths = true;
        }
        let text = "arest";
        let e = Edit::of("ö").expect("an edit");
        assert!(refused("ö", e, (text, 1, 1), &f, &shape).is_some());
    }

    /// `<BS>` over blanks a softtabstop's worth at a time — `'softtabstop'`
    /// set, or in a line's leading blanks with `'smarttab'` — is left to
    /// Neovim, as is one over a combining mark with `'delcombine'`, at the
    /// start of a line, and before where Insert mode began without `start`
    /// in `'backspace'`. Over a character, or a blank mid-line, it is not.
    #[test]
    fn bs_over_more_than_a_character_is_left_to_neovim() {
        let sts = Facts {
            softtabs: true,
            ..facts()
        };
        assert!(
            refuses(&sts, "<BS>", "ab      "),
            "sts=4: ab      cd → ab  cd"
        );
        assert!(!refuses(&facts(), "<BS>", "ab      "), "sts=0: one blank");
        assert!(
            refuses(&facts(), "<BS>", "        "),
            "smarttab in the indent"
        );
        let nosta = Facts {
            smarttab: false,
            ..facts()
        };
        assert!(!refuses(&nosta, "<BS>", "        "));
        assert!(!refuses(&facts(), "<BS>", "ab") && !refuses(&sts, "<BS>", "ab"));
        assert!(refuses(&facts(), "<BS>", ""), "joins the line before");
        let bs = Facts {
            bs: "indent,eol".into(),
            ..facts()
        };
        assert!(refuses(&bs, "<BS>", "ab"));
        let dc = Facts {
            delcombine: true,
            ..facts()
        };
        assert!(refuses(&dc, "<C-h>", "e\u{301}") && !refuses(&facts(), "<C-h>", "e\u{301}"));
    }

    /// What a key does to the line: a character in before the cursor, and the
    /// cursor past it; or the character before it out — with its combining
    /// marks, two cells wide or not — and the cursor back over it.
    #[test]
    fn an_edit_puts_a_character_in_or_takes_one_out() {
        assert_eq!(edited(Edit::Char('x'), "ab", 1), Some(("axb".into(), 2)));
        assert_eq!(edited(Edit::Char('日'), "ab", 2), Some(("ab日".into(), 5)));
        assert_eq!(edited(Edit::Rub, "a日b", 4), Some(("ab".into(), 1)));
        assert_eq!(edited(Edit::Rub, "e\u{301}x", 3), Some(("x".into(), 0)));
        assert_eq!(edited(Edit::Rub, "ab", 0), None);
        for (key, e) in [
            ("a", Some(Edit::Char('a'))),
            (" ", Some(Edit::Char(' '))),
            ("<Space>", Some(Edit::Char(' '))),
            ("<lt>", Some(Edit::Char('<'))),
            ("<Bar>", Some(Edit::Char('|'))),
            ("<Bslash>", Some(Edit::Char('\\'))),
            ("<BS>", Some(Edit::Rub)),
            ("<C-h>", Some(Edit::Rub)),
            ("<CR>", None),
            ("<Tab>", None),
            ("<C-w>", None),
        ] {
            assert_eq!(Edit::of(key), e, "{key:?}");
        }
    }

    /// The agent's word on a buffer's Insert mode, read.
    #[test]
    fn facts_are_read_as_the_agent_sends_them() {
        use rmpv::Value;
        let classes: String = facts()
            .classes
            .iter()
            .map(|c| char::from(b'0' + c))
            .collect();
        let v: Vec<Value> = vec![
            7.into(),
            true.into(),
            Value::Array(vec!["(".into(), "<Space>".into()]),
            Value::Array(vec!["teh".into()]),
            true.into(),
            72.into(),
            "jtln".into(),
            "0=end".into(),
            false.into(),
            false.into(),
            true.into(),
            "(:),<:>".into(),
            false.into(),
            true.into(),
            true.into(),
            "2".into(),
            true.into(),
            false.into(),
            true.into(),
            "".into(),
            classes.into(),
        ];
        let refs: Vec<ValueRef<'_>> = v.iter().map(Value::as_ref).collect();
        let (buf, f) = Facts::read(&refs).expect("read");
        assert_eq!(buf, 7);
        assert_eq!(f.firsts, ["(".into(), "<Space>".into()] as [Box<str>; 2]);
        assert_eq!((f.tw, &*f.fo, &*f.bs), (72, "jtln", "2"));
        assert_eq!(f.matchpairs, [('(', ')'), ('<', '>')]);
        assert!(f.unseen && f.showmatch && f.softtabs && f.delcombine);
        assert!(!f.smarttab && f.conceals.is_empty() && f.editable && f.plain);
        assert_eq!(f.classes, facts().classes);
        assert_eq!(
            (f.keyword('a'), f.keyword('.'), f.keyword('日')),
            (Some(true), Some(false), None)
        );
    }

    /// The agent's word on autocommands of entering Insert mode and of
    /// leaving it, last of all it says of a buffer's Insert mode.
    #[test]
    fn what_entering_and_leaving_insert_mode_set_off_is_read() {
        use rmpv::Value;
        let said = |entered: bool, left: bool| {
            let mut v: Vec<Value> = vec![
                7.into(),
                true.into(),
                Value::Array(vec![]),
                Value::Array(vec![]),
                false.into(),
                0.into(),
                "tcqj".into(),
                "".into(),
                false.into(),
                false.into(),
                false.into(),
                "(:)".into(),
                false.into(),
                true.into(),
                true.into(),
                "indent,eol,start".into(),
                false.into(),
                true.into(),
                false.into(),
                "".into(),
                "".into(),
            ];
            v.extend([Value::from(entered), Value::from(left)]);
            let refs: Vec<ValueRef<'_>> = v.iter().map(Value::as_ref).collect();
            let (_, f) = Facts::read(&refs).expect("read");
            (f.unseen, f.unseen_entered, f.unseen_left)
        };
        assert_eq!(said(true, false), (false, true, false));
        assert_eq!(said(false, true), (false, false, true));
        assert_eq!(said(false, false), (false, false, false));
    }

    /// A window, a line's text, the byte the cursor is on in Normal mode, a
    /// key that enters Insert mode, and where it puts the cursor and `<Esc>`
    /// after it does, each a byte and a display column.
    type Case<'a> = (
        &'a Shape,
        &'a str,
        usize,
        &'a str,
        (usize, usize),
        (usize, usize),
    );

    /// Where `i`, `a`, `A` and `I` put the cursor, and `<Esc>` after them,
    /// as Neovim 0.12.6 was found to: by byte and display column, over a
    /// tab, characters two cells wide, one with a combining mark, an empty
    /// line and one all blanks — and with `'list'` drawing a tab, which has
    /// the cursor on its first cell in Normal mode as well.
    #[test]
    fn insert_mode_is_entered_and_left_where_neovim_puts_the_cursor() {
        let plain = shape(true);
        let listed = listing(">-", None, None, None);
        #[rustfmt::skip]
        let cases: &[Case] = &[
            (&plain, "\tfoo bar", 0, "i", (0, 0), (0, 7)),
            (&plain, "\tfoo bar", 0, "a", (1, 8), (0, 7)),
            (&plain, "\tfoo bar", 7, "a", (8, 15), (7, 14)),
            (&plain, "\tfoo bar", 0, "A", (8, 15), (7, 14)),
            (&plain, "\tfoo bar", 7, "I", (1, 8), (0, 7)),
            (&plain, "日本語", 6, "i", (6, 4), (3, 2)),
            (&plain, "日本語", 0, "a", (3, 2), (0, 0)),
            (&plain, "日本語", 0, "A", (9, 6), (6, 4)),
            (&plain, "", 0, "i", (0, 0), (0, 0)),
            (&plain, "", 0, "a", (0, 0), (0, 0)),
            (&plain, "", 0, "A", (0, 0), (0, 0)),
            (&plain, "", 0, "I", (0, 0), (0, 0)),
            (&plain, "    ", 0, "I", (4, 4), (3, 3)),
            (&plain, "    ", 0, "A", (4, 4), (3, 3)),
            (&plain, "    ", 1, "i", (1, 1), (0, 0)),
            (&plain, "e\u{301}x", 0, "a", (3, 1), (0, 0)),
            (&plain, "  ab", 3, "I", (2, 2), (1, 1)),
            (&listed, "\tfoo bar", 0, "i", (0, 0), (0, 0)),
            (&listed, "\tfoo bar", 0, "a", (1, 8), (0, 0)),
        ];
        for &(shape, text, byte, key, enter, leave) in cases {
            let e = Enter::of(key).expect("a key that enters Insert mode");
            let (at, vcol) = entered(e, text, shape, byte);
            assert_eq!((at, vcol), enter, "{key} at {byte} of {text:?}");
            assert_eq!(
                left(text, shape, at),
                leave,
                "<Esc> after {key} at {byte} of {text:?}"
            );
        }
    }

    /// `<Esc>` after an abbreviation, which it expands, or after blanks —
    /// an indent nothing was typed after, which it takes out, or a comment
    /// leader's — is left to Neovim; anywhere else, it is not.
    #[test]
    fn esc_that_may_do_more_is_left_to_neovim() {
        let f = Facts {
            abbrevs: vec!["teh".into()],
            ..facts()
        };
        for (text, byte, refused) in [
            ("hello", 5, false),
            ("hello", 2, false),
            ("", 0, false),
            ("    ", 4, true),
            ("    ", 0, true),
            ("-- ", 3, true),
            ("hello ", 6, true),
            ("teh", 3, true),
            ("tehx", 4, false),
        ] {
            assert_eq!(
                unleft(text, byte, &f).is_some(),
                refused,
                "{text:?} at {byte}"
            );
        }
    }
}
