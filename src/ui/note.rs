//! What a session's note looks like on the picker (see [`crate::notes`]):
//! with `[picker] notes = "signs"`, the default, one sign beside the row and
//! the selected session's note in full under the list; with `"column"`, a
//! short note in the row itself, after the name. Glyphs and text only — where
//! they go, dim, is [`super::draw`]'s.
//!
//! # One column, everywhere
//!
//! Every sign is one column on every terminal, for the reason the marker and
//! the back mark are (see [`super::draw`]): a terminal that draws a glyph two
//! wide where `unicode-width` says one shifts the rest of the row. The
//! spinner and the gauge are braille, as the attaching screen's spinner and
//! the picker's trails are. `∗` is the asterisk operator, U+2217, which is
//! neither emoji nor of ambiguous width; `!` is plain ASCII.

use std::time::Duration;

use super::attaching::{FRAME, GLYPHS};
use crate::notes::{Kind, Note};

/// How long a frame of a busy session's spinner lasts: the attaching screen's.
pub(super) const SPIN: Duration = FRAME;

/// The gauge a percentage is drawn with: one to eight of a cell's dots,
/// filling the left column from the bottom, then the right.
const GAUGE: [char; 8] = ['⡀', '⡄', '⡆', '⡇', '⣇', '⣧', '⣷', '⣿'];

/// The sign for a notification.
const NOTIFIED: char = '∗';

/// The sign for a failure.
const FAILED: char = '!';

/// The sign beside a session's row: the spinner's `frame` while it is busy
/// without saying how far it has got, the gauge when it says, `∗` for a
/// notification and `!` for a failure.
pub fn sign(note: &Note, frame: usize) -> char {
    match note.kind {
        Kind::Notified => NOTIFIED,
        Kind::Failed => FAILED,
        Kind::Busy(Some(percent)) => gauge(percent),
        Kind::Busy(None) => GLYPHS[frame % GLYPHS.len()],
    }
}

/// Whether the note turns — the spinner — and so wants the picker drawn a
/// frame at a time while it is on screen.
pub fn turns(note: &Note) -> bool {
    note.kind == Kind::Busy(None)
}

/// A percentage as one cell of the gauge: any progress at all shows a dot,
/// and only all of it fills the cell.
pub fn gauge(percent: u8) -> char {
    let level = ((u32::from(percent.min(100)) * 8 + 50) / 100).clamp(1, 8);
    GAUGE[level as usize - 1]
}

/// The selected session's note, under the list: what it is, then how long
/// ago — `Claude Code · Claude needs your permission · 2m`, `claude: working
/// · 3m`, `lua_ls: 42% indexing · 12s`.
///
/// Progress reads the way Neovim prints a progress message, title first.
pub fn caption(note: &Note) -> String {
    let what = match note.kind {
        Kind::Notified => [note.title.as_str(), note.text.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · "),
        Kind::Failed => progress(note, None, "failed"),
        Kind::Busy(percent) => progress(note, percent, "busy"),
    };
    format!("{what} · {}", age(note.age_now()))
}

/// A note short enough for a column in the row: the notification's body
/// rather than who sent it, a failure's text, a percentage, and the spinner's
/// `frame` before what is running. No age: the column is for scanning, and
/// the caption a selection would show it has none in this mode either.
pub fn column(note: &Note, frame: usize) -> String {
    let or = |text: &str, otherwise: &str| {
        if text.is_empty() {
            otherwise.to_string()
        } else {
            text.to_string()
        }
    };
    match note.kind {
        Kind::Notified => or(&note.text, &note.title),
        Kind::Failed => or(&note.text, "failed"),
        Kind::Busy(Some(percent)) if note.text.is_empty() => format!("{percent}%"),
        Kind::Busy(Some(percent)) => format!("{percent}% {}", note.text),
        Kind::Busy(None) => format!(
            "{} {}",
            GLYPHS[frame % GLYPHS.len()],
            or(&note.text, "busy")
        ),
    }
}

/// `title: 42% text`, with what is missing left out, and `otherwise` when
/// there is nothing at all to say.
fn progress(note: &Note, percent: Option<u8>, otherwise: &str) -> String {
    let mut body: Vec<String> = Vec::new();
    if let Some(p) = percent {
        body.push(format!("{p}%"));
    }
    if !note.text.is_empty() {
        body.push(note.text.clone());
    }
    let body = if body.is_empty() {
        otherwise.to_string()
    } else {
        body.join(" ")
    };
    if note.title.is_empty() {
        body
    } else {
        format!("{}: {body}", note.title)
    }
}

/// How long ago, in the one unit that says it: `12s`, `3m`, `2h`, `1d`.
fn age(d: Duration) -> String {
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
        assert_eq!(sign(&note(Kind::Busy(Some(42)), "", "", 0), 0), '⡆');
        let busy = note(Kind::Busy(None), "", "", 0);
        assert_eq!(sign(&busy, 0), '⠋');
        assert_eq!(sign(&busy, 2), '⠹');
        assert_eq!(sign(&busy, 12), '⠹', "the frames go round");
        assert!(turns(&busy));
        assert!(!turns(&note(Kind::Busy(Some(42)), "", "", 0)));
    }

    /// The marker's rule, kept: a sign a terminal draws two columns wide
    /// where `unicode-width` says one would shift the row it stands beside.
    #[test]
    fn every_sign_is_one_column() {
        let signs = GLYPHS
            .iter()
            .chain(GAUGE.iter())
            .chain([NOTIFIED, FAILED].iter());
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
    }

    #[test]
    fn a_column_note_is_short() {
        assert_eq!(
            column(
                &note(
                    Kind::Notified,
                    "Claude Code",
                    "Claude needs your permission",
                    0
                ),
                0
            ),
            "Claude needs your permission"
        );
        assert_eq!(column(&note(Kind::Notified, "Build", "", 0), 0), "Build");
        assert_eq!(
            column(&note(Kind::Busy(None), "claude", "working", 0), 2),
            "⠹ working"
        );
        assert_eq!(column(&note(Kind::Busy(None), "", "", 0), 0), "⠋ busy");
        assert_eq!(column(&note(Kind::Busy(Some(42)), "", "", 0), 0), "42%");
        assert_eq!(
            column(&note(Kind::Busy(Some(42)), "lua_ls", "indexing", 0), 0),
            "42% indexing"
        );
        assert_eq!(column(&note(Kind::Failed, "", "", 0), 0), "failed");
    }
}
