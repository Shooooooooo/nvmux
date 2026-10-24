//! Erase animation for a killed session's row: characters are deleted from
//! the end behind a cursor, then the cursor goes.
//!
//! Played after a kill is confirmed and before it runs, so the row is already
//! gone while the kill and relisting are awaited. It cannot run during the
//! kill: the transport is not `Send` (see [`crate::dirs`]) and a kill over ssh
//! blocks the drawing thread.
//!
//! The whole row takes [`ERASE`] whatever its length; only the rate changes.
//! Every character, spaces included, is one step; a wide character goes in
//! one. Drawn plain with the `▋` cursor, so it is correct under `NO_COLOR`.
//! The frame depends only on elapsed time.

use std::time::Duration;

/// Time to delete the whole row, whatever its length.
pub const ERASE: Duration = Duration::from_millis(240);

/// How long the cursor stays on the emptied row, so it reads as cleared
/// rather than skipped.
pub const LINGER: Duration = Duration::from_millis(50);

pub const LENGTH: Duration =
    Duration::from_millis(ERASE.as_millis() as u64 + LINGER.as_millis() as u64);

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

    /// Whether the cursor has gone too.
    pub fn done(&self) -> bool {
        self.age >= LENGTH
    }

    /// The prefix of `text` still shown, and whether the cursor follows it.
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

    #[test]
    fn at_the_start_the_row_is_whole() {
        assert_eq!(at(0).render("▸ 2  dotfiles"), ("▸ 2  dotfiles", true));
    }

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

    /// A 16 ms frame never deletes two characters of a short row.
    #[test]
    fn a_short_row_goes_a_character_at_a_time() {
        let text = "▸ 2  dotfiles";
        for ms in (0..ERASE.as_millis() as u64).step_by(16) {
            let now = at(ms).render(text).0.chars().count();
            let next = at(ms + 16).render(text).0.chars().count();
            assert!(now - next <= 1, "{now} to {next} at {ms} ms");
        }
    }

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

    #[test]
    fn a_wide_character_goes_whole() {
        for ms in (0..=ERASE.as_millis() as u64).step_by(5) {
            let (kept, _) = at(ms).render("日本語");
            assert!(["日本語", "日本", "日", ""].contains(&kept), "{kept:?}");
        }
    }
}
