//! What the prefix puts on the screen while it waits: the sessions along the
//! top, the keys along the bottom, and — wherever the session's screen can be
//! dimmed — the session dimmed behind them.
//!
//! `<prefix>` used to be silent. [`crate::keys::Prefix`] swallows the byte,
//! arms, and waits `keys.timeout_ms` for the second key, and until that key
//! arrived the screen said nothing at all — so the table was only learnable
//! from `<prefix> ?`, which is itself one of the keys you have to know, and a
//! chord pressed by accident looked for half a second like a keystroke the
//! editor dropped. The key row put that right. The sessions row does the same
//! for the digits: `<prefix> 3` is only a shortcut if you know which session is
//! the third, and the one place that used to say so was the picker the
//! shortcut exists to skip.
//!
//! # The key row
//!
//! Along the bottom, built from [`crate::keys::BINDINGS`] like the help screen
//! ([`crate::ui::help`]), so a command cannot be added without appearing on it.
//!
//! The commands, and nothing else. Not the machine's other two branches: a
//! doubled prefix sends a literal, and any other key is replayed, both of which
//! the help *screen* has room to describe. This is one row read in a fraction
//! of a second with a command half-typed, and "press it again to send it to
//! Neovim" is not something anyone needs told at that moment. Nor the digit
//! rule, which it used to end on: the sessions row is that rule spelled out, a
//! session at a time.
//!
//! And never the prefix itself. Every entry is the *second* key, so the row is
//! the same whatever `[keys] prefix` is set to and there is no way for it to
//! spell `Ctrl-Space` at somebody who has remapped it. Where a string does have
//! to name the prefix, [`crate::keys::prefix_label`] is what names it.
//!
//! # The sessions row
//!
//! Along the top: every session by the number `<prefix> <n>` takes —
//! `1 api  ▸2 dotfiles  3 notes` — with the picker's marker on the one in
//! front. Drawn from the listing the session loop resolves those numbers
//! against ([`Listing`]), so what the row says a digit does is what the digit
//! does. A half-typed number narrows it to the sessions the number can still
//! reach.
//!
//! Not with only one session: there is nowhere for a digit to go.
//!
//! # Why the first row and the last
//!
//! The last row is Neovim's message row, so the key row covers `-- INSERT --`,
//! a `:` command line, a search prompt or the ruler for as long as it is up; the
//! first is the tabline, or the top of a buffer. [`crate::announce`]'s header
//! rejects "print a line on the message row" — but that objection is to
//! *asking Neovim* to print there, which is editor state nvmux has no business
//! writing. Drawing over the rows from the terminal side touches none of it:
//! nothing is created in the session, nothing is typed at it, and the cells
//! come back.
//!
//! The two edges rather than two rows together, so that between them the
//! session is all still there, and each thing is where the eye looks for it:
//! the way to other sessions at the top, and the keys at the bottom, the row
//! already watched for modal state.
//!
//! # The veil
//!
//! With the rows up, the session behind them is dimmed — half dissolved into
//! the terminal's background, by the same composite a fade paints
//! ([`Shadow::veiled`]) — so that the rows are the brightest thing on the
//! screen. Both are drawn over it at full strength, in the terminal's own
//! colours. Over a live screen they are dim, as every nvmux hint row is, but a
//! dim row over a dimmed screen is lost in it: a terminal dims text by about as
//! much as the veil does, and some by more, so the key row would sit no
//! brighter than the editor behind it. The veil is what lets the rows stand
//! out, and it only can if they do not dim themselves as well.
//!
//! It needs the shadow ([`crate::shadow`]), the only copy of the session's
//! cells nvmux has — and so everything the fade needs: the fade on, a palette,
//! and `[fade] session`. Without one the rows go up as the key row always did,
//! both dim, over a session at full colour.
//!
//! The veil moves like everything else nvmux draws ([`Glide`]): it rises over
//! half of a dissolve's time and lifts over a whole one. The rows do not rise
//! with it. They are up from its first frame — they are what the prefix is for,
//! and a dissolve of them would be that much time of not yet saying what the
//! next key does.
//!
//! A command that leaves the session for another screen does not lift it. The
//! fade out carries on from wherever the veil stands, rows and all, rather than
//! bringing the session back to full colour only to dissolve it again (see
//! [`Bar::on_screen`] and [`crate::fade::fade_out_session`]).
//!
//! # The lull, and the clocks this has and has not
//!
//! nvmux owns no cells while a session is attached, so all of this is written
//! over the session's screen — and only at a lull ([`SETTLE`]). The attach
//! notice used to wait for the same lull, and now goes up between the session's
//! escape sequences instead, asking the decoder in [`crate::boundary`] — which
//! is watched only for as long as the notice is up. So this keeps the lull:
//! with nothing parsing the child's output, it is the one state in which the
//! child cannot be mid-sequence.
//!
//! The veil's frames are paced by the clock and still wait for a lull: a
//! session that talks through a rise shows fewer frames of it, and the veil
//! arrives where it was going on time regardless. There is no give-up clock:
//! all of it is bounded by the prefix, so a session too chatty to give a lull
//! shows the rows late, or not at all, which is what a lull rule means.
//!
//! # Coming down
//!
//! Most of the ways the prefix resolves end the relay — the picker, a detach, a
//! switch — and there the screen is dissolved or cleared on the way out and the
//! rows go with it. What is left is the prefix resolving *without* ending the
//! relay: a lone prefix timing out into a literal, a doubled prefix, a key that
//! is not a command. Then the screen is handed back to the session.
//!
//! The veil lifts: the session comes back up out of it as the rows go, and the
//! last frame is the session's own screen, painted back from the shadow as the
//! client drew it ([`Shadow::paint`]). Close, not exact — the shadow keeps the
//! colours and the common attributes, not an undercurl or an underline's colour
//! — so the server is asked to repaint on top of it, as a kept client's return
//! asks. A session that has drawn over the veil since it last went up is
//! handed back at once instead of dissolved: some of it is at full colour
//! already, and a dissolve would dim that again on the way up.
//!
//! Without a veil the rows are blanked, since nothing here knows what was under
//! them, and the server is asked for the rest. Both are [`Act::HandBack`].

use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr;

use crate::announce;
use crate::fade::{self, Dissolve, Glide, FRAME};
use crate::keys::{self, Pending};
use crate::pty::PtySize;
use crate::session::Session;
use crate::shadow::{Over, Shadow, Strip, Veil, Veiled, RESET_DIM};
use crate::ui::draw::truncate;

/// How long the child must have been quiet before anything here may be written.
///
/// A child whose own write blocked because the pty buffer filled leaves the
/// master momentarily unreadable in the *middle* of a frame. That gap is
/// microseconds, and a wait this long steps over it. `pty::HOLD_SETTLE` is the
/// same number for the same reason, and this is its own constant rather than a
/// shared one because the two agree by coincidence of the same hardware, not by
/// agreement.
///
/// It costs the rows nothing in practice. A human reaching for the prefix is
/// almost always further than 25 ms from the last byte the editor wrote, so the
/// lull is already satisfied when the keystroke is read and the rows go up on
/// that very pass of the relay.
const SETTLE: Duration = Duration::from_millis(25);

/// Columns between entries. `src/ui/draw.rs`'s, because the key row is the
/// picker's hint row on another screen.
const GAP: &str = "  ";

/// The shortest screen the key row will draw on. Two, so it is never a
/// session's only row: an editor left with nothing but nvmux's hints is worse
/// than a user left to find `<prefix> ?`.
const MIN_ROWS: u16 = 2;

/// The shortest screen the sessions row will draw on, for the same reason: with
/// the key row on the last row, one more leaves the session a row of its own.
const SESSIONS_MIN_ROWS: u16 = 3;

/// What marks the session in front on the sessions row: the picker's marker
/// for its selected row, without the space the picker's column needs. The
/// picker opens with it on the session just left, which is this one.
const HERE: &str = "▸";

/// The most columns of a name the sessions row gives any one session, so one
/// long name cannot push every other session off the row.
const LABEL_WIDTH: usize = 20;

/// What ends a name cut to fit.
const CUT: &str = "…";

/// How far the session is dissolved behind the rows while the prefix waits.
///
/// Halfway: far enough back that the rows are the brightest thing on the
/// screen, near enough that the editor is still legible behind them, so that
/// where you are is not lost while you look at where you could go.
const VEIL: f32 = 0.5;

/// The rows up over a session not yet dimmed: where the veil rises from.
const UNVEILED: Veil = Veil {
    session: 0.0,
    rows: 0.0,
    under: 1.0,
};

/// The veil at rest, while the prefix waits.
const VEILED: Veil = Veil {
    session: VEIL,
    rows: 0.0,
    under: 1.0,
};

/// The session back, under the rows as well, and the rows gone: where the
/// veil lifts to.
const LIFTED: Veil = Veil {
    session: 0.0,
    rows: 1.0,
    under: 0.0,
};

/// The sessions the top row names, as the relay has them: each one's number and
/// name, and which is in front.
///
/// The listing the session loop resolves `<prefix> <n>` against, not a fresh
/// one, so the row says exactly what a digit will do. It can be stale in one
/// direction only, which is the loop's own: a session made elsewhere since is
/// missing, and its number reaches it all the same, since a number the listing
/// lacks is what makes the loop list again (see `session_loop` in `main.rs`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    /// `(number, label)` in number order, the label being the name made safe
    /// for a raw terminal ([`announce::label`]).
    entries: Vec<(u32, String)>,
    /// The number of the session in front, where the listing has it.
    current: Option<u32>,
}

impl Listing {
    /// The listing for these sessions, the one with id `current` in front.
    pub fn of(sessions: &[Session], current: &str) -> Self {
        Self::new(
            sessions.iter().map(|s| (s.state.num, s.name.clone())),
            sessions
                .iter()
                .find(|s| s.id == current)
                .map(|s| s.state.num),
        )
    }

    /// A listing of `(number, name)` pairs, `current` being the number of the
    /// one in front. Every name is made safe here, once: it is about to reach a
    /// raw terminal, where a control character smuggled into `<id>.json` would
    /// be executed rather than shown.
    pub fn new(entries: impl IntoIterator<Item = (u32, String)>, current: Option<u32>) -> Self {
        let mut entries: Vec<(u32, String)> = entries
            .into_iter()
            .map(|(num, name)| (num, announce::label(&name)))
            .collect();
        entries.sort_by_key(|(num, _)| *num);
        Self { entries, current }
    }

    /// The highest number in it, or 0 for none: what the prefix machine needs
    /// to know whether a digit could still be the start of a longer number.
    pub fn highest(&self) -> u32 {
        self.entries.iter().map(|(num, _)| *num).max().unwrap_or(0)
    }
}

/// What the relay must do about the prefix's rows on this pass of its loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// Nothing this time round.
    Idle,
    /// Write these bytes over the session's screen, and that is all.
    Write(Vec<u8>),
    /// Write these bytes — and then ask the server to repaint.
    ///
    /// The rows are down and the screen is handed back to the session, as far
    /// as nvmux can hand it back: painted back from the shadow where there was
    /// a veil, which is close but has no undercurl and no underline colour in
    /// it, or blanked where there was no shadow to paint from. Only the server
    /// has the rest, and the relay asks for it the way it asks for the attach
    /// notice's (see `pty::Erasing`).
    HandBack(Vec<u8>),
}

/// The key row's entries, in the order it offers them: [`keys::BINDINGS`]'.
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
        .collect()
}

/// What the key row says on a screen `cols` wide, or `None` when not even the
/// first entry fits.
///
/// Entries are dropped **whole**, from the right, rather than truncated the way
/// `draw::truncate` cuts the picker's row: this row is written over somebody's
/// editor, and half of `detach` there reads as corruption rather than as a
/// hint. Which entry goes first is [`keys::BINDINGS`]' order and not a second
/// opinion about priority — one table, one order — so `? help` goes before
/// `p prev`, and the two that a user pressing the prefix by accident most needs,
/// the picker and the detach, are the two that survive longest.
fn keys_text(pending: Pending, cols: u16) -> Option<String> {
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

/// What the sessions row says on a screen `cols` wide, or `None` when there is
/// nothing to say: one session or none, or not room for a single one.
///
/// Each session is its number and its name, `3 notes`, which is the key row's
/// grammar — the key, then what it does — for a key that is a digit. A name
/// longer than [`LABEL_WIDTH`] is cut, and says so. Sessions that do not fit are
/// dropped whole from the right, as the key row's entries are, and counted,
/// because a row that stops without saying so reads as the whole list.
fn sessions_text(pending: Pending, listing: &Listing, cols: u16) -> Option<String> {
    if listing.entries.len() < 2 {
        return None;
    }
    let shown: Vec<String> = listing
        .entries
        .iter()
        .filter(|(num, _)| reachable(pending, *num))
        .map(|(num, label)| entry(*num, label, listing.current == Some(*num)))
        .collect();
    fit(&shown, usize::from(cols))
}

/// Whether the session numbered `num` is one `pending` could still end on: any
/// of them for a lone prefix, and for a half-typed number, the ones whose
/// number it begins.
fn reachable(pending: Pending, num: u32) -> bool {
    match pending {
        Pending::Command => true,
        Pending::Number(n) => num.to_string().starts_with(&n.to_string()),
    }
}

/// One session on the sessions row.
fn entry(num: u32, label: &str, here: bool) -> String {
    let marker = if here { HERE } else { "" };
    let label = if label.width() > LABEL_WIDTH {
        format!("{}{CUT}", truncate(label, LABEL_WIDTH - CUT.width()))
    } else {
        label.to_string()
    };
    if label.is_empty() {
        format!("{marker}{num}")
    } else {
        format!("{marker}{num} {label}")
    }
}

/// As many of `entries` as fit in `cols`, and a count of the rest.
fn fit(entries: &[String], cols: usize) -> Option<String> {
    let all = entries.join(GAP);
    if all.width() <= cols {
        return (!all.is_empty()).then_some(all);
    }
    (1..entries.len()).rev().find_map(|kept| {
        let row = format!(
            "{}{GAP}+{} more",
            entries[..kept].join(GAP),
            entries.len() - kept
        );
        (row.width() <= cols).then_some(row)
    })
}

/// A row of the screen, `top`, the whole width of it, with `text` centred.
///
/// The full width, padded out with spaces either side, because the cells beside
/// the text are what clears the editor's own row from under it — a row of hints
/// interleaved with half a statusline would read as damage. That it is safe to
/// fill the last row to its final column is the property [`announce::placed`]
/// writes down: nothing printable follows the row, so the pending-wrap flag its
/// last cell sets is discarded rather than scrolling the session's screen.
fn centred(top: u16, text: &str, cols: u16) -> Over {
    // Both subtractions are safe: every caller measured `text` to fit `cols`.
    // Odd slack falls to the right, which is what integer division does and is
    // not worth a correction nobody could see.
    let width = usize::from(cols);
    let left = (width - text.width()) / 2;
    let right = width - text.width() - left;
    Over {
        top,
        left: 0,
        width: cols,
        rows: vec![format!("{}{text}{}", " ".repeat(left), " ".repeat(right))],
    }
}

/// Everything the prefix puts up for `pending` on a screen of `size`: the
/// sessions row along the top, where it has something to say and room to, and
/// the key row along the bottom — and nothing at all where there is no room for
/// the keys, which are what the prefix is for.
///
/// `veiled` is whether the session will be dimmed behind them, which decides
/// the one thing about them that changes: over a live screen they are dim, as
/// every nvmux hint row is, and over a veil they are not (see the module docs).
///
/// Pure, so the geometry is testable without a terminal, and named, because
/// every paint and every erase needs the same answer.
pub fn strips(pending: Pending, listing: &Listing, size: PtySize, veiled: bool) -> Vec<Strip> {
    if size.rows < MIN_ROWS || size.cols == 0 {
        return Vec::new();
    }
    let Some(keys) = keys_text(pending, size.cols) else {
        return Vec::new();
    };
    let mut strips = Vec::with_capacity(2);
    if size.rows >= SESSIONS_MIN_ROWS {
        if let Some(text) = sessions_text(pending, listing, size.cols) {
            strips.push(Strip {
                over: centred(0, &text, size.cols),
                dim: !veiled,
            });
        }
    }
    strips.push(Strip {
        over: centred(size.rows - 1, &keys, size.cols),
        dim: !veiled,
    });
    strips
}

/// The rows as they are written over a live screen: each under its own SGR, all
/// in one synchronized update.
fn placed(strips: &[Strip]) -> Vec<u8> {
    let blocks: Vec<(&Over, &str)> = strips
        .iter()
        .map(|s| (&s.over, if s.dim { RESET_DIM } else { "\x1b[0m" }))
        .collect();
    announce::placed_each(&blocks)
}

/// The same rows with nothing in them: what blanks them where nothing knows
/// what was under them.
fn blanked<'a>(strips: impl IntoIterator<Item = &'a Strip>) -> Vec<u8> {
    let blanks: Vec<Over> = strips
        .into_iter()
        .map(|s| Over {
            rows: s
                .over
                .rows
                .iter()
                .map(|row| " ".repeat(row.width()))
                .collect(),
            ..s.over.clone()
        })
        .collect();
    let blocks: Vec<(&Over, &str)> = blanks.iter().map(|o| (o, "\x1b[0m")).collect();
    announce::placed_each(&blocks)
}

/// What of the prefix's is on the terminal.
#[derive(Debug)]
enum Screen {
    /// Nothing.
    Clear,
    /// Rows written straight over a live screen, with no veil: all a session
    /// without a shadow can have. Kept, rather than worked out again, because
    /// an erase has to blank the cells the rows *covered*, not whichever cells
    /// the rows would cover now.
    Rows(Vec<Strip>),
    /// The veil, and the rows on it.
    Veiled(Veiling),
}

/// A veil over the session, from its first frame to the moment the screen is
/// handed back.
#[derive(Debug)]
struct Veiling {
    /// The rows on it, as last drawn.
    strips: Vec<Strip>,
    /// Where it stands: the values the last frame was drawn at, or the next one
    /// will be.
    at: Veil,
    /// A rise or a lift under way; `None` once the veil is up and still.
    glide: Option<Glide>,
    /// When the glide's next frame is due.
    next_frame: Instant,
    /// Whether the terminal shows `at` and `strips`: false once the session or
    /// the notice has drawn over the screen, or the glide has moved on.
    painted: bool,
    /// Whether the session has drawn over the veil since its last frame — which
    /// a lift then does not dissolve, but hands back at once.
    drawn_over: bool,
    /// Coming down: the prefix has resolved, and the glide is the lift.
    lifting: bool,
}

/// The prefix's rows, and the veil under them, over the life of one relay.
///
/// Owned by the relay loop, one per attach, and told the time rather than
/// reading a clock — like [`announce::Popup`] and [`keys::Prefix`], and for the
/// same reason: every state it can reach is then reachable from a test.
///
/// What it holds is the difference between two pictures of the screen: the one
/// on the terminal, and the one the prefix machine implies. A write is owed
/// whenever they differ, and whenever a veil is on its way somewhere.
#[derive(Debug)]
pub struct Bar {
    /// When the child was last seen with nothing to say, or `None` if it spoke
    /// on the last pass. Kept from the start of the relay rather than from the
    /// moment the prefix arms, so the lull is already behind us when it does and
    /// the rows go up on that pass instead of a wake-up later.
    quiet_since: Option<Instant>,
    /// The screen the last pass was for. Rows and columns only: a pixel-size
    /// change moves no cell.
    size: (u16, u16),
    /// The sessions the top row names.
    listing: Listing,
    /// What a veil is dissolved with, `None` when there can be no veil. Copied
    /// once rather than read at the time, the way [`announce::Popup`] is
    /// handed one: a bar that read a process global mid-relay would have states
    /// no test could reach, since no palette is ever installed under test.
    dissolve: Option<Dissolve>,
    /// The rows the machine's state implies, worked out on every pass.
    want: Vec<Strip>,
    /// What is on the terminal.
    screen: Screen,
    /// Whether this bar has had its say about `want` on this screen. False is a
    /// pass owed at the next lull, and is part of when [`Bar::wake_at`] asks to
    /// be woken.
    agreed: bool,
}

impl Bar {
    /// A bar with nothing on screen, on a child presumed quiet — which it is,
    /// at the top of a relay, until it says otherwise.
    pub fn new(now: Instant, size: PtySize, listing: Listing) -> Self {
        Self::with_dissolve(now, size, listing, fade::dissolve())
    }

    /// [`Bar::new`] with the dissolve handed in rather than read, which is how
    /// the tests reach the veil at all.
    fn with_dissolve(
        now: Instant,
        size: PtySize,
        listing: Listing,
        dissolve: Option<Dissolve>,
    ) -> Self {
        Self {
            quiet_since: Some(now),
            size: (size.rows, size.cols),
            listing,
            dissolve,
            want: Vec::new(),
            screen: Screen::Clear,
            agreed: true,
        }
    }

    /// When the relay must next wake up on the bar's account, or `None` when it
    /// owes nothing.
    ///
    /// `None` is the common answer, and it has to be: the bar is down and idle
    /// for almost all of a session, and a relay woken every 25 ms on its account
    /// would be paying for rows nobody has asked for.
    ///
    /// It is also what stops a spin. A bar that *cannot* do what it wants — a
    /// screen too narrow for one entry — records the decision by agreeing with
    /// itself, so this stops asking. Were it to keep asking, the poll timeout
    /// would round to zero and the relay would spin on a write it can never
    /// make: the hazard `pty::Hold::wake_at` steps around for the held first
    /// paint.
    pub fn wake_at(&self, now: Instant) -> Option<Instant> {
        let lull = self.quiet_since.unwrap_or(now) + SETTLE;
        let owed = match &self.screen {
            Screen::Veiled(v) => match v.glide {
                Some(_) => Some(v.next_frame),
                None => (!self.agreed || !v.painted).then_some(now),
            },
            _ => (!self.agreed).then_some(now),
        }?;
        Some(owed.max(lull))
    }

    /// Something that is not the child has written over the session's screen —
    /// a frame of the attach notice — so the rows may no longer be on it.
    pub fn overdrawn(&mut self) {
        match &mut self.screen {
            Screen::Clear => {}
            Screen::Rows(_) => self.agreed = false,
            Screen::Veiled(v) => v.painted = false,
        }
    }

    /// The sessions the top row names: the listing the prefix's digits resolve
    /// against, which the relay's prefix machine reads its highest number from.
    pub fn listing(&self) -> &Listing {
        &self.listing
    }

    /// Whether the veil is on the terminal. The attach notice's box is under
    /// it once it is, since every frame of the veil repaints the whole screen,
    /// and the relay then lets the notice go (see `pty::pump`).
    pub fn veiled(&self) -> bool {
        matches!(self.screen, Screen::Veiled(_))
    }

    /// Where the veil stands, for a fade out to carry on from — or `None` when
    /// there is no veil on the terminal, and the fade out starts from the
    /// session at full colour as it always did. See
    /// [`crate::fade::fade_out_session`].
    pub fn on_screen(&self) -> Option<Veiled> {
        match &self.screen {
            Screen::Veiled(v) => Some(Veiled {
                strips: v.strips.clone(),
                veil: v.at,
            }),
            _ => None,
        }
    }

    /// One pass of the relay loop.
    ///
    /// `busy` is whether the child had anything for the terminal this time
    /// round, or is having its first paint held: both mean the same two things
    /// here, that this is no moment to write and that anything already on the
    /// screen may have been drawn over. `under` is the session's screen as the
    /// shadow has it, which is what the veil is painted from and what hands the
    /// screen back.
    pub fn step(
        &mut self,
        now: Instant,
        busy: bool,
        size: PtySize,
        pending: Option<Pending>,
        under: Option<&Shadow>,
    ) -> Act {
        if self.size != (size.rows, size.cols) {
            self.size = (size.rows, size.cols);
            self.resized();
        }

        // Before the lull, not after it: the bar has to know a write is owed in
        // order to ask to be woken to make it, and the pass that reads the
        // prefix is often one the child is still talking on.
        let shadow = under.filter(|s| s.is_usable());
        let veiling = self.dissolve.is_some() && shadow.is_some();
        let want = pending
            .map(|p| strips(p, &self.listing, size, veiling))
            .unwrap_or_default();
        if want != self.want {
            self.want = want;
            self.agreed = false;
        }

        if busy {
            self.quiet_since = None;
            self.drawn_over_by_the_session();
        } else {
            self.quiet_since.get_or_insert(now);
        }

        let settled = self
            .quiet_since
            .is_some_and(|quiet| now.duration_since(quiet) >= SETTLE);
        if !settled {
            return Act::Idle;
        }

        let screen = std::mem::replace(&mut self.screen, Screen::Clear);
        let act = match (screen, shadow, self.dissolve) {
            (Screen::Veiled(v), Some(shadow), Some(d)) => self.veil(now, v, shadow, d),
            // The shadow was retired under the veil, and nothing here can paint
            // the screen back: the rows are blanked, and the server's repaint
            // takes the rest of the veil off with everything else. A prefix
            // still waiting is still owed its rows — plain, now, at the next
            // lull, as they would have been with no veil all along.
            (Screen::Veiled(v), _, _) => {
                self.agreed = self.want.is_empty();
                return Act::HandBack(blanked(&v.strips));
            }
            (Screen::Clear, Some(shadow), Some(d)) if !self.want.is_empty() => {
                let rise = Veiling {
                    strips: self.want.clone(),
                    at: UNVEILED,
                    glide: Some(Glide::start(UNVEILED, VEILED, d.one_way, now)),
                    next_frame: now,
                    painted: false,
                    drawn_over: false,
                    lifting: false,
                };
                self.veil(now, rise, shadow, d)
            }
            (Screen::Rows(shown), _, _) => self.plain(shown),
            (Screen::Clear, _, _) => self.plain(Vec::new()),
        };
        // Whatever came of this pass, the bar has now had its say about this
        // state on this screen — including deciding it can say nothing, which
        // is what keeps `wake_at` quiet.
        self.agreed = true;
        act
    }

    /// The rows with no veil: written at a lull, once, and blanked when they
    /// come down.
    fn plain(&mut self, shown: Vec<Strip>) -> Act {
        if self.agreed {
            self.screen = if shown.is_empty() {
                Screen::Clear
            } else {
                Screen::Rows(shown)
            };
            return Act::Idle;
        }
        if self.want.is_empty() {
            self.screen = Screen::Clear;
            return if shown.is_empty() {
                Act::Idle
            } else {
                Act::HandBack(blanked(&shown))
            };
        }
        // A row that was up and is wanted no more, with nothing wanted in its
        // place to cover it, has to be blanked on the way — which only the
        // server can then put right.
        let stale: Vec<&Strip> = shown
            .iter()
            .filter(|s| !self.want.iter().any(|w| w.over.top == s.over.top))
            .collect();
        let mut bytes = if stale.is_empty() {
            Vec::new()
        } else {
            blanked(stale.iter().copied())
        };
        bytes.extend_from_slice(&placed(&self.want));
        self.screen = Screen::Rows(self.want.clone());
        if stale.is_empty() {
            Act::Write(bytes)
        } else {
            Act::HandBack(bytes)
        }
    }

    /// A pass with a veil to paint, or one already painted.
    fn veil(&mut self, now: Instant, mut v: Veiling, shadow: &Shadow, d: Dissolve) -> Act {
        if self.want.is_empty() {
            // The prefix has resolved, and the screen goes back to the session.
            if v.drawn_over {
                return self.hand_back(shadow);
            }
            if !v.lifting {
                v.lifting = true;
                v.glide = Some(Glide::start(v.at, LIFTED, d.one_way, now));
                v.next_frame = now;
            }
        } else {
            if v.lifting {
                // The prefix again before the veil was down: back up from where
                // it stands, and the rows up at once, as they always are —
                // covering what is under them at once with it.
                let from = Veil {
                    session: v.at.session,
                    ..UNVEILED
                };
                v.lifting = false;
                v.at = from;
                v.glide = Some(Glide::start(from, VEILED, d.one_way, now));
                v.next_frame = now;
                v.painted = false;
            }
            if v.strips != self.want {
                v.strips = self.want.clone();
                v.painted = false;
            }
        }

        if let Some(glide) = v.glide.as_mut().filter(|_| now >= v.next_frame) {
            if let Some(at) = glide.next(now) {
                if at != v.at {
                    v.at = at;
                    v.painted = false;
                }
                v.next_frame = now + FRAME;
            }
            if glide.finished() {
                v.glide = None;
                if v.lifting {
                    return self.hand_back(shadow);
                }
            }
        }

        if v.painted {
            self.screen = Screen::Veiled(v);
            return Act::Idle;
        }
        let bytes = shadow.veiled(&v.strips, &d.palette, v.at, self.size);
        v.painted = true;
        v.drawn_over = false;
        self.screen = Screen::Veiled(v);
        Act::Write(bytes)
    }

    /// Give the screen back to the session: its own screen, painted from the
    /// shadow as the client drew it, cursor and all — and the server asked for
    /// the rest (see [`Act::HandBack`]).
    fn hand_back(&mut self, shadow: &Shadow) -> Act {
        self.screen = Screen::Clear;
        Act::HandBack(shadow.paint())
    }

    /// The session has written to the terminal, so whatever is on it may have
    /// been drawn over — but only if the bar had anything there to be covered.
    /// A bar that is down has nothing to put back, and saying otherwise would
    /// have [`Bar::wake_at`] ask for a wake-up after every burst of output for
    /// the whole of a session.
    fn drawn_over_by_the_session(&mut self) {
        match &mut self.screen {
            Screen::Clear => {}
            Screen::Rows(_) => self.agreed = false,
            Screen::Veiled(v) => {
                v.painted = false;
                v.drawn_over = true;
            }
        }
    }

    /// The terminal changed size, and a resize is its own erase: the size
    /// reaches the client through the pty and the client repaints the whole
    /// grid, which is the session taking its screen back.
    ///
    /// So rows drawn plain are forgotten rather than blanked. Blanking them
    /// would be worse than redundant after a shrink — the rows that were
    /// painted are off the grid, and the terminal would clamp every `CUP` in
    /// them onto a row of somebody's text. A veil on its way down is down: the
    /// client's repaint is the hand back, and there is no server to ask. A veil
    /// that is up has been drawn over, or is about to be, and goes back up at
    /// the next lull where it stood, at the new size.
    fn resized(&mut self) {
        self.screen = match std::mem::replace(&mut self.screen, Screen::Clear) {
            Screen::Veiled(mut v) if !v.lifting => {
                v.painted = false;
                v.drawn_over = true;
                Screen::Veiled(v)
            }
            Screen::Veiled(_) | Screen::Rows(_) | Screen::Clear => Screen::Clear,
        };
        self.agreed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{big, palette, screen, size};
    use crate::ui::test_support::TINY_SIZES;

    /// Three sessions, the second in front: the listing most of these tests are
    /// drawn from.
    fn three() -> Listing {
        Listing::new(
            [(1, "api"), (2, "dotfiles"), (3, "notes")].map(|(n, s)| (n, s.to_string())),
            Some(2),
        )
    }

    /// A listing of `n` sessions named `s1` … `sn`, the first in front.
    fn many(n: u32) -> Listing {
        Listing::new((1..=n).map(|i| (i, format!("s{i}"))), Some(1))
    }

    /// The text of each strip, top to bottom, trimmed.
    fn texts(strips: &[Strip]) -> Vec<String> {
        strips
            .iter()
            .map(|s| s.over.rows[0].trim().to_string())
            .collect()
    }

    /// What the prefix would draw on `size`, as text, over a live screen.
    fn drawn(pending: Pending, listing: &Listing, size: PtySize) -> Vec<String> {
        texts(&strips(pending, listing, size, false))
    }

    // The key row.

    /// The load-bearing one: the key row is [`keys::BINDINGS`] rendered, so a
    /// command cannot be added without appearing on it — the guarantee the help
    /// screen already gives, on the row you see before you have chosen a key.
    #[test]
    fn the_key_row_lists_every_binding_in_table_order() {
        let text = keys_text(Pending::Command, 200).expect("a row");
        let mut rest = text.as_str();
        for b in keys::BINDINGS {
            let entry = format!("{} {}", keys::key_glyph(b.key), b.hint);
            let at = rest.find(&entry).unwrap_or_else(|| {
                panic!("{entry:?} is not on the row, or is out of order: {text:?}")
            });
            rest = &rest[at + entry.len()..];
        }
        assert!(rest.is_empty(), "the row goes on past the table: {rest:?}");
    }

    /// The prefix is the user's to set, so the row must not name it: every
    /// entry is the *second* key, which is the same whatever the chord is. A
    /// row that said `Ctrl-Space` would be wrong for everyone who has remapped
    /// it, and wrong in the one place they cannot miss.
    #[test]
    fn the_row_never_spells_the_prefix() {
        let text = keys_text(Pending::Command, 200).expect("a row");
        for prefix in [keys::PREFIX, keys::parse_prefix("C-t").expect("a chord")] {
            let label = keys::prefix_label(prefix);
            assert!(
                !text.contains(&label),
                "the row spells the prefix ({label}): {text:?}"
            );
        }
        assert!(!text.contains("Ctrl"), "the row names a chord: {text:?}");
    }

    /// The space bar is the one key on the row with no character of its own, and
    /// the picker already settled what to draw for it. A row that spelled it
    /// `space` while the picker two keystrokes away spelled it `␣` would be the
    /// same row contradicting itself.
    #[test]
    fn the_space_bar_is_the_glyph_the_picker_uses() {
        let text = keys_text(Pending::Command, 200).expect("a row");
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
    /// doubled prefix sends a literal, any other key is replayed — and the key
    /// row deliberately does not; nor does it teach the digits any longer, which
    /// the sessions row does a session at a time.
    #[test]
    fn the_key_row_has_the_commands_and_nothing_else() {
        let text = keys_text(Pending::Command, 200).expect("a row");
        assert!(
            !text.contains("literal"),
            "the row offers the literal prefix: {text:?}"
        );
        assert!(!text.contains("other"), "the row offers a replay: {text:?}");
        assert!(!text.contains("1-"), "the row teaches the digits: {text:?}");
        assert_eq!(
            text.split(GAP).count(),
            keys::BINDINGS.len(),
            "the row has entries beyond the commands: {text:?}"
        );
    }

    /// Narrow screens drop entries whole. A fragment of one — `det` — is written
    /// over somebody's editor and reads as damage rather than as a hint, which
    /// is why this row does not use `draw::truncate`.
    #[test]
    fn entries_are_dropped_whole_to_fit() {
        let all = entries();
        for cols in 1..=200u16 {
            let Some(text) = keys_text(Pending::Command, cols) else {
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

    /// Below the first entry the row says nothing at all, rather than a word and
    /// a half of one. The attach notice's answer to the same question.
    #[test]
    fn a_screen_too_narrow_for_one_entry_says_nothing() {
        let first = entries().remove(0);
        for cols in 0..first.width() {
            assert_eq!(
                keys_text(Pending::Command, u16::try_from(cols).expect("small")),
                None,
                "drew part of {first:?} in {cols} columns"
            );
        }
        assert!(keys_text(
            Pending::Command,
            u16::try_from(first.width()).expect("small")
        )
        .is_some());
    }

    /// A half-typed number is shown, or the row would look like it had swallowed
    /// the digit — the reason the picker shows one on its own hint row.
    #[test]
    fn a_pending_number_is_shown() {
        assert_eq!(keys_text(Pending::Number(1), 80).as_deref(), Some("#1"));
        assert_eq!(keys_text(Pending::Number(12), 80).as_deref(), Some("#12"));
        let rows = drawn(Pending::Number(1), &many(12), big());
        assert_eq!(rows.last().map(String::as_str), Some("#1"));
    }

    // The sessions row.

    /// Every session by the number a digit takes to it, and the one in front
    /// marked the way the picker marks it.
    #[test]
    fn the_sessions_row_names_each_session_by_its_number() {
        let rows = drawn(Pending::Command, &three(), big());
        assert_eq!(rows[0], "1 api  ▸2 dotfiles  3 notes");
        assert_eq!(rows.len(), 2, "{rows:?}");
    }

    /// Only one session is nowhere for a digit to go.
    #[test]
    fn a_single_session_has_no_sessions_row() {
        let one = Listing::new([(1, "api".to_string())], Some(1));
        assert_eq!(drawn(Pending::Command, &one, big()).len(), 1);
        assert_eq!(drawn(Pending::Command, &Listing::default(), big()).len(), 1);
    }

    /// A long name is cut, and says so, so that it cannot push every other
    /// session off the row.
    #[test]
    fn a_long_name_is_cut_and_says_so() {
        let listing = Listing::new([(1, "a".repeat(40)), (2, "web".to_string())], Some(2));
        let row = &drawn(Pending::Command, &listing, big())[0];
        let first = row.split(GAP).next().expect("an entry");
        assert_eq!(first, format!("1 {}{CUT}", "a".repeat(LABEL_WIDTH - 1)));
        assert!(row.ends_with("▸2 web"), "{row:?}");
    }

    /// Sessions that do not fit are dropped whole from the right, as the key
    /// row's entries are — and counted, because a row that stops without saying
    /// so reads as the whole list.
    #[test]
    fn sessions_that_do_not_fit_are_dropped_whole_and_counted() {
        let listing = many(30);
        for cols in 12..=120u16 {
            let rows = drawn(Pending::Command, &listing, size(cols, 24));
            if rows.len() < 2 {
                continue;
            }
            let row = &rows[0];
            assert!(row.width() <= usize::from(cols), "{cols}: {row:?}");
            let entries: Vec<&str> = row.split(GAP).collect();
            let (kept, more) = entries.split_at(entries.len() - 1);
            let dropped: usize = more[0]
                .strip_prefix('+')
                .and_then(|m| m.strip_suffix(" more"))
                .and_then(|n| n.parse().ok())
                .unwrap_or_else(|| panic!("{cols}: no count at the end of {row:?}"));
            assert_eq!(kept.len() + dropped, 30, "{cols}: {row:?}");
            for (i, entry) in kept.iter().enumerate() {
                let marker = if i == 0 { HERE } else { "" };
                assert_eq!(*entry, format!("{marker}{} s{}", i + 1, i + 1), "{cols}");
            }
        }
        // And a row that fits whole says nothing about more.
        let row = &drawn(Pending::Command, &many(3), big())[0];
        assert!(!row.contains("more"), "{row:?}");
    }

    /// A half-typed number narrows the row to the sessions it can still end
    /// on, which is the question a user who has typed `1` and paused is asking.
    #[test]
    fn a_pending_number_narrows_the_row_to_what_it_can_still_reach() {
        let rows = drawn(Pending::Number(1), &many(12), big());
        assert_eq!(rows[0], "▸1 s1  10 s10  11 s11  12 s12");
    }

    /// A name is about to reach a raw terminal, with no escaping between here
    /// and the wire: a control character smuggled into `<id>.json` must not
    /// arrive as one.
    #[test]
    fn a_name_cannot_carry_an_escape_sequence_onto_the_screen() {
        let listing = Listing::new(
            [(1, "evil\x1b[2J".to_string()), (2, "web".to_string())],
            None,
        );
        let row = &drawn(Pending::Command, &listing, big())[0];
        assert!(!row.contains('\x1b'), "{row:?}");
        assert!(row.starts_with("1 evil[2J"), "{row:?}");
    }

    /// The listing is in number order whatever order it was given in, knows its
    /// highest number for the prefix machine, and finds the session in front by
    /// its id rather than trusting a number.
    #[test]
    fn a_listing_is_in_number_order_and_knows_its_highest() {
        let listing = Listing::new(
            [
                (3, "c".to_string()),
                (1, "a".to_string()),
                (2, "b".to_string()),
            ],
            None,
        );
        assert_eq!(drawn(Pending::Command, &listing, big())[0], "1 a  2 b  3 c");
        assert_eq!(listing.highest(), 3);
        assert_eq!(Listing::default().highest(), 0);

        let session = |id: &str, name: &str, num: u32| {
            let mut s = Session::new(id.into(), name.into(), 1, num);
            s.state.num = num;
            s
        };
        let sessions = [session("aaaaaaaa", "api", 1), session("bbbbbbbb", "web", 2)];
        let listing = Listing::of(&sessions, "bbbbbbbb");
        assert_eq!(drawn(Pending::Command, &listing, big())[0], "1 api  ▸2 web");
    }

    // Where the rows go.

    /// The sessions along the first row and the keys along the last, each the
    /// whole width of it, with the text centred.
    #[test]
    fn the_rows_are_the_first_and_the_last_at_full_width() {
        for (cols, rows) in [(80u16, 24u16), (60, 3), (200, 50)] {
            let strips = strips(Pending::Command, &three(), size(cols, rows), false);
            assert_eq!(strips.len(), 2, "{cols}x{rows}");
            for (strip, top) in strips.iter().zip([0, rows - 1]) {
                assert_eq!(strip.over.top, top, "{cols}x{rows}");
                assert_eq!(strip.over.left, 0, "{cols}x{rows}");
                assert_eq!(strip.over.width, cols, "{cols}x{rows}");
                assert_eq!(strip.over.rows.len(), 1, "{cols}x{rows}");
                let row = &strip.over.rows[0];
                assert_eq!(row.width(), usize::from(cols), "{cols}x{rows}");
                let before = row.len() - row.trim_start().len();
                let after = row.len() - row.trim_end().len();
                assert!(
                    before.abs_diff(after) <= 1,
                    "{cols}x{rows}: not centred: {row:?}"
                );
            }
        }
    }

    /// Nothing of nvmux's ever takes every row of a session: one row gets
    /// nothing, two get the keys, and the sessions need a third.
    #[test]
    fn the_rows_never_take_every_row_of_a_session() {
        assert!(drawn(Pending::Command, &three(), size(200, 1)).is_empty());
        assert_eq!(drawn(Pending::Command, &three(), size(200, 2)).len(), 1);
        assert_eq!(drawn(Pending::Command, &three(), size(200, 3)).len(), 2);
    }

    /// Over a live screen both rows are dim, as every nvmux hint row is; over a
    /// veil neither is, since a dim row over a dimmed screen is lost in it.
    #[test]
    fn the_rows_are_dim_only_where_there_is_no_veil() {
        let live = strips(Pending::Command, &three(), big(), false);
        assert_eq!(live.len(), 2);
        assert!(live.iter().all(|s| s.dim), "{live:?}");
        let veiled = strips(Pending::Command, &three(), big(), true);
        assert_eq!(veiled.len(), 2);
        assert!(veiled.iter().all(|s| !s.dim), "{veiled:?}");
    }

    /// The degenerate screens, where the rows must either be declined or stay
    /// inside the grid — never a cell the terminal would clamp somewhere else.
    #[test]
    fn a_tiny_screen_is_either_declined_or_drawn_inside_itself() {
        for &(cols, rows) in TINY_SIZES {
            for pending in [Pending::Command, Pending::Number(7)] {
                for strip in strips(pending, &three(), size(cols, rows), false) {
                    let at = format!("{cols}x{rows} {pending:?}");
                    assert!(strip.over.top < rows, "{at}: off the screen");
                    assert_eq!(strip.over.left, 0, "{at}");
                    assert_eq!(strip.over.width, cols, "{at}");
                    assert_eq!(
                        strip.over.rows[0].width(),
                        usize::from(cols),
                        "{at}: the row overflows the screen"
                    );
                }
            }
        }
    }

    /// The escape-sequence discipline `announce::placed` writes down, for both
    /// rows at once: one synchronized update, so neither is seen without the
    /// other, and nothing after the last row's final cell — it is the *last*
    /// row, so a newline or a printable byte there would scroll the session's
    /// screen by a line rather than merely draw in the wrong place.
    #[test]
    fn the_bytes_are_one_synchronized_update_that_cannot_scroll() {
        let bytes = placed(&strips(Pending::Command, &three(), big(), false));
        let text = String::from_utf8(bytes).expect("utf-8");

        assert!(text.starts_with("\x1b[?2026h\x1b7"), "{text:?}");
        assert!(text.ends_with("\x1b8\x1b[?2026l"), "{text:?}");
        assert_eq!(text.matches("\x1b[?2026h").count(), 1, "{text:?}");
        assert!(!text.contains('\n'), "a newline would scroll: {text:?}");
        assert!(!text.contains('\r'), "a carriage return: {text:?}");
        assert!(text.contains("\x1b[1;1H"), "the top row: {text:?}");
        assert!(text.contains("\x1b[24;1H"), "the bottom row: {text:?}");
        assert_eq!(text.matches(RESET_DIM).count(), 2, "not both dim: {text:?}");
        let tail = text.rsplit_once("\x1b8").expect("a restore").1;
        assert_eq!(tail, "\x1b[?2026l", "something follows the row: {tail:?}");
    }

    // Over the life of a relay, with and without a veil.

    /// A terminal the bar's writes land on: the session's screen as the client
    /// drew it — an `x` in every cell — and then everything the bar writes.
    struct Glass(vt100::Parser);

    impl Glass {
        fn new(size: PtySize) -> Self {
            let mut parser = vt100::Parser::new(size.rows, size.cols, 0);
            for row in 1..=size.rows {
                parser.process(
                    format!("\x1b[{row};1H{}", "x".repeat(usize::from(size.cols))).as_bytes(),
                );
            }
            Self(parser)
        }

        fn take(&mut self, act: &Act) {
            if let Act::Write(bytes) | Act::HandBack(bytes) = act {
                self.0.process(bytes);
            }
        }

        fn row(&self, row: u16) -> String {
            let (_, cols) = self.0.screen().size();
            self.0
                .screen()
                .rows(0, cols)
                .nth(usize::from(row))
                .expect("a row")
        }

        /// The foreground of the cell at (`row`, `col`), 0-based.
        fn fg(&self, row: u16, col: u16) -> vt100::Color {
            self.0.screen().cell(row, col).expect("a cell").fgcolor()
        }

        fn dim(&self, row: u16, col: u16) -> bool {
            self.0.screen().cell(row, col).expect("a cell").dim()
        }
    }

    /// How dissolved the session's text is on the glass, read off a cell in the
    /// middle of the screen: its foreground's grey level against the test
    /// palette's, 0 as drawn and 1 the background.
    fn level(glass: &Glass) -> f32 {
        match glass.fg(12, 40) {
            vt100::Color::Default => 0.0,
            vt100::Color::Rgb(r, _, _) => 1.0 - f32::from(r) / 200.0,
            other => panic!("the session is in an unexpected colour: {other:?}"),
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
        glass: Glass,
    }

    impl Driver {
        /// A bar with no veil: no dissolve, and no shadow — what a `NO_COLOR`
        /// user has.
        fn new(size: PtySize) -> Self {
            let now = Instant::now();
            Self {
                bar: Bar::with_dissolve(now, size, three(), None),
                now,
                shadow: None,
                glass: Glass::new(size),
            }
        }

        /// A bar with a screen to veil and to hand back, and the colours to do
        /// it in.
        fn veiling(size: PtySize) -> Self {
            let now = Instant::now();
            let dissolve = Dissolve {
                one_way: Duration::from_millis(100),
                palette: palette(),
            };
            Self {
                bar: Bar::with_dissolve(now, size, three(), Some(dissolve)),
                now,
                shadow: Some(screen(size.rows, size.cols)),
                glass: Glass::new(size),
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
            let act = self
                .bar
                .step(self.now, busy, size, pending, self.shadow.as_ref());
            self.glass.take(&act);
            act
        }

        fn wake_at(&self) -> Option<Instant> {
            self.bar.wake_at(self.now)
        }

        /// Quiet passes until the bar asks for nothing more, and every act on
        /// the way — so a test does not have to know how many frames a glide
        /// happens to take on this clock.
        fn settle(&mut self, pending: Option<Pending>, size: PtySize) -> Vec<Act> {
            let mut acts = vec![self.quiet(pending, size)];
            for _ in 0..50 {
                if self.wake_at().is_none() {
                    return acts;
                }
                acts.push(self.quiet(pending, size));
            }
            panic!("the bar never settled: {:?}", self.bar);
        }
    }

    const ARMED: Option<Pending> = Some(Pending::Command);

    /// The rows are not written while the child is talking: the relay parses
    /// the child's output only while the attach notice is up, so for the rows
    /// any other moment could be the middle of one of its escape sequences.
    ///
    /// And they wait out the lull rather than writing on the first quiet pass:
    /// that pass is where the lull *starts*, and a child whose own write blocked
    /// on a full pty buffer is momentarily unreadable in the middle of a frame.
    #[test]
    fn nothing_is_written_until_a_lull() {
        for mut d in [Driver::new(big()), Driver::veiling(big())] {
            for _ in 0..10 {
                assert_eq!(
                    d.busy(ARMED, big()),
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
                d.quiet(ARMED, big()),
                Act::Idle,
                "the lull starts on this pass, it has not passed yet"
            );
            assert!(matches!(d.quiet(ARMED, big()), Act::Write(_)));
        }
    }

    /// With no veil, both rows go up at once, dim, over the session as it is,
    /// and they are drawn once, not once per pass: the rows do not change while
    /// the prefix waits, and repainting them every 25 ms would be a write over
    /// the session for nothing.
    #[test]
    fn without_a_veil_the_rows_go_up_once_over_the_session_as_it_is() {
        let mut d = Driver::new(big());
        assert!(matches!(d.quiet(ARMED, big()), Act::Write(_)));
        assert_eq!(d.glass.row(0).trim(), "1 api  ▸2 dotfiles  3 notes");
        assert!(d.glass.row(23).trim().starts_with("␣ picker"));
        assert!(d.glass.dim(0, 40) && d.glass.dim(23, 40), "not dim");
        assert_eq!(d.glass.row(12), "x".repeat(80), "the session was touched");
        assert_eq!(d.glass.fg(12, 40), vt100::Color::Default);
        for _ in 0..5 {
            assert_eq!(d.quiet(ARMED, big()), Act::Idle);
        }
        assert_eq!(d.wake_at(), None, "nothing is owed");
    }

    /// The child drawing over the rows is the one thing that makes a repaint
    /// due, and `overdrawn` is the same news from the attach notice. Either way
    /// the rows go back up at the next lull.
    #[test]
    fn covered_rows_are_painted_again_at_the_next_lull() {
        for cover in ["the child", "the notice"] {
            let mut d = Driver::new(big());
            assert!(matches!(d.quiet(ARMED, big()), Act::Write(_)));
            if cover == "the child" {
                assert_eq!(d.busy(ARMED, big()), Act::Idle, "{cover}: while it talks");
                // The pass that ends the burst starts the lull; the next one is
                // past it.
                assert_eq!(d.quiet(ARMED, big()), Act::Idle);
            } else {
                d.bar.overdrawn();
            }
            assert!(
                d.wake_at().is_some(),
                "{cover}: a repaint is owed and not asked for"
            );
            assert!(
                matches!(d.quiet(ARMED, big()), Act::Write(_)),
                "{cover}: the rows did not go back up"
            );
        }
    }

    /// A bar that is down has nothing on the screen to be covered, so a chatty
    /// session must not have it asking to be woken every 25 ms for the whole of
    /// a relay in which nobody touches the prefix.
    #[test]
    fn a_bar_that_is_down_is_not_woken_by_output() {
        for mut d in [Driver::new(big()), Driver::veiling(big())] {
            for _ in 0..10 {
                assert_eq!(d.busy(None, big()), Act::Idle);
                assert_eq!(d.wake_at(), None, "woken for a bar that is not up");
            }
            d.bar.overdrawn();
            assert_eq!(d.wake_at(), None, "woken for a bar that is not up");
        }
    }

    /// The prefix resolving before any lull means nothing was ever drawn, so
    /// there is nothing to put back — and no server to bother about it either.
    #[test]
    fn a_bar_that_never_painted_has_nothing_to_hand_back() {
        for mut d in [Driver::new(big()), Driver::veiling(big())] {
            assert_eq!(d.busy(ARMED, big()), Act::Idle);
            d.quiet(None, big());
            assert_eq!(d.quiet(None, big()), Act::Idle, "handed back a bare screen");
            assert_eq!(d.wake_at(), None);
            assert!(d.bar.on_screen().is_none());
        }
    }

    /// With no veil there is no shadow to put the rows back from, so they are
    /// blanked once and the relay is told to ask the server — once, and not on
    /// every pass afterwards, which is the difference between a repaint and a
    /// stall.
    #[test]
    fn without_a_veil_the_rows_are_blanked_exactly_once() {
        let mut d = Driver::new(big());
        assert!(matches!(d.quiet(ARMED, big()), Act::Write(_)));
        let act = d.quiet(None, big());
        assert!(matches!(act, Act::HandBack(_)), "{act:?}");
        assert_eq!(d.glass.row(0).trim(), "", "the top row was not blanked");
        assert_eq!(d.glass.row(23).trim(), "", "the bottom row was not blanked");
        assert_eq!(d.quiet(None, big()), Act::Idle, "asked twice");
        assert_eq!(d.wake_at(), None);
    }

    /// A resize is the client's own repaint, so rows drawn plain are forgotten
    /// rather than blanked, and drawn again where the last row now is. Blanking
    /// the old ones after a shrink would clamp every `CUP` in them onto a row of
    /// the editor's text.
    #[test]
    fn a_resize_forgets_the_rows_it_painted_and_draws_the_new_ones() {
        let mut d = Driver::new(big());
        let Act::Write(bytes) = d.quiet(ARMED, big()) else {
            panic!("the rows did not go up");
        };
        assert!(String::from_utf8_lossy(&bytes).contains("\x1b[24;1H"));

        let taller = size(80, 40);
        let Act::Write(bytes) = d.quiet(ARMED, taller) else {
            panic!("the rows did not move");
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

    /// A screen too narrow to draw on decides that once and stops asking to be
    /// woken. Without the latch the poll timeout rounds to zero and the relay
    /// spins on a write it can never make — the hazard `pty::Hold::wake_at`
    /// steps around for the held first paint.
    #[test]
    fn a_bar_with_nothing_to_draw_asks_for_no_wake_up() {
        let narrow = size(6, 24);
        assert!(
            strips(Pending::Command, &three(), narrow, true).is_empty(),
            "the fixture is wide enough after all"
        );
        for mut d in [Driver::new(narrow), Driver::veiling(narrow)] {
            assert_eq!(d.quiet(ARMED, narrow), Act::Idle);
            assert_eq!(
                d.wake_at(),
                None,
                "still asking to be woken for rows it cannot draw"
            );
            // And a number, which does fit six columns, is still drawn: the
            // decision is about this state on this screen, not about the screen
            // for good.
            assert!(matches!(
                d.quiet(Some(Pending::Number(3)), narrow),
                Act::Write(_)
            ));
        }
    }

    /// The veil's whole reason: the session dimmed behind the rows, and the rows
    /// up from its first frame, in the terminal's own colours at full strength.
    /// It rises rather than snapping — a frame at a time on the clock — and ends
    /// at [`VEIL`].
    #[test]
    fn the_veil_rises_behind_rows_that_are_up_from_its_first_frame() {
        let mut d = Driver::veiling(big());
        assert!(matches!(d.quiet(ARMED, big()), Act::Write(_)));
        assert_eq!(d.glass.row(0).trim(), "1 api  ▸2 dotfiles  3 notes");
        assert!(d.glass.row(23).trim().starts_with("␣ picker"));
        for (row, which) in [(0, "the sessions row"), (23, "the key row")] {
            let col = d.glass.row(row).find(|c: char| c != ' ').expect("text") as u16;
            assert_eq!(d.glass.fg(row, col), vt100::Color::Default, "{which}");
            assert!(!d.glass.dim(row, col), "{which} is dim over a veil");
        }
        let first = level(&d.glass);
        assert!(
            first > 0.0 && first < VEIL,
            "the first frame is not part of the way: {first}"
        );
        assert!(
            d.wake_at().is_some(),
            "the rest of the rise is not asked for"
        );

        let mut levels = vec![first];
        for act in d.settle(ARMED, big()) {
            if matches!(act, Act::Write(_)) {
                levels.push(level(&d.glass));
            }
        }
        assert!(levels.windows(2).all(|w| w[1] >= w[0]), "{levels:?}");
        assert!(
            (level(&d.glass) - VEIL).abs() < 0.01,
            "the veil stopped at {}",
            level(&d.glass)
        );
        assert_eq!(
            d.glass.row(12),
            "x".repeat(80),
            "the session's text is not all there"
        );
        assert!(
            d.glass.0.screen().hide_cursor(),
            "the cursor still blinks in it"
        );
    }

    /// A veil that is up and still asks for nothing: no frame a pass, and no
    /// wake-up either.
    #[test]
    fn a_risen_veil_is_left_alone() {
        let mut d = Driver::veiling(big());
        d.settle(ARMED, big());
        for _ in 0..5 {
            assert_eq!(d.quiet(ARMED, big()), Act::Idle);
        }
        assert_eq!(d.wake_at(), None);
        assert!(d.bar.veiled());
    }

    /// While it rises, the relay is woken for each frame and not before — the
    /// clock, not the session, paces the veil.
    #[test]
    fn a_rising_veil_asks_for_its_next_frame() {
        let mut d = Driver::veiling(big());
        d.quiet(ARMED, big());
        let wake = d.wake_at().expect("the next frame");
        assert!(wake > d.now, "asked to be woken in the past: a spin");
        assert!(wake <= d.now + SETTLE.max(FRAME), "{:?}", wake - d.now);
    }

    /// The session drawing over a veil that is up puts it back up at the next
    /// lull where it stood — not from the start of its rise.
    #[test]
    fn a_veil_the_session_draws_over_goes_back_up_where_it_stood() {
        let mut d = Driver::veiling(big());
        d.settle(ARMED, big());
        // The client redraws a row at full colour.
        d.glass.0.process(b"\x1b[13;1Hyyyyyyyy");
        assert_eq!(d.busy(ARMED, big()), Act::Idle);
        assert_eq!(d.quiet(ARMED, big()), Act::Idle, "the lull has only begun");
        assert!(matches!(d.quiet(ARMED, big()), Act::Write(_)));
        assert!((level(&d.glass) - VEIL).abs() < 0.01, "{}", level(&d.glass));
        assert_eq!(
            d.wake_at(),
            None,
            "a veil put back is not a rise to run again"
        );
    }

    /// The prefix resolving into a keystroke lifts the veil: frames on the way
    /// up, the session brightening as the rows go, and then the session's own
    /// screen handed back — painted from the shadow as the client drew it, in
    /// its own colours and with its cursor showing — and the server asked for
    /// the rest. Once.
    #[test]
    fn a_lifted_veil_hands_the_session_back() {
        let mut d = Driver::veiling(big());
        d.settle(ARMED, big());
        let acts = d.settle(None, big());
        let frames = acts.iter().filter(|a| matches!(a, Act::Write(_))).count();
        let handed = acts
            .iter()
            .filter(|a| matches!(a, Act::HandBack(_)))
            .count();
        assert!(frames >= 2, "the veil did not lift, it vanished: {acts:?}");
        assert_eq!(handed, 1, "{acts:?}");
        assert!(matches!(acts.last(), Some(Act::HandBack(_))), "{acts:?}");
        for row in [0, 12, 23] {
            assert_eq!(
                d.glass.row(row),
                "x".repeat(80),
                "row {row} did not come back"
            );
        }
        assert_eq!(
            d.glass.fg(12, 40),
            vt100::Color::Default,
            "not the session's own colours"
        );
        assert!(
            !d.glass.0.screen().hide_cursor(),
            "the cursor was not given back"
        );
        assert!(!d.bar.veiled());
        assert_eq!(d.quiet(None, big()), Act::Idle, "handed back twice");
    }

    /// A session that drew over the veil while it was coming down is handed
    /// back at once: some of it is at full colour already, and a dissolve would
    /// dim that again on its way up.
    #[test]
    fn a_veil_the_session_drew_over_is_handed_back_at_once() {
        let mut d = Driver::veiling(big());
        d.settle(ARMED, big());
        // The key that resolved the prefix, answered by the editor.
        assert_eq!(d.busy(None, big()), Act::Idle);
        assert_eq!(d.quiet(None, big()), Act::Idle);
        assert!(matches!(d.quiet(None, big()), Act::HandBack(_)));
        assert!(!d.bar.veiled());
    }

    /// The prefix again before the veil is down puts it back up from where it
    /// stood, with the rows at once — not from nothing, and not after finishing
    /// the lift.
    #[test]
    fn the_prefix_again_raises_a_lifting_veil_from_where_it_stood() {
        let mut d = Driver::veiling(big());
        d.settle(ARMED, big());
        assert!(matches!(d.quiet(None, big()), Act::Write(_)));
        let lifting = level(&d.glass);
        assert!(lifting < VEIL, "the lift had not begun: {lifting}");
        assert!(matches!(d.quiet(ARMED, big()), Act::Write(_)));
        assert!(
            level(&d.glass) >= lifting,
            "it fell back to nothing first: {}",
            level(&d.glass)
        );
        assert_eq!(d.glass.row(0).trim(), "1 api  ▸2 dotfiles  3 notes");
        d.settle(ARMED, big());
        assert!((level(&d.glass) - VEIL).abs() < 0.01, "{}", level(&d.glass));
    }

    /// A resize under a veil that is up puts it back up at the new size once the
    /// client has repainted; one on its way down is simply down, the client's
    /// repaint being the hand back.
    #[test]
    fn a_resize_under_a_veil() {
        let taller = size(80, 30);

        let mut d = Driver::veiling(big());
        d.settle(ARMED, big());
        d.shadow = Some(screen(30, 80));
        d.glass = Glass::new(taller);
        let acts = d.settle(ARMED, taller);
        assert!(acts.iter().any(|a| matches!(a, Act::Write(_))), "{acts:?}");
        assert!(
            d.glass.row(29).trim().starts_with("␣ picker"),
            "{:?}",
            d.glass.row(29)
        );
        assert!((level(&d.glass) - VEIL).abs() < 0.01, "{}", level(&d.glass));

        let mut d = Driver::veiling(big());
        d.settle(ARMED, big());
        assert!(
            matches!(d.quiet(None, big()), Act::Write(_)),
            "the lift began"
        );
        d.shadow = Some(screen(30, 80));
        assert_eq!(
            d.quiet(None, taller),
            Act::Idle,
            "a resize needs no hand back"
        );
        assert!(!d.bar.veiled());
        assert_eq!(d.wake_at(), None);
    }

    /// A shadow retired under the veil can paint nothing back, so the rows are
    /// blanked and the server asked for the rest — the veil with it.
    #[test]
    fn a_shadow_retired_under_the_veil_hands_back_through_the_server() {
        let mut d = Driver::veiling(big());
        d.settle(ARMED, big());
        d.shadow = None;
        assert!(matches!(d.quiet(ARMED, big()), Act::HandBack(_)));
        assert!(!d.bar.veiled());
        // And the rows go back up without one, as they would have without a
        // veil all along.
        assert!(matches!(d.quiet(ARMED, big()), Act::Write(_)));
        assert!(d.glass.dim(0, 40), "the sessions row, with no veil, is dim");
    }

    /// Only a usable shadow *and* colours to paint it in can veil: without
    /// either the rows go up plain, as they did before there was a veil.
    #[test]
    fn only_a_usable_shadow_with_colours_veils() {
        let mut d = Driver::veiling(big());
        d.bar.dissolve = None;
        assert!(matches!(d.quiet(ARMED, big()), Act::Write(_)));
        assert!(!d.bar.veiled(), "veiled with no colours to do it in");

        let mut d = Driver::veiling(big());
        d.shadow = None;
        assert!(matches!(d.quiet(ARMED, big()), Act::Write(_)));
        assert!(!d.bar.veiled(), "veiled with no screen to veil");
    }

    /// What the fade out carries on from: nothing before the veil's first
    /// frame, and after it exactly where the veil stands, with the rows on it.
    #[test]
    fn on_screen_says_where_the_veil_stands() {
        let mut d = Driver::veiling(big());
        assert!(d.bar.on_screen().is_none());
        d.settle(ARMED, big());
        let veiled = d.bar.on_screen().expect("a veil");
        assert_eq!(veiled.veil, VEILED);
        assert_eq!(texts(&veiled.strips)[0], "1 api  ▸2 dotfiles  3 notes");
        // Mid-lift, it is where the lift has got to.
        d.quiet(None, big());
        let veil = d.bar.on_screen().expect("still a veil").veil;
        assert!(veil.session < VEIL && veil.rows > 0.0, "{veil:?}");
    }

    /// A half-typed number changes the rows under a veil, and the veil stays
    /// where it is while they change.
    #[test]
    fn a_pending_number_redraws_the_rows_on_the_veil() {
        let mut d = Driver::veiling(big());
        d.bar.listing = many(12);
        d.settle(ARMED, big());
        assert!(matches!(
            d.quiet(Some(Pending::Number(1)), big()),
            Act::Write(_)
        ));
        assert_eq!(d.glass.row(0).trim(), "▸1 s1  10 s10  11 s11  12 s12");
        assert_eq!(d.glass.row(23).trim(), "#1");
        assert!((level(&d.glass) - VEIL).abs() < 0.01, "{}", level(&d.glass));
    }
}
