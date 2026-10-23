//! The brief "you are now on this session" box, drawn over the session's
//! screen after an attach.
//!
//! Not armed any more: a margin note drawn by nvmux's own client says it now
//! (see [`crate::card`]). Kept, with its callers, in case the box is wanted
//! back.
//!
//! While attached nvmux owns no cells ([`crate::pty`] copies the client's
//! bytes through unread), so it writes the box over the screen itself and
//! `repaint` restores what it covered. No editor state is touched.
//!
//! Frames go out only between the session's escape sequences
//! ([`crate::boundary`]), again after every session write and inside the same
//! synchronized update (`Attachment::write_session_frame`), so the terminal
//! never presents the box wiped. No gap within [`GIVE_UP`] means no box.
//!
//! The box cross-fades ([`crate::fade`]) with the session's cells from
//! [`crate::shadow`]. The repaint still runs, as the shadow lacks undercurl
//! and strikethrough; without a shadow the interior is blank until then. The
//! relay cannot sleep between frames, so the popup holds a
//! [`fade::Schedule`] and is asked for one clock-paced frame at a time. With
//! [`fade::enabled`] false there is no fade phase.

use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr;

use crate::fade::{self, Direction, Dissolve, Schedule};
use crate::palette::Rgb;
use crate::pty::PtySize;
use crate::shadow::{Over, Shadow};
use crate::ui::draw::truncate;

/// How long to wait for a gap between the session's sequences. A client with
/// none in 2 s has stopped mid-sequence, and a box drawn then is as likely to
/// corrupt the screen as inform.
const GIVE_UP: Duration = Duration::from_secs(2);

/// How long the box stays up at full strength: long enough to catch the name,
/// short enough not to be in the way. The dissolves add `fade.duration_ms`
/// on top (1.2 s in all at the default).
const DURATION: Duration = Duration::from_secs(1);

/// Columns between the box's border and its text.
const PAD: usize = 1;

/// The smallest screen the box fits: borders, padding and one column of text
/// across, three rows down.
const MIN_COLS: u16 = 5;
const MIN_ROWS: u16 = 3;

/// What the box says: the session's name (not its number), with control
/// characters dropped. [`crate::session::validate_name`] only guards creation
/// and `<id>.json` can be hand-edited; here a name reaches a raw terminal
/// unescaped, where an escape sequence in it would be executed.
pub fn label(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).collect()
}

/// What the relay found on this pass of its loop. A busy session sets both on
/// the same pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pass {
    /// The session wrote, so the box may have been drawn over.
    pub wrote: bool,
    /// The terminal is between the session's escape sequences, outside any
    /// session span or saved cursor, so nvmux may write. See
    /// [`crate::boundary`].
    pub between: bool,
}

/// What the relay must do about the announcement on this pass of its loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    Idle,
    /// Write these bytes over the session's screen.
    Paint(Vec<u8>),
    /// Time is up: take the box off the screen, then drop the popup.
    Erase,
    /// Nothing more will happen; drop the popup.
    Done,
}

/// The box's phase once first painted.
#[derive(Debug)]
enum Phase {
    In(Schedule),
    Held { until: Instant },
    Out(Schedule),
}

/// The box from its first paint on; [`Popup`] has none until then.
#[derive(Debug)]
struct Life {
    phase: Phase,
    /// How dissolved the box is (0 drawn, 1 gone): what the terminal shows, or
    /// will show again on the next pass that can write.
    t: f32,
    /// When the next frame of a dissolve is due. Idle while held.
    next_frame: Instant,
}

impl Life {
    /// Starts the box's clocks, on the pass that first paints it. With no
    /// dissolve the box goes up whole and its hold starts at once.
    fn begin(now: Instant, dissolve: Option<Dissolve>) -> Self {
        let Some(d) = dissolve else {
            return Self {
                phase: Phase::Held {
                    until: now + DURATION,
                },
                t: 0.0,
                next_frame: now,
            };
        };
        let mut life = Self {
            phase: Phase::In(Schedule::start(d.one_way, Direction::In, now)),
            // Replaced by the schedule's first frame before anything is drawn.
            t: 1.0,
            next_frame: now,
        };
        take_frame(&mut life, now);
        life
    }
}

/// An announcement waiting for a moment to draw, then living out its time.
///
/// One per attach, owned by the relay loop. Told the time rather than reading
/// a clock, so every state is reachable from a test. Deadlines are absolute
/// [`Instant`]s: a chatty child keeps `poll` returning early, and a deadline
/// restarted on each wake-up would never fire.
#[derive(Debug)]
pub struct Popup {
    label: String,
    dissolve: Option<Dissolve>,
    /// The box's colour at rest: the theme's ink, or `None` for the terminal's
    /// own foreground (see [`crate::theme`]).
    ink: Option<Rgb>,
    /// When to stop waiting for a moment to draw.
    give_up_at: Instant,
    /// Set on first paint.
    life: Option<Life>,
    /// False when the terminal may not show [`Life::t`]: the session drew over
    /// the box or the dissolve moved on. Repaint at the next gap.
    painted: bool,
}

impl Popup {
    /// `None` when there is nothing to say, including a label [`label`]
    /// stripped to nothing, so the relay then carries no state.
    pub fn arm(label: Option<String>, now: Instant) -> Option<Self> {
        Self::armed(label, now, fade::dissolve(), crate::theme::ink())
    }

    /// [`Popup::arm`] with the effect and ink passed in, for tests: no palette
    /// or config is installed under test, so [`fade::dissolve`] and
    /// [`crate::theme::ink`] are always `None` there.
    fn armed(
        label: Option<String>,
        now: Instant,
        dissolve: Option<Dissolve>,
        ink: Option<Rgb>,
    ) -> Option<Self> {
        let label = label.filter(|l| !l.is_empty())?;
        Some(Self {
            label,
            dissolve,
            ink,
            give_up_at: now + GIVE_UP,
            life: None,
            painted: false,
        })
    }

    /// When the relay must next wake for this popup: the next dissolve frame,
    /// the end of the hold, or the give-up deadline. Gaps need no wake-up: the
    /// pty wakes the relay whenever the session writes.
    pub fn wake_at(&self) -> Instant {
        match &self.life {
            None => self.give_up_at,
            Some(life) => match &life.phase {
                // Always in the future: `frame` advances it on every pass while
                // a schedule runs, so the relay cannot spin on a past deadline.
                Phase::In(_) | Phase::Out(_) => life.next_frame,
                Phase::Held { until } => *until,
            },
        }
    }

    /// One pass of the relay loop. `under` is the session's screen per the
    /// shadow: what the box covers and what a dissolve cross-fades with.
    pub fn step(&mut self, now: Instant, pass: Pass, size: PtySize, under: Option<&Shadow>) -> Act {
        if pass.wrote {
            self.painted = false;
        }

        // The life runs on the clock, so the box comes down on time even on a
        // session that talks over it throughout.
        if self.advance(now) {
            return Act::Erase;
        }

        if self.life.is_none() && !pass.between {
            // Not drawn yet, and no gap since arming.
            return if now >= self.give_up_at {
                Act::Done
            } else {
                Act::Idle
            };
        }

        // Before the gate: the dissolve must advance on passes that cannot
        // paint too, which also keeps `wake_at` in the future.
        let t = self.frame(now);
        if self.painted || !pass.between {
            return Act::Idle;
        }
        let Some(over) = overlay(&self.label, size) else {
            // Too small to draw on, and nothing to erase.
            return Act::Done;
        };
        self.painted = true;
        Act::Paint(self.draw(&over, under, t))
    }

    /// Move the box out of a phase it has outlived, painted or not. Returns
    /// whether the box's life is over.
    fn advance(&mut self, now: Instant) -> bool {
        if !self.outlived(now) {
            return false;
        }
        let dissolve = self.dissolve;
        let Some(life) = self.life.as_mut() else {
            return false;
        };
        match (&life.phase, dissolve) {
            (Phase::In(_), _) => {
                life.phase = Phase::Held {
                    until: now + DURATION,
                };
                false
            }
            // Hold over: start dissolving, first frame due at once.
            (Phase::Held { .. }, Some(d)) => {
                life.phase = Phase::Out(Schedule::start(d.one_way, Direction::Out, now));
                life.next_frame = now;
                false
            }
            // Dissolved, or a hold ending with no fade.
            (Phase::Held { .. }, None) | (Phase::Out(_), _) => true,
        }
    }

    /// Whether the current phase's time is up, spending the rest of its
    /// schedule if so.
    ///
    /// Phases end on the clock ([`Schedule::over`], not
    /// [`Schedule::finished`]): frames only go out in the session's gaps, and
    /// a session with none would otherwise hold the box open for ever.
    /// Spending the schedule lands the box on its exact end value (fully drawn
    /// after a fade in) however few frames went out; an unshown value marks
    /// the box for repaint. A fade out is erased as it ends, so its last shown
    /// frame is merely almost gone.
    fn outlived(&mut self, now: Instant) -> bool {
        let Some(life) = self.life.as_mut() else {
            return false;
        };
        let mut end = None;
        let over = match &mut life.phase {
            Phase::Held { until } => now >= *until,
            Phase::In(schedule) | Phase::Out(schedule) => {
                schedule.over(now) && {
                    while let Some(t) = schedule.next(now) {
                        end = Some(t);
                    }
                    true
                }
            }
        };
        if let Some(t) = end.filter(|t| *t != life.t) {
            life.t = t;
            self.painted = false;
        }
        over
    }

    /// How dissolved to draw the box on this pass, taking the next scheduled
    /// frame if due and starting the life on the first paint. Otherwise the
    /// value stays put, so a repaint after the child drew over the box resumes
    /// the dissolve rather than restarting it.
    fn frame(&mut self, now: Instant) -> f32 {
        let dissolve = self.dissolve;
        let life = self.life.get_or_insert_with(|| Life::begin(now, dissolve));
        let took = now >= life.next_frame && take_frame(life, now).is_some();
        let t = life.t;
        if took {
            // The screen is a frame behind again.
            self.painted = false;
        }
        t
    }

    /// This frame's bytes: cross-faded with the session's screen while
    /// dissolving, the plain box otherwise.
    ///
    /// At rest the plain path is deliberate: the composite at `t == 0` looks
    /// the same but sets explicit colours, where the box at rest belongs in
    /// the terminal's foreground (or the theme's ink) like every nvmux screen.
    /// With `[effects.fade] session = false` there is no shadow to dissolve
    /// into.
    fn draw(&self, over: &Over, under: Option<&Shadow>, t: f32) -> Vec<u8> {
        match (under, self.dissolve) {
            (Some(shadow), Some(d)) if t > 0.0 && shadow.is_usable() => {
                shadow.under(over, &d.palette, d.ink, t)
            }
            _ => plain_bytes(over, self.colour(t)),
        }
    }

    /// Dissolving, the ink partway to the background; otherwise the theme's
    /// ink, or `None` without a theme.
    fn colour(&self, t: f32) -> Option<Rgb> {
        self.dissolve.and_then(|d| d.colour(t)).or(self.ink)
    }
}

/// Take and record the running schedule's next value. `None` while held or
/// once the schedule is spent.
fn take_frame(life: &mut Life, now: Instant) -> Option<f32> {
    let schedule = match &mut life.phase {
        Phase::In(schedule) | Phase::Out(schedule) => schedule,
        Phase::Held { .. } => return None,
    };
    let t = schedule.next(now)?;
    life.t = t;
    life.next_frame = now + fade::FRAME;
    Some(t)
}

/// Where the box goes (centred) and what it says. `None` when the screen
/// cannot hold a one-column box. Shared by [`plain_bytes`] and
/// [`Shadow::under`], which needs the covered cells.
pub fn overlay(label: &str, size: PtySize) -> Option<Over> {
    if size.cols < MIN_COLS || size.rows < MIN_ROWS {
        return None;
    }

    // Sized to the truncated text, which a dropped wide character can leave
    // narrower than its budget.
    let text = truncate(label, usize::from(size.cols) - 2 - 2 * PAD);
    if text.is_empty() {
        return None;
    }
    let inner = text.width() + 2 * PAD;
    let width = inner + 2;

    // 0-based; the renderer adds 1. Both subtractions are safe: `rows >= 3`,
    // and the text was truncated to fit `cols`. Odd slack goes above and left.
    let left = (usize::from(size.cols) - width) / 2;
    let top = (usize::from(size.rows) - 3) / 2;
    let pad = " ".repeat(PAD);
    let bar = "─".repeat(inner);
    Some(Over {
        top: u16::try_from(top).ok()?,
        left: u16::try_from(left).ok()?,
        width: u16::try_from(width).ok()?,
        rows: vec![
            format!("╭{bar}╮"),
            format!("│{pad}{text}{pad}│"),
            format!("╰{bar}╯"),
        ],
    })
}

/// A block of nvmux's own rows written over a session's screen under `sgr`,
/// for this box and the prefix hint bar ([`crate::hint`]). `sgr` is a reset,
/// possibly carrying a foreground (see [`pen`]) or `DIM`. Rows of spaces
/// erase; only [`Shadow::under`] can restore what was there.
///
/// The sequence needs, in order:
///
/// * a synchronized-update span, so the block is never seen half-drawn;
/// * `DECSC`/`DECRC` (`ESC 7` / `ESC 8`) around it all, restoring the cursor
///   and the editor's SGR. The single save slot is safe only because nvmux
///   writes between the child's sequences, never between the child's own
///   save and restore ([`crate::boundary`]);
/// * an SGR reset leading every row, since spaces erase with the current
///   background, which the editor set. `sgr` extras ride on that reset's CSI,
///   as `shadow::write_sgr` does;
/// * an absolute `CUP` per row and no newline or carriage return: `OPOST` is
///   off in raw mode (see [`crate::term`]). A full-width row never scrolls:
///   its pending-wrap flag is cleared by the next `CUP` or the final `DECRC`,
///   which lets the hint bar span the whole last row.
pub(crate) fn placed(over: &Over, sgr: &str) -> Vec<u8> {
    let left = over.left + 1;
    let mut out = String::from("\x1b[?2026h\x1b7");
    for (i, row) in over.rows.iter().enumerate() {
        let line = usize::from(over.top) + i + 1;
        out.push_str(&format!("\x1b[{line};{left}H{sgr}{row}"));
    }
    out.push_str("\x1b8\x1b[?2026l");
    out.into_bytes()
}

/// The box alone in foreground `fg`, or in no colour at all (the box at rest
/// without a theme). Used at rest and whenever there is no session screen to
/// dissolve into (see [`Popup::step`]).
///
/// Only the foreground is ever set: the interior is spaces, which erase with
/// the terminal's own background after [`placed`]'s reset (as `fade::apply`
/// does over a ratatui buffer).
pub fn plain_bytes(over: &Over, fg: Option<Rgb>) -> Vec<u8> {
    placed(over, &pen(fg))
}

/// The pen for nvmux's text over a screen it does not own: a reset (see
/// [`placed`]) with `fg` as a true colour on the same CSI. Also used by
/// [`crate::handoff`].
pub(crate) fn pen(fg: Option<Rgb>) -> String {
    match fg {
        Some(Rgb(r, g, b)) => format!("\x1b[0;38;2;{r};{g};{b}m"),
        None => "\x1b[0m".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{big, palette, screen, size};
    use crate::ui::test_support::TINY_SIZES;

    /// `overlay` then `plain_bytes`.
    fn overlay_bytes(label: &str, size: PtySize, fg: Option<Rgb>) -> Option<Vec<u8>> {
        Some(plain_bytes(&overlay(label, size)?, fg))
    }

    /// No palette is installed under test, so dissolving states need this.
    fn dissolve() -> Dissolve {
        Dissolve {
            one_way: Duration::from_millis(100),
            palette: palette(),
            ink: palette().fg,
        }
    }

    struct Row {
        at: (usize, usize),
        sgr: String,
        text: String,
    }

    /// Split `overlay_bytes` output into rows, cutting at each `ESC`.
    fn placed(bytes: &[u8]) -> Vec<Row> {
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
            } else if let Some(sgr) = piece.strip_prefix("[0") {
                // Every row is drawn from a reset, with or without a colour on
                // the same CSI: `0m`, or `0;38;2;r;g;bm`.
                let (sgr, drawn) = sgr.split_once('m').expect("an SGR terminator");
                out.push(Row {
                    at: at.take().expect("a row is placed before it is drawn"),
                    sgr: format!("0{sgr}"),
                    text: drawn.to_string(),
                });
            }
        }
        out
    }

    fn rows_of(bytes: &[u8]) -> Vec<String> {
        placed(bytes).into_iter().map(|row| row.text).collect()
    }

    /// An idle session: no write, terminal free.
    fn quiet() -> Pass {
        Pass {
            wrote: false,
            between: true,
        }
    }

    /// The ordinary busy case: the session wrote and left the terminal free.
    fn wrote() -> Pass {
        Pass {
            wrote: true,
            between: true,
        }
    }

    /// The session wrote and left the terminal inside a sequence.
    fn mid_sequence() -> Pass {
        Pass {
            wrote: true,
            between: false,
        }
    }

    /// Drive a popup to the end of its life on a silent child, waking when
    /// `wake_at` asks. Returns each paint (time since arming, `t`) and the act
    /// that ended the run.
    fn drive(popup: &mut Popup, from: Instant, size: PtySize) -> (Vec<(Duration, f32)>, Act) {
        let mut painted = vec![];
        let mut now = from;
        for _ in 0..10_000 {
            match popup.step(now, quiet(), size, None) {
                Act::Paint(_) => {
                    painted.push((now - from, popup.life.as_ref().expect("a life").t));
                }
                Act::Idle => {}
                end => return (painted, end),
            }
            // Never stand still, or a `wake_at` already in the past would spin.
            now = popup.wake_at().max(now + Duration::from_millis(1));
        }
        panic!("the popup never ended");
    }

    fn held_until(popup: &Popup) -> Option<Instant> {
        match popup.life.as_ref()?.phase {
            Phase::Held { until } => Some(until),
            _ => None,
        }
    }

    #[test]
    fn the_label_is_the_name_and_nothing_else() {
        assert_eq!(label("dotfiles"), "dotfiles");
        assert_eq!(label("api server"), "api server");
        assert_eq!(label("日本語"), "日本語");
    }

    #[test]
    fn a_control_character_in_a_name_cannot_reach_the_terminal() {
        let sneaky = label("ok\x1b[31mred\x07\n");
        assert!(!sneaky.contains('\x1b'), "{sneaky:?}");
        assert!(!sneaky.contains('\x07'), "{sneaky:?}");
        assert!(!sneaky.contains('\n'), "{sneaky:?}");
        assert_eq!(sneaky, "ok[31mred");
    }

    #[test]
    fn a_label_with_nothing_in_it_arms_nothing() {
        let now = Instant::now();
        assert!(Popup::arm(None, now).is_none());
        assert!(Popup::arm(Some(label("\x07\x1b")), now).is_none());
        assert!(Popup::arm(Some(label("dotfiles")), now).is_some());
    }

    #[test]
    fn the_overlay_saves_and_restores_inside_one_synchronized_frame() {
        for fg in [None, Some(Rgb(1, 2, 3))] {
            let bytes = overlay_bytes("2  dotfiles", big(), fg).expect("drawn");
            let text = String::from_utf8(bytes).expect("utf-8");
            assert!(text.starts_with("\x1b[?2026h\x1b7"), "{text:?}");
            assert!(text.ends_with("\x1b8\x1b[?2026l"), "{text:?}");
            assert_eq!(text.matches('\x07').count(), 0);
        }
    }

    /// With `OPOST` off a newline moves the cursor straight down, not to the
    /// next line's start, shearing a raw overlay.
    #[test]
    fn the_overlay_never_writes_a_newline_or_a_carriage_return() {
        for fg in [None, Some(Rgb(1, 2, 3))] {
            let bytes = overlay_bytes("2  dotfiles", big(), fg).expect("drawn");
            assert!(!bytes.contains(&b'\n'), "a newline reached the terminal");
            assert!(
                !bytes.contains(&b'\r'),
                "a carriage return reached the terminal"
            );
        }
    }

    /// The raw-byte counterpart of `test_support::assert_no_colour`: with no
    /// theme only a running dissolve colours the box.
    #[test]
    fn nothing_in_the_overlay_sets_a_colour() {
        let bytes = overlay_bytes("2  dotfiles", big(), None).expect("drawn");
        let text = String::from_utf8(bytes).expect("utf-8");
        for sgr in text.split("\x1b[").filter_map(|p| p.strip_suffix('m')) {
            assert_eq!(sgr, "0", "the only SGR may be a reset, found {sgr:?}");
        }
    }

    /// The colour rides on the reset's CSI, as `shadow::write_sgr` emits it.
    /// The background is never set: the interior erases with the terminal's
    /// own, which the glyphs dissolve into.
    #[test]
    fn a_dissolving_row_sets_one_truecolor_foreground_from_a_reset() {
        let bytes = overlay_bytes("2  dotfiles", big(), Some(Rgb(100, 110, 120))).expect("drawn");
        let rows = placed(&bytes);
        assert_eq!(rows.len(), 3);
        for row in &rows {
            assert_eq!(row.sgr, "0;38;2;100;110;120", "at {:?}", row.at);
        }
        let text = String::from_utf8(bytes).expect("utf-8");
        assert!(
            !text.contains("48;2;"),
            "the box set a background: {text:?}"
        );
    }

    /// At rest the box sets no colour (byte-identical to the unfaded box);
    /// fully dissolved it is the terminal's background.
    #[test]
    fn a_dissolve_runs_from_no_colour_at_all_to_the_background() {
        let d = dissolve();
        assert_eq!(d.colour(0.0), None, "the box at rest picks no colour");
        assert_eq!(d.colour(1.0), Some(d.palette.bg));
        assert_eq!(d.colour(0.5), Some(Rgb(100, 100, 100)));

        let at_rest = overlay_bytes("2  dotfiles", big(), d.colour(0.0)).expect("drawn");
        let unfaded = overlay_bytes("2  dotfiles", big(), None).expect("drawn");
        assert_eq!(at_rest, unfaded);
    }

    /// A row past the right edge wraps, and a wrap at the bottom scrolls the
    /// editor's screen, which nvmux cannot undo.
    #[test]
    fn every_row_is_positioned_absolutely_and_fits_the_screen() {
        for &(cols, rows) in &[(80u16, 24u16), (20, 5), (5, 3), (13, 3)] {
            let Some(bytes) = overlay_bytes("2  dotfiles", size(cols, rows), None) else {
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
                .map(|row| row.at)
                .collect();
            assert_eq!(placements.len(), 3, "one placement per row");
            for (i, (row, col)) in placements.iter().enumerate() {
                assert_eq!(
                    *row,
                    (usize::from(rows) - 3) / 2 + 1 + i,
                    "the box is not vertically centred"
                );
                assert!(*row <= usize::from(rows), "row {row} past {rows}");
                // Centred to within one column.
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

    #[test]
    fn a_wide_name_is_measured_in_columns_not_bytes() {
        let bytes = overlay_bytes("1  日本語", size(40, 10), None).expect("drawn");
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

    #[test]
    fn a_terminal_too_small_for_the_box_gets_nothing() {
        for &(cols, rows) in TINY_SIZES {
            let drawn = overlay_bytes("2  dotfiles", size(cols, rows), None);
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

    /// The first frame shows the session's own cells; the label appears only
    /// once the box is the more visible layer.
    #[test]
    fn the_box_arrives_out_of_the_screen_it_is_covering() {
        let t0 = Instant::now();
        let screen = screen(24, 80);
        let mut popup =
            Popup::armed(Some("dotfiles".into()), t0, Some(dissolve()), None).expect("armed");

        let mut now = t0;
        let mut frames = vec![];
        for _ in 0..64 {
            if let Act::Paint(bytes) = popup.step(now, quiet(), big(), Some(&screen)) {
                frames.push(String::from_utf8(bytes).expect("utf-8"));
                if held_until(&popup).is_some() {
                    break;
                }
            }
            now = popup.wake_at().max(now + Duration::from_millis(1));
        }

        let first = frames.first().expect("a first frame");
        assert!(
            first.contains('x') && !first.contains("dotfiles"),
            "the arrival did not start from the screen: {first:?}"
        );
        assert!(
            frames.iter().any(|f| f.contains("dotfiles")),
            "the box never arrived"
        );
        let last = frames.last().expect("a last frame");
        assert!(
            last.contains("dotfiles") && !last.contains('x'),
            "the box did not end up covering the screen: {last:?}"
        );
    }

    /// The last frame is the session's cells at nearly full colour, so the
    /// screen is back before the repaint is even requested.
    #[test]
    fn the_box_leaves_back_into_the_screen_it_was_covering() {
        let t0 = Instant::now();
        let screen = screen(24, 80);
        let mut popup =
            Popup::armed(Some("dotfiles".into()), t0, Some(dissolve()), None).expect("armed");

        let mut now = t0;
        let mut last = String::new();
        for _ in 0..10_000 {
            match popup.step(now, quiet(), big(), Some(&screen)) {
                Act::Paint(bytes) => last = String::from_utf8(bytes).expect("utf-8"),
                Act::Idle => {}
                end => {
                    assert_eq!(end, Act::Erase);
                    break;
                }
            }
            now = popup.wake_at().max(now + Duration::from_millis(1));
        }
        assert!(
            last.contains('x') && !last.contains("dotfiles"),
            "the box did not dissolve back into the screen: {last:?}"
        );
    }

    /// No frame may leave a synchronized update or cursor save open:
    /// [`crate::boundary`] tracks nvmux's writes too, so an unbalanced frame
    /// would shut the box out for good. Covers both renderers.
    #[test]
    fn every_frame_of_the_box_leaves_the_terminal_where_it_found_it() {
        let t0 = Instant::now();
        let screen = screen(24, 80);
        let mut popup =
            Popup::armed(Some("dotfiles".into()), t0, Some(dissolve()), None).expect("armed");
        let mut now = t0;
        let mut frames = 0;
        for _ in 0..10_000 {
            match popup.step(now, quiet(), big(), Some(&screen)) {
                Act::Paint(bytes) => {
                    let mut boundary = crate::boundary::Boundary::new();
                    boundary.saw_own(&bytes);
                    assert!(
                        boundary.between_sequences(),
                        "frame {frames} left the terminal somewhere: {:?}",
                        String::from_utf8_lossy(&bytes)
                    );
                    frames += 1;
                }
                Act::Idle => {}
                _ => break,
            }
            now = popup.wake_at().max(now + Duration::from_millis(1));
        }
        assert!(frames > 8, "only {frames} frames were looked at");
    }

    /// Without a shadow (`[effects.fade] session = false`) the plain box is
    /// drawn, in the dissolve's colour.
    #[test]
    fn with_no_shadow_the_box_dissolves_as_it_did_before() {
        let t0 = Instant::now();
        let d = dissolve();
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, Some(d), None).expect("armed");
        let Act::Paint(bytes) = popup.step(t0, quiet(), big(), None) else {
            panic!("the box did not go up");
        };
        let t = popup.life.as_ref().expect("a life").t;
        let over = overlay("dotfiles", big()).expect("a box");
        assert_eq!(bytes, plain_bytes(&over, d.colour(t)));
    }

    #[test]
    fn the_box_at_rest_is_drawn_plain_even_with_a_screen_to_hand() {
        let t0 = Instant::now();
        let screen = screen(24, 80);
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, None, None).expect("armed");
        let Act::Paint(bytes) = popup.step(t0, quiet(), big(), Some(&screen)) else {
            panic!("the box did not go up");
        };
        let over = overlay("dotfiles", big()).expect("a box");
        assert_eq!(bytes, plain_bytes(&over, None));
    }

    #[test]
    fn the_box_is_centred_and_sized_to_its_text() {
        let over = overlay("dotfiles", size(80, 24)).expect("a box");
        assert_eq!(over.rows.len(), 3);
        // "dotfiles" plus a pad each side and a border each side.
        assert_eq!(over.width, 8 + 2 * PAD as u16 + 2);
        assert_eq!(over.top, (24 - 3) / 2);
        assert_eq!(over.left, (80 - over.width) / 2);
        assert!(over
            .rows
            .iter()
            .all(|r| r.width() == usize::from(over.width)));
    }

    /// A window repainting at 60 fps draws over the box every frame; the box
    /// must be back on the next pass without gaining time on screen. A 25 ms
    /// quiet-period wait never showed it at all at 60 fps.
    #[test]
    fn a_box_drawn_over_is_back_up_on_the_next_pass() {
        let t0 = Instant::now();
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, None, None).expect("armed");
        assert!(matches!(
            popup.step(t0, wrote(), big(), None),
            Act::Paint(_)
        ));
        assert_eq!(held_until(&popup), Some(t0 + DURATION));

        let mut at = t0;
        for _ in 0..40 {
            at += Duration::from_millis(16);
            assert!(
                matches!(popup.step(at, wrote(), big(), None), Act::Paint(_)),
                "the box stayed down after the session drew over it, at {:?}",
                at - t0
            );
            assert_eq!(
                held_until(&popup),
                Some(t0 + DURATION),
                "a repaint bought the box more time on screen"
            );
        }

        // And it still ends on time.
        assert_eq!(
            popup.step(
                t0 + DURATION + Duration::from_millis(1),
                wrote(),
                big(),
                None
            ),
            Act::Erase
        );
    }

    #[test]
    fn a_terminal_never_left_between_sequences_gets_no_box() {
        let t0 = Instant::now();
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, None, None).expect("armed");
        let mut at = t0;
        while at + Duration::from_millis(10) < t0 + GIVE_UP {
            at += Duration::from_millis(10);
            assert_eq!(
                popup.step(at, mid_sequence(), big(), None),
                Act::Idle,
                "at {at:?}"
            );
        }
        assert_eq!(
            popup.step(t0 + GIVE_UP, mid_sequence(), big(), None),
            Act::Done,
            "it must stop trying rather than write inside a sequence"
        );
    }

    /// The dissolve advances even when nothing can be painted, keeping
    /// [`Popup::wake_at`] in the future; a past deadline would spin the relay
    /// on a `poll` that returns at once.
    #[test]
    fn a_box_that_can_never_be_painted_does_not_spin_the_relay() {
        let t0 = Instant::now();
        let mut popup =
            Popup::armed(Some("dotfiles".into()), t0, Some(dissolve()), None).expect("armed");
        assert!(matches!(
            popup.step(t0, quiet(), big(), None),
            Act::Paint(_)
        ));

        let life = dissolve().one_way * 2 + DURATION;
        let mut now = t0;
        let mut passes = 0;
        while popup.step(now, mid_sequence(), big(), None) != Act::Erase {
            passes += 1;
            assert!(
                passes < 200,
                "woken {passes} times in {:?}: the relay is spinning",
                now - t0
            );
            now = popup.wake_at().max(now);
        }
        let lived = now - t0;
        assert!(
            lived + Duration::from_millis(50) >= life,
            "it came down after {lived:?}, short of {life:?}"
        );
    }

    /// What `NO_COLOR` and `[effects.fade] enabled = false` get: one paint, the
    /// hold, and the erase.
    #[test]
    fn with_no_effect_the_box_goes_up_whole_and_comes_down_whole() {
        let t0 = Instant::now();
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, None, None).expect("armed");
        let (painted, end) = drive(&mut popup, t0, big());

        assert_eq!(painted.len(), 1, "more than one paint: {painted:?}");
        assert_eq!(painted[0].1, 0.0, "the box went up part-drawn");
        assert_eq!(
            painted[0].0,
            Duration::ZERO,
            "it waited for something, with the terminal free from the start"
        );
        assert_eq!(end, Act::Erase);
    }

    #[test]
    fn the_box_dissolves_in_then_holds_then_dissolves_out() {
        let t0 = Instant::now();
        let d = dissolve();
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, Some(d), None).expect("armed");
        let (painted, end) = drive(&mut popup, t0, big());
        assert_eq!(end, Act::Erase);

        let ts: Vec<f32> = painted.iter().map(|(_, t)| *t).collect();
        // The fade in ends exactly at 0 and the fade out rises from there; the
        // turn is the one frame at 0.
        let turn = ts.iter().position(|t| *t == 0.0).expect("a frame at rest");
        let (into, out_of) = ts.split_at(turn + 1);
        assert!(into.len() >= 4 && out_of.len() >= 4, "{ts:?}");
        assert!(
            into[0] < 1.0,
            "the first frame is the box already dissolved"
        );
        assert!(into[0] > 0.0, "the box went up whole");
        assert!(into.windows(2).all(|w| w[1] < w[0]), "not falling: {ts:?}");
        assert!(out_of.windows(2).all(|w| w[1] > w[0]), "not rising: {ts:?}");
        // The fade out is erased as it ends: the last frame shown is nearly
        // gone, and the schedule is still spent to exactly 1.
        assert!(*out_of.last().expect("a last frame") > 0.9, "{ts:?}");
        assert_eq!(popup.life.as_ref().expect("a life").t, 1.0);

        // The hold lasts `DURATION`; each dissolve `one_way`.
        let held = painted[turn + 1].0 - painted[turn].0;
        assert!(
            held.abs_diff(DURATION) < Duration::from_millis(20),
            "held for {held:?}"
        );
        let fading_in = painted[turn].0;
        assert!(
            fading_in.abs_diff(d.one_way) < Duration::from_millis(20),
            "faded in over {fading_in:?}"
        );
        let fading_out = painted.last().expect("a last frame").0 - painted[turn + 1].0;
        assert!(
            fading_out < d.one_way,
            "faded out over more than its length: {fading_out:?}"
        );
    }

    /// Exactly one paint is the unfaded box: the hold. A dissolve ending
    /// anywhere else would leave a coloured box up for a second.
    #[test]
    fn the_box_it_holds_is_the_box_it_would_have_drawn_unfaded() {
        let t0 = Instant::now();
        let mut popup =
            Popup::armed(Some("dotfiles".into()), t0, Some(dissolve()), None).expect("armed");
        let unfaded = overlay_bytes("dotfiles", big(), None).expect("drawn");

        let mut now = t0;
        let mut at_rest = 0;
        for _ in 0..10_000 {
            match popup.step(now, quiet(), big(), None) {
                Act::Paint(bytes) => {
                    if bytes == unfaded {
                        at_rest += 1;
                        assert!(held_until(&popup).is_some(), "an uncoloured box mid-fade");
                    }
                }
                Act::Idle => {}
                _ => break,
            }
            now = popup.wake_at().max(now + Duration::from_millis(1));
        }
        assert_eq!(at_rest, 1, "the box at rest was painted {at_rest} times");
    }

    /// Under a theme the box at rest is in its ink, with or without an effect,
    /// and each dissolve frame is that ink on its way to the background, so
    /// coming to rest never jumps.
    #[test]
    fn a_themed_box_is_in_its_ink_and_dissolves_from_it() {
        let ink = Rgb(0x7a, 0xa2, 0xf7);
        let t0 = Instant::now();
        let at_rest = overlay_bytes("dotfiles", big(), Some(ink)).expect("drawn");

        let mut still = Popup::armed(Some("dotfiles".into()), t0, None, Some(ink)).expect("armed");
        assert_eq!(
            still.step(t0, quiet(), big(), None),
            Act::Paint(at_rest.clone()),
            "with no effect, the box goes up whole in its ink"
        );

        let d = Dissolve { ink, ..dissolve() };
        let mut popup =
            Popup::armed(Some("dotfiles".into()), t0, Some(d), Some(ink)).expect("armed");
        let mut now = t0;
        let mut rested = 0;
        for _ in 0..10_000 {
            match popup.step(now, quiet(), big(), None) {
                Act::Paint(bytes) => {
                    let t = popup.life.as_ref().expect("a life").t;
                    if t == 0.0 {
                        assert_eq!(bytes, at_rest, "at rest, not in its ink");
                        rested += 1;
                        continue;
                    }
                    let Rgb(r, g, b) = ink.lerp(d.palette.bg, t);
                    for row in placed(&bytes) {
                        assert_eq!(row.sgr, format!("0;38;2;{r};{g};{b}"), "t = {t}");
                    }
                }
                Act::Idle => {}
                _ => break,
            }
            now = popup.wake_at().max(now + Duration::from_millis(1));
        }
        assert_eq!(rested, 1, "the box at rest was painted {rested} times");
    }

    #[test]
    fn the_pen_is_one_csi_from_a_reset() {
        assert_eq!(pen(None), "\x1b[0m");
        assert_eq!(pen(Some(Rgb(1, 2, 3))), "\x1b[0;38;2;1;2;3m");
    }

    /// `painted` only marks the screen stale; the clock alone moves the
    /// dissolve.
    #[test]
    fn a_repaint_mid_dissolve_resumes_where_it_had_got_to() {
        let t0 = Instant::now();
        let mut popup =
            Popup::armed(Some("dotfiles".into()), t0, Some(dissolve()), None).expect("armed");
        let mut now = t0;
        assert!(matches!(
            popup.step(now, quiet(), big(), None),
            Act::Paint(_)
        ));
        let first = popup.life.as_ref().expect("a life").t;

        // The child draws over it and stays mid-sequence: no repaint yet.
        now += Duration::from_millis(10);
        assert_eq!(popup.step(now, mid_sequence(), big(), None), Act::Idle);
        assert_eq!(
            popup.life.as_ref().expect("a life").t,
            first,
            "the dissolve moved while nothing was drawn"
        );

        // The box goes back on wherever the clock has the dissolve.
        now += fade::FRAME;
        assert!(matches!(
            popup.step(now, wrote(), big(), None),
            Act::Paint(_)
        ));
        let resumed = popup.life.as_ref().expect("a life").t;
        assert!(resumed < first, "{first} -> {resumed}");
        assert!(resumed > 0.0, "it jumped straight to the end");
    }

    #[test]
    fn a_box_talked_over_for_its_whole_life_still_comes_down_on_time() {
        let t0 = Instant::now();
        let mut popup =
            Popup::armed(Some("dotfiles".into()), t0, Some(dissolve()), None).expect("armed");
        let mut now = t0;
        assert!(matches!(
            popup.step(now, quiet(), big(), None),
            Act::Paint(_)
        ));

        // From here nothing can be painted.
        let life = dissolve().one_way * 2 + DURATION;
        let end = now + life + Duration::from_millis(100);
        while now < end {
            now += Duration::from_millis(10);
            if popup.step(now, mid_sequence(), big(), None) == Act::Erase {
                let lived = now - t0;
                assert!(
                    lived.abs_diff(life) < Duration::from_millis(50),
                    "lived {lived:?}"
                );
                return;
            }
        }
        panic!("the box never came down");
    }

    /// Nothing to erase either: the client repaints at the size it was just
    /// told.
    #[test]
    fn a_screen_too_small_mid_dissolve_stops_rather_than_drawing_a_fragment() {
        let t0 = Instant::now();
        let mut popup =
            Popup::armed(Some("dotfiles".into()), t0, Some(dissolve()), None).expect("armed");
        let mut now = t0;
        assert!(matches!(
            popup.step(now, quiet(), big(), None),
            Act::Paint(_)
        ));
        now += fade::FRAME;
        assert_eq!(popup.step(now, quiet(), size(2, 1), None), Act::Done);
    }
}
