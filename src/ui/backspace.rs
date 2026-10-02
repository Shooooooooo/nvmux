//! The backspace a killed session's row is erased with: deleted from its end
//! one character at a time behind a cursor, the way you would clear it by
//! hand, until nothing is left of it but the cursor, and then not that.
//!
//! Played once a kill has been confirmed and before it runs (see
//! [`crate::ui`]'s kill arm), so by the time the transport is asked, the row is
//! already gone from the screen and the wait for the kill and the fresh
//! listing reads as the list settling rather than as nothing happening. It
//! cannot run *during* the kill: the transport is not `Send` (see
//! [`crate::dirs`]), and a kill over ssh blocks the thread that would draw.
//!
//! # A fixed length
//!
//! The whole row goes in [`ERASE`] however long it is, so a long name costs no
//! more than a short one: the rate is what changes. A short row is deleted at
//! about a typist's held-down backspace; a row the width of the list's cap
//! goes faster, a few characters a frame. The cursor then stands alone on the
//! emptied row for [`LINGER`], so the eye sees the row was cleared rather than
//! skipped.
//!
//! Every character counts as one keystroke, the marker, the number and the
//! blanks between them included: a backspace does not skip spaces. A wide
//! character goes in one, and takes both its columns with it.
//!
//! # Characters, and nothing else
//!
//! No colour and no modifier: the row is drawn plain, as it is once it is no
//! longer the selection, and the cursor is the `▋` the hint row's prompts
//! already use. So the effect is correct under `NO_COLOR` by construction.
//!
//! # Time
//!
//! Moved on by elapsed time, like everything else that moves here. How much
//! of a row is left is worked out from the age alone, so the same age always
//! draws the same frame.

use std::time::Duration;

/// How long the whole row takes to delete, whatever its length.
pub const ERASE: Duration = Duration::from_millis(240);

/// How long the cursor stands on the emptied row before it goes too.
pub const LINGER: Duration = Duration::from_millis(50);

/// The whole effect, from the key to the cursor going.
pub const LENGTH: Duration =
    Duration::from_millis(ERASE.as_millis() as u64 + LINGER.as_millis() as u64);

/// One row's backspace. See the module docs.
#[derive(Debug, Clone, Default)]
pub struct Backspace {
    age: Duration,
}

impl Backspace {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&mut self, elapsed: Duration) {
        self.age += elapsed;
    }

    /// Whether the row and the cursor have both gone, whatever the row was.
    pub fn done(&self) -> bool {
        self.age >= LENGTH
    }

    /// What is left of `text` now — its first so many characters — and
    /// whether the cursor is drawn after them.
    pub fn render<'a>(&self, text: &'a str) -> (&'a str, bool) {
        let chars = text.chars().count();
        let through = (self.age.as_secs_f32() / ERASE.as_secs_f32()).min(1.0);
        let kept = chars - ((chars as f32 * through).floor() as usize).min(chars);
        let end = text
            .char_indices()
            .nth(kept)
            .map_or(text.len(), |(at, _)| at);
        (&text[..end], !self.done())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Backspace {
        let mut b = Backspace::new();
        b.advance(Duration::from_millis(ms));
        b
    }

    /// At first the row is all there, with the cursor after it.
    #[test]
    fn at_the_start_the_row_is_whole() {
        assert_eq!(at(0).render("▸ 2  dotfiles"), ("▸ 2  dotfiles", true));
    }

    /// It goes from the end: what is left is always the start of the row, and
    /// there is less of it as time goes on.
    #[test]
    fn it_deletes_from_the_end() {
        let text = "▸ 2  dotfiles";
        let mut last = text.chars().count();
        for ms in (0..=ERASE.as_millis() as u64).step_by(10) {
            let (kept, cursor) = at(ms).render(text);
            assert!(text.starts_with(kept), "{kept:?} at {ms} ms");
            assert!(kept.chars().count() <= last, "it never grows back");
            assert!(cursor);
            last = kept.chars().count();
        }
        assert_eq!(last, 0, "all gone by the end of the erase");
    }

    /// One character at a time on a short row, as a held-down backspace
    /// would: a frame never takes two.
    #[test]
    fn a_short_row_goes_a_character_at_a_time() {
        let text = "▸ 2  dotfiles";
        for ms in (0..ERASE.as_millis() as u64).step_by(16) {
            let now = at(ms).render(text).0.chars().count();
            let next = at(ms + 16).render(text).0.chars().count();
            assert!(now - next <= 1, "{now} to {next} at {ms} ms");
        }
    }

    /// The cursor stands alone for a moment once the row has gone, and then
    /// it goes too, however long the row was.
    #[test]
    fn the_cursor_lingers_and_then_goes() {
        let long = "x".repeat(48);
        let after = ERASE.as_millis() as u64 + 10;
        assert_eq!(at(after).render(&long), ("", true));
        assert_eq!(at(after).render("y"), ("", true));
        let end = at(LENGTH.as_millis() as u64);
        assert!(end.done());
        assert_eq!(end.render(&long), ("", false));
        assert!(LENGTH <= Duration::from_millis(350), "{LENGTH:?}");
    }

    /// A wide character goes in one keystroke, never half of it.
    #[test]
    fn a_wide_character_goes_whole() {
        for ms in (0..=ERASE.as_millis() as u64).step_by(5) {
            let (kept, _) = at(ms).render("日本語");
            assert!(["日本語", "日本", "日", ""].contains(&kept), "{kept:?}");
        }
    }
}
