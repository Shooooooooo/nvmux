//! Insert-mode typeahead over a slow link: what a key does to the cursor's
//! line, computed by the client before Neovim sees it (see `super::predict`).
//!
//! Most keys just insert a character (or `<BS>` deletes one) and move the
//! cursor. Anything more (a mapping or abbreviation, a re-indent, a
//! `'textwidth'` break, `'showmatch'`, a softtabstop delete) is reported per
//! buffer by the agent ([`Facts`]). If none applies to a key ([`refused`]),
//! the client draws the result at once, re-laying out Neovim's row around the
//! new text ([`relayout`]).
//!
//! The agent cannot tell what a plugin autocommand does on a key
//! (`InsertCharPre`, `TextChangedI`, `CursorMovedI`) or on entering or leaving
//! Insert mode (`InsertEnter`, `InsertLeavePre`, `InsertLeave`, `ModeChanged`),
//! only that one exists. Then the client predicts without drawing until enough
//! predictions prove right (see `super::predict::Trust`).

use rmpv::ValueRef;
use unicode_width::UnicodeWidthChar;

use super::anim::chain;
use super::grid::{Cell, Text};
use super::predict::{self, Look, Shape};
use super::redraw;
use super::words;

/// What the agent last reported about Insert mode in a buffer: everything that
/// would make a key do more than plain typing, as Neovim 0.12 behaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// Reported on entering Insert mode (so including mappings plugins made
    /// then), not merely on a window or option change before the first entry.
    pub from_insert: bool,
    /// First keys of Insert-mode mappings (or language mappings, when
    /// `'iminsert'` enables them), local or global, in `keytrans` form.
    pub firsts: Vec<Box<str>>,
    /// Insert-mode abbreviations, as typed.
    pub abbrevs: Vec<Box<str>>,
    /// An `InsertCharPre`, `TextChangedI` or `CursorMovedI` autocommand other
    /// than matchparen's or nvmux's: its effect on a key is unknown.
    pub unseen: bool,
    /// Likewise for entering (`InsertEnter`, `ModeChanged`) and leaving
    /// (`InsertLeavePre`, `InsertLeave`, `ModeChanged`) Insert mode; it may
    /// move the cursor or edit the buffer.
    pub unseen_entered: bool,
    pub unseen_left: bool,
    /// Column past which typing breaks the line, 0 for none: `'textwidth'`, or
    /// window width minus `'wrapmargin'` and gutter, as `comp_textwidth`
    /// computes it (or narrower).
    pub tw: usize,
    /// `'formatoptions'`.
    pub fo: Box<str>,
    /// Keys that re-indent: `'indentkeys'` with `'indentexpr'`, `'cinkeys'`
    /// with `'cindent'`, else none.
    pub indentkeys: Indentkeys,
    /// `'smartindent'`, `'lisp'`, `'showmatch'`, `'matchpairs'` (open, close).
    pub smartindent: bool,
    pub lisp: bool,
    pub showmatch: bool,
    pub matchpairs: Vec<(char, char)>,
    /// `'autocomplete'`: opens a completion menu while typing.
    pub autocomplete: bool,
    /// `'modifiable'`, and neither `'readonly'` and unchanged (warns and waits
    /// on the first change) nor a prompt buffer (text goes after the prompt
    /// wherever Insert mode starts).
    pub editable: bool,
    /// None of `'revins'`, `'rightleft'`, `'paste'`, an input method
    /// (`'iminsert'` 2) or `'digraph'` (char, `<BS>`, char makes a digraph).
    pub plain: bool,
    /// `'backspace'`.
    pub bs: Box<str>,
    /// `<BS>` deletes several blanks at once: `softtabs` if `'softtabstop'` or
    /// `'varsofttabstop'` is set; `smarttab` a `'shiftwidth'` before the first
    /// non-blank.
    pub softtabs: bool,
    pub smarttab: bool,
    /// `'delcombine'`: `<BS>` deletes only the last combining mark.
    pub delcombine: bool,
    /// `'concealcursor'` mode letters, when `'conceallevel'` conceals any.
    /// `i`: the typed line is not drawn as its text; exactly one of `n` and
    /// `i`: the line's drawing changes on entering or leaving Insert mode.
    pub conceals: Box<str>,
    /// `'iskeyword'` class of each character below 256 (see
    /// [`Facts::keyword`]).
    pub classes: Vec<u8>,
    /// `'autoindent'`; and `'cindent'` or `'indentexpr'`, with which Neovim
    /// re-indents when `<C-u>` deletes the text after the indent.
    pub autoindent: bool,
    pub cindent: bool,
    /// Whether `<C-w>`/`<C-u>` have Neovim's default mappings (`<C-g>u`
    /// first), so they stop at `Insstart_orig` (see `super::predict::Stop`).
    pub word_undo: bool,
    pub line_undo: bool,
}

impl Facts {
    /// Parse the agent's array into `(buf, facts)`.
    /// `matchpairs` and `conceals` are raw option values; `classes` has one
    /// digit per character code: `0` blank, `1` punctuation, `2` word.
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
            autoindent: flag(23),
            cindent: flag(24),
            word_undo: flag(25),
            line_undo: flag(26),
        };
        Some((int(0)?, facts))
    }

    /// Whether `'backspace'` lets `<C-w>`/`<C-u>` go past the Insert start
    /// instead of stopping there once: `nostop`, or `3`.
    pub fn nostop(&self) -> bool {
        self.bs.contains("nostop") || &*self.bs == "3"
    }

    /// Whether an Insert-mode mapping starts with `key` (Neovim notation).
    pub fn starts(&self, key: &str) -> bool {
        self.firsts.iter().any(|k| same_key(k, key))
    }

    /// Whether `c` is a word character per `'iskeyword'`; `None` past the 256
    /// characters `classes` covers.
    pub fn keyword(&self, c: char) -> Option<bool> {
        let code = usize::try_from(u32::from(c)).ok()?;
        self.classes.get(code).map(|class| *class >= 2)
    }
}

/// `'matchpairs'` as (open, close) pairs.
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

/// The `'indentkeys'`/`'cinkeys'` entries a typed character can trigger, as
/// Neovim's `in_cinkeys` reads them. A leading `*` or `!` (indent before or
/// instead of inserting) is skipped: either way the key does more than insert.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Indentkeys(Vec<Key>);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Key {
    /// A character anywhere, or with `first` (`0x`) only after blanks. `<…>`
    /// gives characters the notation reserves, or Neovim's key names.
    Char { c: char, first: bool },
    /// `=word`, `=~word` (`icase`) or `0=word` (`first`): typing the word's
    /// last character completes it, with no word character before it (or,
    /// with `first`, only blanks).
    Word {
        word: String,
        icase: bool,
        first: bool,
    },
    /// `e`: `else` completed with only blanks before it.
    Else,
    /// `:`: Neovim acts on it after a label, a `case` or a `::`; treated as
    /// always.
    Colon,
}

impl Indentkeys {
    /// Parse entries as `in_cinkeys` walks them (comma, then optional blanks),
    /// dropping non-character keys (`^X`, `o`, `O`, `<CR>`).
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

    /// Whether typing `c` after `before` (the text before the cursor) makes
    /// Neovim re-indent, as `in_cinkeys` judges the line with `c` inserted.
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
                // A whole word: at line start or after a non-word byte (Neovim
                // checks the byte, not the character).
                let alone = start == 0
                    || facts.keyword(char::from(typed.as_bytes()[start - 1])) != Some(true);
                same && alone && (!first || typed[..start].chars().all(blank))
            }
        })
    }
}

/// A plain Insert-mode edit: insert a character, delete the one before the
/// cursor, or for `<C-w>`/`<C-u>` the word or line before it (see
/// [`rubbed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    Char(char),
    Rub,
    Word,
    Line,
}

impl Edit {
    /// The edit for `key` in Neovim notation: a non-control character (or
    /// `<Space>`, `<lt>`, `<Bar>`, `<Bslash>`), `<BS>`/`<C-h>`, `<C-w>` or
    /// `<C-u>`; else `None`.
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
            "<c-w>" => Some(Edit::Word),
            "<c-u>" => Some(Edit::Line),
            _ => None,
        }
    }
}

/// A plain Normal-mode key entering Insert mode: `i`, `a`, `A` or `I`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enter {
    Before,
    After,
    End,
    First,
}

impl Enter {
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

/// Cursor `(byte, display column)` after entering Insert mode by `e` from
/// Normal-mode `byte` (column per [`sits`]), as in Neovim 0.12.6. `a` skips
/// the character with its combining marks (a tab or wide char whole) and
/// stays put on an empty line; `I` on an all-blank line goes to its end.
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

/// Cursor `(byte, display column)` after `<Esc>` at `byte`: on the previous
/// character unless at line start, drawn as in Normal mode (a tab's last
/// column), as in Neovim 0.12.6.
pub fn left(text: &str, shape: &Shape, byte: usize) -> (usize, usize) {
    let cs = predict::chars(text, shape);
    let at = cs
        .iter()
        .rfind(|ch| ch.byte < byte)
        .or(cs.first())
        .map_or(0, |ch| ch.byte);
    (at, super::moves::drawn(text, shape, at))
}

/// Why `<Esc>` at `byte` might do more than move back a character, or `None`.
/// It expands an abbreviation before the cursor, and removes an unused
/// `'autoindent'` or comment-leader indent; the client cannot tell that from
/// typed blanks, so blanks before the cursor are refused.
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

/// Compare keys: characters by value whatever their notation, `<…>` names
/// case-insensitively.
fn same_key(a: &str, b: &str) -> bool {
    let norm = |k: &str| -> String {
        match Edit::of(k) {
            Some(Edit::Char(c)) => c.to_string(),
            _ => k.to_ascii_lowercase(),
        }
    };
    norm(a) == norm(b)
}

/// Why `key` (edit `e` at `byte`, cursor on display column `vcol`) might do
/// more than plain typing, per `facts` and Neovim 0.12; `None` if it will not.
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
            // Width must agree with Neovim's: only ASCII where widths may
            // differ, and no emoji or symbols whose emoji presentation
            // depends on what follows.
            let w = c.width().unwrap_or(0);
            let widths = shape.stepping.as_ref().is_none_or(|s| s.widths);
            let emoji = matches!(c,
                '\u{1f000}'..='\u{1faff}' | '\u{2600}'..='\u{27bf}' | '\u{2b00}'..='\u{2bff}');
            if !(1..=2).contains(&w) || c.is_control() || (!c.is_ascii() && (widths || emoji)) {
                return Some("a character Neovim may not draw as the client does");
            }
            // A non-word character after an abbreviation expands it (the
            // client ignores what precedes the abbreviation).
            if f.keyword(c) != Some(true) && f.abbrevs.iter().any(|a| before.ends_with(&**a)) {
                return Some("it ends an abbreviation");
            }
            // A non-blank past `'textwidth'` breaks the line with `t` or `c`
            // in `'formatoptions'` (`c` only in comments, which the client
            // cannot tell); `a` formats on every key. Neovim measures the
            // column without `'list'` (a `^I` tab as a full tab), which the
            // client does not.
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
        Edit::Word | Edit::Line => {
            if before.is_empty() {
                return Some("it would join the line to the one before");
            }
            let bs = &f.bs;
            if !(bs.contains("start") || bs.contains("nostop") || matches!(&**bs, "2" | "3")) {
                return Some("'backspace' may not take out what was there before");
            }
            // Without `indent`, Neovim keeps `'autoindent'` indent, which the
            // client cannot tell apart.
            if !(bs.contains("indent") || matches!(&**bs, "1" | "2" | "3")) {
                return Some("'backspace' may not take out an indent");
            }
            if f.delcombine && before.chars().any(|c| c.width() == Some(0)) {
                return Some("a combining mark may come out alone");
            }
            if e == Edit::Line && f.cindent {
                return Some("it has the line indented again");
            }
            None
        }
    }
}

/// The line and cursor byte after `e` at `byte`. `<BS>` deletes a character
/// with its combining marks (Neovim without `'delcombine'`). `None` if nothing
/// precedes the cursor, and for `<C-w>`/`<C-u>` (see [`rubbed`]).
pub fn edited(e: Edit, text: &str, byte: usize) -> Option<(String, usize)> {
    let (before, after) = (text.get(..byte)?, text.get(byte..)?);
    match e {
        Edit::Char(c) => Some((format!("{before}{c}{after}"), byte + c.len_utf8())),
        Edit::Rub => {
            let at = before
                .char_indices()
                .rev()
                .find(|(_, c)| c.width() != Some(0))
                .map(|(i, _)| i)?;
            Some((format!("{}{after}", &before[..at]), at))
        }
        Edit::Word | Edit::Line => None,
    }
}

/// The line and cursor byte after `<C-w>` or `<C-u>` at `byte`, as Neovim's
/// `ins_bs` deletes (a character with its combining marks at a time).
///
/// `<C-w>` deletes blanks, then a run of one class (see
/// [`super::words::class`]; `classes` from `'iskeyword'`, below 256). `<C-u>`
/// deletes to line start, or to the first non-blank if past it and `keeps`
/// (`'autoindent'`, `'cindent'` or `'indentexpr'`). Either stops once at the
/// Insert start `stop`; a repeat goes past it. `None` if nothing precedes the
/// cursor.
pub fn rubbed(
    e: Edit,
    (text, byte): (&str, usize),
    stop: Option<usize>,
    classes: &[u8],
    keeps: bool,
) -> Option<(String, usize)> {
    let (before, after) = (text.get(..byte)?, text.get(byte..)?);
    let cs: Vec<(usize, char)> = before
        .char_indices()
        .filter(|&(_, c)| !predict::joins(c))
        .collect();
    let blank = |c: char| matches!(c, ' ' | '\t');
    // Neovim's `ascii_isspace`.
    let space = |c: char| matches!(c, ' ' | '\t'..='\r');
    let class = |c: char| words::class(c, classes, false);
    let indent = before.len() - before.trim_start_matches(blank).len();
    let least = match e {
        Edit::Line if keeps && indent < byte => indent,
        _ => 0,
    };
    let mut i = cs.len();
    // Whether the first non-blank is a word character, and the previous
    // character's class.
    let mut word: Option<bool> = None;
    let mut last = 0;
    loop {
        i = i.checked_sub(1)?;
        let c = cs[i].1;
        let now = class(c);
        if e == Edit::Word {
            match word {
                None if !space(c) => word = Some(now >= 2),
                Some(w) if space(c) || (now >= 2) != w || now != last => {
                    i += 1;
                    break;
                }
                _ => {}
            }
        }
        last = now;
        let col = cs[i].0;
        if col <= least || stop == Some(col) {
            break;
        }
    }
    let at = cs.get(i).map_or(byte, |&(b, _)| b);
    Some((format!("{}{after}", &text[..at]), at))
}

/// Insert-mode cursor display column at `byte`: its character's first column
/// (a tab's first, unlike Normal mode), or just past the line's end.
pub fn sits(text: &str, shape: &Shape, byte: usize) -> usize {
    predict::chars(text, shape)
        .iter()
        .find(|ch| ch.byte >= byte)
        .map_or_else(|| predict::width(text, shape), |ch| ch.vcol)
}

/// Rebuild Neovim's drawn `row` (cells past the gutter) for `new`, where `put`
/// bytes replaced `old[from..from + gone]`, cursor on display column `cursor`.
///
/// Glyphs come from the client's layout (`predict::line`); unchanged
/// characters keep Neovim's colours by glyph kind, new `'list'` glyphs take
/// the row's (or `look`'s) colour for their kind, and new text [`colour`]'s.
/// Past the end ([`Past`]), virtual text moves with the end; fill,
/// `'cursorline'` and `'colorcolumn'` stay put.
///
/// `None` if `old` does not match the row, but for what Neovim drew over its
/// blanks before the edit ([`chain::shows`]), which is kept; if `new` would
/// wrap (or put the cursor) onto the next row, or the view is or would be
/// scrolled sideways; or if colours are unknowable: a `'colorcolumn'` among
/// moved or new characters or non-blank past the end ([`Past::of`]), or end
/// content crossing one or needing more room than the row had.
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
    // Lay out with a sentinel id per glyph kind (text, `'list'` glyphs,
    // special keys) to tell each cell's kind.
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
    let edited = sits(old, shape, from);
    let over: Vec<bool> = (0..old_end.min(row.len()))
        .map(|k| row[k].text != was[k].text)
        .collect();
    // Past the edit, what is drawn over a blank may move with the text.
    let stays = |k: usize| k < edited && chain::shows(&was[k].text, &row[k].text);
    if row.len() < old_end || over.iter().enumerate().any(|(k, o)| *o && !stays(k)) {
        return None;
    }
    let cc = colorcolumn(shape);
    if cc
        .iter()
        .any(|c| (edited..old_end.max(new_end)).contains(c))
    {
        return None;
    }
    let (old_w, new_w) = (predict::width(old, shape), predict::width(new, shape));
    let past = Past::of(row, old_end, cc)?;
    let mut colours: Vec<Option<u32>> = vec![None; new_end];
    // Characters outside the edit keep Neovim's colours, kind for kind; a
    // tab that changed width maps onto its first and last cells.
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
    // So does a `'listchars'` `eol` glyph.
    if new_w < new_end && old_w < old_end {
        colours[new_w] = Some(row[old_w].hl);
    }
    // Past the end: move the end's run with it (blanking a wide char cut by
    // the row edge; refusing if a cut run would come back, or it crosses a
    // `'colorcolumn'`), and fill the rest, with `'colorcolumn'` columns in
    // their colour.
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
    // Cells without carried colour: a glyph takes the row's colour for its
    // kind (or the window's); text goes by its neighbours, left to right, a
    // right half as its left. `'colorcolumn'` cells are never used, as
    // their colour is mixed.
    let own = |kind: u32| -> u32 {
        let window = match kind {
            WHITESPACE => look.whitespace,
            NONTEXT => look.nontext,
            _ => look.specialkey,
        };
        (0..old_end)
            .find(|k| was[*k].hl == kind && !cc.contains(k) && !over[*k])
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
    for k in (0..over.len()).filter(|&k| over[k]) {
        out[k] = row[k].clone();
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
            // Neighbours must be already-coloured text: not `'list'` glyphs
            // (window colours) nor `'colorcolumn'` cells.
            let by = |k: usize| {
                let glyph = kind.get(k).is_some_and(|k| *k != TEXT) || over.get(k) == Some(&true);
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

/// What Neovim (0.12.6) drew past a line's end: the fill (the most common
/// blank colour, `'cursorline'` included) and blank `'colorcolumn'` columns,
/// both fixed to the window; and from the end to the last other cell, a run
/// that moves with the end (virtual text with its blanks). Virtual text at a
/// fixed window column is reported by the agent instead (see
/// `super::predict::Predictor::pinned`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Past {
    /// `None` if the only blanks past the end are `'colorcolumn'` ones.
    fill: Option<u32>,
    /// Columns that move with the line's end.
    run: std::ops::Range<usize>,
}

impl Past {
    /// Read `row` from `end` (past the text and `eol` glyph), with
    /// `'colorcolumn'` columns `cc`; `None` if one of those is not blank.
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

/// `'colorcolumn'` display columns, as the agent reports them.
fn colorcolumn(shape: &Shape) -> &[usize] {
    shape
        .stepping
        .as_ref()
        .map_or(&[], |s| s.colorcolumn.as_slice())
}

/// Glyph-kind ids for [`relayout`]; real highlight ids count up from 1 and
/// never get near.
const TEXT: u32 = u32::MAX - 3;
const WHITESPACE: u32 = u32::MAX - 2;
const NONTEXT: u32 = u32::MAX - 1;
const SPECIALKEY: u32 = u32::MAX;

/// A guessed (never judged) colour for typed `c` until Neovim draws it: the
/// neighbours' if equal (inside a string or comment), the left one's if both
/// are word characters (continuing a name), else the row's `fill`.
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
    use crate::client::predict::{Cursorline, Listchars, Stepping, Stop};

    /// 20 columns, no gutter, tabstop 8, unambiguous widths.
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
                lmap: false,
                sections: None,
                paragraphs: None,
                smoothscroll: false,
            }),
        }
    }

    /// [`shape`] with `'list'` and the given `'listchars'`.
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

    /// Neovim's defaults.
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
            autoindent: true,
            cindent: false,
            word_undo: true,
            line_undo: true,
        }
    }

    /// Text 1, `NonText` 2, `Whitespace` 3, `SpecialKey` 4.
    fn look() -> Look {
        Look {
            text: 1,
            nontext: 2,
            whitespace: 3,
            specialkey: 4,
            ..Look::default()
        }
    }

    /// A 20-cell row: `text` (`_` is a wide char's right half), with one
    /// highlight digit per cell from `hls`, 1 past them.
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

    /// A row's text and highlight digits, trailing blanks trimmed.
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

    /// Each key's relaid row; `None` from the first not drawn ahead.
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
                    Edit::Rub | Edit::Word | Edit::Line => (to, byte - to, 0),
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

    /// The tab narrows and `b` stays at the tab stop in its colours, as in
    /// Neovim 0.12.6.
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

    /// Trailing blanks show as `trail` in `Whitespace` until text follows;
    /// `<BS>` restores them.
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
        // A `tab` glyph narrowing, and a blank as `space`.
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

    #[test]
    fn a_character_two_cells_wide_goes_in_and_out_whole() {
        let rows = typed(&shape(true), ("ab", 1), row("ab", "11"), &["日", "<BS>"]);
        assert_eq!(rows, [seen("a日_b", "1111"), seen("ab", "11")]);
    }

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

    /// Guides drawn over the indent, as snacks.nvim's are (8, 9), stay; the
    /// text after them is laid out as without.
    #[test]
    fn guides_drawn_over_the_indent_stay_as_text_is_typed() {
        let rows = typed(
            &shape(true),
            ("    ab", 6),
            row("▏ ▏ ab", "819111"),
            &["c", "<BS>", "<BS>"],
        );
        assert_eq!(
            rows,
            [
                seen("▏ ▏ abc", "8191111"),
                seen("▏ ▏ ab", "819111"),
                seen("▏ ▏ a", "81911"),
            ]
        );
        // Typed into the indent, or onto what was drawn over the text.
        let drawn = row("▏ ▏ ab", "819111");
        assert_eq!(laid(&shape(true), ("    ab", 1), &drawn, "x"), None);
        assert_eq!(
            laid(&shape(true), ("    ab", 6), &row("▏ ▏ xb", ""), "c"),
            None
        );
    }

    /// The relaid row's highlight digits, or `None` if not drawn ahead.
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
            Edit::Rub | Edit::Word | Edit::Line => (to, byte - to, 0),
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

    /// [`shape`] with `'colorcolumn'` at `cc`.
    fn coloured(cc: &[usize]) -> Shape {
        let mut s = shape(true);
        if let Some(stepping) = s.stepping.as_mut() {
            stepping.colorcolumn = cc.to_vec();
        }
        s
    }

    /// The column stays put past the end; edits moving a character or
    /// virtual text into or out of it are not drawn ahead (Neovim 0.12.6).
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
        // Virtual text moves up to the column, not over it.
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
        // A column after the edit within the text refuses; one before does
        // not.
        let cc = coloured(&[8]);
        let drawn = row("foobarbazqux", "111111117111");
        assert_eq!(laid(&cc, ("foobarbazqux", 1), &drawn, "x"), None);
        assert_eq!(
            laid(&cc, ("foobarbazqux", 12), &drawn, "x").as_deref(),
            Some("11111111711111111111")
        );
        // Typed next to the column, a character does not take its colour.
        let cc = coloured(&[2]);
        let drawn = row("abcd", "1171");
        assert_eq!(
            laid(&cc, ("abcd", 4), &drawn, "e").as_deref(),
            Some("11711111111111111111")
        );
    }

    /// A diagnostic's virtual text moves as one run with its fill gaps
    /// (Neovim 0.12.6). Text cut by the row edge cannot move back, and nothing
    /// non-blank may sit in a `'colorcolumn'` past the end.
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

    /// With `'wrap'`, filling the row's last column at line end moves the
    /// cursor to the next row (unless an `eol` glyph fills it). Without, a
    /// key that would scroll sideways (per `'sidescrolloff'`) is refused.
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

    /// Whether `key` typed after `before` is refused.
    fn refuses(f: &Facts, key: &str, before: &str) -> bool {
        let shape = shape(true);
        let e = Edit::of(key).expect("an edit");
        let text = format!("{before}rest");
        let vcol = sits(&text, &shape, before.len());
        refused(key, e, (&text, before.len(), vcol), f, &shape).is_some()
    }

    /// As in Neovim 0.12.6: C's `}`, `#`, `{` only as the first non-blank,
    /// `else` and `:`; Lua's `end` after blanks only; Python's `elif` after
    /// any non-word character.
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

    /// Only a non-word character after an abbreviation is refused.
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

    /// A non-blank ending past `'textwidth'` is refused; one ending on it, or
    /// a blank, is not (Neovim 0.12.6). `fo+=a` refuses every key.
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

    /// `'showmatch'` closing brackets, buffers that type otherwise, and
    /// mapped first keys (compared by character, whatever the notation).
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

    /// Refused: blanks with `'softtabstop'`, indent blanks with `'smarttab'`, a
    /// combining mark with `'delcombine'`, line start, and past the Insert
    /// start without `start` in `'backspace'`.
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

    #[test]
    fn an_edit_puts_a_character_in_or_takes_one_out() {
        assert_eq!(edited(Edit::Char('x'), "ab", 1), Some(("axb".into(), 2)));
        assert_eq!(edited(Edit::Char('日'), "ab", 2), Some(("ab日".into(), 5)));
        assert_eq!(edited(Edit::Rub, "a日b", 4), Some(("ab".into(), 1)));
        assert_eq!(edited(Edit::Rub, "e\u{301}x", 3), Some(("x".into(), 0)));
        assert_eq!(edited(Edit::Rub, "ab", 0), None);
        assert_eq!(edited(Edit::Word, "ab", 2), None, "see `rubbed`");
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
            ("<C-w>", Some(Edit::Word)),
            ("<C-u>", Some(Edit::Line)),
            ("<C-o>", None),
        ] {
            assert_eq!(Edit::of(key), e, "{key:?}");
        }
    }

    /// A line and the cursor byte.
    type Left<'a> = (&'a str, usize);

    /// Line and cursor after `A` then `keys`. `undo`: `<C-w>`/`<C-u>` have
    /// Neovim's default mappings; `keeps`: `<C-u>` keeps the indent.
    fn typed_out(line: &str, keys: &str, keeps: bool, undo: bool) -> (String, usize) {
        let classes = facts().classes;
        let (mut text, mut byte) = (line.to_string(), line.len());
        let mut stop = Stop {
            line: 0,
            byte: line.len(),
            sticks: false,
        };
        for key in predict::tokens(keys) {
            let e = Edit::of(key).expect("an edit");
            (text, byte) = match e {
                Edit::Word | Edit::Line => {
                    rubbed(e, (&text, byte), Some(stop.byte), &classes, keeps)
                }
                _ => edited(e, &text, byte),
            }
            .expect("made");
            stop = match e {
                Edit::Char(_) => stop,
                Edit::Rub => stop.rubbed((0, byte), false),
                Edit::Word | Edit::Line => stop.rubbed((0, byte), undo),
            };
        }
        (text, byte)
    }

    /// Cases from Neovim 0.12.6, with and without `'autoindent'`. Each
    /// script is its own class; both stop once at the Insert start.
    #[test]
    fn words_and_lines_come_out_as_neovim_takes_them_out() {
        #[rustfmt::skip]
        let cases: &[(&str, &str, Left, Left)] = &[
            ("hello world", "<C-w>", ("hello ", 6), ("hello ", 6)),
            ("hello", "world<C-w>", ("hello", 5), ("hello", 5)),
            ("hello", " world<C-w>", ("hello ", 6), ("hello ", 6)),
            ("hello", " world<C-w><C-w>", ("hello", 5), ("hello", 5)),
            ("hello", " world<C-w><C-w><C-w>", ("", 0), ("", 0)),
            ("foo.bar", "<C-w>", ("foo.", 4), ("foo.", 4)),
            ("foo...", "<C-w>", ("foo", 3), ("foo", 3)),
            ("  indented", "<C-u>", ("  ", 2), ("", 0)),
            ("abc", "def<C-u>", ("abc", 3), ("abc", 3)),
            ("abc", "def<C-u><C-u>", ("", 0), ("", 0)),
            ("日本語abc", "<C-w>", ("日本語", 9), ("日本語", 9)),
            ("x  ", "<C-w>", ("", 0), ("", 0)),
            ("a\t\tb", "<C-w>", ("a\t\t", 3), ("a\t\t", 3)),
            ("a\t\tb", "<C-w><C-w>", ("", 0), ("", 0)),
            ("  ab cd", "<C-u>", ("  ", 2), ("", 0)),
            ("  ab", "cd<C-u><C-u>", ("  ", 2), ("", 0)),
            ("café", "<C-w>", ("", 0), ("", 0)),
            ("e\u{301}te\u{301}", "<C-w>", ("", 0), ("", 0)),
            ("hello", "<BS><BS>xy<C-w>", ("", 0), ("", 0)),
            ("hello", "<BS><BS><C-w>", ("", 0), ("", 0)),
            ("abc", "<C-u>", ("", 0), ("", 0)),
        ];
        for &(line, keys, ai, noai) in cases {
            for (keeps, (text, byte)) in [(true, ai), (false, noai)] {
                assert_eq!(
                    typed_out(line, keys, keeps, true),
                    (text.to_string(), byte),
                    "{line:?} + {keys:?}, the indent kept: {keeps}"
                );
            }
        }
        let classes = facts().classes;
        assert_eq!(rubbed(Edit::Word, ("abc", 0), None, &classes, true), None);
    }

    /// As in Neovim 0.12.6: with its default mappings the stop sticks at the
    /// line start reached; without, it stays at the Insert start each time.
    #[test]
    fn where_words_and_lines_stop_sticks_as_neovims_own_mappings_have_it() {
        #[rustfmt::skip]
        let cases: &[(&str, &str, Left, Left)] = &[
            ("hello", "<C-w>abcdefg<C-w>", ("", 0), ("abcde", 5)),
            ("hello", "<C-u>abc def<C-u>", ("", 0), ("abc d", 5)),
            ("hello", "<C-w>abcdefg<BS><C-w>", ("", 0), ("abcde", 5)),
            ("hello world", "<C-w>xyz<C-w>", ("hello ", 6), ("hello ", 6)),
            ("hello", "xy<C-w>zz<C-w>", ("hello", 5), ("hello", 5)),
            ("ab", "<BS><BS>cdef<C-w>", ("cd", 2), ("cd", 2)),
            ("hello", "<C-w>", ("", 0), ("", 0)),
        ];
        for &(line, keys, own, plain) in cases {
            for (undo, (text, byte)) in [(true, own), (false, plain)] {
                assert_eq!(
                    typed_out(line, keys, true, undo),
                    (text.to_string(), byte),
                    "{line:?} + {keys:?}, Neovim's own mappings: {undo}"
                );
            }
        }
    }

    #[test]
    fn words_and_lines_are_refused_where_neovim_may_take_out_otherwise() {
        let shape = shape(true);
        let f = facts();
        let refuses = |f: &Facts, key: &str, text: &str, byte: usize| {
            let e = Edit::of(key).expect("an edit");
            refused(key, e, (text, byte, byte), f, &shape).is_some()
        };
        for key in ["<C-w>", "<C-u>"] {
            assert!(!refuses(&f, key, "ab cd", 5), "{key}");
            assert!(refuses(&f, key, "ab cd", 0), "{key} at the line's start");
            for bs in ["indent,eol", "eol,start", "1", "0"] {
                let f = Facts {
                    bs: bs.into(),
                    ..facts()
                };
                assert!(refuses(&f, key, "ab cd", 5), "{key} with 'backspace' {bs}");
            }
            for bs in ["indent,eol,start", "indent,eol,nostop", "2", "3"] {
                let f = Facts {
                    bs: bs.into(),
                    ..facts()
                };
                assert!(!refuses(&f, key, "ab cd", 5), "{key} with 'backspace' {bs}");
            }
            let f = Facts {
                delcombine: true,
                ..facts()
            };
            assert!(refuses(&f, key, "e\u{301}te", 5), "{key} with 'delcombine'");
            assert!(
                !refuses(&f, key, "ab cd", 5),
                "{key} with 'delcombine', no mark"
            );
        }
        let f = Facts {
            cindent: true,
            ..facts()
        };
        assert!(refuses(&f, "<C-u>", "  ab", 4) && !refuses(&f, "<C-w>", "  ab", 4));
        assert_eq!(
            [Edit::of("<C-w>"), Edit::of("<C-u>"), Edit::of("<c-W>")],
            [Some(Edit::Word), Some(Edit::Line), Some(Edit::Word)]
        );
    }

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
            false.into(),
            false.into(),
            true.into(),
            true.into(),
        ];
        let refs: Vec<ValueRef<'_>> = v.iter().map(Value::as_ref).collect();
        let (buf, f) = Facts::read(&refs).expect("read");
        assert!(f.autoindent && f.cindent);
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

    /// `unseen_entered`/`unseen_left` are fields 21 and 22; later ones may be
    /// missing.
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

    /// Shape, text, byte, key, `(byte, vcol)` after it and after `<Esc>`.
    type Case<'a> = (
        &'a Shape,
        &'a str,
        usize,
        &'a str,
        (usize, usize),
        (usize, usize),
    );

    /// Cases from Neovim 0.12.6. With `'list'` drawing a tab, Normal mode
    /// also puts the cursor on its first cell.
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
