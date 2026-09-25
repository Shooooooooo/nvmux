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
//! # Between the session's sequences
//!
//! The box goes up on the first pass of the relay loop on which the terminal
//! is between the session's escape sequences, and goes back up on every pass
//! after one where the session wrote — which is what stops the editor's next
//! frame taking it off the screen for good. [`crate::boundary`] is what
//! answers that question and what makes the answer true often enough to be
//! worth asking.
//!
//! It used to be a clock: 25 ms of the child saying nothing, on the reasoning
//! that a write cannot land inside a sequence that nobody is writing. The test
//! was sound and the question was the wrong one. A session with a window
//! repainting at sixty frames a second never goes quiet for 25 ms — so on a
//! busy screen the box blinked at whatever rate the lulls came, and on a
//! genuinely busy one it never appeared at all. Reproduced, before this was
//! changed, by running a starfield at full tilt in a `:terminal` buffer and
//! switching into the session: the box went up and was wiped eleven times in
//! its one second, and at sixty frames a second nvmux gave up on it entirely.
//!
//! Putting it back is only half of it, and the half that is not enough on its
//! own. A terminal parses asynchronously and presents on its own clock, so
//! the state it puts on the glass is whatever prefix of the stream it has
//! reached — and against a window repainting at full speed most of those fall
//! in the middle of a repaint, with the box wiped and the rewrite not yet
//! read. Measured on the flight above with the box merely rewritten after
//! every frame: the whole of it stood in about one presented moment in ten,
//! which is a box that blinks rather than one that is up.
//!
//! So the relay takes the choice away. It wraps each write of the session's
//! and the frame of the notice that follows it in one synchronized update —
//! `Attachment::write_session_frame` in [`crate::pty`] — and a terminal
//! presents nothing inside one of those. The only states it can show are the
//! ones nvmux ends a span on, and nvmux ends every one of them with the box.
//!
//! If no such moment arrives within [`GIVE_UP`], nothing is shown — which now
//! means a client that has stopped halfway through a sequence and never
//! finished it, rather than merely a busy one.
//!
//! Nothing here is ever worth delaying or failing an attach for. A terminal too
//! small for the box, a session that never lets go of the terminal: both are
//! silence.
//!
//! # The dissolve
//!
//! The box does not snap on and off. It fades, like every other transition
//! nvmux makes ([`crate::fade`]) — and it fades into the editor rather than
//! into a hole. What it dissolves out of, and back into, is the session's own
//! screen: [`crate::shadow`] has been keeping that all along so a session can
//! be dissolved, and it holds the very cells the box is covering. So a frame
//! of the dissolve paints both layers at once, each cell showing whichever of
//! the two is the more visible — the box's border and name rising as the
//! editor's text under them dims away, and sinking back as it returns.
//!
//! The rectangle is therefore never a hole for anything to fill. The repaint
//! that has always followed the notice still runs, because the shadow carries
//! colours and not decoration and an italic would come back plain, but it is
//! correction now rather than restoration — and where it cannot be served at
//! all, which is a server busy or at a prompt, or a terminal whose client
//! ignores the resize nudge, the screen has already been put back.
//!
//! What pays for that is `[fade] session`: no shadow, no cells to dissolve
//! into, and the box then does what it always did — an interior of spaces,
//! a blank rectangle, and the repaint to fill it.
//!
//! The mechanism is the one difference from the rest of the fade. Every other
//! screen dissolves inside a loop that sleeps between frames, and this one
//! cannot: it lives in the relay, which must never stop passing bytes. So the
//! popup holds a [`fade::Schedule`] and is asked for one frame at a time by
//! that loop, at whatever moments the loop can spare — which is what [`Popup`]
//! being told the time rather than reading a clock was always for. A frame is
//! still a write to a screen nvmux does not own, so it still waits for a gap
//! between the session's sequences, and a session that never leaves one gets
//! fewer frames: the schedule is paced by the clock, so the fade lands on its
//! end value on time whatever the loop managed.
//!
//! Every gate is [`fade::enabled`], and with the fade off — `NO_COLOR`, the
//! config, or a terminal that never said what its colours are — there is no
//! fade-in phase and no fade-out phase, and the bytes and the timing are
//! exactly what they were before the effect existed.

use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr;

use crate::fade::{self, Direction, Dissolve, Schedule};
use crate::palette::Rgb;
use crate::pty::PtySize;
use crate::shadow::{Over, Shadow};
use crate::ui::draw::truncate;

/// How long to wait for a gap between the session's sequences before giving up
/// on the announcement.
///
/// A client that has not left one in two solid seconds has stopped in the
/// middle of writing something and never finished, which is a screen where a
/// box dropped on top is as likely to be corruption as information.
const GIVE_UP: Duration = Duration::from_secs(2);

/// How long the box stays up at full strength.
///
/// A second: long enough to catch a name out of the corner of the eye, short
/// enough that a box in the middle of the screen — squarely over the text you
/// have just switched to — is gone before it is in the way of anything. Not a
/// setting, because there is no second answer worth the config key: shorter and
/// it is a flicker, longer and it is something you wait out on every switch.
///
/// The dissolves are on top of this rather than carved out of it: the box
/// dissolves in and out for `fade.duration_ms` between them, and a second of
/// the box fully drawn is the second this constant is arguing for. At the
/// default that is 1.2s in all.
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

/// What the relay found on this pass of its loop, as far as the announcement
/// is concerned.
///
/// Two questions rather than one, because they are two: whether the box may
/// have been drawn over, and whether there is anywhere to put it back. A busy
/// session answers yes to both on the same pass, which is exactly the case the
/// box used to lose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pass {
    /// The session wrote to the terminal, so whatever nvmux had on screen may
    /// have been drawn over.
    pub wrote: bool,
    /// The terminal is between the session's escape sequences and none of its
    /// own is still open, so a write of nvmux's cannot land inside one. See
    /// [`crate::boundary`].
    pub between: bool,
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

/// Where the box is in its life, once it has been painted at all.
#[derive(Debug)]
enum Phase {
    /// Rising out of the background.
    In(Schedule),
    /// Fully drawn, until this moment.
    Held { until: Instant },
    /// Sinking back into it.
    Out(Schedule),
}

/// The box from its first paint onwards. Absent until then, which is what says
/// the popup has still found nowhere to put itself.
#[derive(Debug)]
struct Life {
    phase: Phase,
    /// How dissolved the box is: what the terminal shows, or — when the child
    /// has drawn over it — what it will be shown again on the next pass that
    /// can write.
    t: f32,
    /// When the next frame of a dissolve is due. Idle while held.
    next_frame: Instant,
}

impl Life {
    /// The box going up, on the pass that first paints it — which is where
    /// every clock it lives by starts.
    ///
    /// With no effect to dissolve with there is no fade in at all: the box goes
    /// up whole and begins its second there, which is what it did before it
    /// could dissolve.
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
            // Fully dissolved, until the schedule's first frame says otherwise
            // — which it does before anything is drawn.
            t: 1.0,
            next_frame: now,
        };
        take_frame(&mut life, now);
        life
    }
}

/// An announcement waiting for its moment, and then living out its time.
///
/// Owned by the relay loop, one per attach. Told the time rather than reading a
/// clock, like [`crate::keys::Prefix`] is told its bytes, so every state it can
/// reach is reachable from a test.
///
/// The deadlines are absolute [`Instant`]s for the reason written down in
/// [`crate::pty`]: a child that keeps producing output keeps `poll` returning
/// early, and a deadline that restarted on every wake-up would never fire. A
/// session with a spinner in its statusline would otherwise hold the box on
/// screen for ever.
#[derive(Debug)]
pub struct Popup {
    label: String,
    /// How the box dissolves, or `None` when it does not.
    dissolve: Option<Dissolve>,
    /// When to stop waiting for a moment to draw in that is not coming.
    give_up_at: Instant,
    /// Set when the box is first painted: everything that happens to it after.
    life: Option<Life>,
    /// False whenever the terminal is not showing what [`Life::t`] says it is —
    /// because the session may have drawn over the box, or because the dissolve
    /// has moved on since the last frame got through. Either way the cue is the
    /// same: paint it again as soon as there is a gap to paint it in.
    painted: bool,
}

impl Popup {
    /// `None` when there is nothing to say — including a label with nothing
    /// left in it, which is what a name of nothing but control characters comes
    /// to once [`label`] has been through it. The relay then carries no state at
    /// all rather than a popup that would decline to draw itself later.
    pub fn arm(label: Option<String>, now: Instant) -> Option<Self> {
        Self::armed(label, now, fade::dissolve())
    }

    /// [`Popup::arm`] with the effect handed in rather than read, which is how
    /// the tests reach the dissolving states: no palette is ever installed
    /// under test, so [`fade::dissolve`] is always `None` there.
    fn armed(label: Option<String>, now: Instant, dissolve: Option<Dissolve>) -> Option<Self> {
        let label = label.filter(|l| !l.is_empty())?;
        Some(Self {
            label,
            dissolve,
            give_up_at: now + GIVE_UP,
            life: None,
            painted: false,
        })
    }

    /// When the relay must next wake up on this popup's account.
    ///
    /// Nothing here waits for a gap in the session's output, because nothing
    /// here can bring one about: the relay is woken by the pty the moment the
    /// session writes, and the pass that follows that write is the pass that
    /// paints. So a popup asks only for its own clock — the next frame of a
    /// dissolve, the end of the hold, or the deadline for a box that has never
    /// found anywhere to go.
    pub fn wake_at(&self) -> Instant {
        match &self.life {
            None => self.give_up_at,
            Some(life) => match &life.phase {
                // Always in the future: `frame` moves it on every pass a
                // schedule is running, painted or not, so this cannot be a
                // deadline already past that the relay spins on.
                Phase::In(_) | Phase::Out(_) => life.next_frame,
                Phase::Held { until } => *until,
            },
        }
    }

    /// One pass of the relay loop. `pass` is what the relay found — whether
    /// the session wrote, and whether nvmux may write now — and `under` is the
    /// session's screen as the shadow has it, which is what the box is
    /// covering and what a dissolving frame cross-fades with.
    pub fn step(&mut self, now: Instant, pass: Pass, size: PtySize, under: Option<&Shadow>) -> Act {
        if pass.wrote {
            // Whatever was on screen may have been drawn over.
            self.painted = false;
        }

        // The life runs on the clock, ahead of anywhere to draw: a dissolve
        // whose schedule is spent moves on whether or not the screen ever
        // caught up with it, and the box comes down on time on a session that
        // has been talking over it throughout.
        if self.advance(now) {
            return Act::Erase;
        }

        if self.life.is_none() && !pass.between {
            // Nothing has been drawn yet and the session has not let go of the
            // terminal since the box was armed: let it be.
            return if now >= self.give_up_at {
                Act::Done
            } else {
                Act::Idle
            };
        }

        // Before the gate below, not after it: the dissolve is paced by the
        // clock, so it must move on a pass that cannot paint as surely as on
        // one that can — and it is what keeps `wake_at` in the future.
        let t = self.frame(now);
        if self.painted || !pass.between {
            return Act::Idle;
        }
        let Some(over) = overlay(&self.label, size) else {
            // Too small to draw on. Saying nothing is the right answer, and
            // there is nothing to erase either.
            return Act::Done;
        };
        self.painted = true;
        Act::Paint(self.draw(&over, under, t))
    }

    /// Move the box out of a phase it has outlived, whether or not a frame of
    /// it was ever drawn. Says whether that was the end of the box — the one
    /// transition the relay has to do anything about.
    fn advance(&mut self, now: Instant) -> bool {
        if !self.outlived(now) {
            return false;
        }
        let dissolve = self.dissolve;
        let Some(life) = self.life.as_mut() else {
            return false;
        };
        match (&life.phase, dissolve) {
            // Up, and standing there for its second.
            (Phase::In(_), _) => {
                life.phase = Phase::Held {
                    until: now + DURATION,
                };
                false
            }
            // The second is up: start dissolving, with the first frame due at
            // once — it is what replaces the box that has been sitting there.
            (Phase::Held { .. }, Some(d)) => {
                life.phase = Phase::Out(Schedule::start(d.one_way, Direction::Out, now));
                life.next_frame = now;
                false
            }
            // Dissolved; or, with no fade, a hold ending is the whole of the
            // box's end, as it was before the effect existed.
            (Phase::Held { .. }, None) | (Phase::Out(_), _) => true,
        }
    }

    /// Whether the current phase's time is up, spending whatever is left of its
    /// schedule if it is.
    ///
    /// A phase ends on the clock — [`Schedule::over`] rather than
    /// [`Schedule::finished`] — because frames are only drawn where the
    /// session leaves a gap, and one that never left one would otherwise hold
    /// the box open for ever. The box came down on time before it dissolved at
    /// all, and it has to still: the deadline is absolute, for the reason
    /// written down in [`crate::pty`].
    ///
    /// Spending the rest of the schedule rather than discarding it is what
    /// lands the box on exactly the value it was going to land on — fully
    /// drawn, at the end of a fade in — however few of its frames the relay
    /// found a moment to write. A value the screen has not been shown leaves
    /// the box due a repaint, like any other.
    ///
    /// Which is why only one end of the life is exact on the terminal. A fade
    /// in has to be: the box then sits at that value for a whole second, and
    /// one stuck a frame short of drawn is a box drawn wrong. A fade out is
    /// erased the moment it ends, so its last frame is whatever the last gap
    /// allowed — a box already all but gone — and the pass that would have
    /// painted the rest of the way is spent on the erase instead.
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

    /// How dissolved to draw the box on this pass, taking the next frame of a
    /// running schedule if one is due — and laying out the life itself, if this
    /// is the pass that first paints it.
    ///
    /// A frame that is not due, or a phase with no schedule, leaves the value
    /// where it is. That is what a repaint after the child drew over the box
    /// puts back, so a dissolve resumes where it had got to rather than
    /// starting again.
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

    /// This frame's bytes: cross-faded with the session's screen while the box
    /// is dissolving, and the box on its own the rest of the time.
    ///
    /// The box at rest stays on the plain path deliberately. The composite
    /// would paint the same thing at `t == 0` — every cell of the rectangle
    /// fully dissolved, which is the background — but it would paint it in
    /// explicit colour, and for the whole second the box is up it should be
    /// drawn in the terminal's own foreground rather than in nvmux's reading
    /// of it, exactly as every other nvmux screen is.
    ///
    /// Without a shadow there is nothing to dissolve into and the box behaves
    /// as it did before this existed: `[fade] session = false` buys off the
    /// parse of the session's output, and this is one of the things that
    /// parse pays for.
    fn draw(&self, over: &Over, under: Option<&Shadow>, t: f32) -> Vec<u8> {
        match (under, self.dissolve) {
            (Some(shadow), Some(d)) if t > 0.0 && shadow.is_usable() => {
                shadow.under(over, &d.palette, t)
            }
            _ => plain_bytes(over, self.colour(t)),
        }
    }

    /// The colour this frame is drawn in — nothing at all without an effect,
    /// and nothing at rest.
    fn colour(&self, t: f32) -> Option<Rgb> {
        self.dissolve.and_then(|d| d.colour(t))
    }
}

/// Take the next value of whichever schedule the phase is running, and record
/// it. `None` while the box is held, or once a schedule is spent.
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

/// Where the box goes and what it says, worked out once.
///
/// The middle, where it cannot be missed. It is squarely over the text you have
/// just switched to, which is the whole reason it is up for only a second: this
/// is a notice to be caught out of the corner of the eye and then gone, not
/// something to read.
///
/// `None` when the screen cannot hold even a one-column box. Pure, so the
/// geometry can be tested without a terminal — and named, rather than left as
/// locals inside the renderer, because two things draw it now: [`plain_bytes`]
/// here, and [`Shadow::under`], which needs to know which cells of the
/// session's screen the box is covering.
pub fn overlay(label: &str, size: PtySize) -> Option<Over> {
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

    // Centred, in the grid's own 0-based coordinates — the renderer adds the
    // one the terminal counts from. Both subtractions are safe: the guard above
    // puts `rows` at three or more, and `width` cannot exceed `cols` because
    // the text was truncated to fit it. Odd slack falls above and to the left,
    // which is what integer division does and is not worth a correction nobody
    // could see.
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

/// A block of nvmux's own rows, written over a session's screen under `sgr`.
///
/// The one place it is done. Two things draw over a live session — the box
/// here, and the rows the prefix puts up ([`crate::hint`]) — and the escape
/// sequence they have to get right is the same one, so it is written once.
/// `sgr` is the one thing that differs: a reset, a reset carrying a dissolve's
/// foreground, or a reset carrying `DIM`.
///
/// [`placed_each`] is the same for several blocks at once, each under its own
/// SGR, and inside the one update — which is what the prefix's two rows are.
///
/// Rows of spaces erase: a block hides what is under it by writing over it, and
/// only [`Shadow::under`] can give it back.
///
/// What the sequence has to get right, in order:
///
/// * a synchronized-update span, so the block cannot be seen half-drawn;
/// * `DECSC`/`DECRC` (`ESC 7` / `ESC 8`) around everything — without them the
///   cursor would sit in the block's last cell for as long as the block is up,
///   and the editor's own SGR attributes would be left as whatever this drew
///   with. It is a single shared save slot, which is safe here only because
///   nothing is written except between the child's own sequences, and never
///   between a save of its own and the restore that takes it back
///   ([`crate::boundary`]);
/// * an SGR *reset* leading every row, because a row's spaces are erased with
///   *the current background* — which is whatever the editor last set, and
///   would otherwise bleed into the block. Anything `sgr` adds rides along with
///   that reset rather than following it, the shape `shadow::write_sgr` emits
///   for the same reason;
/// * an absolute `CUP` per row, and not one newline or carriage return
///   anywhere. `OPOST` is off in raw mode (see [`crate::term`]), so those move
///   the cursor rather than wrapping and are the classic way to corrupt a raw
///   overlay. It also means a block as wide as the screen never scrolls it: the
///   pending-wrap flag its last cell sets is discarded by the next `CUP`, or —
///   on the closing row — by the `DECRC` that is the last thing written.
///   Nothing printable ever follows it. That is what lets the prefix's key row
///   be the full width of the *last* row, where a wrap would scroll the
///   session.
pub(crate) fn placed(over: &Over, sgr: &str) -> Vec<u8> {
    placed_each(&[(over, sgr)])
}

/// [`placed`], for several blocks in one synchronized update, each under its
/// own SGR: two rows at opposite edges of the screen that must not be seen one
/// without the other.
pub(crate) fn placed_each(blocks: &[(&Over, &str)]) -> Vec<u8> {
    let mut out = String::from("\x1b[?2026h\x1b7");
    for (over, sgr) in blocks {
        let left = over.left + 1;
        for (i, row) in over.rows.iter().enumerate() {
            let line = usize::from(over.top) + i + 1;
            out.push_str(&format!("\x1b[{line};{left}H{sgr}{row}"));
        }
    }
    out.push_str("\x1b8\x1b[?2026l");
    out.into_bytes()
}

/// The box on its own, drawn in `fg` — or in no colour at all, which is what
/// the box at rest is.
///
/// What nvmux drew before it could dissolve, and what it still draws whenever
/// there is no session screen to dissolve into (see [`Popup::step`]).
///
/// `fg` is the one thing [`placed`]'s reset does not cover. `None` is the box
/// as it was before there was a fade and as it still is at rest, and it must
/// stay that: the box sets no colour, so it is drawn in whatever the terminal's
/// own foreground is, exactly as every nvmux screen is. `Some` is a frame of a
/// dissolve, and only the *foreground* moves — the interior is spaces, and a
/// space is erased with the current background, which the reset has just put
/// back to the terminal's own. The same division `fade::apply` makes over a
/// ratatui buffer.
pub fn plain_bytes(over: &Over, fg: Option<Rgb>) -> Vec<u8> {
    let sgr = match fg {
        Some(Rgb(r, g, b)) => format!("\x1b[0;38;2;{r};{g};{b}m"),
        None => "\x1b[0m".to_string(),
    };
    placed(over, &sgr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{big, palette, screen, size};
    use crate::ui::test_support::TINY_SIZES;

    /// The box for this label on this screen, drawn on its own: `overlay` and
    /// `plain_bytes` composed, which is all most of the tests here want.
    fn overlay_bytes(label: &str, size: PtySize, fg: Option<Rgb>) -> Option<Vec<u8>> {
        Some(plain_bytes(&overlay(label, size)?, fg))
    }

    /// An effect to dissolve with. No palette is ever installed under test, so
    /// this is the only way to reach the dissolving states.
    fn dissolve() -> Dissolve {
        Dissolve {
            one_way: Duration::from_millis(100),
            palette: palette(),
        }
    }

    /// One row of the overlay: where it was placed, the SGR it was drawn with,
    /// and the text itself.
    struct Row {
        at: (usize, usize),
        sgr: String,
        text: String,
    }

    /// Split what `overlay_bytes` emits into its rows. Every escape run starts
    /// with `ESC`, so the pieces between them are exactly the sequence and
    /// whatever followed it.
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

    /// Just the text of each row, in order.
    fn rows_of(bytes: &[u8]) -> Vec<String> {
        placed(bytes).into_iter().map(|row| row.text).collect()
    }

    /// A pass on which the session said nothing and the terminal is free: an
    /// idle editor, which is what most of these tests are about.
    fn quiet() -> Pass {
        Pass {
            wrote: false,
            between: true,
        }
    }

    /// A pass on which the session wrote and left the terminal free again —
    /// the ordinary busy case, and the one the box used to lose.
    fn wrote() -> Pass {
        Pass {
            wrote: true,
            between: true,
        }
    }

    /// A pass on which the session wrote and left the terminal inside a
    /// sequence, so nvmux may not write at all.
    fn mid_sequence() -> Pass {
        Pass {
            wrote: true,
            between: false,
        }
    }

    /// Drive a popup to the end of its life on a child that never says a word,
    /// waking exactly when `wake_at` asks to be woken.
    ///
    /// Gives back every frame it painted — how long after the arming, and how
    /// dissolved — and whatever ended the run.
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

    /// When the box is being held, if it is.
    fn held_until(popup: &Popup) -> Option<Instant> {
        match popup.life.as_ref()?.phase {
            Phase::Held { until } => Some(until),
            _ => None,
        }
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
        for fg in [None, Some(Rgb(1, 2, 3))] {
            let bytes = overlay_bytes("2  dotfiles", big(), fg).expect("drawn");
            let text = String::from_utf8(bytes).expect("utf-8");
            assert!(text.starts_with("\x1b[?2026h\x1b7"), "{text:?}");
            assert!(text.ends_with("\x1b8\x1b[?2026l"), "{text:?}");
            assert_eq!(text.matches('\x07').count(), 0);
        }
    }

    /// With `OPOST` off a newline moves the cursor down a column rather than to
    /// the start of the next line, which is the classic way to shear a raw
    /// overlay across the screen.
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

    /// The picker sets no colour at all so it inherits the terminal's palette;
    /// a box painted over the editor has even less business choosing one. The
    /// raw-byte counterpart of `test_support::assert_no_colour`.
    ///
    /// Still true of the box nvmux draws with the fade off, and of every box it
    /// draws at rest — the dissolve is the one thing that colours it, and only
    /// while it is running.
    #[test]
    fn nothing_in_the_overlay_sets_a_colour() {
        let bytes = overlay_bytes("2  dotfiles", big(), None).expect("drawn");
        let text = String::from_utf8(bytes).expect("utf-8");
        for sgr in text.split("\x1b[").filter_map(|p| p.strip_suffix('m')) {
            assert_eq!(sgr, "0", "the only SGR may be a reset, found {sgr:?}");
        }
    }

    /// A frame of a dissolve carries its colour on the reset's own CSI, the
    /// shape `shadow::write_sgr` emits: the reset is what puts the background
    /// back, so a colour that followed it would be a second state to get wrong.
    /// The background is never set — the box's interior erases with the
    /// terminal's own, which is what the glyphs are dissolving into.
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

    /// Both ends of the interpolation: at rest the box sets no colour at all,
    /// so it is byte-identical to the one nvmux drew before there was a fade;
    /// fully dissolved it is the terminal's own background, which against the
    /// background its own interior erased with is nothing to see.
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

    /// Every row is placed absolutely and inside the screen. A box that runs
    /// off the right-hand edge wraps, and a wrap at the bottom scrolls the
    /// editor's screen up by a line — which nvmux cannot undo.
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

    /// A terminal with no room for the box gets no box, rather than a fragment
    /// of one wrapped across the editor's text.
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

    /// The box grows out of the editor's text rather than out of a hole. The
    /// first frame of the arrival is the session's own cells — nothing has
    /// changed on screen yet — and the box itself only appears once it is the
    /// more visible of the two, by which point the text under it has dimmed
    /// most of the way out.
    #[test]
    fn the_box_arrives_out_of_the_screen_it_is_covering() {
        let t0 = Instant::now();
        let screen = screen(24, 80);
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, Some(dissolve())).expect("armed");

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

    /// And leaves the same way: the last frame of the departure is the
    /// session's cells at full colour, so the screen is already back before
    /// the repaint that follows the notice is even asked for.
    #[test]
    fn the_box_leaves_back_into_the_screen_it_was_covering() {
        let t0 = Instant::now();
        let screen = screen(24, 80);
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, Some(dissolve())).expect("armed");

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

    /// Every frame the box is ever drawn as leaves the terminal exactly where
    /// it found it: between sequences, with no synchronized update and no
    /// cursor save of nvmux's own still open.
    ///
    /// The relay writes these into the gaps in the session's own output, and
    /// [`crate::boundary`] follows both along one parser — so a frame that
    /// did not balance would not corrupt anything, it would quietly shut the
    /// notice out of the terminal for the rest of its life. Both renderers
    /// are covered: the plain box and the composite.
    #[test]
    fn every_frame_of_the_box_leaves_the_terminal_where_it_found_it() {
        let t0 = Instant::now();
        let screen = screen(24, 80);
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, Some(dissolve())).expect("armed");
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

    /// With no shadow there is nothing to dissolve into, and the box is the
    /// one nvmux drew before any of this: `[fade] session = false` buys off
    /// the parse that pays for the backdrop, and must cost nothing else.
    #[test]
    fn with_no_shadow_the_box_dissolves_as_it_did_before() {
        let t0 = Instant::now();
        let d = dissolve();
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, Some(d)).expect("armed");
        let Act::Paint(bytes) = popup.step(t0, quiet(), big(), None) else {
            panic!("the box did not go up");
        };
        let t = popup.life.as_ref().expect("a life").t;
        let over = overlay("dotfiles", big()).expect("a box");
        assert_eq!(bytes, plain_bytes(&over, d.colour(t)));
    }

    /// The box at rest is drawn plain even with a screen to hand. The
    /// composite would paint the same thing — the rectangle fully dissolved is
    /// the background — but in explicit colour, and for the whole second the
    /// box is up it belongs in the terminal's own foreground, like every other
    /// nvmux screen.
    #[test]
    fn the_box_at_rest_is_drawn_plain_even_with_a_screen_to_hand() {
        let t0 = Instant::now();
        let screen = screen(24, 80);
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, None).expect("armed");
        let Act::Paint(bytes) = popup.step(t0, quiet(), big(), Some(&screen)) else {
            panic!("the box did not go up");
        };
        let over = overlay("dotfiles", big()).expect("a box");
        assert_eq!(bytes, plain_bytes(&over, None));
    }

    /// The geometry the two renderers share: three rows, centred, sized to the
    /// text's own width and never wider than the screen.
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

    /// **The flicker this mechanism exists for.** A window repainting at sixty
    /// frames a second draws over the box on every frame, and the box goes
    /// straight back up on the pass that follows — the relay writes the
    /// session's frame and then asks the popup, and the popup has nothing left
    /// to wait for. Without buying itself any more time on screen for the
    /// trouble: the life is on the clock and the repaints are not part of it.
    ///
    /// Before this, the box waited 25 ms with the child silent, which a
    /// session like that never gives it: measured through a real terminal, the
    /// box went up and was wiped eleven times in its one second at ten frames
    /// a second, and never went up at all at sixty.
    #[test]
    fn a_box_drawn_over_is_back_up_on_the_next_pass() {
        let t0 = Instant::now();
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, None).expect("armed");
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

    /// A session that never leaves the terminal between its own sequences —
    /// a client stopped halfway through writing one and never finished — gets
    /// no box, rather than one dropped inside it.
    #[test]
    fn a_terminal_never_left_between_sequences_gets_no_box() {
        let t0 = Instant::now();
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, None).expect("armed");
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

    /// A dissolve is paced by the clock whether or not a frame of it can be
    /// put out — which is also what keeps [`Popup::wake_at`] naming a moment
    /// that has not arrived. Were it gated on being able to paint, a box
    /// nothing could paint would leave the relay a deadline already past and
    /// spin it on a `poll` that returns at once: the trap `Hold::wake_at`
    /// steps around in [`crate::pty`].
    #[test]
    fn a_box_that_can_never_be_painted_does_not_spin_the_relay() {
        let t0 = Instant::now();
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, Some(dissolve())).expect("armed");
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

    /// With no effect there is no dissolve at either end: one paint, a second
    /// of it, and the erase. The behaviour nvmux had before the fade existed,
    /// which is what `NO_COLOR` and `[fade] enabled = false` must still get.
    #[test]
    fn with_no_effect_the_box_goes_up_whole_and_comes_down_whole() {
        let t0 = Instant::now();
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, None).expect("armed");
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

    /// The whole life, in order: up out of the background, a second of it fully
    /// drawn, back down into it, and only then the erase.
    #[test]
    fn the_box_dissolves_in_then_holds_then_dissolves_out() {
        let t0 = Instant::now();
        let d = dissolve();
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, Some(d)).expect("armed");
        let (painted, end) = drive(&mut popup, t0, big());
        assert_eq!(end, Act::Erase);

        let ts: Vec<f32> = painted.iter().map(|(_, t)| *t).collect();
        // The fade in falls to exactly nothing; the fade out rises from there
        // to exactly the background. The turn is the one frame at zero.
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
        // The fade out is erased the moment it ends, so the last frame the
        // terminal is shown is the last one there was room for — all but gone
        // — and the schedule is spent to exactly the background behind it.
        assert!(*out_of.last().expect("a last frame") > 0.9, "{ts:?}");
        assert_eq!(popup.life.as_ref().expect("a life").t, 1.0);

        // A second of the box fully drawn, between the two dissolves, and a
        // dissolve's own length at each end.
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

    /// Exactly one frame of the whole life is the box as nvmux draws it with no
    /// fade at all: the one at rest, which is every frame of the hold. A
    /// dissolve that ended anywhere else would leave a coloured box sitting on
    /// the editor for a second.
    #[test]
    fn the_box_it_holds_is_the_box_it_would_have_drawn_unfaded() {
        let t0 = Instant::now();
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, Some(dissolve())).expect("armed");
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

    /// The editor drawing over a dissolving box does not rewind it. `painted`
    /// only ever says the screen is behind; where the dissolve has got to is
    /// the schedule's business, and it is still the clock that decides that.
    #[test]
    fn a_repaint_mid_dissolve_resumes_where_it_had_got_to() {
        let t0 = Instant::now();
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, Some(dissolve())).expect("armed");
        let mut now = t0;
        assert!(matches!(
            popup.step(now, quiet(), big(), None),
            Act::Paint(_)
        ));
        let first = popup.life.as_ref().expect("a life").t;

        // The child draws over it, and leaves the terminal mid-sequence so
        // nothing can be put back yet.
        now += Duration::from_millis(10);
        assert_eq!(popup.step(now, mid_sequence(), big(), None), Act::Idle);
        assert_eq!(
            popup.life.as_ref().expect("a life").t,
            first,
            "the dissolve moved while nothing was drawn"
        );

        // The rest of the sequence arrives and the box goes back on, at
        // whatever the clock says the dissolve has reached.
        now += fade::FRAME;
        assert!(matches!(
            popup.step(now, wrote(), big(), None),
            Act::Paint(_)
        ));
        let resumed = popup.life.as_ref().expect("a life").t;
        assert!(resumed < first, "{first} -> {resumed}");
        assert!(resumed > 0.0, "it jumped straight to the end");
    }

    /// A dissolve is paced by the clock, so a box the relay could never find
    /// anywhere to redraw still comes down on time rather than sitting there
    /// until the session lets go of the terminal.
    #[test]
    fn a_box_talked_over_for_its_whole_life_still_comes_down_on_time() {
        let t0 = Instant::now();
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, Some(dissolve())).expect("armed");
        let mut now = t0;
        assert!(matches!(
            popup.step(now, quiet(), big(), None),
            Act::Paint(_)
        ));

        // From here the child never leaves the terminal alone, so nothing can
        // be painted.
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

    /// A screen that shrinks below the box mid-dissolve gets no fragment of
    /// one. There is nothing to erase either: what is on the terminal is the
    /// client's to repaint at the size it was just told.
    #[test]
    fn a_screen_too_small_mid_dissolve_stops_rather_than_drawing_a_fragment() {
        let t0 = Instant::now();
        let mut popup = Popup::armed(Some("dotfiles".into()), t0, Some(dissolve())).expect("armed");
        let mut now = t0;
        assert!(matches!(
            popup.step(now, quiet(), big(), None),
            Act::Paint(_)
        ));
        now += fade::FRAME;
        assert_eq!(popup.step(now, quiet(), size(2, 1), None), Act::Done);
    }
}
