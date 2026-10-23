//! The hint bar: the last row, saying what the next key does while
//! [`crate::keys::Prefix`] waits for it. Built from the user's
//! [`crate::keys::Keymap`]; entries are second keys, so the prefix is never
//! named.
//!
//! nvmux owns no cells while attached, so the bar is drawn over Neovim's
//! message row and put back. It goes up at a lull ([`SETTLE`]): with no parser
//! followed, the only time the child cannot be mid-sequence. While it is up,
//! the relay follows the terminal's parser ([`crate::boundary`]) and redraws
//! the bar on the same pass the session draws over it, between its sequences;
//! otherwise a statusline redrawing faster than a lull would keep it down.
//! Following the parser has costs (see `Attachment::write_session_frame` in
//! [`crate::pty`]), so it stops once the bar is down. Unlike the attach notice
//! there is no fade (the bar must be up as the prefix arms) and no give-up
//! clock (the prefix bounds its life).
//!
//! When the prefix resolves without ending the relay, the row is restored from
//! [`crate::shadow`], or failing that by the server (see [`Act::Blanked`]).

use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr;

use crate::announce;
use crate::keys::{self, Keymap, Pending};
use crate::palette::{self, Palette};
use crate::pty::PtySize;
use crate::shadow::{Over, Shadow};

/// Quiet time before the bar goes up: steps over the microseconds a child
/// blocked on a full pty buffer leaves the master unreadable mid-frame. Equal
/// to `pty::HOLD_SETTLE` by coincidence.
const SETTLE: Duration = Duration::from_millis(25);

const GAP: &str = "  ";

/// `1-n`, not `1-9`: `Prefix::feed` reads a number, so `12` reaches the
/// twelfth session.
const DIGITS: &str = "1-n session";

/// Reset (restoring the background), then dim.
const SGR: &str = "\x1b[0;2m";

const QUESTION_SGR: &str = "\x1b[0m";

/// The bar never takes a session's only row.
const MIN_ROWS: u16 = 2;

/// What the relay saw on this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pass {
    /// The child wrote (or its first paint is held or just released).
    pub wrote: bool,
    /// When the parser is followed: `Some(true)` between the session's
    /// sequences, `Some(false)` mid-sequence. `None` means wait for a lull.
    pub between: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    Idle,
    Write(Vec<u8>),
    /// Write these bytes, then ask the server to repaint: with no shadow the
    /// row can only be blanked.
    Blanked(Vec<u8>),
}

/// Without `go` (no card up), `⏎ go` is left out since it would do nothing.
/// Keys are spelled as on the picker's hint row ([`keys::key_glyph`]).
fn entries(keymap: &Keymap, go: bool) -> Vec<String> {
    keymap
        .bindings()
        .filter(|b| go || b.action != keys::Action::Go)
        .map(|b| format!("{} {}", keys::key_glyph(b.key), b.hint))
        .chain(std::iter::once(DIGITS.to_string()))
        .collect()
}

/// Entries are dropped whole from the right, not truncated: a fragment over
/// the editor reads as corruption. `None` if not even the first fits.
fn text(pending: Pending, keymap: &Keymap, go: bool, cols: u16) -> Option<String> {
    let cols = usize::from(cols);
    let row = match pending {
        // No `▋` cursor, unlike the picker: Neovim's cursor is elsewhere.
        Pending::Number(n) => format!("#{n}"),
        Pending::Command => {
            let mut row = String::new();
            for entry in entries(keymap, go) {
                let gap = if row.is_empty() { 0 } else { GAP.width() };
                if row.width() + gap + entry.width() > cols {
                    break;
                }
                if gap > 0 {
                    row.push_str(GAP);
                }
                row.push_str(&entry);
            }
            row
        }
    };
    (!row.is_empty() && row.width() <= cols).then_some(row)
}

/// [`overlay`] for a question: `text`, cut to the screen.
fn question_overlay(text: &str, size: PtySize) -> Option<Over> {
    if size.rows < MIN_ROWS || size.cols == 0 {
        return None;
    }
    let cols = usize::from(size.cols);
    let mut cut = String::new();
    for c in text.chars() {
        if cut.width() + c.to_string().width() > cols {
            break;
        }
        cut.push(c);
    }
    let left = (cols - cut.width()) / 2;
    let right = cols - cut.width() - left;
    Some(Over {
        top: size.rows - 1,
        left: 0,
        width: size.cols,
        rows: vec![format!("{}{cut}{}", " ".repeat(left), " ".repeat(right))],
    })
}

/// The whole last row with the text centred, padded so the editor's row does
/// not show beside it. Filling the last column is safe: nothing printable
/// follows, so `DECRC` discards the pending wrap (see [`announce::placed`]).
pub fn overlay(pending: Pending, keymap: &Keymap, go: bool, size: PtySize) -> Option<Over> {
    if size.rows < MIN_ROWS || size.cols == 0 {
        return None;
    }
    let text = text(pending, keymap, go, size.cols)?;
    let cols = usize::from(size.cols);
    let left = (cols - text.width()) / 2;
    let right = cols - text.width() - left;
    Some(Over {
        top: size.rows - 1,
        left: 0,
        width: size.cols,
        rows: vec![format!("{}{text}{}", " ".repeat(left), " ".repeat(right))],
    })
}

/// The bar for one relay. Told the time rather than reading a clock, so tests
/// reach every state.
#[derive(Debug)]
pub struct Bar {
    /// The last pass the child wrote on, or the relay's start. The lull is
    /// counted from the write, not the first quiet pass, which would make it
    /// two lulls long.
    spoke: Instant,
    /// The erase must restore the cells covered, not the current last row.
    shown: Option<Over>,
    want: Option<Over>,
    /// Whether the terminal matches `want`; false means a write is owed.
    agreed: bool,
    watched: bool,
    /// Rows and columns only: a pixel-size change moves no cell.
    size: (u16, u16),
    /// Handed in, not read from the global, so tests can reach every state.
    palette: Option<Palette>,
    keymap: Keymap,
    question: Option<String>,
}

impl Bar {
    pub fn new(now: Instant, size: PtySize, keymap: Keymap) -> Self {
        Self::with_palette(now, size, keymap, palette::get().copied())
    }

    fn with_palette(now: Instant, size: PtySize, keymap: Keymap, palette: Option<Palette>) -> Self {
        Self {
            spoke: now,
            shown: None,
            want: None,
            agreed: true,
            watched: false,
            size: (size.rows, size.cols),
            palette,
            keymap,
            question: None,
        }
    }

    /// Show `question` instead of the commands (e.g. `<prefix> d` with unsaved
    /// settings) until cleared with `None`.
    pub fn ask(&mut self, question: Option<String>) {
        self.question = question;
    }

    /// While true the relay follows the terminal's parser.
    pub fn is_up(&self) -> bool {
        self.shown.is_some() || self.want.is_some()
    }

    /// When the relay must next wake for the bar. A bar that cannot draw marks
    /// itself agreed, or the poll timeout would round to zero and the relay
    /// spin. `None` while the parser is followed: no clock ends a sequence, and
    /// the session's next bytes wake the relay.
    pub fn wake_at(&self) -> Option<Instant> {
        if self.agreed || self.watched {
            return None;
        }
        Some(self.spoke + SETTLE)
    }

    /// Something other than the child (the attach notice) drew over the screen.
    pub fn overdrawn(&mut self) {
        self.may_have_been_covered();
    }

    /// `under` is what an erase restores; `go` is whether a card is up for
    /// `<prefix> Enter`.
    pub fn step(
        &mut self,
        now: Instant,
        pass: Pass,
        size: PtySize,
        pending: Option<Pending>,
        go: bool,
        under: Option<&Shadow>,
    ) -> Act {
        // A resize makes the client repaint, so forget the bar; after a
        // shrink, restoring would clamp every `CUP` onto a row of text.
        if self.size != (size.rows, size.cols) {
            self.size = (size.rows, size.cols);
            self.shown = None;
        }

        // Before the lull check, so a wake-up can be asked for.
        let want = match &self.question {
            Some(question) => question_overlay(question, size),
            None => pending.and_then(|p| overlay(p, &self.keymap, go, size)),
        };
        if want != self.want {
            self.want = want;
            self.agreed = false;
        }

        if pass.wrote {
            self.spoke = now;
            self.may_have_been_covered();
        }

        // The parser's word, when there is one, decides alone.
        self.watched = pass.between.is_some();
        let free = pass
            .between
            .unwrap_or_else(|| now.duration_since(self.spoke) >= SETTLE);
        if !free || self.agreed {
            return Act::Idle;
        }

        // Even if nothing is drawn, so `wake_at` goes quiet.
        self.agreed = true;
        match self.want.clone() {
            Some(over) => {
                let sgr = if self.question.is_some() {
                    QUESTION_SGR
                } else {
                    SGR
                };
                let bytes = announce::placed(&over, sgr);
                self.shown = Some(over);
                Act::Write(bytes)
            }
            None => match self.shown.take() {
                Some(over) => erase(&over, under, self.palette.as_ref()),
                None => Act::Idle,
            },
        }
    }

    /// Only a shown bar can be covered; otherwise every output would cause a
    /// wake-up.
    fn may_have_been_covered(&mut self) {
        self.agreed &= self.shown.is_none();
    }
}

/// Restore the row from the shadow, a local erase (undercurl and strikethrough
/// come back plain). Without a usable shadow or palette, blank the row and
/// have the relay ask the server.
fn erase(over: &Over, under: Option<&Shadow>, palette: Option<&Palette>) -> Act {
    match (under.filter(|s| s.is_usable()), palette) {
        // At `t = 1.0` none of the bar is drawn, so its colour is moot.
        (Some(shadow), Some(palette)) => Act::Write(shadow.under(over, palette, palette.fg, 1.0)),
        _ => Act::Blanked(announce::plain_bytes(&blank(over), None)),
    }
}

fn blank(over: &Over) -> Over {
    Over {
        rows: over
            .rows
            .iter()
            .map(|row| " ".repeat(row.width()))
            .collect(),
        ..over.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{big, palette, screen, size};
    use crate::ui::test_support::TINY_SIZES;

    /// On the default keys.
    fn text(pending: Pending, go: bool, cols: u16) -> Option<String> {
        super::text(pending, &Keymap::default(), go, cols)
    }

    fn overlay(pending: Pending, go: bool, size: PtySize) -> Option<Over> {
        super::overlay(pending, &Keymap::default(), go, size)
    }

    fn entries(go: bool) -> Vec<String> {
        super::entries(&Keymap::default(), go)
    }

    fn row(pending: Pending, size: PtySize) -> String {
        overlay(pending, true, size).expect("a bar").rows.remove(0)
    }

    /// A bar on the test's clock, advanced a lull per pass.
    struct Driver {
        bar: Bar,
        now: Instant,
        shadow: Option<Shadow>,
    }

    impl Driver {
        fn new(size: PtySize) -> Self {
            let now = Instant::now();
            Self {
                bar: Bar::new(now, size, Keymap::default()),
                now,
                shadow: None,
            }
        }

        fn restoring(size: PtySize) -> Self {
            let now = Instant::now();
            Self {
                bar: Bar::with_palette(now, size, Keymap::default(), Some(palette())),
                now,
                shadow: Some(screen(size.rows, size.cols)),
            }
        }

        /// A pass `SETTLE` later, the child quiet; parser not followed.
        fn quiet(&mut self, pending: Option<Pending>, size: PtySize) -> Act {
            self.step(false, pending, size)
        }

        /// A pass `SETTLE` later, the child writing; parser not followed.
        fn busy(&mut self, pending: Option<Pending>, size: PtySize) -> Act {
            self.step(true, pending, size)
        }

        fn step(&mut self, wrote: bool, pending: Option<Pending>, size: PtySize) -> Act {
            self.pass(
                Pass {
                    wrote,
                    between: None,
                },
                pending,
                size,
            )
        }

        fn watched(
            &mut self,
            wrote: bool,
            between: bool,
            pending: Option<Pending>,
            size: PtySize,
        ) -> Act {
            self.pass(
                Pass {
                    wrote,
                    between: Some(between),
                },
                pending,
                size,
            )
        }

        fn pass(&mut self, pass: Pass, pending: Option<Pending>, size: PtySize) -> Act {
            self.now += SETTLE;
            self.bar
                .step(self.now, pass, size, pending, true, self.shadow.as_ref())
        }

        fn wake_at(&self) -> Option<Instant> {
            self.bar.wake_at()
        }
    }

    #[test]
    fn the_row_lists_every_binding_in_table_order() {
        let text = text(Pending::Command, true, 200).expect("a row");
        let mut rest = text.as_str();
        for b in keys::BINDINGS {
            let entry = format!("{} {}", keys::key_glyph(b.key), b.hint);
            let at = rest.find(&entry).unwrap_or_else(|| {
                panic!("{entry:?} is not on the bar, or is out of order: {text:?}")
            });
            rest = &rest[at + entry.len()..];
        }
        assert!(
            rest.contains(DIGITS),
            "the digit rule is not on the bar: {text:?}"
        );
    }

    /// Not onto `x`, which fills the test screen.
    fn moved() -> Keymap {
        let mut keymap = Keymap::default();
        for (name, key) in [("picker", b'D'), ("detach", b'z'), ("go", b'g')] {
            assert!(keymap.bind(name, key), "no command is called {name}");
        }
        keymap
    }

    #[test]
    fn the_row_names_the_keys_the_commands_were_moved_to() {
        let keymap = moved();
        let text = super::text(Pending::Command, &keymap, true, 200).expect("a row");
        let mut rest = text.as_str();
        for entry in ["D picker", "z detach", "c new", "g go", "? help"] {
            let at = rest.find(entry).unwrap_or_else(|| {
                panic!("{entry:?} is not on the bar, or is out of order: {text:?}")
            });
            rest = &rest[at + entry.len()..];
        }
        assert!(
            !text.contains("d detach"),
            "the old key is offered: {text:?}"
        );
        assert!(
            !text.contains(keys::SPACE_GLYPH) && !text.contains(keys::ENTER_GLYPH),
            "a key no command is on any more is offered: {text:?}"
        );

        let without = super::text(Pending::Command, &keymap, false, 200).expect("a row");
        assert!(
            !without.contains("g go"),
            "go is offered with no card to go to: {without:?}"
        );
    }

    #[test]
    fn the_bar_draws_the_keymap_it_was_given() {
        let size = big();
        let now = Instant::now() + SETTLE;
        let mut bar = Bar::new(now - SETTLE, size, moved());
        let quiet = Pass {
            wrote: false,
            between: None,
        };
        let Act::Write(bytes) = bar.step(now, quiet, size, Some(Pending::Command), true, None)
        else {
            panic!("the bar did not go up");
        };
        let painted = String::from_utf8_lossy(&bytes);
        assert!(painted.contains("z detach"), "{painted:?}");
        assert!(!painted.contains("d detach"), "{painted:?}");
    }

    #[test]
    fn the_row_never_spells_the_prefix() {
        let text = text(Pending::Command, true, 200).expect("a row");
        for prefix in [keys::PREFIX, keys::parse_prefix("C-t").expect("a chord")] {
            let label = keys::prefix_label(prefix);
            assert!(
                !text.contains(&label),
                "the bar spells the prefix ({label}): {text:?}"
            );
        }
        assert!(!text.contains("Ctrl"), "the bar names a chord: {text:?}");
    }

    #[test]
    fn the_space_bar_is_the_glyph_the_picker_uses() {
        let text = text(Pending::Command, true, 200).expect("a row");
        assert!(
            text.starts_with(&format!("{} picker", keys::SPACE_GLYPH)),
            "the space bar is not the glyph: {text:?}"
        );
        assert!(
            !text.contains("space") && !text.contains("Space"),
            "the space bar is spelled out as well: {text:?}"
        );
    }

    #[test]
    fn the_row_does_not_offer_a_repeated_prefix_or_a_replay() {
        let text = text(Pending::Command, true, 200).expect("a row");
        assert!(
            !text.contains("literal"),
            "the bar offers the literal prefix: {text:?}"
        );
        assert!(!text.contains("other"), "the bar offers a replay: {text:?}");
        let entries = text.split(GAP).count();
        assert_eq!(
            entries,
            keys::BINDINGS.len() + 1,
            "the bar has entries beyond the commands and the digits: {text:?}"
        );
    }

    #[test]
    fn entries_are_dropped_whole_to_fit() {
        let all = entries(true);
        for cols in 1..=200u16 {
            let Some(text) = text(Pending::Command, true, cols) else {
                continue;
            };
            assert!(
                text.width() <= usize::from(cols),
                "{cols} columns overflowed: {text:?}"
            );
            let shown: Vec<&str> = text.split(GAP).collect();
            assert_eq!(
                shown,
                all[..shown.len()]
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                "at {cols} columns the row is not a whole-entry prefix of the table"
            );
        }
    }

    #[test]
    fn a_screen_too_narrow_for_one_entry_says_nothing() {
        let first = entries(true).remove(0);
        for cols in 0..first.width() {
            assert_eq!(
                text(Pending::Command, true, u16::try_from(cols).expect("small")),
                None,
                "drew part of {first:?} in {cols} columns"
            );
        }
        assert!(text(
            Pending::Command,
            true,
            u16::try_from(first.width()).expect("small")
        )
        .is_some());
    }

    #[test]
    fn the_bar_never_takes_a_sessions_only_row() {
        assert_eq!(overlay(Pending::Command, true, size(200, 1)), None);
        assert!(overlay(Pending::Command, true, size(200, 2)).is_some());
    }

    /// The bar fits some of these; the rectangle must hold at every size.
    #[test]
    fn a_tiny_screen_is_either_declined_or_drawn_inside_itself() {
        for &(cols, rows) in TINY_SIZES {
            for pending in [Pending::Command, Pending::Number(7)] {
                let Some(over) = overlay(pending, true, size(cols, rows)) else {
                    continue;
                };
                assert_eq!(
                    over.top,
                    rows - 1,
                    "{cols}x{rows} {pending:?}: not the last row"
                );
                assert_eq!(
                    over.left, 0,
                    "{cols}x{rows} {pending:?}: not from column one"
                );
                assert_eq!(
                    over.width, cols,
                    "{cols}x{rows} {pending:?}: not the full width"
                );
                assert_eq!(
                    over.rows.len(),
                    1,
                    "{cols}x{rows} {pending:?}: more than one row"
                );
                assert_eq!(
                    over.rows[0].width(),
                    usize::from(cols),
                    "{cols}x{rows} {pending:?}: the row overflows the screen"
                );
            }
        }
    }

    #[test]
    fn the_bar_is_the_last_row_at_full_width() {
        for (cols, rows) in [(80u16, 24u16), (13, 2), (200, 50)] {
            let over = overlay(Pending::Command, true, size(cols, rows)).expect("a bar");
            assert_eq!(over.top, rows - 1, "not the last row at {cols}x{rows}");
            assert_eq!(over.left, 0, "not the full width at {cols}x{rows}");
            assert_eq!(over.width, cols);
            assert_eq!(over.rows.len(), 1, "more than one row");
            assert_eq!(
                over.rows[0].width(),
                usize::from(cols),
                "the row is not {cols} columns wide"
            );
        }
    }

    #[test]
    fn a_question_takes_the_row_until_it_is_taken_down() {
        let size = big();
        let mut d = Driver::restoring(size);
        d.bar
            .ask(Some("1 unsaved setting — w save and detach".into()));
        let Act::Write(bytes) = d.quiet(None, size) else {
            panic!("the question did not go up");
        };
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("1 unsaved setting — w save and detach"),
            "{text:?}"
        );
        assert!(
            text.contains(QUESTION_SGR) && !text.contains(SGR),
            "dim: {text:?}"
        );
        assert_eq!(
            d.quiet(Some(Pending::Command), size),
            Act::Idle,
            "nothing else takes it"
        );
        d.bar.ask(None);
        assert!(
            matches!(d.quiet(None, size), Act::Write(_)),
            "the row is put back"
        );
        assert!(!d.bar.is_up());
    }

    #[test]
    fn a_question_is_cut_to_the_screen() {
        let over = question_overlay("abcdefghij", size(6, 3)).expect("a row");
        assert_eq!(over.rows, vec!["abcdef".to_string()]);
        let over = question_overlay("ab", size(6, 3)).expect("a row");
        assert_eq!(over.rows, vec!["  ab  ".to_string()]);
        assert_eq!(question_overlay("ab", size(6, 1)), None, "no room");
    }

    #[test]
    fn the_text_is_centred_on_the_row() {
        let row = row(Pending::Command, size(100, 24));
        let before = row.len() - row.trim_start().len();
        let after = row.len() - row.trim_end().len();
        assert!(before > 0 && after > 0, "not centred: {row:?}");
        assert!(
            before.abs_diff(after) <= 1,
            "off centre by more than the odd column: {row:?}"
        );
    }

    /// On the last row, a newline or trailing printable byte would scroll.
    #[test]
    fn the_bytes_are_one_synchronized_update_that_cannot_scroll() {
        let over = overlay(Pending::Command, true, big()).expect("a bar");
        let bytes = announce::placed(&over, SGR);
        let text = String::from_utf8(bytes).expect("utf-8");

        assert!(text.starts_with("\x1b[?2026h\x1b7"), "{text:?}");
        assert!(text.ends_with("\x1b8\x1b[?2026l"), "{text:?}");
        assert!(!text.contains('\n'), "a newline would scroll: {text:?}");
        assert!(!text.contains('\r'), "a carriage return: {text:?}");
        assert_eq!(
            text.matches("\x1b[").count(),
            4,
            "one row, one CUP: {text:?}"
        );
        assert!(
            text.contains("\x1b[24;1H"),
            "not placed absolutely: {text:?}"
        );
        assert!(text.contains(SGR), "not drawn dim: {text:?}");
        let tail = text.rsplit_once("\x1b8").expect("a restore").1;
        assert_eq!(tail, "\x1b[?2026l", "something follows the row: {tail:?}");
    }

    /// Quiet sooner than [`SETTLE`] after the last write is no lull.
    #[test]
    fn the_bar_waits_for_a_lull() {
        let mut d = Driver::new(big());
        for _ in 0..10 {
            assert_eq!(
                d.busy(Some(Pending::Command), big()),
                Act::Idle,
                "wrote over a talking child"
            );
            assert_eq!(
                d.wake_at(),
                Some(d.now + SETTLE),
                "a write is owed, so a wake-up is asked for"
            );
        }
        let quiet = Pass {
            wrote: false,
            between: None,
        };
        let wrote = d.now;
        assert_eq!(
            d.bar.step(
                wrote + SETTLE / 2,
                quiet,
                big(),
                Some(Pending::Command),
                true,
                None
            ),
            Act::Idle,
            "wrote in a gap in the middle of the child's write"
        );
        assert_eq!(
            d.wake_at(),
            Some(wrote + SETTLE),
            "the lull is not counted from the child's last write"
        );
        assert!(matches!(
            d.quiet(Some(Pending::Command), big()),
            Act::Write(_)
        ));
    }

    /// Regression: counting from the first quiet pass needed two lulls, so a
    /// statusline redrawn every two lulls kept the bar down.
    #[test]
    fn a_child_writing_every_other_lull_leaves_room_for_the_bar() {
        let size = big();
        let start = Instant::now();
        let mut bar = Bar::new(start, size, Keymap::default());
        let pending = Some(Pending::Command);
        let busy = Pass {
            wrote: true,
            between: None,
        };
        let quiet = Pass {
            wrote: false,
            between: None,
        };
        assert_eq!(
            bar.step(start, busy, size, pending, true, None),
            Act::Idle,
            "wrote over a talking child"
        );
        let wake = bar.wake_at().expect("a write is owed");
        assert!(
            wake < start + 2 * SETTLE,
            "the relay is not woken until the next redraw, {:?} after this one",
            wake - start
        );
        assert!(
            matches!(
                bar.step(wake, quiet, size, pending, true, None),
                Act::Write(_)
            ),
            "the bar did not go up in the lull between two redraws"
        );
    }

    #[test]
    fn the_bar_is_painted_once_while_the_prefix_waits() {
        let mut d = Driver::new(big());
        assert!(matches!(
            d.quiet(Some(Pending::Command), big()),
            Act::Write(_)
        ));
        for _ in 0..5 {
            assert_eq!(d.quiet(Some(Pending::Command), big()), Act::Idle);
        }
        assert_eq!(d.wake_at(), None, "nothing is owed");
    }

    #[test]
    fn a_covered_bar_is_painted_again_at_the_next_lull() {
        for cover in ["the child", "the notice"] {
            let mut d = Driver::new(big());
            assert!(matches!(
                d.quiet(Some(Pending::Command), big()),
                Act::Write(_)
            ));

            if cover == "the child" {
                assert_eq!(
                    d.busy(Some(Pending::Command), big()),
                    Act::Idle,
                    "{cover}: not while it is still talking"
                );
            } else {
                d.bar.overdrawn();
            }
            assert!(
                d.wake_at().is_some(),
                "{cover}: a repaint is owed and not asked for"
            );
            assert!(
                matches!(d.quiet(Some(Pending::Command), big()), Act::Write(_)),
                "{cover}: the bar did not go back up"
            );
        }
    }

    #[test]
    fn a_bar_that_is_down_is_not_woken_by_output() {
        let mut d = Driver::new(big());
        for _ in 0..10 {
            assert_eq!(d.busy(None, big()), Act::Idle);
            assert_eq!(d.wake_at(), None, "woken for a bar that is not up");
        }
        d.bar.overdrawn();
        assert_eq!(d.wake_at(), None, "woken for a bar that is not up");
    }

    #[test]
    fn a_watched_bar_goes_back_up_on_the_pass_the_session_drew_over_it() {
        let mut d = Driver::new(big());
        assert!(matches!(
            d.quiet(Some(Pending::Command), big()),
            Act::Write(_)
        ));
        for _ in 0..10 {
            let act = d.watched(true, true, Some(Pending::Command), big());
            let Act::Write(bytes) = act else {
                panic!("the session drew over the bar and it stayed down: {act:?}");
            };
            assert!(
                String::from_utf8_lossy(&bytes).contains("picker"),
                "what went back up is not the bar"
            );
            assert_eq!(
                d.watched(false, true, Some(Pending::Command), big()),
                Act::Idle,
                "painted again over a row nothing had touched"
            );
        }
    }

    #[test]
    fn a_watched_bar_is_never_written_into_a_sequence() {
        let mut d = Driver::new(big());
        assert!(matches!(
            d.quiet(Some(Pending::Command), big()),
            Act::Write(_)
        ));
        assert_eq!(
            d.watched(true, false, Some(Pending::Command), big()),
            Act::Idle,
            "written into the middle of the session's sequence"
        );
        for _ in 0..10 {
            assert_eq!(
                d.watched(false, false, Some(Pending::Command), big()),
                Act::Idle,
                "a lull let the bar into the middle of a sequence"
            );
            assert_eq!(
                d.wake_at(),
                None,
                "asked to be woken for a moment no clock brings"
            );
        }
        assert!(
            matches!(
                d.watched(true, true, Some(Pending::Command), big()),
                Act::Write(_)
            ),
            "the session finished its sequence and the bar stayed down"
        );
    }

    #[test]
    fn a_watched_bar_comes_down_without_waiting_for_a_lull() {
        let mut d = Driver::restoring(big());
        assert!(matches!(
            d.quiet(Some(Pending::Command), big()),
            Act::Write(_)
        ));
        let act = d.watched(true, true, None, big());
        let Act::Write(down) = act else {
            panic!("the bar waited to come down: {act:?}");
        };
        let down = String::from_utf8_lossy(&down).to_string();
        assert!(
            down.contains(&"x".repeat(80)) && !down.contains("picker"),
            "the session's own row did not come back: {down:?}"
        );
        assert_eq!(
            d.watched(true, true, None, big()),
            Act::Idle,
            "erased twice"
        );
    }

    #[test]
    fn the_bar_is_up_from_the_prefix_to_the_erase() {
        let mut d = Driver::restoring(big());
        assert!(!d.bar.is_up(), "up before anything was asked of it");
        assert_eq!(d.busy(Some(Pending::Command), big()), Act::Idle);
        assert!(d.bar.is_up(), "a bar waiting for its lull is not up");
        assert!(matches!(
            d.quiet(Some(Pending::Command), big()),
            Act::Write(_)
        ));
        assert!(d.bar.is_up());

        assert_eq!(d.watched(true, false, None, big()), Act::Idle);
        assert!(d.bar.is_up(), "down with the bar still on the row");
        assert!(matches!(d.watched(false, true, None, big()), Act::Write(_)));
        assert!(!d.bar.is_up(), "still up once the row was put back");

        let mut d = Driver::new(size(6, 24));
        d.quiet(Some(Pending::Command), size(6, 24));
        assert!(!d.bar.is_up(), "up on a screen it cannot draw on");
    }

    #[test]
    fn a_bar_that_never_painted_has_nothing_to_erase() {
        let mut d = Driver::new(big());
        assert_eq!(d.busy(Some(Pending::Command), big()), Act::Idle);
        d.quiet(None, big());
        assert_eq!(d.quiet(None, big()), Act::Idle, "erased a bare row");
        assert_eq!(d.wake_at(), None);
    }

    #[test]
    fn the_erase_is_the_screen_underneath() {
        let mut d = Driver::restoring(big());
        let Act::Write(up) = d.quiet(Some(Pending::Command), big()) else {
            panic!("the bar did not go up");
        };
        assert!(
            String::from_utf8_lossy(&up).contains("picker"),
            "the bar does not say what it is for"
        );

        let act = d.quiet(None, big());
        let Act::Write(down) = act else {
            panic!("a restore is not a blanked row and needs no server: {act:?}");
        };
        let down = String::from_utf8_lossy(&down).to_string();
        assert!(
            down.contains(&"x".repeat(80)),
            "the session's own row did not come back: {down:?}"
        );
        assert!(
            !down.contains("picker"),
            "the bar is still on the row: {down:?}"
        );
        assert_eq!(d.quiet(None, big()), Act::Idle, "restored twice");
    }

    #[test]
    fn with_no_shadow_the_row_is_blanked_exactly_once() {
        let mut d = Driver::new(big());
        assert!(matches!(
            d.quiet(Some(Pending::Command), big()),
            Act::Write(_)
        ));
        let act = d.quiet(None, big());
        let Act::Blanked(bytes) = act else {
            panic!("expected a blanked row, got {act:?}");
        };
        let text = String::from_utf8_lossy(&bytes).to_string();
        assert!(!text.contains("picker"), "not blanked: {text:?}");
        assert!(
            text.contains(&" ".repeat(80)),
            "not the whole row: {text:?}"
        );

        assert_eq!(d.quiet(None, big()), Act::Idle, "asked twice");
        assert_eq!(d.wake_at(), None);
    }

    /// A retired shadow's grid is stale and must not be used.
    #[test]
    fn only_a_usable_shadow_with_colours_puts_the_row_back() {
        let over = overlay(Pending::Command, true, big()).expect("a bar");
        let shadow = screen(24, 80);
        assert!(shadow.is_usable(), "the fixture starts usable");

        assert!(matches!(
            erase(&over, Some(&shadow), Some(&palette())),
            Act::Write(_)
        ));
        assert!(
            matches!(erase(&over, Some(&shadow), None), Act::Blanked(_)),
            "painted a restore with no colours to resolve the cells into"
        );
        assert!(
            matches!(erase(&over, None, Some(&palette())), Act::Blanked(_)),
            "put a row back with no screen to put it back from"
        );
    }

    #[test]
    fn a_resize_forgets_the_row_it_painted_and_draws_the_new_one() {
        let mut d = Driver::restoring(big());
        let Act::Write(bytes) = d.quiet(Some(Pending::Command), big()) else {
            panic!("the bar did not go up");
        };
        assert!(
            String::from_utf8_lossy(&bytes).contains("\x1b[24;1H"),
            "not on row 24"
        );

        let taller = size(80, 40);
        let Act::Write(bytes) = d.quiet(Some(Pending::Command), taller) else {
            panic!("the bar did not move");
        };
        let text = String::from_utf8_lossy(&bytes).to_string();
        assert!(
            text.contains("\x1b[40;1H"),
            "not redrawn on row 40: {text:?}"
        );
        assert!(
            !text.contains("\x1b[24;1H"),
            "the row it left behind was written to as well: {text:?}"
        );
    }

    #[test]
    fn go_is_offered_only_with_a_card_to_go_to() {
        let go = format!("{} go", keys::ENTER_GLYPH);
        let with = text(Pending::Command, true, 200).expect("a row");
        let without = text(Pending::Command, false, 200).expect("a row");
        assert!(with.contains(&go), "{with:?}");
        assert!(!without.contains(&go), "{without:?}");
        assert_eq!(
            without,
            with.split(GAP)
                .filter(|e| *e != go)
                .collect::<Vec<_>>()
                .join(GAP)
        );
    }

    #[test]
    fn a_card_coming_down_redraws_the_bar() {
        let size = big();
        let mut d = Driver::new(size);
        assert!(matches!(
            d.quiet(Some(Pending::Command), size),
            Act::Write(_)
        ));
        d.now += SETTLE;
        let quiet = Pass {
            wrote: false,
            between: None,
        };
        let act = d
            .bar
            .step(d.now, quiet, size, Some(Pending::Command), false, None);
        let Act::Write(bytes) = act else {
            panic!("the bar was not redrawn: {act:?}");
        };
        let painted = String::from_utf8_lossy(&bytes);
        assert!(!painted.contains("go"), "{painted:?}");
    }

    #[test]
    fn a_pending_number_is_shown() {
        assert_eq!(text(Pending::Number(1), true, 80).as_deref(), Some("#1"));
        assert_eq!(text(Pending::Number(12), true, 80).as_deref(), Some("#12"));
        assert!(row(Pending::Number(7), big()).contains("#7"));
    }

    #[test]
    fn a_bar_with_nothing_to_draw_asks_for_no_wake_up() {
        let narrow = size(6, 24);
        assert_eq!(
            overlay(Pending::Command, true, narrow),
            None,
            "the fixture is wide enough after all"
        );
        let mut d = Driver::new(narrow);
        assert_eq!(d.quiet(Some(Pending::Command), narrow), Act::Idle);
        assert_eq!(
            d.wake_at(),
            None,
            "still asking to be woken for a row it cannot draw"
        );
        // The decision is per state, not per screen.
        assert!(matches!(
            d.quiet(Some(Pending::Number(3)), narrow),
            Act::Write(_)
        ));
    }
}
