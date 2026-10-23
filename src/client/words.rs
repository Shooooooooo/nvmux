//! Client-side Normal-mode word motions (`w`, `b`, `e`, `ge`, their WORD
//! forms and the Shift/Ctrl arrows), ported from Neovim 0.12's `fwd_word`,
//! `bck_word`, `end_word` and `bckend_word` (see [`super::moves`]). `w`, `b`
//! and `ge` stop on an empty line, `e` does not; a count stops at the
//! buffer's start or end.

use super::predict;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Way {
    /// `w`: start of the next word.
    Next,
    /// `b`: start of this word, or of the previous one if already there.
    Back,
    /// `e`: end of the current word, or of the next one if already there.
    End,
    /// `ge`: end of the previous word.
    BackEnd,
}

/// Neovim's `cls()`: 0 blank, 1 punctuation, 2 word; below 256 from `keyword`
/// (`'iskeyword'`), above from [`CLASSES`]. With `big` every non-blank is 1.
pub fn class(c: char, keyword: &[u8], big: bool) -> u32 {
    let class = match u32::from(c) {
        0x20 | 0x09 => 0,
        code if code < 0x100 => keyword.get(code as usize).map_or(1, |&k| u32::from(k)),
        code => CLASSES
            .binary_search_by(|&(first, last, _)| {
                if last < code {
                    std::cmp::Ordering::Less
                } else if first > code {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .map_or(2, |i| CLASSES[i].2),
    };
    if big && class != 0 {
        1
    } else {
        class
    }
}

/// `text` gives a 0-based line only where the client has it whole and it is
/// [`predict::ordinary`]; `keyword` holds the [`class`] of each byte below 256.
pub struct Lines<'a> {
    pub text: &'a dyn Fn(i64) -> Option<&'a str>,
    pub count: i64,
    pub keyword: &'a [u8],
}

/// Where `way` (WORD if `big`) `count` times takes the cursor from `(line,
/// byte)`, as Neovim 0.12.6 does even when the motion fails. `None` if it
/// would cross a missing line or `'iskeyword'` is unknown.
pub fn moved(
    way: Way,
    big: bool,
    count: i64,
    (line, byte): (i64, usize),
    lines: &Lines,
) -> Option<(i64, usize)> {
    if lines.keyword.len() != 256 {
        return None;
    }
    let mut at = Walk::new(line, lines, big)?;
    at.i = at.cs.iter().rposition(|&(b, _)| b <= byte).unwrap_or(0);
    let from = (at.line, at.i);
    match way {
        Way::Next => at.fwd_word(count)?,
        Way::Back => at.bck_word(count)?,
        Way::End => at.end_word(count)?,
        Way::BackEnd => at.bckend_word(count)?,
    };
    // Neovim's `adjust_cursor`: a forward move that ends on a line end steps
    // back onto its last character.
    if matches!(way, Way::Next | Way::End) && (at.line, at.i) > from && at.i > 0 && at.end() {
        at.i -= 1;
    }
    // The cursor rests on a line end only on an empty line.
    match at.cs.get(at.i) {
        Some(&(byte, _)) => Some((at.line, byte)),
        None => at.cs.is_empty().then_some((at.line, 0)),
    }
}

/// A position as Neovim's `inc()`/`dec()` step: a character, or the line end.
struct Walk<'a, 'b> {
    lines: &'a Lines<'b>,
    big: bool,
    line: i64,
    /// Characters with their byte offsets.
    cs: Vec<(usize, char)>,
    /// Index into `cs`; `cs.len()` is the line end.
    i: usize,
}

impl<'a, 'b> Walk<'a, 'b> {
    fn new(line: i64, lines: &'a Lines<'b>, big: bool) -> Option<Self> {
        let mut walk = Self {
            lines,
            big,
            line,
            cs: Vec::new(),
            i: 0,
        };
        walk.load(line)?;
        Some(walk)
    }

    /// Load `line`, merging combining marks as [`predict::chars`] does.
    fn load(&mut self, line: i64) -> Option<()> {
        let text = (self.lines.text)(line)?;
        self.cs = text
            .char_indices()
            .filter(|&(_, c)| !predict::joins(c))
            .collect();
        self.line = line;
        Some(())
    }

    fn end(&self) -> bool {
        self.i >= self.cs.len()
    }

    /// `cls()`: the class here; a line end is a blank.
    fn cls(&self) -> u32 {
        self.cs
            .get(self.i)
            .map_or(0, |&(_, c)| class(c, self.lines.keyword, self.big))
    }

    fn on_empty(&self) -> bool {
        self.cs.is_empty()
    }

    /// `inc()`: 0 within the line, 2 onto the line end, 1 from the end onto
    /// the next line, -1 (no move) at the buffer's end.
    fn inc(&mut self) -> Option<i8> {
        if !self.end() {
            self.i += 1;
            return Some(if self.end() { 2 } else { 0 });
        }
        if self.line + 1 < self.lines.count {
            self.load(self.line + 1)?;
            self.i = 0;
            return Some(1);
        }
        Some(-1)
    }

    /// `dec()`: 0 within the line (including end to last character), 1 onto
    /// the previous line's end, -1 (no move) at the buffer's start.
    fn dec(&mut self) -> Option<i8> {
        if self.i > 0 {
            self.i -= 1;
            return Some(0);
        }
        if self.line > 0 {
            self.load(self.line - 1)?;
            self.i = self.cs.len();
            return Some(1);
        }
        Some(-1)
    }

    /// `skip_chars()`; returns whether it hit the buffer's start or end.
    fn skip(&mut self, class: u32, forward: bool) -> Option<bool> {
        while self.cls() == class {
            let went = if forward { self.inc()? } else { self.dec()? };
            if went == -1 {
                return Some(true);
            }
        }
        Some(false)
    }

    /// `fwd_word()` without an operator; `false` is Neovim's FAIL.
    fn fwd_word(&mut self, count: i64) -> Option<bool> {
        for _ in 0..count.max(1) {
            let sclass = self.cls();
            let last_line = self.line + 1 >= self.lines.count;
            let i = self.inc()?;
            if i == -1 || (i >= 1 && last_line) {
                return Some(false);
            }
            // Skip the rest of the current word.
            if sclass != 0 {
                while self.cls() == sclass {
                    if self.inc()? == -1 {
                        return Some(true);
                    }
                }
            }
            // Skip blanks, stopping on an empty line.
            while self.cls() == 0 {
                if self.i == 0 && self.on_empty() {
                    break;
                }
                if self.inc()? == -1 {
                    return Some(true);
                }
            }
        }
        Some(true)
    }

    /// `bck_word()`; `false` is Neovim's FAIL.
    fn bck_word(&mut self, count: i64) -> Option<bool> {
        for _ in 0..count.max(1) {
            if self.dec()? == -1 {
                return Some(false);
            }
            // Skip blanks backwards, stopping on an empty line.
            let mut empty = false;
            while self.cls() == 0 {
                if self.i == 0 && self.on_empty() {
                    empty = true;
                    break;
                }
                if self.dec()? == -1 {
                    return Some(true);
                }
            }
            if empty {
                continue;
            }
            // Back past the word's start, then forward one.
            if self.skip(self.cls(), false)? {
                return Some(true);
            }
            self.inc()?;
        }
        Some(true)
    }

    /// `end_word()` without an operator; `false` is Neovim's FAIL.
    fn end_word(&mut self, count: i64) -> Option<bool> {
        for _ in 0..count.max(1) {
            let sclass = self.cls();
            if self.inc()? == -1 {
                return Some(false);
            }
            if self.cls() == sclass && sclass != 0 {
                // In the middle of a word: on to its end.
                if self.skip(sclass, true)? {
                    return Some(false);
                }
            } else {
                // At a word's end: skip blanks, empty lines included, to the
                // next word's end.
                while self.cls() == 0 {
                    if self.inc()? == -1 {
                        return Some(false);
                    }
                }
                if self.skip(self.cls(), true)? {
                    return Some(false);
                }
            }
            // Step back from the overshoot.
            self.dec()?;
        }
        Some(true)
    }

    /// `bckend_word()` without an operator; `false` is Neovim's FAIL.
    fn bckend_word(&mut self, count: i64) -> Option<bool> {
        for _ in 0..count.max(1) {
            let sclass = self.cls();
            if self.dec()? == -1 {
                return Some(false);
            }
            // Skip back over the current word.
            if sclass != 0 {
                while self.cls() == sclass {
                    if self.dec()? == -1 {
                        return Some(true);
                    }
                }
            }
            // Skip blanks back to the previous word's end or an empty line.
            while self.cls() == 0 {
                if self.i == 0 && self.on_empty() {
                    break;
                }
                if self.dec()? == -1 {
                    return Some(true);
                }
            }
        }
        Some(true)
    }
}

/// Classes for the default `'iskeyword'` (`@,48-57,_,192-255`), matching
/// Neovim for every character the tests use.
#[cfg(test)]
pub fn defaults() -> Vec<u8> {
    (0..256u32)
        .map(|b| match b {
            0x00 | 0x09 | 0x20 | 0xa0 => 0,
            192.. => 2,
            b if char::from_u32(b).is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') => 2,
            _ => 1,
        })
        .collect()
}

/// Neovim 0.12.6's classes for code points from 256 that are not 2 (word):
/// sorted `(first, last, class)` runs, 0 blank, 1 punctuation, 3 emoji, else
/// a script's first code point. Read off `charclass()` for every code point.
#[rustfmt::skip]
const CLASSES: &[(u32, u32, u32)] = &[
    (0x37e, 0x37e, 1), (0x387, 0x387, 1), (0x55a, 0x55f, 1),
    (0x589, 0x589, 1), (0x5be, 0x5be, 1), (0x5c0, 0x5c0, 1),
    (0x5c3, 0x5c3, 1), (0x5f3, 0x5f4, 1), (0x60c, 0x60c, 1),
    (0x61b, 0x61b, 1), (0x61f, 0x61f, 1), (0x66a, 0x66d, 1),
    (0x6d4, 0x6d4, 1), (0x700, 0x70d, 1), (0x964, 0x965, 1),
    (0x970, 0x970, 1), (0xdf4, 0xdf4, 1), (0xe4f, 0xe4f, 1),
    (0xe5a, 0xe5b, 1), (0xf04, 0xf12, 1), (0xf3a, 0xf3d, 1),
    (0xf85, 0xf85, 1), (0x104a, 0x104f, 1), (0x10fb, 0x10fb, 1),
    (0x1361, 0x1368, 1), (0x166d, 0x166e, 1), (0x1680, 0x1680, 0),
    (0x169b, 0x169c, 1), (0x16eb, 0x16ed, 1), (0x1735, 0x1736, 1),
    (0x17d4, 0x17dc, 1), (0x1800, 0x180a, 1), (0x2000, 0x200b, 0),
    (0x200c, 0x2027, 1), (0x2028, 0x2029, 0), (0x202a, 0x202e, 1),
    (0x202f, 0x202f, 0), (0x2030, 0x203b, 1), (0x203c, 0x203c, 3),
    (0x203d, 0x2048, 1), (0x2049, 0x2049, 3), (0x204a, 0x205e, 1),
    (0x205f, 0x205f, 0), (0x2060, 0x206f, 1), (0x2070, 0x207f, 8304),
    (0x2080, 0x2094, 8320), (0x20a0, 0x2121, 1), (0x2122, 0x2122, 3),
    (0x2123, 0x2138, 1), (0x2139, 0x2139, 3), (0x213a, 0x2193, 1),
    (0x2194, 0x2199, 3), (0x219a, 0x21a8, 1), (0x21a9, 0x21aa, 3),
    (0x21ab, 0x2319, 1), (0x231a, 0x231b, 3), (0x231c, 0x2327, 1),
    (0x2328, 0x2328, 3), (0x2329, 0x23ce, 1), (0x23cf, 0x23cf, 3),
    (0x23d0, 0x23e8, 1), (0x23e9, 0x23f3, 3), (0x23f4, 0x23f7, 1),
    (0x23f8, 0x23fa, 3), (0x23fb, 0x24c1, 1), (0x24c2, 0x24c2, 3),
    (0x24c3, 0x25a9, 1), (0x25aa, 0x25ab, 3), (0x25ac, 0x25b5, 1),
    (0x25b6, 0x25b6, 3), (0x25b7, 0x25bf, 1), (0x25c0, 0x25c0, 3),
    (0x25c1, 0x25fa, 1), (0x25fb, 0x25fe, 3), (0x25ff, 0x25ff, 1),
    (0x2600, 0x2604, 3), (0x2605, 0x260d, 1), (0x260e, 0x260e, 3),
    (0x260f, 0x2610, 1), (0x2611, 0x2611, 3), (0x2612, 0x2613, 1),
    (0x2614, 0x2615, 3), (0x2616, 0x2617, 1), (0x2618, 0x2618, 3),
    (0x2619, 0x261c, 1), (0x261d, 0x261d, 3), (0x261e, 0x261f, 1),
    (0x2620, 0x2620, 3), (0x2621, 0x2621, 1), (0x2622, 0x2623, 3),
    (0x2624, 0x2625, 1), (0x2626, 0x2626, 3), (0x2627, 0x2629, 1),
    (0x262a, 0x262a, 3), (0x262b, 0x262d, 1), (0x262e, 0x262f, 3),
    (0x2630, 0x2637, 1), (0x2638, 0x263a, 3), (0x263b, 0x263f, 1),
    (0x2640, 0x2640, 3), (0x2641, 0x2641, 1), (0x2642, 0x2642, 3),
    (0x2643, 0x2647, 1), (0x2648, 0x2653, 3), (0x2654, 0x265e, 1),
    (0x265f, 0x2660, 3), (0x2661, 0x2662, 1), (0x2663, 0x2663, 3),
    (0x2664, 0x2664, 1), (0x2665, 0x2666, 3), (0x2667, 0x2667, 1),
    (0x2668, 0x2668, 3), (0x2669, 0x267a, 1), (0x267b, 0x267b, 3),
    (0x267c, 0x267d, 1), (0x267e, 0x267f, 3), (0x2680, 0x2691, 1),
    (0x2692, 0x2697, 3), (0x2698, 0x2698, 1), (0x2699, 0x2699, 3),
    (0x269a, 0x269a, 1), (0x269b, 0x269c, 3), (0x269d, 0x269f, 1),
    (0x26a0, 0x26a1, 3), (0x26a2, 0x26a6, 1), (0x26a7, 0x26a7, 3),
    (0x26a8, 0x26a9, 1), (0x26aa, 0x26ab, 3), (0x26ac, 0x26af, 1),
    (0x26b0, 0x26b1, 3), (0x26b2, 0x26bc, 1), (0x26bd, 0x26be, 3),
    (0x26bf, 0x26c3, 1), (0x26c4, 0x26c5, 3), (0x26c6, 0x26c7, 1),
    (0x26c8, 0x26c8, 3), (0x26c9, 0x26cd, 1), (0x26ce, 0x26cf, 3),
    (0x26d0, 0x26d0, 1), (0x26d1, 0x26d1, 3), (0x26d2, 0x26d2, 1),
    (0x26d3, 0x26d4, 3), (0x26d5, 0x26e8, 1), (0x26e9, 0x26ea, 3),
    (0x26eb, 0x26ef, 1), (0x26f0, 0x26f5, 3), (0x26f6, 0x26f6, 1),
    (0x26f7, 0x26fa, 3), (0x26fb, 0x26fc, 1), (0x26fd, 0x26fd, 3),
    (0x26fe, 0x2701, 1), (0x2702, 0x2702, 3), (0x2703, 0x2704, 1),
    (0x2705, 0x2705, 3), (0x2706, 0x2707, 1), (0x2708, 0x270d, 3),
    (0x270e, 0x270e, 1), (0x270f, 0x270f, 3), (0x2710, 0x2711, 1),
    (0x2712, 0x2712, 3), (0x2713, 0x2713, 1), (0x2714, 0x2714, 3),
    (0x2715, 0x2715, 1), (0x2716, 0x2716, 3), (0x2717, 0x271c, 1),
    (0x271d, 0x271d, 3), (0x271e, 0x2720, 1), (0x2721, 0x2721, 3),
    (0x2722, 0x2727, 1), (0x2728, 0x2728, 3), (0x2729, 0x2732, 1),
    (0x2733, 0x2734, 3), (0x2735, 0x2743, 1), (0x2744, 0x2744, 3),
    (0x2745, 0x2746, 1), (0x2747, 0x2747, 3), (0x2748, 0x274b, 1),
    (0x274c, 0x274c, 3), (0x274d, 0x274d, 1), (0x274e, 0x274e, 3),
    (0x274f, 0x2752, 1), (0x2753, 0x2755, 3), (0x2756, 0x2756, 1),
    (0x2757, 0x2757, 3), (0x2758, 0x2762, 1), (0x2763, 0x2764, 3),
    (0x2765, 0x2794, 1), (0x2795, 0x2797, 3), (0x2798, 0x27a0, 1),
    (0x27a1, 0x27a1, 3), (0x27a2, 0x27af, 1), (0x27b0, 0x27b0, 3),
    (0x27b1, 0x27be, 1), (0x27bf, 0x27bf, 3), (0x27c0, 0x27ff, 1),
    (0x2800, 0x28ff, 10240), (0x2900, 0x2933, 1), (0x2934, 0x2935, 3),
    (0x2936, 0x2998, 1), (0x29d8, 0x29db, 1), (0x29fc, 0x29fd, 1),
    (0x2b05, 0x2b07, 3), (0x2b1b, 0x2b1c, 3), (0x2b50, 0x2b50, 3),
    (0x2b55, 0x2b55, 3), (0x2e00, 0x2e7f, 1), (0x3000, 0x3000, 0),
    (0x3001, 0x3020, 1), (0x3030, 0x3030, 3), (0x303d, 0x303d, 3),
    (0x3040, 0x309f, 12352), (0x30a0, 0x30ff, 12448), (0x3297, 0x3297, 3),
    (0x3299, 0x3299, 3), (0x3300, 0x9fff, 19968), (0xac00, 0xd7a3, 44032),
    (0xf900, 0xfaff, 19968), (0xfd3e, 0xfd3f, 1), (0xfe30, 0xfe6b, 1),
    (0xff00, 0xff0f, 1), (0xff1a, 0xff20, 1), (0xff3b, 0xff40, 1),
    (0xff5b, 0xff65, 1), (0x1d000, 0x1d24f, 1), (0x1d400, 0x1d7ff, 1),
    (0x1f000, 0x1f003, 1), (0x1f004, 0x1f004, 3), (0x1f005, 0x1f0ce, 1),
    (0x1f0cf, 0x1f0cf, 3), (0x1f0d0, 0x1f16f, 1), (0x1f170, 0x1f171, 3),
    (0x1f172, 0x1f17d, 1), (0x1f17e, 0x1f17f, 3), (0x1f180, 0x1f18d, 1),
    (0x1f18e, 0x1f18e, 3), (0x1f18f, 0x1f190, 1), (0x1f191, 0x1f19a, 3),
    (0x1f19b, 0x1f1e5, 1), (0x1f1e6, 0x1f1ff, 3), (0x1f200, 0x1f200, 1),
    (0x1f201, 0x1f202, 3), (0x1f203, 0x1f219, 1), (0x1f21a, 0x1f21a, 3),
    (0x1f21b, 0x1f22e, 1), (0x1f22f, 0x1f22f, 3), (0x1f230, 0x1f231, 1),
    (0x1f232, 0x1f23a, 3), (0x1f23b, 0x1f24f, 1), (0x1f250, 0x1f251, 3),
    (0x1f252, 0x1f2ff, 1), (0x1f300, 0x1f321, 3), (0x1f322, 0x1f323, 1),
    (0x1f324, 0x1f393, 3), (0x1f394, 0x1f395, 1), (0x1f396, 0x1f397, 3),
    (0x1f398, 0x1f398, 1), (0x1f399, 0x1f39b, 3), (0x1f39c, 0x1f39d, 1),
    (0x1f39e, 0x1f3f0, 3), (0x1f3f1, 0x1f3f2, 1), (0x1f3f3, 0x1f3f5, 3),
    (0x1f3f6, 0x1f3f6, 1), (0x1f3f7, 0x1f3fa, 3), (0x1f3fb, 0x1f3ff, 1),
    (0x1f400, 0x1f4fd, 3), (0x1f4fe, 0x1f4fe, 1), (0x1f4ff, 0x1f53d, 3),
    (0x1f53e, 0x1f548, 1), (0x1f549, 0x1f54e, 3), (0x1f54f, 0x1f54f, 1),
    (0x1f550, 0x1f567, 3), (0x1f568, 0x1f56e, 1), (0x1f56f, 0x1f570, 3),
    (0x1f571, 0x1f572, 1), (0x1f573, 0x1f57a, 3), (0x1f57b, 0x1f586, 1),
    (0x1f587, 0x1f587, 3), (0x1f588, 0x1f589, 1), (0x1f58a, 0x1f58d, 3),
    (0x1f58e, 0x1f58f, 1), (0x1f590, 0x1f590, 3), (0x1f591, 0x1f594, 1),
    (0x1f595, 0x1f596, 3), (0x1f597, 0x1f5a3, 1), (0x1f5a4, 0x1f5a5, 3),
    (0x1f5a6, 0x1f5a7, 1), (0x1f5a8, 0x1f5a8, 3), (0x1f5a9, 0x1f5b0, 1),
    (0x1f5b1, 0x1f5b2, 3), (0x1f5b3, 0x1f5bb, 1), (0x1f5bc, 0x1f5bc, 3),
    (0x1f5bd, 0x1f5c1, 1), (0x1f5c2, 0x1f5c4, 3), (0x1f5c5, 0x1f5d0, 1),
    (0x1f5d1, 0x1f5d3, 3), (0x1f5d4, 0x1f5db, 1), (0x1f5dc, 0x1f5de, 3),
    (0x1f5df, 0x1f5e0, 1), (0x1f5e1, 0x1f5e1, 3), (0x1f5e2, 0x1f5e2, 1),
    (0x1f5e3, 0x1f5e3, 3), (0x1f5e4, 0x1f5e7, 1), (0x1f5e8, 0x1f5e8, 3),
    (0x1f5e9, 0x1f5ee, 1), (0x1f5ef, 0x1f5ef, 3), (0x1f5f0, 0x1f5f2, 1),
    (0x1f5f3, 0x1f5f3, 3), (0x1f5f4, 0x1f5f9, 1), (0x1f5fa, 0x1f64f, 3),
    (0x1f650, 0x1f67f, 1), (0x1f680, 0x1f6c5, 3), (0x1f6c6, 0x1f6ca, 1),
    (0x1f6cb, 0x1f6d2, 3), (0x1f6d3, 0x1f6d4, 1), (0x1f6d5, 0x1f6d8, 3),
    (0x1f6d9, 0x1f6db, 1), (0x1f6dc, 0x1f6e5, 3), (0x1f6e6, 0x1f6e8, 1),
    (0x1f6e9, 0x1f6e9, 3), (0x1f6ea, 0x1f6ea, 1), (0x1f6eb, 0x1f6ec, 3),
    (0x1f6ed, 0x1f6ef, 1), (0x1f6f0, 0x1f6f0, 3), (0x1f6f1, 0x1f6f2, 1),
    (0x1f6f3, 0x1f6fc, 3), (0x1f6fd, 0x1f7df, 1), (0x1f7e0, 0x1f7eb, 3),
    (0x1f7ec, 0x1f7ef, 1), (0x1f7f0, 0x1f7f0, 3), (0x1f7f1, 0x1f90b, 1),
    (0x1f90c, 0x1f93a, 3), (0x1f93b, 0x1f93b, 1), (0x1f93c, 0x1f945, 3),
    (0x1f946, 0x1f946, 1), (0x1f947, 0x1f9ff, 3), (0x1fa70, 0x1fa7c, 3),
    (0x1fa80, 0x1fa8a, 3), (0x1fa8e, 0x1fac6, 3), (0x1fac8, 0x1fac8, 3),
    (0x1facd, 0x1fadc, 3), (0x1fadf, 0x1faea, 3), (0x1faef, 0x1faf8, 3),
    (0x20000, 0x2a6df, 19968), (0x2a700, 0x2b81f, 19968), (0x2f800, 0x2fa1f, 19968),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected values read off Neovim 0.12.6's `charclass()`.
    #[test]
    fn characters_are_of_the_classes_neovim_gives_them() {
        let k = defaults();
        for (c, cls) in [
            ('a', 2),
            ('_', 2),
            ('9', 2),
            ('-', 1),
            ('.', 1),
            (' ', 0),
            ('\t', 0),
            ('\u{a0}', 0),
            ('é', 2),
            ('—', 1),
            ('→', 1),
            ('…', 1),
            ('✓', 1),
            ('✔', 3),
            ('😀', 3),
            ('🇦', 3),
            ('中', 0x4e00),
            ('𠀀', 0x4e00),
            ('あ', 0x3040),
            ('ア', 0x30a0),
            ('가', 0xac00),
            ('⠹', 0x2800),
            ('\u{3000}', 0),
            ('、', 1),
            ('ž', 2),
            ('Ω', 2),
        ] {
            assert_eq!(class(c, &k, false), cls, "{c:?}");
            assert_eq!(class(c, &k, true), cls.min(1), "{c:?}, big");
        }
    }

    #[test]
    fn the_table_is_runs_in_order() {
        assert!(CLASSES.windows(2).all(|w| w[0].1 < w[1].0));
        assert!(CLASSES
            .iter()
            .all(|&(a, b, c)| a <= b && c != 2 && a >= 0x100));
    }
}
