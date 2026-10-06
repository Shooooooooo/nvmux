//! Shapes finer than a cell, in the characters a terminal has for them.
//!
//! A cell is sampled at eight by eight points, and the shape — the cursor's
//! four corners, wherever its animation has put them — is reduced to which of
//! those 64 points it covers. That is then matched against the block elements
//! (`U+2580`–`U+259F`: the halves, the eighths, the quadrants), each one a
//! pattern over the same 64 points, drawn either way round: a glyph in the
//! shape's colour on the cell's background, or the background's colour on the
//! shape's, which turns `▁` into a cell seven-eighths full. The nearest one
//! wins. Block elements, because every monospace font has them and draws them
//! edge to edge; the finer sextants and octants of Unicode's legacy computing
//! block are not in enough fonts to be relied on.
//!
//! Sparks are drawn in braille instead (see [`super::vfx`]): eight dots to a
//! cell, each one a spark, the way the picker's starfield draws its stars.

/// The point every 8×8 sample sits at within its eighth of the cell.
const SAMPLES: usize = 8;

/// A pattern of a cell's 64 sample points: bit `row * 8 + col`.
pub type Mask = u64;

pub const EMPTY: Mask = 0;
pub const FULL: Mask = u64::MAX;

/// The rows `from..to` of a cell, every column.
const fn rows(from: usize, to: usize) -> Mask {
    let mut m = 0u64;
    let mut r = from;
    while r < to {
        m |= 0xffu64 << (r * 8);
        r += 1;
    }
    m
}

/// The columns `from..to` of a cell, every row.
const fn cols(from: usize, to: usize) -> Mask {
    let mut row = 0u64;
    let mut c = from;
    while c < to {
        row |= 1 << c;
        c += 1;
    }
    let mut m = 0u64;
    let mut r = 0;
    while r < 8 {
        m |= row << (r * 8);
        r += 1;
    }
    m
}

const UL: Mask = rows(0, 4) & cols(0, 4);
const UR: Mask = rows(0, 4) & cols(4, 8);
const LL: Mask = rows(4, 8) & cols(0, 4);
const LR: Mask = rows(4, 8) & cols(4, 8);

/// Every block element, and the points it fills.
pub const BLOCKS: [(char, Mask); 29] = [
    ('█', FULL),
    ('▀', rows(0, 4)),
    ('▔', rows(0, 1)),
    ('▁', rows(7, 8)),
    ('▂', rows(6, 8)),
    ('▃', rows(5, 8)),
    ('▄', rows(4, 8)),
    ('▅', rows(3, 8)),
    ('▆', rows(2, 8)),
    ('▇', rows(1, 8)),
    ('▏', cols(0, 1)),
    ('▎', cols(0, 2)),
    ('▍', cols(0, 3)),
    ('▌', cols(0, 4)),
    ('▋', cols(0, 5)),
    ('▊', cols(0, 6)),
    ('▉', cols(0, 7)),
    ('▐', cols(4, 8)),
    ('▕', cols(7, 8)),
    ('▘', UL),
    ('▝', UR),
    ('▖', LL),
    ('▗', LR),
    ('▚', UL | LR),
    ('▞', UR | LL),
    ('▙', UL | LL | LR),
    ('▛', UL | UR | LL),
    ('▜', UL | UR | LR),
    ('▟', UR | LL | LR),
];

/// How a cell is to show the part of a shape that falls in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    /// None of it: the cell is left as it is.
    Empty,
    /// All of it: the cell takes the shape's colour, its text kept.
    Full,
    /// Some: this glyph, in the shape's colour if `inverted` is false, in the
    /// cell's background on the shape's colour if it is true.
    Glyph { glyph: char, inverted: bool },
}

/// The block element nearest to the points a shape covers in a cell.
pub fn fill(covered: Mask) -> Fill {
    if covered == EMPTY {
        return Fill::Empty;
    }
    if covered == FULL {
        return Fill::Full;
    }
    let mut best = (covered.count_ones(), Fill::Empty);
    let mut consider = |dist: u32, fill: Fill| {
        if dist < best.0 {
            best = (dist, fill);
        }
    };
    consider((!covered).count_ones(), Fill::Full);
    for (glyph, mask) in BLOCKS.iter().skip(1) {
        consider(
            (covered ^ mask).count_ones(),
            Fill::Glyph {
                glyph: *glyph,
                inverted: false,
            },
        );
        consider(
            (covered ^ !mask).count_ones(),
            Fill::Glyph {
                glyph: *glyph,
                inverted: true,
            },
        );
    }
    best.1
}

/// A point, in cells: `x` along, `y` down.
pub type Point = (f32, f32);

/// The sample points of the cell at `row`, `col` inside the polygon `poly`.
pub fn coverage(poly: &[Point], row: i64, col: i64) -> Mask {
    let mut m = 0u64;
    for sy in 0..SAMPLES {
        let y = row as f32 + (sy as f32 + 0.5) / SAMPLES as f32;
        for sx in 0..SAMPLES {
            let x = col as f32 + (sx as f32 + 0.5) / SAMPLES as f32;
            if inside(poly, (x, y)) {
                m |= 1 << (sy * SAMPLES + sx);
            }
        }
    }
    m
}

/// Whether `p` is inside `poly`, by its winding number: right for any
/// polygon, convex or not, however its corners have been pulled about.
pub fn inside(poly: &[Point], p: Point) -> bool {
    let mut winding = 0i32;
    for i in 0..poly.len() {
        let a = poly[i];
        let b = poly[(i + 1) % poly.len()];
        let side = (b.0 - a.0) * (p.1 - a.1) - (p.0 - a.0) * (b.1 - a.1);
        if a.1 <= p.1 {
            if b.1 > p.1 && side > 0.0 {
                winding += 1;
            }
        } else if b.1 <= p.1 && side < 0.0 {
            winding -= 1;
        }
    }
    winding != 0
}

/// The cells a polygon touches: rows and columns, each a half-open range.
pub fn bounds(poly: &[Point]) -> (std::ops::Range<i64>, std::ops::Range<i64>) {
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(x, y) in poly {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    (
        y0.floor() as i64..y1.ceil() as i64,
        x0.floor() as i64..x1.ceil() as i64,
    )
}

/// The braille dot for a point within a cell, `fx` and `fy` from 0 to 1.
/// Braille numbers its dots down the left column, then the right, with the
/// bottom pair last — which is why the bits are not in order.
pub fn braille_dot(fx: f32, fy: f32) -> u8 {
    const DOTS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
    let col = ((fx * 2.0) as usize).min(1);
    let row = ((fy * 4.0) as usize).min(3);
    DOTS[row][col]
}

/// The braille character with these dots.
pub fn braille(dots: u8) -> char {
    char::from_u32(0x2800 + u32::from(dots)).unwrap_or(' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> [Point; 4] {
        [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
    }

    #[test]
    fn a_whole_cell_is_full_and_an_untouched_one_empty() {
        let r = rect(3.0, 2.0, 4.0, 3.0);
        assert_eq!(fill(coverage(&r, 2, 3)), Fill::Full);
        assert_eq!(fill(coverage(&r, 2, 4)), Fill::Empty);
        assert_eq!(fill(coverage(&r, 1, 3)), Fill::Empty);
    }

    /// A bar cursor a quarter of a cell wide is a quarter block.
    #[test]
    fn a_bar_is_a_left_block() {
        let r = rect(5.0, 0.0, 5.25, 1.0);
        assert_eq!(
            fill(coverage(&r, 0, 5)),
            Fill::Glyph {
                glyph: '▎',
                inverted: false
            }
        );
    }

    /// An underline a fifth of a cell high is the nearest lower eighths.
    #[test]
    fn an_underline_is_a_lower_block() {
        let r = rect(0.0, 0.8, 1.0, 1.0);
        assert_eq!(
            fill(coverage(&r, 0, 0)),
            Fill::Glyph {
                glyph: '▂',
                inverted: false
            }
        );
    }

    /// Most of a cell is drawn as the little that is not, the other way
    /// round: three quarters from the top is a lower quarter, inverted.
    #[test]
    fn mostly_full_is_drawn_inverted() {
        let r = rect(0.0, 0.0, 1.0, 0.75);
        assert_eq!(
            fill(coverage(&r, 0, 0)),
            Fill::Glyph {
                glyph: '▂',
                inverted: true
            }
        );
    }

    /// A diagonal smear across a cell lands on a quadrant pattern.
    #[test]
    fn a_diagonal_is_a_quadrant() {
        let tri = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)];
        match fill(coverage(&tri, 0, 0)) {
            Fill::Glyph { glyph, inverted } => {
                assert!("▛▘▀▌".contains(glyph) || inverted, "{glyph}")
            }
            other => panic!("{other:?}"),
        }
    }

    /// The winding rule fills a quad however its corners are pulled, even
    /// crossed over.
    #[test]
    fn a_crossed_quad_is_still_filled() {
        // Crossed at (1, 1): its two lobes are left and right, and the two
        // between them, above and below, are outside.
        let bowtie = [(0.0, 0.0), (2.0, 2.0), (2.0, 0.0), (0.0, 2.0)];
        assert!(inside(&bowtie, (0.4, 1.0)));
        assert!(inside(&bowtie, (1.6, 1.0)));
        assert!(!inside(&bowtie, (1.0, 0.3)));
        assert!(!inside(&bowtie, (1.0, 1.7)));
    }

    #[test]
    fn bounds_cover_every_cell_touched() {
        let (rows, cols) = bounds(&rect(1.5, 2.25, 3.0, 4.75));
        assert_eq!(rows, 2..5);
        assert_eq!(cols, 1..3);
    }

    #[test]
    fn braille_dots_are_where_they_look() {
        assert_eq!(braille(braille_dot(0.1, 0.1)), '⠁');
        assert_eq!(braille(braille_dot(0.9, 0.9)), '⢀');
        assert_eq!(braille(braille_dot(0.1, 0.9)), '⡀');
        assert_eq!(braille(0), '⠀');
    }
}
