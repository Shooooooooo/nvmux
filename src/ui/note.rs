//! Glyphs and text for a session's note on the picker (layout is in
//! [`super::draw`]).
//!
//! Every sign must be one column on every terminal, or the row shifts, so
//! none is emoji or ambiguous-width: braille, `∗` (U+2217), `✓` (U+2713, not
//! the emoji U+2714) and `!`.

use std::time::Duration;

use super::attaching::{FRAME, GLYPHS};
use crate::notes::{Kind, Note};

pub(super) const SPIN: Duration = FRAME;

/// One to eight braille dots, left column bottom-up, then the right.
const GAUGE: [char; 8] = ['⡀', '⡄', '⡆', '⡇', '⣇', '⣧', '⣷', '⣿'];

const NOTIFIED: char = '∗';
const FAILED: char = '!';
const FINISHED: char = '✓';

/// The sign beside a row; `frame` animates a busy note's spinner.
pub fn sign(note: &Note, frame: usize) -> char {
    glyph(note.kind, frame)
}

/// The static sign for `kind`, as [`crate::tell`] sends it.
pub fn mark(kind: Kind) -> char {
    glyph(kind, 0)
}

fn glyph(kind: Kind, frame: usize) -> char {
    match kind {
        Kind::Notified => NOTIFIED,
        Kind::Failed => FAILED,
        Kind::Finished => FINISHED,
        Kind::Busy(Some(percent)) => gauge(percent),
        Kind::Busy(None) => GLYPHS[frame % GLYPHS.len()],
    }
}

/// Whether the note shows the spinner, so the picker must redraw each frame.
pub fn turns(note: &Note) -> bool {
    note.kind == Kind::Busy(None)
}

/// `percent` to the nearest eighth, at least one dot; 94% and up fill the
/// cell.
pub fn gauge(percent: u8) -> char {
    let level = ((u32::from(percent.min(100)) * 8 + 50) / 100).clamp(1, 8);
    GAUGE[level as usize - 1]
}

/// The selected note's caption, e.g. `lua_ls: 42% indexing · 12s`; progress
/// reads as Neovim prints it, title first.
pub fn caption(note: &Note) -> String {
    format!(
        "{} · {}",
        what(note.kind, &note.title, &note.text),
        age(note.age_now())
    )
}

/// The caption without its age; also what [`crate::tell`] sends the editor.
pub fn what(kind: Kind, title: &str, text: &str) -> String {
    match kind {
        Kind::Notified => [title, text]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · "),
        Kind::Failed => progress(title, text, None, "failed"),
        Kind::Finished => progress(title, text, None, "finished"),
        Kind::Busy(percent) => progress(title, text, percent, "busy"),
    }
}

/// `title: 42% text`, omitting missing parts; `otherwise` replaces an empty
/// `42% text`.
fn progress(title: &str, text: &str, percent: Option<u8>, otherwise: &str) -> String {
    let mut body: Vec<String> = Vec::new();
    if let Some(p) = percent {
        body.push(format!("{p}%"));
    }
    if !text.is_empty() {
        body.push(text.to_string());
    }
    let body = if body.is_empty() {
        otherwise.to_string()
    } else {
        body.join(" ")
    };
    if title.is_empty() {
        body
    } else {
        format!("{title}: {body}")
    }
}

/// Age in its largest whole unit: `12s`, `3m`, `2h`, `1d`.
pub fn age(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use unicode_width::UnicodeWidthChar;

    fn note(kind: Kind, title: &str, text: &str, secs: u64) -> Note {
        Note {
            kind,
            title: title.to_string(),
            text: text.to_string(),
            age: Duration::from_secs(secs),
            read_at: Instant::now(),
        }
    }

    #[test]
    fn each_kind_has_its_sign() {
        assert_eq!(sign(&note(Kind::Notified, "", "x", 0), 0), '∗');
        assert_eq!(sign(&note(Kind::Failed, "", "x", 0), 0), '!');
        assert_eq!(sign(&note(Kind::Finished, "", "x", 0), 0), '✓');
        assert_eq!(sign(&note(Kind::Busy(Some(42)), "", "", 0), 0), '⡆');
        let busy = note(Kind::Busy(None), "", "", 0);
        assert_eq!(sign(&busy, 0), '⠋');
        assert_eq!(sign(&busy, 2), '⠹');
        assert_eq!(sign(&busy, 12), '⠹', "the frames go round");
        assert!(turns(&busy));
        assert!(!turns(&note(Kind::Busy(Some(42)), "", "", 0)));
    }

    #[test]
    fn every_sign_is_one_column() {
        let signs = GLYPHS
            .iter()
            .chain(GAUGE.iter())
            .chain([NOTIFIED, FAILED, FINISHED].iter());
        for c in signs {
            assert_eq!(c.width(), Some(1), "{c:?}");
            assert_eq!(c.width_cjk(), Some(1), "{c:?} is wide on a CJK terminal");
        }
    }

    #[test]
    fn the_gauge_shows_any_progress_and_fills_only_at_the_end() {
        assert_eq!(gauge(1), '⡀');
        assert_eq!(gauge(42), '⡆');
        assert_eq!(gauge(99), '⣿');
        assert_eq!(gauge(100), '⣿');
        assert_eq!(gauge(200), '⣿');
        let levels: Vec<char> = (1..=100).map(gauge).collect();
        assert!(levels.windows(2).all(
            |w| GAUGE.iter().position(|g| *g == w[0]) <= GAUGE.iter().position(|g| *g == w[1])
        ));
    }

    #[test]
    fn a_caption_says_what_and_how_long_ago() {
        assert_eq!(
            caption(&note(
                Kind::Notified,
                "Claude Code",
                "Claude needs your permission",
                120
            )),
            "Claude Code · Claude needs your permission · 2m"
        );
        assert_eq!(
            caption(&note(Kind::Notified, "", "Task done", 5)),
            "Task done · 5s"
        );
        assert_eq!(
            caption(&note(Kind::Busy(None), "claude", "working", 200)),
            "claude: working · 3m"
        );
        assert_eq!(
            caption(&note(Kind::Busy(Some(42)), "lua_ls", "indexing", 12)),
            "lua_ls: 42% indexing · 12s"
        );
        assert_eq!(caption(&note(Kind::Busy(None), "", "", 7200)), "busy · 2h");
        assert_eq!(
            caption(&note(Kind::Failed, "claude", "exited with code 1", 90_000)),
            "claude: exited with code 1 · 1d"
        );
        assert_eq!(caption(&note(Kind::Failed, "", "", 0)), "failed · 0s");
        assert_eq!(
            caption(&note(
                Kind::Finished,
                "cargo test",
                "exited with code 0",
                45
            )),
            "cargo test: exited with code 0 · 45s"
        );
        assert_eq!(caption(&note(Kind::Finished, "", "", 0)), "finished · 0s");
    }
}
