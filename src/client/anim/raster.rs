//! Sub-cell shapes drawn with block elements.
//!
//! A cell is sampled at 8×8 points and matched against each block element
//! (`U+2580`–`U+259F`), normal or inverted; the nearest wins. Block elements
//! are drawn edge to edge by every monospace font; legacy-computing sextants
//! and octants are not in enough fonts.

const SAMPLES: usize = 8;

/// A cell's 64 sample points: bit `row * 8 + col`.
pub type Mask = u64;

pub const EMPTY: Mask = 0;
pub const FULL: Mask = u64::MAX;

const fn rows(from: usize, to: usize) -> Mask {
    let mut m = 0u64;
    let mut r = from;
    while r < to {
        m |= 0xffu64 << (r * 8);
        r += 1;
    }
    m
}

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

/// `█` must stay first: [`fill`] skips it.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    Empty,
    /// The shape's colour; the cell's text is kept.
    Full,
    /// `glyph` in the shape's colour or, if `inverted`, in the cell's
    /// background on the shape's colour.
    Glyph {
        glyph: char,
        inverted: bool,
    },
}

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

/// In cells: `x` across, `y` down.
pub type Point = (f32, f32);

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

/// Winding-number test: correct even for self-crossing polygons.
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

/// Braille numbers dots down the left column, then the right, with the bottom
/// pair last, hence the bit order.
pub fn braille_dot(fx: f32, fy: f32) -> u8 {
    const DOTS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
    let col = ((fx * 2.0) as usize).min(1);
    let row = ((fy * 4.0) as usize).min(3);
    DOTS[row][col]
}

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

    /// A fifth of a cell rounds to two eighths.
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

    /// Three quarters from the top is a lower quarter, inverted.
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

    #[test]
    fn a_crossed_quad_is_still_filled() {
        // Crossed at (1, 1): left and right lobes inside, above and below not.
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
