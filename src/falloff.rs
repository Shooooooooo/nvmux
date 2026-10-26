//! How far cells around nvmux's own overlays are faded toward the background:
//! around the margin cards (see [`crate::client::cards`]) and around the
//! settings floating over a session (see `crate::pty::panel`).
//!
//! Strength falls by a smoothstep from [`PEAK`] beside the overlay to 0 at
//! [`REACH`] columns, a row counting as [`ASPECT`] columns. At [`EMPTIED`] or
//! more a cell is blanked.

/// Where the fade ends, in columns.
pub const REACH: f32 = 18.0;

/// Strength right beside the overlay: a cell keeps a tenth of itself.
pub const PEAK: f32 = 0.9;

/// Columns per row: a cell is about twice as tall as it is wide.
pub const ASPECT: f32 = 2.1;

/// From this strength up a cell is blanked rather than drawn faintly.
pub const EMPTIED: f32 = 0.98;

/// Smoothstep, with `x` clamped to `0..=1`.
pub fn ease(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// Strength for a cell `distance` columns beside (not under) the overlay:
/// `peak` at 0, falling to 0 at `reach`.
pub fn around(distance: f32, peak: f32, reach: f32) -> f32 {
    peak * (1.0 - ease(distance / reach))
}

/// Distance in columns from (`col`, `row`) to the rectangle with its corners
/// rounded `radius` columns, a row counting as [`ASPECT`]; 0 inside it.
///
/// Measured between cell centres, so any radius above 0 cuts the corner
/// cells. It is at most half the shorter side.
pub fn distance(
    col: u16,
    row: u16,
    left: u16,
    top: u16,
    width: u16,
    height: u16,
    radius: f32,
) -> f32 {
    // Past the nearer edge: positive outside, negative inside.
    let past = |at: u16, from: u16, len: u16| {
        let (at, from) = (i32::from(at), i32::from(from));
        (from - at).max(at - from - i32::from(len) + 1) as f32
    };
    let dx = past(col, left, width);
    let dy = past(row, top, height) * ASPECT;
    let radius = radius
        .min(f32::from(width.saturating_sub(1)) / 2.0)
        .min(f32::from(height.saturating_sub(1)) * ASPECT / 2.0);
    // Within `radius` of a corner on both axes, from its arc; elsewhere from
    // the nearer edge and exact, so a row off is ASPECT on the dot.
    let (x, y) = (dx + radius, dy + radius);
    if x > 0.0 && y > 0.0 {
        (x.hypot(y) - radius).max(0.0)
    } else {
        dx.max(dy).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_step_starts_and_ends_flat() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert_eq!(ease(0.5), 0.5);
        assert_eq!(ease(-1.0), 0.0, "clamped below");
        assert_eq!(ease(2.0), 1.0, "clamped above");
    }

    #[test]
    fn it_is_strongest_beside_the_thing_and_gone_at_its_reach() {
        assert!((around(0.0, PEAK, REACH) - PEAK).abs() < f32::EPSILON);
        assert_eq!(around(REACH, PEAK, REACH), 0.0);
        assert_eq!(around(REACH * 2.0, PEAK, REACH), 0.0);
        let near = around(2.0, PEAK, REACH);
        let far = around(10.0, PEAK, REACH);
        assert!(near > far && far > 0.0, "nearer is further: {near} {far}");
    }

    #[test]
    fn a_row_counts_as_more_than_a_column() {
        // A rectangle at columns 10..20, rows 5..8.
        let d = |col, row| distance(col, row, 10, 5, 10, 3, 0.0);
        assert_eq!(d(10, 5), 0.0);
        assert_eq!(d(19, 7), 0.0, "its far corner is in it");
        assert_eq!(d(9, 6), 1.0, "a column left of it");
        assert_eq!(d(22, 6), 3.0, "three columns right of it");
        assert!((d(12, 4) - ASPECT).abs() < f32::EPSILON, "a row above it");
        assert!(
            (d(12, 9) - 2.0 * ASPECT).abs() < f32::EPSILON,
            "two rows below it"
        );
        let corner = d(9, 4);
        assert!((corner - (1.0 + ASPECT * ASPECT).sqrt()).abs() < 1e-6);
    }

    #[test]
    fn a_round_corner_cuts_its_cells_and_is_measured_from_its_arc() {
        // Columns 10..60, rows 5..25.
        let d = |col, row| distance(col, row, 10, 5, 50, 20, 4.0);
        let square = |col, row| distance(col, row, 10, 5, 50, 20, 0.0);
        for col in [10, 13, 56, 59] {
            assert!(d(col, 5) > 0.0 && d(col, 24) > 0.0, "column {col}");
        }
        assert_eq!(d(14, 5), 0.0);
        assert_eq!(d(55, 24), 0.0);
        assert!(d(10, 6) > 0.0 && d(59, 23) > 0.0, "the outer cell below");
        assert_eq!(d(11, 6), 0.0);
        assert_eq!(d(10, 7), 0.0);
        assert_eq!(d(9, 15), 1.0, "beside a side, as square");
        assert_eq!(d(14, 4), ASPECT);
        assert!(d(9, 4) > square(9, 4), "further off the corner");
    }

    #[test]
    fn the_radius_is_at_most_half_the_shorter_side() {
        for col in 10..60 {
            assert_eq!(distance(col, 5, 10, 5, 50, 1, 4.0), 0.0, "{col}");
        }
        let low = |col| distance(col, 5, 10, 5, 50, 3, 9.0);
        assert_eq!(low(10), distance(10, 5, 10, 5, 50, 3, ASPECT));
    }
}
