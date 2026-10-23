//! Neovide-style animated cursor for a terminal.
//!
//! The cursor is a quadrilateral whose four corners are each sprung towards
//! the target cell (or bar/underline). The two leading corners are faster than
//! the trailing two ([`Settings::trail`]), so it stretches along the move. A
//! move of one or two cells along a line (typing) uses the short duration and
//! no trail, like Neovide's `cursor_short_animation_length`.
//!
//! While moving it is rasterised over the cells ([`super::raster`]): fully
//! covered cells take the cursor colour and keep their text; partly covered
//! ones get the nearest block element. The terminal cursor is hidden until it
//! arrives. The colour fades towards the tail ([`Settings::gradient`]), mixed
//! with the cells' colours; a `gradient` of 0 matches Neovide's solid trail.
//!
//! Geometry is in cells (`x` right, `y` down); directions treat a row as
//! [`ASPECT`] columns tall.

use super::raster::{self, Fill, Point};
use super::spring::Spring;
use crate::client::compose::Frame;
use crate::client::grid::Text;
use crate::client::style::{self, Color, Colors};
use crate::palette::Rgb;

/// Cell height / width.
pub const ASPECT: f32 = 2.0;

/// Distance (cells) at which every corner counts as arrived: just under
/// 1/16, half the rasteriser's sample spacing.
const ARRIVED: f32 = 0.06;

/// A cursor's shape, in cells.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Rect {
    fn corner(&self, i: usize) -> Point {
        match i {
            0 => (self.x0, self.y0),
            1 => (self.x1, self.y0),
            2 => (self.x1, self.y1),
            _ => (self.x0, self.y1),
        }
    }

    fn center(&self) -> Point {
        ((self.x0 + self.x1) / 2.0, (self.y0 + self.y1) / 2.0)
    }
}

/// Corner directions from the centre: TL, TR, BR, BL.
const OUTWARD: [Point; 4] = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)];

/// Timing and look, fixed by the client (`[effects.smear]` only turns it on).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    /// Seconds for a move to settle.
    pub duration: f32,
    /// Seconds for a move of a cell or two along a line.
    pub short: f32,
    /// How far ahead the leading corners are: 0 not at all, 1 instant.
    pub trail: f32,
    /// Colour fade towards the tail: 0 none, 1 none of the cursor colour left
    /// at the very end.
    pub gradient: f32,
}

#[derive(Debug, Clone, Copy, Default)]
struct Corner {
    x: Spring,
    y: Spring,
    duration: f32,
}

#[derive(Debug, Clone, Default)]
pub struct Smear {
    corners: [Corner; 4],
    target: Option<Rect>,
}

impl Smear {
    pub fn target(&self) -> Option<Rect> {
        self.target
    }

    pub fn snap(&mut self, rect: Rect) {
        self.target = Some(rect);
        self.corners = [Corner::default(); 4];
    }

    /// Animate to `rect` from the current position; snaps if there is no
    /// previous target.
    pub fn go(&mut self, rect: Rect, s: &Settings) {
        let Some(from) = self.target else {
            return self.snap(rect);
        };
        let now = self.quad();
        let (fx, fy) = centroid(&now);
        let (tx, ty) = rect.center();
        let travel = (tx - fx, (ty - fy) * ASPECT);
        let len = (travel.0 * travel.0 + travel.1 * travel.1).sqrt();
        // A cell or two along the same line (typing).
        let short = (rect.y0 - from.y0).abs() < 0.01 && (rect.x0 - from.x0).abs() <= 2.0;
        let mut order: Vec<(usize, f32)> = (0..4)
            .map(|i| {
                let (ox, oy) = OUTWARD[i];
                let (ox, oy) = (ox, oy * ASPECT);
                let olen = (ox * ox + oy * oy).sqrt();
                let along = if len > 0.0 {
                    (ox * travel.0 + oy * travel.1) / (olen * len)
                } else {
                    0.0
                };
                (i, along)
            })
            .collect();
        order.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (rank, (i, _)) in order.into_iter().enumerate() {
            let (tx, ty) = rect.corner(i);
            let c = &mut self.corners[i];
            c.x.position = now[i].0 - tx;
            c.y.position = now[i].1 - ty;
            c.duration = if short || len < 0.01 {
                s.short.min(s.duration)
            } else if rank < 2 {
                s.duration * (1.0 - s.trail).clamp(0.0, 1.0)
            } else {
                s.duration
            };
        }
        self.target = Some(rect);
    }

    /// Advance `dt` seconds; returns whether still moving. Stops once every
    /// corner is within `ARRIVED`, well before the springs settle, so the
    /// terminal cursor returns as soon as nothing visible is left to draw.
    pub fn step(&mut self, dt: f32) -> bool {
        let mut moving = false;
        for c in &mut self.corners {
            moving |= c.x.step(dt, c.duration);
            moving |= c.y.step(dt, c.duration);
        }
        let near = |s: &Spring| s.position.abs() < ARRIVED;
        if moving && self.corners.iter().all(|c| near(&c.x) && near(&c.y)) {
            self.corners = [Corner::default(); 4];
            return false;
        }
        moving
    }

    pub fn moving(&self) -> bool {
        self.corners.iter().any(|c| c.x.moving() || c.y.moving())
    }

    pub fn quad(&self) -> [Point; 4] {
        let Some(t) = self.target else {
            return [(0.0, 0.0); 4];
        };
        std::array::from_fn(|i| {
            let (x, y) = t.corner(i);
            (
                x + self.corners[i].x.position,
                y + self.corners[i].y.position,
            )
        })
    }

    /// Draw the cursor over `frame` in `color`. Fully covered cells get text
    /// in `text`, or their own background if `None`. Both fade towards the
    /// cell's colours along the trail by `s.gradient`.
    pub fn paint(
        &self,
        frame: &mut Frame,
        colors: &Colors,
        color: Rgb,
        text: Option<Rgb>,
        s: &Settings,
    ) {
        let Some(target) = self.target else {
            return;
        };
        let quad = self.quad();
        let shade = Shade::new(&quad, target.center(), s.gradient);
        let (rows, cols) = raster::bounds(&quad);
        for row in rows {
            for col in cols.clone() {
                let (Ok(r), Ok(c)) = (usize::try_from(row), usize::try_from(col)) else {
                    continue;
                };
                let Some(cell) = frame.get_mut(r, c) else {
                    continue;
                };
                let bg = colors.visual_bg(&cell.style);
                let fade = shade.at(row, col);
                let color = style::mix(fade, color, bg);
                match raster::fill(raster::coverage(&quad, row, col)) {
                    Fill::Empty => {}
                    Fill::Full => {
                        let fg = colors.visual_fg(&cell.style);
                        cell.style.fg = Color::Rgb(style::mix(fade, text.unwrap_or(bg), fg));
                        cell.style.bg = Color::Rgb(color);
                        cell.style.reverse = false;
                    }
                    Fill::Glyph { glyph, inverted } => {
                        cell.text = Text::Char(glyph);
                        cell.wide = false;
                        let (fg, bg) = if inverted { (bg, color) } else { (color, bg) };
                        cell.style = style::Style {
                            fg: Color::Rgb(fg),
                            bg: Color::Rgb(bg),
                            ..Default::default()
                        };
                    }
                }
            }
        }
    }
}

/// Percent of the cursor colour a trail cell shows: 100 at and ahead of the
/// head, falling linearly to the tail by the gradient. Measured along the
/// line from the quad's centroid to the target centre (aspect-corrected), so
/// a diagonal smear fades along its length.
struct Shade {
    /// Unit direction; `None` draws a single colour.
    axis: Option<Point>,
    /// Projections of the tail and head onto `axis`.
    tail: f32,
    head: f32,
    gradient: f32,
}

impl Shade {
    fn new(quad: &[Point; 4], head: Point, gradient: f32) -> Self {
        let (cx, cy) = centroid(quad);
        let way = (head.0 - cx, (head.1 - cy) * ASPECT);
        let len = (way.0 * way.0 + way.1 * way.1).sqrt();
        let axis = (len > 0.01 && gradient > 0.0).then(|| (way.0 / len, way.1 / len));
        let (tail, head) = match axis {
            Some(a) => (
                quad.iter()
                    .map(|p| along(*p, a))
                    .fold(f32::INFINITY, f32::min),
                along(head, a),
            ),
            None => (0.0, 0.0),
        };
        Self {
            axis,
            tail,
            head,
            gradient: gradient.clamp(0.0, 1.0),
        }
    }

    /// Sampled at the cell centre.
    fn at(&self, row: i64, col: i64) -> u8 {
        let Some(a) = self.axis else {
            return 100;
        };
        let p = along((col as f32 + 0.5, row as f32 + 0.5), a);
        let t = ((p - self.tail) / (self.head - self.tail).max(0.01)).clamp(0.0, 1.0);
        ((1.0 - self.gradient * (1.0 - t)) * 100.0).round() as u8
    }
}

/// Projection of `p` onto unit vector `a`, aspect-corrected.
fn along(p: Point, a: Point) -> f32 {
    p.0 * a.0 + p.1 * ASPECT * a.1
}

fn centroid(q: &[Point; 4]) -> Point {
    let x = q.iter().map(|p| p.0).sum::<f32>() / 4.0;
    let y = q.iter().map(|p| p.1).sum::<f32>() / 4.0;
    (x, y)
}

/// The whole cell for a block, the left `percent` for a bar, the bottom
/// `percent` for an underline.
pub fn shape_rect(row: i64, col: i64, shape: crate::client::redraw::Shape, percent: u8) -> Rect {
    use crate::client::redraw::Shape;
    let (x, y) = (col as f32, row as f32);
    let p = f32::from(percent.clamp(1, 100)) / 100.0;
    match shape {
        Shape::Block => Rect {
            x0: x,
            y0: y,
            x1: x + 1.0,
            y1: y + 1.0,
        },
        Shape::Vertical => Rect {
            x0: x,
            y0: y,
            x1: x + p,
            y1: y + 1.0,
        },
        Shape::Horizontal => Rect {
            x0: x,
            y0: y + 1.0 - p,
            x1: x + 1.0,
            y1: y + 1.0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::redraw::Shape;

    const S: Settings = Settings {
        duration: 0.15,
        short: 0.04,
        trail: 0.8,
        gradient: 0.9,
    };

    fn cell(row: i64, col: i64) -> Rect {
        shape_rect(row, col, Shape::Block, 0)
    }

    #[test]
    fn the_cursor_snaps_first_and_travels_after() {
        let mut s = Smear::default();
        s.go(cell(0, 0), &S);
        assert!(!s.moving(), "nowhere to come from");
        s.go(cell(10, 40), &S);
        assert!(s.moving());
        let mut t = 0.0;
        while s.step(0.004) {
            t += 0.004;
            assert!(t < 1.0, "never arrived");
        }
        assert_eq!(
            s.quad(),
            [(40.0, 10.0), (41.0, 10.0), (41.0, 11.0), (40.0, 11.0)]
        );
    }

    #[test]
    fn the_leading_edge_is_ahead_of_the_trailing_one() {
        let mut s = Smear::default();
        s.snap(cell(5, 0));
        s.go(cell(5, 30), &S);
        s.step(0.03);
        let q = s.quad();
        let width = q[1].0 - q[0].0;
        assert!(width > 5.0, "stretched to {width}");
        assert!(
            q[1].0 > 25.0,
            "the leading edge is nearly there: {}",
            q[1].0
        );
        assert!(
            q[0].0 < 15.0,
            "the trailing edge is well behind: {}",
            q[0].0
        );
    }

    #[test]
    fn a_short_hop_is_quick_and_does_not_stretch() {
        let mut s = Smear::default();
        s.snap(cell(5, 3));
        s.go(cell(5, 4), &S);
        s.step(0.01);
        let q = s.quad();
        assert!((q[1].0 - q[0].0 - 1.0).abs() < 1e-3, "{q:?}");
        assert!(!s.step(0.05), "settled within the short time");
    }

    #[test]
    fn a_new_target_starts_from_where_the_cursor_is() {
        let mut s = Smear::default();
        s.snap(cell(0, 0));
        s.go(cell(0, 20), &S);
        s.step(0.05);
        let before = s.quad();
        s.go(cell(10, 20), &S);
        let after = s.quad();
        for i in 0..4 {
            assert!((before[i].0 - after[i].0).abs() < 1e-4);
            assert!((before[i].1 - after[i].1).abs() < 1e-4);
        }
    }

    /// (column, red) of each painted cell in `row`, for a pure red cursor over
    /// black.
    fn painted_reds(s: &Smear, settings: &Settings, row: usize) -> Vec<(usize, u8)> {
        use crate::client::compose::Frame;
        use crate::palette::Palette;
        let colors = Colors {
            rgb: true,
            defaults: Default::default(),
            term: Palette {
                fg: Rgb(255, 255, 255),
                bg: Rgb(0, 0, 0),
                ansi: [Rgb(0, 0, 0); 16],
            },
        };
        let mut frame = Frame::new(40, 12, Default::default());
        s.paint(&mut frame, &colors, Rgb(255, 0, 0), None, settings);
        (0..40)
            .filter_map(|c| {
                let cell = frame.get(row, c)?;
                match (cell.style.bg, cell.style.fg) {
                    (Color::Rgb(Rgb(r, 0, 0)), _) if r > 0 && cell.text == Text::Char(' ') => {
                        Some((c, r))
                    }
                    _ => None,
                }
            })
            .collect()
    }

    #[test]
    fn the_trail_fades_towards_its_end() {
        let mut s = Smear::default();
        s.snap(cell(5, 2));
        s.go(cell(5, 34), &S);
        s.step(0.03);
        let reds = painted_reds(&s, &S, 5);
        assert!(reds.len() > 8, "a long trail: {reds:?}");
        let (head, tail) = (reds[reds.len() - 1].1, reds[0].1);
        assert!(head > 240, "the head is the cursor's own: {reds:?}");
        assert!(tail < 90, "the end of the trail has little of it: {reds:?}");
        assert!(
            reds.windows(2).all(|w| w[0].1 <= w[1].1),
            "and more of it towards the head, all the way: {reds:?}"
        );

        let solid = Settings { gradient: 0.0, ..S };
        let reds = painted_reds(&s, &solid, 5);
        assert!(reds.iter().all(|(_, r)| *r == 255), "{reds:?}");
    }

    #[test]
    fn a_cursor_at_rest_does_not_fade() {
        let mut s = Smear::default();
        s.snap(cell(3, 7));
        assert_eq!(painted_reds(&s, &S, 3), vec![(7, 255)]);
    }

    #[test]
    fn shapes_take_their_part_of_the_cell() {
        assert_eq!(
            shape_rect(1, 2, Shape::Vertical, 25),
            Rect {
                x0: 2.0,
                y0: 1.0,
                x1: 2.25,
                y1: 2.0
            }
        );
        assert_eq!(
            shape_rect(1, 2, Shape::Horizontal, 20),
            Rect {
                x0: 2.0,
                y0: 1.8,
                x1: 3.0,
                y1: 2.0
            }
        );
    }
}
