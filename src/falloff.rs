//! How far the screen around something of nvmux's own is taken toward the
//! background: around the cards in the margin, which nvmux's own client
//! draws over the editor (see [`crate::client::cards`]), and around the
//! settings when they float over a session (see `crate::pty::panel`).
//!
//! The nearer a cell is to the thing, the further it goes: a row counting as
//! [`ASPECT`] columns, a cell being about twice as tall as it is wide, out to
//! [`REACH`] columns, where it is left as it was. Right beside the thing a
//! cell keeps a tenth of itself ([`PEAK`]), and under it, at full strength,
//! it is emptied ([`EMPTIED`]). The step between is a smoothstep, so the
//! quiet thins out rather than stopping at an edge: what is under the thing
//! reads on a busy screen, and the screen around it still reads as itself.

/// How far it reaches from the thing, in columns.
pub const REACH: f32 = 18.0;

/// How far it takes what is right beside the thing: a tenth of what was there
/// left.
pub const PEAK: f32 = 0.9;

/// How many columns a row counts as: a cell is about twice as tall as it is
/// wide.
pub const ASPECT: f32 = 2.1;

/// Taken this far, a cell is emptied rather than drawn faintly.
pub const EMPTIED: f32 = 0.98;

/// Smoothstep: 0 at 0 and 1 at 1, and flat at both, so whatever it shapes
/// starts and ends without a jolt. Clamped either side.
pub fn ease(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// How far toward the background a cell `distance` columns from the thing is
/// taken, by a thing that takes what is right beside it `peak` of the way and
/// reaches `reach` columns: `peak` near it, and nothing from `reach` out. For
/// a cell beside the thing, not under it — what is under it is the caller's.
pub fn around(distance: f32, peak: f32, reach: f32) -> f32 {
    peak * (1.0 - ease(distance / reach))
}

/// How far the cell at (`col`, `row`) is from the rectangle `left`, `top`,
/// `width` columns and `height` rows, in columns, a row counting as
/// [`ASPECT`]: 0 for a cell inside it.
pub fn distance(col: u16, row: u16, left: u16, top: u16, width: u16, height: u16) -> f32 {
    let gap = |at: u16, from: u16, len: u16| {
        if at < from {
            from - at
        } else {
            at.saturating_sub((from + len).saturating_sub(1))
        }
    };
    let dx = f32::from(gap(col, left, width));
    let dy = f32::from(gap(row, top, height)) * ASPECT;
    (dx * dx + dy * dy).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The step is flat at both ends and half way at the middle.
    #[test]
    fn the_step_starts_and_ends_flat() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert_eq!(ease(0.5), 0.5);
        assert_eq!(ease(-1.0), 0.0, "clamped below");
        assert_eq!(ease(2.0), 1.0, "clamped above");
    }

    /// Beside the thing a cell keeps a tenth of itself, and from the reach out
    /// it keeps all of it.
    #[test]
    fn it_is_strongest_beside_the_thing_and_gone_at_its_reach() {
        assert!((around(0.0, PEAK, REACH) - PEAK).abs() < f32::EPSILON);
        assert_eq!(around(REACH, PEAK, REACH), 0.0);
        assert_eq!(around(REACH * 2.0, PEAK, REACH), 0.0);
        let near = around(2.0, PEAK, REACH);
        let far = around(10.0, PEAK, REACH);
        assert!(near > far && far > 0.0, "nearer is further: {near} {far}");
    }

    /// Inside the rectangle is no distance at all; beside it, along a row, a
    /// column a cell; above or below it, a row counts as more than two.
    #[test]
    fn a_row_counts_as_more_than_a_column() {
        // A rectangle at columns 10..20, rows 5..8.
        let d = |col, row| distance(col, row, 10, 5, 10, 3);
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
}
