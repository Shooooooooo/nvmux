//! The brief "you are now on this session" announcement.
//!
//! Cycling with `<prefix> n` and `<prefix> p` means changing session without
//! naming one, so an attach has to say where it landed. That is easy in the
//! picker and awkward here: while a session is attached, nvmux owns no cells at
//! all. `nvim --remote-ui` draws the screen and [`crate::pty`] copies its bytes
//! through without looking at them, so there is nothing to composite into and
//! nothing to read back.
//!
//! So nvmux writes a box over the session's screen itself, and
//! [`crate::pty`]'s `repaint` then puts back what it covered — the same request
//! a resume makes, since only the server knows what was underneath. No editor
//! state is touched: nothing is created in the session, nothing is typed at it,
//! and a wedged editor is announced over as readily as an idle one.
//!
//! Two other shapes were built and compared before this one was kept. Neovim
//! could open a floating window over RPC, or print a line on the message row;
//! both are drawn by the editor, so neither can be torn or covered, and neither
//! needs erasing. Both were dropped for the same reason: a session manager that
//! creates windows in your editor, or writes on the row your editor talks to
//! you on, has stopped being a proxy. What is left is the one that draws on the
//! terminal nvmux already owns the bytes of.
//!
//! # The lull
//!
//! The box goes up on the first pass of the relay loop on which the child has
//! been quiet for [`SETTLE`]. That is the *only* safe moment: `pump` never
//! parses the child's output, so it cannot otherwise know that a write of its
//! own would not land in the middle of one of the child's escape sequences, and
//! a lull is the one state where it cannot.
//!
//! If no lull arrives within [`GIVE_UP`], nothing is shown. A screen that has
//! been busy for two solid seconds is one where a box dropped on top is as
//! likely to be corruption as information, and the name is not worth that.
//!
//! Nothing here is ever worth delaying or failing an attach for. A terminal too
//! small for the box, a screen that never settles: both are silence.

use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr;

use crate::pty::PtySize;
use crate::ui::draw::truncate;

/// How long the child must have been quiet before the announcement is shown.
///
/// Not zero, and this is the whole reason: a child whose own write blocked
/// because the pty buffer filled leaves the master momentarily unreadable in the
/// *middle* of a frame. That gap is microseconds; a wait this long steps over
/// it, and is still short enough that the announcement reads as part of the
/// attach rather than as something that happened afterwards.
const SETTLE: Duration = Duration::from_millis(25);

/// How long to wait for that lull before giving up on the announcement.
const GIVE_UP: Duration = Duration::from_secs(2);

/// How long the box stays up.
///
/// A second: long enough to catch a name out of the corner of the eye, short
/// enough that a box in the middle of the screen — squarely over the text you
/// have just switched to — is gone before it is in the way of anything. Not a
/// setting, because there is no second answer worth the config key: shorter and
/// it is a flicker, longer and it is something you wait out on every switch.
const DURATION: Duration = Duration::from_secs(1);

/// Columns between the box's border and its text.
const PAD: usize = 1;

/// The smallest screen the box fits on: two borders, two padding columns and a
/// column of text across, and three rows down.
const MIN_COLS: u16 = 5;
const MIN_ROWS: u16 = 3;

/// What the box says: the session's name, and nothing else.
///
/// Not the number. The number is how you *reach* a session — it is on the
/// picker's rows and it is what `<prefix> 3` takes — but this box is answering
/// a different question, asked after you have arrived, and a name answers it on
/// its own. `<prefix> n` and `<prefix> p` are exactly the keys that make the
/// number beside the answer noise: they are how you get somewhere without
/// naming a number in the first place.
///
/// Control characters are dropped. [`crate::session::validate_name`] already
/// refuses them, but it guards the *creating* path only: `<id>.json` is a file
/// on disk that a person can write, and this is the one place a name reaches a
/// raw terminal — `OPOST` off, no escaping between here and the wire — where an
/// escape sequence smuggled into a name would be executed rather than shown.
pub fn label(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).collect()
}

/// What the relay must do about the announcement on this pass of its loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// Nothing this time round.
    Idle,
    /// Write these bytes to the terminal, over the session's screen.
    Paint(Vec<u8>),
    /// The overlay's time is up. Take it off the screen, then drop the popup.
    Erase,
    /// Nothing more will happen. Drop the popup.
    Done,
}

/// An announcement waiting for its moment, and then living out its time.
///
/// Owned by the relay loop, one per attach. Told the time rather than reading a
/// clock, like [`crate::keys::Prefix`] is told its bytes, so every state it can
/// reach is reachable from a test.
///
/// The deadlines are absolute [`Instant`]s for the reason written down in
/// [`crate::pty`]: a child that keeps producing output keeps `poll` returning
/// early, and only the *quiet* clock may restart on that. If the other two
/// restarted with it, a session with a spinner in its statusline would hold the
/// box on screen for ever.
#[derive(Debug)]
pub struct Popup {
    label: String,
    /// When the child was last seen with nothing to say, or `None` if it spoke
    /// on the last pass.
    quiet_since: Option<Instant>,
    /// When to stop waiting for a lull that is not coming.
    give_up_at: Instant,
    /// Set when the box is first painted: the end of its life.
    until: Option<Instant>,
    /// False whenever the child may have drawn over the box since it was
    /// painted, which is the cue to paint it again at the next lull.
    painted: bool,
}

impl Popup {
    /// `None` when there is nothing to say — including a label with nothing
    /// left in it, which is what a name of nothing but control characters comes
    /// to once [`label`] has been through it. The relay then carries no state at
    /// all rather than a popup that would decline to draw itself later.
    pub fn arm(label: Option<String>, now: Instant) -> Option<Self> {
        let label = label.filter(|l| !l.is_empty())?;
        Some(Self {
            label,
            quiet_since: Some(now),
            give_up_at: now + GIVE_UP,
            until: None,
            painted: false,
        })
    }

    /// When the relay must next wake up on this popup's account.
    pub fn wake_at(&self, now: Instant) -> Instant {
        let settle = self.quiet_since.unwrap_or(now) + SETTLE;
        match self.until {
            // Repaint at the next lull, but never past the end of its life.
            Some(until) if !self.painted => settle.min(until),
            Some(until) => until,
            None => settle.min(self.give_up_at),
        }
    }

    /// One pass of the relay loop. `child_spoke` is whether the pty master had
    /// anything for the terminal this time round.
    pub fn step(&mut self, now: Instant, child_spoke: bool, size: PtySize) -> Act {
        if child_spoke {
            self.quiet_since = None;
            // Whatever was on screen may have been drawn over.
            self.painted = false;
        } else {
            self.quiet_since.get_or_insert(now);
        }

        if self.until.is_some_and(|until| now >= until) {
            return Act::Erase;
        }
        let settled = self
            .quiet_since
            .is_some_and(|quiet| now.duration_since(quiet) >= SETTLE);
        if !settled {
            // Nothing is on screen yet and the editor has been busy throughout:
            // let it be.
            return if self.until.is_none() && now >= self.give_up_at {
                Act::Done
            } else {
                Act::Idle
            };
        }

        if self.painted {
            return Act::Idle;
        }
        let Some(bytes) = overlay_bytes(&self.label, size) else {
            // Too small to draw on. Saying nothing is the right answer, and
            // there is nothing to erase either.
            return Act::Done;
        };
        self.painted = true;
        self.until.get_or_insert(now + DURATION);
        Act::Paint(bytes)
    }
}

/// The overlay as terminal bytes: a bordered box in the middle of the screen.
///
/// Pure, so the geometry can be tested without a terminal. `None` when the
/// screen cannot hold even a one-column box.
///
/// The middle, where it cannot be missed. It is squarely over the text you have
/// just switched to, which is the whole reason it is up for only a second: this
/// is a notice to be caught out of the corner of the eye and then gone, not
/// something to read.
///
/// What the sequence has to get right, in order:
///
/// * a synchronized-update span, so the box cannot be seen half-drawn;
/// * `DECSC`/`DECRC` (`ESC 7` / `ESC 8`) around everything — without them the
///   cursor would sit in the box's corner for as long as the box is up, and the
///   editor's own SGR attributes would be left as whatever this drew with. It
///   is a single shared save slot, which is safe here only because nothing is
///   written except at a lull, between the child's frames;
/// * `ESC [ 0 m` on each row, because the interior is spaces and a space is
///   erased with *the current background* — which is whatever the editor last
///   set, and would otherwise bleed into the box;
/// * an absolute `CUP` per row, and not one newline or carriage return
///   anywhere. `OPOST` is off in raw mode (see [`crate::term`]), so those move
///   the cursor rather than wrapping and are the classic way to corrupt a raw
///   overlay. It also means a box as wide as the screen never scrolls it: the
///   pending-wrap flag its last cell sets is discarded by the next `CUP`, or —
///   on the closing row — by the `DECRC` that is the last thing written.
///   Nothing printable ever follows it.
pub fn overlay_bytes(label: &str, size: PtySize) -> Option<Vec<u8>> {
    if size.cols < MIN_COLS || size.rows < MIN_ROWS {
        return None;
    }

    // Measured on the truncated text's own width, not on the room it was given:
    // a wide character that did not fit leaves the text narrower than its
    // budget, and a box sized to the budget would sit visibly loose around it.
    let text = truncate(label, usize::from(size.cols) - 2 - 2 * PAD);
    if text.is_empty() {
        return None;
    }
    let inner = text.width() + 2 * PAD;
    let width = inner + 2;

    // Centred, in the terminal's own 1-based coordinates. Both subtractions are
    // safe: the guard above puts `rows` at three or more, and `width` cannot
    // exceed `cols` because the text was truncated to fit it. Odd slack falls
    // above and to the left, which is what integer division does and is not
    // worth a correction nobody could see.
    let left = (usize::from(size.cols) - width) / 2 + 1;
    let top = (usize::from(size.rows) - 3) / 2 + 1;
    let pad = " ".repeat(PAD);
    let bar = "─".repeat(inner);
    let rows = [
        format!("╭{bar}╮"),
        format!("│{pad}{text}{pad}│"),
        format!("╰{bar}╯"),
    ];

    let mut out = String::from("\x1b[?2026h\x1b7");
    for (i, row) in rows.iter().enumerate() {
        out.push_str(&format!("\x1b[{};{left}H\x1b[0m{row}", top + i));
    }
    out.push_str("\x1b8\x1b[?2026l");
    Some(out.into_bytes())
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

    /// Split what `overlay_bytes` emits into `(row, column)` placements and the
    /// text drawn at each. Every escape run starts with `ESC`, so the pieces
    /// between them are exactly the sequence and whatever followed it.
    fn placed(bytes: &[u8]) -> Vec<((usize, usize), String)> {
        let text = String::from_utf8(bytes.to_vec()).expect("utf-8");
        let mut out = vec![];
        let mut at = None;
        for piece in text.split('\x1b') {
            if let Some(coords) = piece.strip_prefix('[').and_then(|p| p.strip_suffix('H')) {
                let (row, col) = coords.split_once(';').expect("row;col");
                at = Some((
                    row.parse::<usize>().expect("row"),
                    col.parse::<usize>().expect("col"),
                ));
            } else if let Some(drawn) = piece.strip_prefix("[0m") {
                out.push((
                    at.take().expect("a row is placed before it is drawn"),
                    drawn.to_string(),
                ));
            }
        }
        out
    }

    /// Just the text of each row, in order.
    fn rows_of(bytes: &[u8]) -> Vec<String> {
        placed(bytes).into_iter().map(|(_, row)| row).collect()
    }

    /// The name, and only the name. The number belongs on the picker's rows and
    /// in `<prefix> 3`, where it is how you reach a session; here it would be
    /// beside an answer that does not need it.
    #[test]
    fn the_label_is_the_name_and_nothing_else() {
        assert_eq!(label("dotfiles"), "dotfiles");
        assert_eq!(label("api server"), "api server");
        assert_eq!(label("日本語"), "日本語");
    }

    /// `validate_name` guards the creating path, not `<id>.json`, which is a
    /// file a person can write. This is the one place a name reaches a raw
    /// terminal with `OPOST` off, so an escape sequence smuggled into one would
    /// be executed rather than shown.
    #[test]
    fn a_control_character_in_a_name_cannot_reach_the_terminal() {
        let sneaky = label("ok\x1b[31mred\x07\n");
        assert!(!sneaky.contains('\x1b'), "{sneaky:?}");
        assert!(!sneaky.contains('\x07'), "{sneaky:?}");
        assert!(!sneaky.contains('\n'), "{sneaky:?}");
        assert_eq!(sneaky, "ok[31mred");
    }

    /// A name of nothing but control characters is stripped to nothing, and a
    /// box around nothing is worse than no box. Refused at the arming rather
    /// than at the drawing, so the relay carries no state for it either.
    #[test]
    fn a_label_with_nothing_in_it_arms_nothing() {
        let now = Instant::now();
        assert!(Popup::arm(None, now).is_none());
        assert!(Popup::arm(Some(label("\x07\x1b")), now).is_none());
        assert!(Popup::arm(Some(label("dotfiles")), now).is_some());
    }

    /// The cursor and the editor's own attributes have to come back exactly as
    /// they were, and the box must never be presented half-drawn.
    #[test]
    fn the_overlay_saves_and_restores_inside_one_synchronized_frame() {
        let bytes = overlay_bytes("2  dotfiles", size(80, 24)).expect("drawn");
        let text = String::from_utf8(bytes).expect("utf-8");
        assert!(text.starts_with("\x1b[?2026h\x1b7"), "{text:?}");
        assert!(text.ends_with("\x1b8\x1b[?2026l"), "{text:?}");
        assert_eq!(text.matches('\x07').count(), 0);
    }

    /// With `OPOST` off a newline moves the cursor down a column rather than to
    /// the start of the next line, which is the classic way to shear a raw
    /// overlay across the screen.
    #[test]
    fn the_overlay_never_writes_a_newline_or_a_carriage_return() {
        let bytes = overlay_bytes("2  dotfiles", size(80, 24)).expect("drawn");
        assert!(!bytes.contains(&b'\n'), "a newline reached the terminal");
        assert!(
            !bytes.contains(&b'\r'),
            "a carriage return reached the terminal"
        );
    }

    /// The picker sets no colour at all so it inherits the terminal's palette;
    /// a box painted over the editor has even less business choosing one. The
    /// raw-byte counterpart of `test_support::assert_no_colour`.
    #[test]
    fn nothing_in_the_overlay_sets_a_colour() {
        let bytes = overlay_bytes("2  dotfiles", size(80, 24)).expect("drawn");
        let text = String::from_utf8(bytes).expect("utf-8");
        for sgr in text.split("\x1b[").filter_map(|p| p.strip_suffix('m')) {
            assert_eq!(sgr, "0", "the only SGR may be a reset, found {sgr:?}");
        }
    }

    /// Every row is placed absolutely and inside the screen. A box that runs
    /// off the right-hand edge wraps, and a wrap at the bottom scrolls the
    /// editor's screen up by a line — which nvmux cannot undo.
    #[test]
    fn every_row_is_positioned_absolutely_and_fits_the_screen() {
        for &(cols, rows) in &[(80u16, 24u16), (20, 5), (5, 3), (13, 3)] {
            let Some(bytes) = overlay_bytes("2  dotfiles", size(cols, rows)) else {
                continue;
            };
            let text = String::from_utf8(bytes).expect("utf-8");
            let widths: Vec<usize> = rows_of(text.as_bytes()).iter().map(|r| r.width()).collect();
            assert_eq!(widths.len(), 3, "three rows at {cols}x{rows}: {text:?}");
            assert!(
                widths.windows(2).all(|w| w[0] == w[1]),
                "the box is ragged at {cols}x{rows}: {widths:?}"
            );

            let placements: Vec<(usize, usize)> = placed(text.as_bytes())
                .into_iter()
                .map(|(at, _)| at)
                .collect();
            assert_eq!(placements.len(), 3, "one placement per row");
            for (i, (row, col)) in placements.iter().enumerate() {
                assert_eq!(
                    *row,
                    (usize::from(rows) - 3) / 2 + 1 + i,
                    "the box is not vertically centred"
                );
                assert!(*row <= usize::from(rows), "row {row} past {rows}");
                // Centred to within the odd column integer division leaves over.
                let before = col - 1;
                let after = usize::from(cols) - (before + widths[i]);
                assert!(
                    before.abs_diff(after) <= 1,
                    "{cols}x{rows}: {before} columns left, {after} right"
                );
                assert!(
                    col + widths[i] <= usize::from(cols) + 1,
                    "{cols}x{rows}: a row runs off the right-hand edge"
                );
            }
        }
    }

    /// Two columns per character, so a Japanese name is boxed to what it
    /// occupies rather than to how many bytes it takes.
    #[test]
    fn a_wide_name_is_measured_in_columns_not_bytes() {
        let bytes = overlay_bytes("1  日本語", size(40, 10)).expect("drawn");
        let rows = rows_of(&bytes);
        assert_eq!(rows.len(), 3);
        assert!(rows[1].contains("日本語"), "{rows:?}");
        // "1" + two spaces + three double-width characters + a pad each side.
        assert_eq!(rows[1].width(), 3 + 6 + 2 * PAD + 2);
        assert!(
            rows.iter().all(|r| r.width() == rows[0].width()),
            "{rows:?}"
        );
    }

    /// A terminal with no room for the box gets no box, rather than a fragment
    /// of one wrapped across the editor's text.
    #[test]
    fn a_terminal_too_small_for_the_box_gets_nothing() {
        for &(cols, rows) in TINY_SIZES {
            let drawn = overlay_bytes("2  dotfiles", size(cols, rows));
            if let Some(bytes) = drawn {
                let widths: Vec<usize> = rows_of(&bytes).iter().map(|r| r.width()).collect();
                assert!(
                    widths.iter().all(|w| *w <= usize::from(cols)),
                    "{cols}x{rows} drew {widths:?}"
                );
                assert!(rows >= MIN_ROWS, "{cols}x{rows} drew three rows");
            }
        }
    }

    /// The lull clock restarts on every byte the child writes; the other two
    /// must not, or a session with a spinner in its statusline would hold the
    /// box on screen for ever and never reach its own deadline. The same
    /// lesson as the absolute deadline in `crate::pty`.
    #[test]
    fn the_lull_clock_restarts_on_output_but_the_life_does_not() {
        let t0 = Instant::now();
        let mut popup = Popup {
            label: "dotfiles".into(),
            quiet_since: Some(t0),
            give_up_at: t0 + GIVE_UP,
            until: None,
            painted: false,
        };
        let big = size(80, 24);

        // Still chattering: nothing is drawn, however long it goes on.
        for ms in [10, 100, 500] {
            let act = popup.step(t0 + Duration::from_millis(ms), true, big);
            assert_eq!(act, Act::Idle, "drew at {ms}ms while the child was talking");
        }
        // Quiet, but not yet long enough.
        assert_eq!(
            popup.step(t0 + Duration::from_millis(510), false, big),
            Act::Idle
        );
        // Quiet for a whole SETTLE: now it paints, and the clock starts here.
        let painted = t0 + Duration::from_millis(600);
        assert!(matches!(popup.step(painted, false, big), Act::Paint(_)));
        assert_eq!(popup.until, Some(painted + DURATION));

        // A repaint by the editor puts the box back at the *next* lull — one
        // whole SETTLE later, not the first pass after it — without buying the
        // box any more time on screen.
        assert_eq!(
            popup.step(painted + Duration::from_millis(10), true, big),
            Act::Idle
        );
        assert_eq!(
            popup.step(painted + Duration::from_millis(20), false, big),
            Act::Idle,
            "one quiet pass is not a lull"
        );
        assert!(matches!(
            popup.step(painted + Duration::from_millis(60), false, big),
            Act::Paint(_)
        ));
        assert_eq!(popup.until, Some(painted + DURATION));

        // And it ends on time regardless.
        assert_eq!(
            popup.step(painted + DURATION + Duration::from_millis(1), false, big),
            Act::Erase
        );
    }

    /// A screen that has been busy for two solid seconds is one where a box
    /// dropped on top is as likely to be corruption as information.
    #[test]
    fn an_overlay_that_never_finds_a_lull_gives_up_rather_than_drawing_late() {
        let t0 = Instant::now();
        let mut popup = Popup {
            label: "dotfiles".into(),
            quiet_since: Some(t0),
            give_up_at: t0 + GIVE_UP,
            until: None,
            painted: false,
        };
        let big = size(80, 24);
        let mut at = t0;
        while at + Duration::from_millis(10) < t0 + GIVE_UP {
            at += Duration::from_millis(10);
            assert_eq!(popup.step(at, true, big), Act::Idle, "at {at:?}");
        }
        assert_eq!(
            popup.step(t0 + GIVE_UP, true, big),
            Act::Done,
            "it must stop trying rather than draw onto a busy screen"
        );
    }
}
