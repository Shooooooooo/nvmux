//! A blinking cursor that fades: Neovide's `cursor_smooth_blink`.
//!
//! `'guicursor'` gives a mode's cursor three times: how long it waits before
//! it starts to blink, how long it is off, and how long it is on. A terminal
//! blinks its own cursor on its own clock, if it was asked to at all; this
//! keeps Neovim's, and instead of switching the cursor off and on it fades it
//! out and back in over part of each, holding it in between.
//!
//! The cursor then has to be the client's own drawing rather than the
//! terminal's, since a terminal's cursor is either there or not. That is
//! a block, and only a block: a bar or an underline drawn in cells would take
//! the place of the character beside it, where the terminal's own sits next
//! to it, so those keep the terminal's cursor and its blink.
//!
//! Any movement starts the wait again, as it does in Neovim's GUIs: a cursor
//! is never caught faded out by the key that moved it.

use std::time::{Duration, Instant};

use crate::client::redraw::ModeInfo;

/// How much of each half of a blink is the fade, the rest holding still.
const FADE: f32 = 0.4;

/// Where a blink's clock starts from.
#[derive(Debug, Clone, Copy)]
pub struct Blink {
    since: Instant,
}

impl Blink {
    pub fn new(now: Instant) -> Self {
        Self { since: now }
    }

    /// Start the wait again.
    pub fn reset(&mut self, now: Instant) {
        self.since = now;
    }

    /// How much of the cursor shows at `now`, from 0 to 1.
    pub fn opacity(&self, mode: &ModeInfo, now: Instant) -> f32 {
        let Some((t, off, on)) = self.phase(mode, now) else {
            return 1.0;
        };
        let fade_out = off * FADE;
        let fade_in = on * FADE;
        if t < fade_out {
            1.0 - smooth(t / fade_out)
        } else if t < off {
            0.0
        } else if t < off + fade_in {
            smooth((t - off) / fade_in)
        } else {
            1.0
        }
    }

    /// When the cursor will next look different, if it is going to: a frame
    /// on from the `last` one drawn while it fades, the start of the next fade
    /// while it holds.
    pub fn next_change(
        &self,
        mode: &ModeInfo,
        now: Instant,
        last: Option<Instant>,
        frame: Duration,
    ) -> Option<Instant> {
        if !mode.blinks() {
            return None;
        }
        let Some((t, off, on)) = self.phase(mode, now) else {
            // Still waiting: the first fade starts when the wait is over.
            return Some(self.since + Duration::from_millis(mode.blinkwait));
        };
        let (fade_out, fade_in) = (off * FADE, on * FADE);
        if t < fade_out || (off..off + fade_in).contains(&t) {
            // From the last frame rather than from `now`: a frame counted from
            // whenever it is asked for is a frame away again when that comes,
            // and is never due at all.
            return Some(last.map_or(now, |last| last + frame));
        }
        let (held, wait) = if t < off {
            (t - fade_out, off - t)
        } else {
            (t - (off + fade_in), off + on - t)
        };
        // Drawn once more as the hold begins, if the last frame came before
        // it: a fade's frames seldom land on its end, and the last of them
        // would otherwise stand, a few percent short, for the whole hold.
        let begun = now.checked_sub(Duration::from_secs_f32(held.max(0.0)));
        if let Some(begun) = begun.filter(|&begun| last.is_none_or(|last| last < begun)) {
            return Some(begun);
        }
        // To the millisecond the cycle is counted in, so a hold ends on the
        // tick the fade starts on rather than a rounding error either side.
        Some(now + Duration::from_millis((wait * 1000.0).round() as u64))
    }

    /// Seconds into the current off-then-on cycle, and the two halves'
    /// lengths — or `None` before the cursor starts blinking, or for a mode
    /// whose cursor does not blink.
    fn phase(&self, mode: &ModeInfo, now: Instant) -> Option<(f32, f32, f32)> {
        if !mode.blinks() {
            return None;
        }
        let ms = |n: u64| n as f32 / 1000.0;
        let (wait, off, on) = (ms(mode.blinkwait), ms(mode.blinkoff), ms(mode.blinkon));
        let elapsed = now.saturating_duration_since(self.since).as_secs_f32();
        if elapsed < wait {
            return None;
        }
        Some(((elapsed - wait) % (off + on), off, on))
    }
}

/// Smoothstep: eased in and out.
fn smooth(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode() -> ModeInfo {
        ModeInfo {
            blinkwait: 700,
            blinkoff: 400,
            blinkon: 250,
            ..ModeInfo::default()
        }
    }

    #[test]
    fn the_cursor_waits_then_fades_out_holds_and_fades_back() {
        let t0 = Instant::now();
        let b = Blink::new(t0);
        let at = |ms| b.opacity(&mode(), t0 + Duration::from_millis(ms));
        assert_eq!(at(0), 1.0);
        assert_eq!(at(699), 1.0, "still waiting");
        let fading = at(700 + 80);
        assert!(fading > 0.0 && fading < 1.0, "{fading}");
        assert_eq!(at(700 + 300), 0.0, "held off");
        let back = at(700 + 400 + 50);
        assert!(back > 0.0 && back < 1.0, "{back}");
        assert_eq!(at(700 + 400 + 200), 1.0, "held on");
        assert_eq!(at(700 + 650 + 300), 0.0, "and round again");
    }

    #[test]
    fn a_cursor_that_does_not_blink_is_always_there() {
        let t0 = Instant::now();
        let b = Blink::new(t0);
        let still = ModeInfo::default();
        assert_eq!(b.opacity(&still, t0 + Duration::from_secs(5)), 1.0);
        assert_eq!(
            b.next_change(&still, t0, None, Duration::from_millis(16)),
            None
        );
    }

    /// Frames while it fades, and none while it holds: an idle cursor costs a
    /// wake-up per fade, not sixty a second.
    #[test]
    fn frames_are_wanted_only_while_it_fades() {
        let t0 = Instant::now();
        let b = Blink::new(t0);
        let frame = Duration::from_millis(16);
        let at = |ms| t0 + Duration::from_millis(ms);
        let last = Some(at(740));
        assert_eq!(
            b.next_change(&mode(), at(100), last, frame),
            Some(at(700)),
            "waiting, until the first fade"
        );
        assert_eq!(
            b.next_change(&mode(), at(750), last, frame),
            Some(at(740) + frame),
            "a frame on from the last"
        );
        assert_eq!(
            b.next_change(&mode(), at(750), None, frame),
            Some(at(750)),
            "at once, with none drawn yet"
        );
        let held = b.next_change(&mode(), at(1000), Some(at(870)), frame);
        assert_eq!(held, Some(at(1100)), "held off until the fade in");
    }

    /// A hold whose fade's last frame came before its end is drawn once more,
    /// as it begins: whole, or gone, rather than the last step short of it.
    #[test]
    fn a_hold_begins_with_the_fade_finished() {
        let t0 = Instant::now();
        let b = Blink::new(t0);
        let frame = Duration::from_millis(16);
        let at = |ms| t0 + Duration::from_millis(ms);
        // The fade out ends at 860; its last frame came at 850.
        let owed = b.next_change(&mode(), at(866), Some(at(850)), frame);
        assert!(
            owed.is_some_and(|due| due <= at(866) && due >= at(859)),
            "{owed:?}"
        );
        assert_eq!(b.opacity(&mode(), at(866)), 0.0, "drawn gone");
        // The fade in ends at 1200, 40% into the 250 ms it is back for.
        let owed = b.next_change(&mode(), at(1206), Some(at(1190)), frame);
        assert!(owed.is_some_and(|due| due <= at(1206)), "{owed:?}");
        assert_eq!(b.opacity(&mode(), at(1206)), 1.0, "drawn whole");
    }

    /// A frame a fade asks for is due when it comes. Waited for as the
    /// client's loop waits — drawn if due, slept until when it says if not —
    /// the rest of a fade out, 100 ms of it, is drawn a frame at a time.
    #[test]
    fn a_fades_next_frame_comes_due() {
        let t0 = Instant::now();
        let b = Blink::new(t0);
        let frame = Duration::from_millis(16);
        let fade_end = t0 + Duration::from_millis(700 + 160);
        let (mut now, mut last, mut drawn) = (t0 + Duration::from_millis(760), None, 0);
        while now < fade_end {
            match b.next_change(&mode(), now, last, frame) {
                Some(at) if at <= now => {
                    drawn += 1;
                    last = Some(now);
                }
                Some(at) => now = at,
                None => break,
            }
        }
        assert!(drawn >= 6, "{drawn}");
    }

    #[test]
    fn moving_starts_the_wait_again() {
        let t0 = Instant::now();
        let mut b = Blink::new(t0);
        let later = t0 + Duration::from_millis(1000);
        assert_eq!(b.opacity(&mode(), later), 0.0);
        b.reset(later);
        assert_eq!(b.opacity(&mode(), later), 1.0);
    }
}
