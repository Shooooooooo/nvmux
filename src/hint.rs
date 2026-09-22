//! The hint bar the prefix puts up: one row, along the bottom, saying what the
//! next key does.
//!
//! `<prefix>` used to be silent. [`crate::keys::Prefix`] swallows the byte,
//! arms, and waits `keys.timeout_ms` for the second key, and until that key
//! arrives the screen says nothing at all — so the table is only learnable from
//! `<prefix> ?`, which is itself one of the keys you have to know, and a chord
//! pressed by accident looks for half a second like a keystroke the editor
//! dropped. This is the row the picker has always had, on the one screen that
//! did not have it.
//!
//! It is built from [`crate::keys::BINDINGS`], like the help screen
//! ([`crate::ui::help`]), so a command cannot be added without appearing on it.
//!
//! # What is on it, and what is not
//!
//! The commands, and the digit rule. Not the machine's other two branches: a
//! doubled prefix sends a literal, and any other key is replayed, both of which
//! the help *screen* has room to describe. This is one row read in a fraction
//! of a second with a command half-typed, and "press it again to send it to
//! Neovim" is not something anyone needs told at that moment.
//!
//! And never the prefix itself. Every entry is the *second* key, so the row is
//! the same whatever `[keys] prefix` is set to and there is no way for it to
//! spell `Ctrl-Space` at somebody who has remapped it. Where a string does have
//! to name the prefix, [`crate::keys::prefix_label`] is what names it.
//!
//! # Why the last row
//!
//! It is Neovim's message row, so the bar covers `-- INSERT --`, a `:` command
//! line, a search prompt or the ruler for as long as it is up.
//! [`crate::announce`]'s header rejects "print a line on the message row" — but
//! that objection is to *asking Neovim* to print there, which is editor state
//! nvmux has no business writing. Drawing over the row from the terminal side
//! touches none of it: nothing is created in the session, nothing is typed at
//! it, and the cells come back exactly.
//!
//! It is the right row because it is the one the eye is already on for modal
//! state, and the one row the attach notice's box never wants above three rows.
//!
//! The cursor is left where the session had it, which at a `:` prompt means on
//! this row, blinking over the hint text. [`crate::shadow`] declines to hide the
//! cursor for an overlay for the same reason: the hide would itself be seen, for
//! as long as the bar is up.
//!
//! # The lull, and the two clocks this does not have
//!
//! nvmux owns no cells while a session is attached, so the bar is written over
//! the session's screen and put back — and, like everything else nvmux draws
//! there, only at a lull ([`SETTLE`]). `pump` never parses the child's output,
//! and a lull is the one state in which it cannot be mid-sequence.
//!
//! There is no fade. The bar has to be up the moment the prefix arms, and the
//! window it lives in is `keys.timeout_ms` — 500 ms by default, against a
//! dissolve that is 100 ms each way. And there is no give-up clock either: the
//! bar's life is already bounded by the prefix, so a session too chatty to give
//! a lull simply shows the bar late, or not at all, which is what a lull rule
//! means. Both of those the attach notice has and needs; neither belongs here.
//!
//! # Coming down
//!
//! Most of the ways the bar is dismissed end the relay — the picker, a detach, a
//! switch — and there the screen is dissolved or cleared on the way out and the
//! bar goes with it. What is left is the prefix resolving *without* ending the
//! relay: a lone prefix timing out into a literal, a doubled prefix, a key that
//! is not a command. Then the cells have to be put back, and only two things
//! have them: [`crate::shadow`], which has been keeping the session's screen all
//! along and gives it back exactly ([`Shadow::under`] at `t = 1.0`), and failing
//! that the server, which the relay asks — see [`Act::Blanked`].

use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr;

use crate::announce;
use crate::keys::{self, Pending};
use crate::palette::{self, Palette};
use crate::pty::PtySize;
use crate::shadow::{Over, Shadow};

/// How long the child must have been quiet before the bar may be written.
///
/// [`crate::announce::SETTLE`]'s number, for its reason: a child whose own write
/// blocked because the pty buffer filled leaves the master momentarily
/// unreadable in the *middle* of a frame, and a wait this long steps over that
/// gap. Its own constant rather than a shared one because the two are the same
/// number by coincidence of the same hardware, not by agreement.
///
/// It costs the bar nothing in practice. A human reaching for the prefix is
/// almost always further than 25 ms from the last byte the editor wrote, so the
/// lull is already satisfied when the keystroke is read and the bar goes up on
/// that very pass of the relay.
const SETTLE: Duration = Duration::from_millis(25);

/// Columns between entries. `src/ui/draw.rs`'s, because this is that row.
const GAP: &str = "  ";

/// The digit rule's entry. `1-n` rather than `1-9` for the reason
/// [`crate::ui::help`] already gives: `Prefix::feed` reads a number, not a
/// digit, so `12` reaches the twelfth session.
const DIGITS: &str = "1-n session";

/// A reset, then `DIM`. Every nvmux hint row is dim (`draw::draw_hint_row`), and
/// the reset is what puts the background back, so the modifier rides along on
/// the same CSI rather than following it.
const SGR: &str = "\x1b[0;2m";

/// The shortest screen the bar will draw on. Two, so it is never a session's
/// only row: an editor left with nothing but nvmux's hints is worse than a user
/// left to find `<prefix> ?`.
const MIN_ROWS: u16 = 2;

/// What the relay must do about the bar on this pass of its loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// Nothing this time round.
    Idle,
    /// Write these bytes over the session's screen, and that is all.
    Write(Vec<u8>),
    /// Write these bytes — and then ask the server to repaint.
    ///
    /// The bar is coming down with no shadow to put the row back from, so the
    /// most these bytes can do is stop showing something wrong: they blank the
    /// row rather than restore it. Only the server has the cells now, which is
    /// the position the attach notice is always in, and the relay asks in the
    /// same way it asks for that one (see `pty::Erasing`).
    Blanked(Vec<u8>),
}

/// Every entry the bar can offer, in the order it offers them: the commands as
/// [`keys::BINDINGS`] lists them, then the digit rule.
///
/// Keys are spelled [`keys::key_glyph`], which is the picker's hint-row grammar
/// and not the help screen's: lowercase throughout, and `␣` for the space bar.
/// This is the picker's row on another screen, so it is spelled the picker's way
/// — two rows that named the same key two ways would be the kind of drift the
/// rest of this crate pins down with a test, and there is one below.
fn entries() -> Vec<String> {
    keys::BINDINGS
        .iter()
        .map(|b| format!("{} {}", keys::key_glyph(b.key), b.hint))
        .chain(std::iter::once(DIGITS.to_string()))
        .collect()
}

/// What the row says on a screen `cols` wide, or `None` when not even the first
/// entry fits.
///
/// Entries are dropped **whole**, from the right, rather than truncated the way
/// `draw::truncate` cuts the picker's row: this row is written over somebody's
/// editor, and half of `1-n sess` there reads as corruption rather than as a
/// hint. Which entry goes first is [`keys::BINDINGS`]' order and not a second
/// opinion about priority — one table, one order — so `? help` goes before
/// `p prev`, and the two that a user pressing the prefix by accident most needs,
/// the picker and the detach, are the two that survive longest.
fn text(pending: Pending, cols: u16) -> Option<String> {
    let cols = usize::from(cols);
    let row = match pending {
        // A number being typed, as the picker shows one on its own hint row —
        // without the `▋` it puts after it. That is a prompt cursor on a screen
        // the picker owns; here Neovim's real cursor is somewhere else on the
        // screen, and a second one would be a lie.
        Pending::Number(n) => format!("#{n}"),
        Pending::Command => {
            let mut row = String::new();
            for entry in entries() {
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

/// Where the bar goes and what it says: the whole of the last row, with the text
/// centred on it.
///
/// The full width, padded out with spaces either side, because the cells beside
/// the text are what clears the editor's own last row from under it — a row of
/// hints interleaved with half a statusline would read as damage. That it is
/// safe to fill the last row to its final column is the property
/// [`announce::placed`] writes down: nothing printable follows the row, so the
/// pending-wrap flag its last cell sets is discarded by the `DECRC` rather than
/// scrolling the session's screen by a line.
///
/// `None` when the screen has no room. Pure, so the geometry is testable without
/// a terminal, and named, because two things need it: the paint, and the erase
/// that has to put back exactly the cells the paint covered.
pub fn overlay(pending: Pending, size: PtySize) -> Option<Over> {
    if size.rows < MIN_ROWS || size.cols == 0 {
        return None;
    }
    let text = text(pending, size.cols)?;
    let cols = usize::from(size.cols);
    // Both subtractions are safe: `text` was measured to fit `cols`. Odd slack
    // falls to the right, which is what integer division does and is not worth a
    // correction nobody could see.
    let left = (cols - text.width()) / 2;
    let right = cols - text.width() - left;
    Some(Over {
        top: size.rows - 1,
        left: 0,
        width: size.cols,
        rows: vec![format!("{}{text}{}", " ".repeat(left), " ".repeat(right))],
    })
}

/// The bar over the life of one relay.
///
/// Owned by the relay loop, one per attach, and told the time rather than
/// reading a clock — like [`announce::Popup`] and [`keys::Prefix`], and for the
/// same reason: every state it can reach is then reachable from a test.
///
/// What it holds is the difference between two pictures of the last row: the one
/// on the terminal, and the one the prefix machine implies. `agreed` is whether
/// they match, and a write is owed whenever they do not.
#[derive(Debug)]
pub struct Bar {
    /// When the child was last seen with nothing to say, or `None` if it spoke
    /// on the last pass. Kept from the start of the relay rather than from the
    /// moment the prefix arms, so the lull is already behind us when it does and
    /// the bar goes up on that pass instead of a wake-up later.
    quiet_since: Option<Instant>,
    /// The rectangle on the terminal now, or `None` when the bar is down. Kept,
    /// rather than worked out again at the time, because the erase has to put
    /// back the cells the bar *covered* and not whichever cells the last row is
    /// now.
    shown: Option<Over>,
    /// The rectangle the machine's state implies, worked out on every pass.
    want: Option<Over>,
    /// Whether the terminal has been made to agree with `want`. False is a write
    /// owed, and is exactly when [`Bar::wake_at`] asks to be woken.
    agreed: bool,
    /// The screen the last pass was for. Rows and columns only: a pixel-size
    /// change moves no cell.
    size: (u16, u16),
    /// The colours a restored row is painted in, `None` when the terminal never
    /// said what they are. Copied once rather than read at the time, the way
    /// [`announce::Popup`] is handed its [`crate::fade::Dissolve`]: a bar that
    /// read a process global mid-relay would have states no test could reach,
    /// since no palette is ever installed under test.
    palette: Option<Palette>,
}

impl Bar {
    /// A bar with nothing on screen, on a child presumed quiet — which it is,
    /// at the top of a relay, until it says otherwise.
    pub fn new(now: Instant, size: PtySize) -> Self {
        Self::with_palette(now, size, palette::get().copied())
    }

    /// [`Bar::new`] with the colours handed in rather than read, which is how
    /// the tests reach the restoring path at all.
    fn with_palette(now: Instant, size: PtySize, palette: Option<Palette>) -> Self {
        Self {
            quiet_since: Some(now),
            shown: None,
            want: None,
            agreed: true,
            size: (size.rows, size.cols),
            palette,
        }
    }

    /// When the relay must next wake up on the bar's account, or `None` when it
    /// owes nothing.
    ///
    /// `None` is the common answer, and it has to be: the bar is down and idle
    /// for almost all of a session, and a relay woken every 25 ms on its account
    /// would be paying for a row nobody has asked for.
    ///
    /// It is also what stops a spin. A bar that *cannot* do what it wants — a
    /// screen too narrow for one entry, a shadow whose parser has been retired —
    /// records the decision by agreeing with itself, so this stops asking. Were
    /// it to keep asking, the poll timeout would round to zero and the relay
    /// would spin on a write it can never make: the hazard `pty::Hold::wake_at`
    /// steps around for the held first paint.
    pub fn wake_at(&self, now: Instant) -> Option<Instant> {
        if self.agreed {
            return None;
        }
        Some(self.quiet_since.unwrap_or(now) + SETTLE)
    }

    /// Something that is not the child has written over the session's screen —
    /// a frame of the attach notice — so the bar may no longer be on it.
    pub fn overdrawn(&mut self) {
        self.may_have_been_covered();
    }

    /// One pass of the relay loop.
    ///
    /// `busy` is whether the child had anything for the terminal this time
    /// round, or is having its first paint held: both mean the same two things
    /// here, that this is no moment to write and that anything already on the
    /// row may have been drawn over. `under` is the session's screen as the
    /// shadow has it, which is what an erase puts back.
    pub fn step(
        &mut self,
        now: Instant,
        busy: bool,
        size: PtySize,
        pending: Option<Pending>,
        under: Option<&Shadow>,
    ) -> Act {
        // A resize is its own erase, so the bar forgets what it drew instead of
        // putting it back: the size reaches the client through the pty and the
        // client repaints the whole grid, which is the session taking its row
        // back. Putting it back here would be worse than redundant after a
        // shrink — the rectangle that was painted is off the grid, and the
        // terminal would clamp every `CUP` in it onto a row of somebody's text.
        if self.size != (size.rows, size.cols) {
            self.size = (size.rows, size.cols);
            self.shown = None;
        }

        // Before the lull, not after it: the bar has to know a write is owed in
        // order to ask to be woken to make it, and the pass that reads the
        // prefix is often one the child is still talking on.
        let want = pending.and_then(|p| overlay(p, size));
        if want != self.want {
            self.want = want;
            self.agreed = false;
        }

        if busy {
            self.quiet_since = None;
            self.may_have_been_covered();
        } else {
            self.quiet_since.get_or_insert(now);
        }

        let settled = self
            .quiet_since
            .is_some_and(|quiet| now.duration_since(quiet) >= SETTLE);
        if !settled || self.agreed {
            return Act::Idle;
        }

        // Whatever comes of this pass, the bar has now had its say about this
        // state on this screen — including deciding it can say nothing, which is
        // what keeps `wake_at` quiet.
        self.agreed = true;
        match self.want.clone() {
            Some(over) => {
                let bytes = announce::placed(&over, SGR);
                self.shown = Some(over);
                Act::Write(bytes)
            }
            None => match self.shown.take() {
                Some(over) => erase(&over, under, self.palette.as_ref()),
                None => Act::Idle,
            },
        }
    }

    /// The terminal may no longer show what `want` says — but only if the bar
    /// had anything there to be covered. A bar that is down has nothing to put
    /// back, and saying otherwise would have [`Bar::wake_at`] ask for a wake-up
    /// after every burst of output for the whole of a session.
    fn may_have_been_covered(&mut self) {
        self.agreed &= self.shown.is_none();
    }
}

/// Take the bar off the row it covered.
///
/// The shadow's copy of those cells is the only one anywhere once the terminal
/// has been written over, and at `t = 1.0` a composite is that copy with none of
/// the overlay left in it — an erase that is exact, local, and costs no round
/// trip. What it cannot carry is decoration: the grid resolves colours, bold and
/// dim, so an italic or an underline on the row comes back plain. For one row
/// that Neovim keeps mostly blank that is the right trade against asking the
/// server on every mistyped chord.
///
/// Failing a shadow — the fade off, `[fade] session = false`, a terminal that
/// never said what its colours are — or a shadow whose parser has been retired,
/// there is nothing here that knows what was underneath. The row is blanked, and
/// the relay is told to ask the server for the rest.
fn erase(over: &Over, under: Option<&Shadow>, palette: Option<&Palette>) -> Act {
    match (under.filter(|s| s.is_usable()), palette) {
        (Some(shadow), Some(palette)) => Act::Write(shadow.under(over, palette, 1.0)),
        _ => Act::Blanked(announce::plain_bytes(&blank(over), None)),
    }
}

/// The same rectangle with nothing in it. What a blanked row is written from.
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
    use crate::ui::test_support::TINY_SIZES;

    fn size(cols: u16, rows: u16) -> PtySize {
        PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    /// A screen with room for the whole row, which most of these want.
    fn big() -> PtySize {
        size(80, 24)
    }

    /// The bar's one row of text, as it would be drawn on `size`.
    fn row(pending: Pending, size: PtySize) -> String {
        overlay(pending, size).expect("a bar").rows.remove(0)
    }

    /// A shadow with something on it to put back: every cell an `x`, which is in
    /// none of the bar's words, so a restored row shows the screen exactly when
    /// it has one in it.
    fn screen(rows: u16, cols: u16) -> Shadow {
        let mut shadow = Shadow::new(rows, cols);
        for row in 1..=rows {
            shadow.feed(format!("\x1b[{row};1H{}", "x".repeat(usize::from(cols))).as_bytes());
        }
        shadow
    }

    /// A terminal that said its text is light grey on black. No palette is ever
    /// installed under test, so this is the only way to reach the restoring
    /// path — which is why [`Bar::with_palette`] exists.
    fn palette() -> Palette {
        Palette {
            fg: crate::palette::Rgb(200, 200, 200),
            bg: crate::palette::Rgb(0, 0, 0),
            ansi: [crate::palette::Rgb(0, 0, 0); 16],
        }
    }

    /// A bar and the clock it is told, advanced a lull at a time.
    ///
    /// The clock has to be the test's, not `Instant::now()`: a lull *starts* on
    /// the first quiet pass and has passed by the next one, so two calls that
    /// read the same clock are indistinguishable from a child that has only just
    /// stopped talking.
    struct Driver {
        bar: Bar,
        now: Instant,
        shadow: Option<Shadow>,
    }

    impl Driver {
        fn new(size: PtySize) -> Self {
            let now = Instant::now();
            Self {
                bar: Bar::new(now, size),
                now,
                shadow: None,
            }
        }

        /// A bar with a screen to put the row back from, and the colours to do
        /// it in.
        fn restoring(size: PtySize) -> Self {
            let now = Instant::now();
            Self {
                bar: Bar::with_palette(now, size, Some(palette())),
                now,
                shadow: Some(screen(size.rows, size.cols)),
            }
        }

        /// One pass `SETTLE` later on a child with nothing to say.
        fn quiet(&mut self, pending: Option<Pending>, size: PtySize) -> Act {
            self.step(false, pending, size)
        }

        /// One pass `SETTLE` later on a child that is talking.
        fn busy(&mut self, pending: Option<Pending>, size: PtySize) -> Act {
            self.step(true, pending, size)
        }

        fn step(&mut self, busy: bool, pending: Option<Pending>, size: PtySize) -> Act {
            self.now += SETTLE;
            self.bar
                .step(self.now, busy, size, pending, self.shadow.as_ref())
        }

        fn wake_at(&self) -> Option<Instant> {
            self.bar.wake_at(self.now)
        }
    }

    /// The load-bearing one: the bar is [`keys::BINDINGS`] rendered, so a
    /// command cannot be added without appearing on it — the guarantee the help
    /// screen already gives, on the row you see before you have chosen a key.
    #[test]
    fn the_row_lists_every_binding_in_table_order() {
        let text = text(Pending::Command, 200).expect("a row");
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

    /// The prefix is the user's to set, so the bar must not name it: every entry
    /// is the *second* key, which is the same whatever the chord is. A row that
    /// said `Ctrl-Space` would be wrong for everyone who has remapped it, and
    /// wrong in the one place they cannot miss.
    #[test]
    fn the_row_never_spells_the_prefix() {
        let text = text(Pending::Command, 200).expect("a row");
        for prefix in [keys::PREFIX, keys::parse_prefix("C-t").expect("a chord")] {
            let label = keys::prefix_label(prefix);
            assert!(
                !text.contains(&label),
                "the bar spells the prefix ({label}): {text:?}"
            );
        }
        assert!(!text.contains("Ctrl"), "the bar names a chord: {text:?}");
    }

    /// The space bar is the one key on the row with no character of its own, and
    /// the picker already settled what to draw for it. A bar that spelled it
    /// `space` while the picker two keystrokes away spelled it `␣` would be the
    /// same row contradicting itself.
    #[test]
    fn the_space_bar_is_the_glyph_the_picker_uses() {
        let text = text(Pending::Command, 200).expect("a row");
        assert!(
            text.starts_with(&format!("{} picker", keys::SPACE_GLYPH)),
            "the space bar is not the glyph: {text:?}"
        );
        assert!(
            !text.contains("space") && !text.contains("Space"),
            "the space bar is spelled out as well: {text:?}"
        );
    }

    /// The help screen has rows for the machine's other two branches — a
    /// doubled prefix sends a literal, any other key is replayed — and the bar
    /// deliberately does not. Neither is a next key worth teaching on a row read
    /// with a command half-typed.
    #[test]
    fn the_row_does_not_offer_a_repeated_prefix_or_a_replay() {
        let text = text(Pending::Command, 200).expect("a row");
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

    /// Narrow screens drop entries whole. A fragment of one — `1-n sess` — is
    /// written over somebody's editor and reads as damage rather than as a hint,
    /// which is why this row does not use `draw::truncate`.
    #[test]
    fn entries_are_dropped_whole_to_fit() {
        let all = entries();
        for cols in 1..=200u16 {
            let Some(text) = text(Pending::Command, cols) else {
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

    /// Below the first entry the bar says nothing at all, rather than a word and
    /// a half of one. The attach notice's answer to the same question.
    #[test]
    fn a_screen_too_narrow_for_one_entry_says_nothing() {
        let first = entries().remove(0);
        for cols in 0..first.width() {
            assert_eq!(
                text(Pending::Command, u16::try_from(cols).expect("small")),
                None,
                "drew part of {first:?} in {cols} columns"
            );
        }
        assert!(text(
            Pending::Command,
            u16::try_from(first.width()).expect("small")
        )
        .is_some());
    }

    /// One row is never the bar's, however wide: an editor left with nothing but
    /// nvmux's hints is worse off than one with no hints at all.
    #[test]
    fn the_bar_never_takes_a_sessions_only_row() {
        assert_eq!(overlay(Pending::Command, size(200, 1)), None);
        assert!(overlay(Pending::Command, size(200, 2)).is_some());
    }

    /// The degenerate screens, where the bar must either decline or stay inside
    /// the grid — never write a cell the terminal would clamp somewhere else.
    ///
    /// Not "always nothing": `␣ picker` is eight columns, so the bar genuinely
    /// fits some of these, and asserting silence would only pin the width of the
    /// first entry. What must hold at every size is the rectangle.
    #[test]
    fn a_tiny_screen_is_either_declined_or_drawn_inside_itself() {
        for &(cols, rows) in TINY_SIZES {
            for pending in [Pending::Command, Pending::Number(7)] {
                let Some(over) = overlay(pending, size(cols, rows)) else {
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

    /// The row is the whole of the last row: the cells beside the text are what
    /// take the editor's own last line out from under it.
    #[test]
    fn the_bar_is_the_last_row_at_full_width() {
        for (cols, rows) in [(80u16, 24u16), (13, 2), (200, 50)] {
            let over = overlay(Pending::Command, size(cols, rows)).expect("a bar");
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

    /// Centred, like every other nvmux hint row.
    #[test]
    fn the_text_is_centred_on_the_row() {
        let row = row(Pending::Command, big());
        let before = row.len() - row.trim_start().len();
        let after = row.len() - row.trim_end().len();
        assert!(before > 0 && after > 0, "not centred: {row:?}");
        assert!(
            before.abs_diff(after) <= 1,
            "off centre by more than the odd column: {row:?}"
        );
    }

    /// The escape-sequence discipline `announce::placed` writes down, asserted
    /// where it matters most: this row is the *last* one, so a newline or a
    /// printable byte after its final cell would scroll the session's screen by
    /// a line rather than merely draw in the wrong place.
    #[test]
    fn the_bytes_are_one_synchronized_update_that_cannot_scroll() {
        let over = overlay(Pending::Command, big()).expect("a bar");
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
        // Nothing printable after the last cell: the pending-wrap flag it sets
        // is discarded by the restore rather than acted on.
        let tail = text.rsplit_once("\x1b8").expect("a restore").1;
        assert_eq!(tail, "\x1b[?2026l", "something follows the row: {tail:?}");
    }

    /// The bar is not written while the child is talking: `pump` never parses
    /// the child's output, so any other moment could be the middle of one of its
    /// escape sequences.
    ///
    /// And it waits out the lull rather than writing on the first quiet pass:
    /// that pass is where the lull *starts*, and a child whose own write blocked
    /// on a full pty buffer is momentarily unreadable in the middle of a frame.
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
        assert_eq!(
            d.quiet(Some(Pending::Command), big()),
            Act::Idle,
            "the lull starts on this pass, it has not passed yet"
        );
        assert!(matches!(
            d.quiet(Some(Pending::Command), big()),
            Act::Write(_)
        ));
    }

    /// An armed prefix is drawn once, not once per pass: the row does not change
    /// while it waits, and repainting it every 25 ms would be a write over the
    /// session for nothing.
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

    /// The child drawing over the row is the one thing that makes a repaint due,
    /// and `overdrawn` is the same news from the attach notice. Either way the
    /// bar goes back up at the next lull.
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
                // The pass that ends the burst starts the lull; the next one is
                // past it.
                assert_eq!(d.quiet(Some(Pending::Command), big()), Act::Idle);
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

    /// A bar that is down has nothing on the row to be covered, so a chatty
    /// session must not have it asking to be woken every 25 ms for the whole of
    /// a relay in which nobody touches the prefix.
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

    /// The prefix resolving before any lull means nothing was ever drawn, so
    /// there is nothing to put back — and no server to bother about it either.
    #[test]
    fn a_bar_that_never_painted_has_nothing_to_erase() {
        let mut d = Driver::new(big());
        assert_eq!(d.busy(Some(Pending::Command), big()), Act::Idle);
        d.quiet(None, big());
        assert_eq!(d.quiet(None, big()), Act::Idle, "erased a bare row");
        assert_eq!(d.wake_at(), None);
    }

    /// What pins the whole erase design: the cells the bar covered come back
    /// from the shadow, which is the only copy of them once the terminal has been
    /// written over — so the bar leaves without the server having to be asked.
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

    /// With no shadow the row is blanked once and the relay is told to ask the
    /// server — once, and not on every pass afterwards, which is the difference
    /// between a repaint and a stall.
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

    /// Each of the erase's three cases, which is the only place they are all
    /// reachable: only a usable shadow *and* colours to paint it in can put the
    /// row back, and everything else blanks it and hands the relay the job.
    ///
    /// A retired shadow is the one that matters. Its grid can no longer be
    /// trusted, so a composite from it would paint whatever it last held over a
    /// row the editor has moved on from, and call the bar erased.
    #[test]
    fn only_a_usable_shadow_with_colours_puts_the_row_back() {
        let over = overlay(Pending::Command, big()).expect("a bar");
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

    /// A resize is the client's own repaint, so the bar forgets the row it drew
    /// instead of putting it back, and draws the new one where the last row now
    /// is. Restoring the old rectangle after a shrink would clamp every `CUP` in
    /// it onto a row of the editor's text.
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

    /// A half-typed number is shown, or the bar would look like it had swallowed
    /// the digit — the reason the picker shows one on its own hint row.
    #[test]
    fn a_pending_number_is_shown() {
        assert_eq!(text(Pending::Number(1), 80).as_deref(), Some("#1"));
        assert_eq!(text(Pending::Number(12), 80).as_deref(), Some("#12"));
        assert!(row(Pending::Number(7), big()).contains("#7"));
    }

    /// A screen too narrow to draw on decides that once and stops asking to be
    /// woken. Without the latch the poll timeout rounds to zero and the relay
    /// spins on a write it can never make — the hazard `pty::Hold::wake_at`
    /// steps around for the held first paint.
    #[test]
    fn a_bar_with_nothing_to_draw_asks_for_no_wake_up() {
        let narrow = size(6, 24);
        assert_eq!(
            overlay(Pending::Command, narrow),
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
        // And a number, which does fit six columns, is still drawn: the decision
        // is about this state on this screen, not about the screen for good.
        assert!(matches!(
            d.quiet(Some(Pending::Number(3)), narrow),
            Act::Write(_)
        ));
    }
}
