//! The cards nvmux shows in the margin of the screen (see [`crate::card`]):
//! kept here as they come, and drawn over the frame the client composes.
//!
//! Over it, and last — after the windows, the floats and the cursor's effects
//! — so that nothing of the editor's is ever drawn over a card. And drawn from
//! what the client keeps rather than written once: the editor clearing its
//! screen (`:mode`, a resize) and drawing it again is a frame like any other,
//! and the card is on it.
//!
//! # A note in the margin
//!
//! No box. Lines set flush right, two columns in from the edge, from a row
//! down, off the tab line. News:
//!
//! ```text
//!                       ∗ api-server  2s
//!     Claude Code · Claude needs your permission
//! ```
//!
//! its sign in the colour the editor has for its kind — `WarningMsg`'s for a
//! notification, `ErrorMsg`'s for a failure, `OkMsg`'s for a command that
//! finished ([`Signs`]) — or in `[style] color`, which takes their place; the
//! session's name bold, in the editor's text colour or the theme's; its age
//! counting on, faint; and what happened in italics, softer than the text.
//! The newest on top, a row between one and the next, as many as there is
//! room for above the status line and the command line, which a note never
//! covers.
//!
//! Where a switch landed is a row of names: the session landed in, bold,
//! between the one `<prefix> p` goes to and the one `<prefix> n` goes to,
//! faint — and under it, that the session is waiting for a key, where it is.
//! It takes no part in the session's own fade: drawn before the session's
//! screen dissolves in, it stands while the screen comes in under it (see
//! [`arrival_footprint`]) — and on a switch by key, from the key on, the
//! session left drawing it as it goes, and the screen handed over between the
//! two keeping it (see [`crate::handoff`]). A switch by `<prefix> n` or
//! `<prefix> p` moves the row a name the way it went, from the moment it is
//! up — the key — and on through both fades, the screen it lands on taking
//! it up as far along as it has come ([`crate::card::Card::up_ms`]): the
//! session left goes faint as the one landed in comes up, a name goes off one
//! end and the next comes on at the other.
//!
//! # The sign first, and it stays
//!
//! News comes a piece at a time: the sign, a little brighter than itself and
//! settling; then who; then what. Its life over, it goes the other way round,
//! all but the sign, which dims and moves to the edge, and stays — the mark
//! the session keeps, one to a row under the notes, until its session is
//! opened, which is a switch, and so another screen. A newer card about the
//! same session takes its place. All of it is timed in nvmux's own fade (see
//! [`crate::fade`]), and with the fade off comes and goes at once.
//!
//! # The key
//!
//! While the prefix is held, the card the key acts on says so under it,
//! `Ctrl-Space ⏎ go there`: the newest that goes somewhere (see
//! [`crate::card::Desk::go`]), its words again with it if they have gone.
//!
//! # What is under them
//!
//! The editor's text goes on under the cards, taken toward the background
//! the nearer it is to one of their lines — a row counting as two columns, a
//! cell being about twice as tall as it is wide — and under a line itself
//! emptied: so the note reads on a busy screen, and the screen around it
//! still reads as itself. A note takes all of that at once, for every line it
//! has, and has it before any of its words come, as its sign does; and gives
//! it back once they have all gone: so the words come up on a patch already
//! quiet, rather than the quiet spreading as they come. An arrival's covers
//! all the way the names on its row go on a step, and stays still as the
//! row moves. A sign on its own takes less of it, and not as far. The status line
//! and the command line are left as they are. What is drawn comes up out of
//! what is under it: that fades to the background over the first half of a
//! piece's coming, and the piece comes up out of it over the second.

use std::time::{Duration, Instant};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::compose::{Frame, Out};
use super::grid::Text;
use super::model::Model;
use super::style::{mix, Color, Colors, Style, Underline};
use crate::card::{Card, Command, Kind};
use crate::falloff::{self, ease, ASPECT, EMPTIED, PEAK, REACH};
use crate::palette::Rgb;

/// The widest a card's line is: the width the design was drawn at, which takes
/// a command and its exit status on one line.
const WIDTH: usize = 46;

/// The smallest screen a card goes up on.
const MIN_COLS: usize = 24;
const MIN_ROWS: usize = 6;

/// The row the first card is on: one down, off the tab line.
const TOP: usize = 1;

/// Columns between a card's lines and the right edge of the screen.
const MARGIN: usize = 2;

/// Rows at the bottom no card covers, and the dimming leaves alone: the
/// status line and the command line.
const BOTTOM: usize = 2;

/// How often a card on the move is drawn again: the client's own frame rate.
const FRAME: Duration = Duration::from_millis(16);

/// How much of the text colour each grey keeps, the rest the background: the
/// greys the design was drawn in, between Neovim's default foreground and
/// background. Dim for the key; faint for an age, and for the sessions either
/// side of the one a switch landed in; soft for what happened, and for a
/// session waiting for a key.
const DIM: u8 = 58;
const FAINT: u8 = 38;
const SOFT: u8 = 79;

/// How much of its own colour a sign comes in with, the rest white.
const LIT: u8 = 55;

const WHITE: Rgb = Rgb(0xff, 0xff, 0xff);

/// How much of itself a sign keeps as a mark.
const MARKED: f32 = 0.5;

/// Columns between the names on an arrival's row.
const GAP: usize = 3;

/// The dimming under the cards is [`crate::falloff`]'s, shared with the
/// settings when they float over a session: how far it reaches from a line,
/// and how far it takes what is right beside one toward the background. Less,
/// and not as far, around a sign.
const SIGN_REACH: f32 = REACH * 0.45;
const SIGN_PEAK: f32 = 0.7;

/// When each piece of a card comes and goes, in fades — nvmux's fade one way,
/// the card's [`Card::fade_ms`] — each a start and a length.
///
/// News: the sign, then who, then what, the sign's colour settling as they
/// come, and all of it up by [`SAID`].
const SIGN_IN: (f32, f32) = (0.0, 1.6);
const SETTLE: (f32, f32) = (1.6, 3.6);
const HEAD_IN: (f32, f32) = (2.6, 2.2);
const BODY_IN: (f32, f32) = (4.3, 2.6);
const SAID: f32 = 6.9;

/// And from the end of its life: what, then who, go, and the sign goes to
/// the edge and dims, a mark.
const BODY_OUT: (f32, f32) = (0.0, 2.2);
const HEAD_OUT: (f32, f32) = (1.2, 2.2);
const TO_MARK: (f32, f32) = (2.6, 3.0);

/// What is under a note is taken back as its sign comes, before any of its
/// words, and given back from the end of its life once they have gone — at
/// which the rows they took are let go of, [`UNSAID`].
const MASK_IN: (f32, f32) = (0.0, 1.6);
const MASK_OUT: (f32, f32) = (3.4, 1.6);
const UNSAID: f32 = MASK_OUT.0 + MASK_OUT.1;

/// A card let go of fades out, all of it, over this.
const LET_GO: f32 = 3.0;

/// An arrival's row moves the way the switch went from the moment it is up —
/// for a switch by key, the key: it is up on the screen being left before
/// that dissolves (see [`Card::up_ms`]) — and the arrival goes over
/// [`ARRIVAL_OUT`] once its life is over.
const SLIDE: (f32, f32) = (0.0, 2.8);
const ARRIVAL_OUT: f32 = 3.0;

/// The colours the editor has for each kind of news, which a sign is drawn
/// in: `WarningMsg`'s for a notification, `ErrorMsg`'s for a failure, and
/// `OkMsg`'s for a command that finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signs {
    pub notified: Rgb,
    pub failed: Rgb,
    pub finished: Rgb,
}

impl Signs {
    /// As `model` has them: each group's text colour as it shows; for one the
    /// editor has not got — `OkMsg` is newer than Neovim 0.11 — the
    /// terminal's own bright yellow, red or green.
    pub fn of(model: &Model) -> Self {
        let colors = model.colors();
        let group = |name: &str, ansi: usize| {
            model
                .groups
                .get(name)
                .map_or(colors.term.ansi[ansi], |&id| {
                    colors.visual_fg(&model.style(id))
                })
        };
        Self {
            notified: group("WarningMsg", 11),
            failed: group("ErrorMsg", 9),
            finished: group("OkMsg", 10),
        }
    }
}

/// The cards up, newest first.
#[derive(Debug, Default)]
pub struct Cards {
    live: Vec<Live>,
    /// Whether the prefix is held (see [`Command::Keys`]).
    held: bool,
}

#[derive(Debug)]
struct Live {
    card: Card,
    shown_at: Instant,
    /// When it was first drawn, which is what it is timed from (see
    /// [`Live::origin`]): a card that came while the editor was still drawing
    /// its first screen has not been seen yet.
    drawn_at: Option<Instant>,
    /// Let go of — acted on, or about what has nothing more to say — and
    /// fading out from then.
    dropped_at: Option<Instant>,
    /// A newer card about the same has come: this one goes with its words,
    /// and leaves no mark.
    superseded: bool,
}

/// How far each piece of a card is in at a moment, from 0 to 1.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Pieces {
    sign: f32,
    /// How much brighter than its colour the sign is: all of it as it comes,
    /// none once it has settled.
    lit: f32,
    head: f32,
    body: f32,
    /// How far the sign has gone to being a mark: dimmer, and at the edge.
    marked: f32,
    /// How much of what is under all of its lines is taken back.
    mask: f32,
    /// Whether its words still take their rows.
    words: bool,
    /// For an arrival: how far its row has moved, and how much of it there is.
    slide: f32,
    whole: f32,
}

impl Pieces {
    /// All of it, as the prefix held shows the card the key acts on.
    const WHOLE: Self = Self {
        sign: 1.0,
        lit: 0.0,
        head: 1.0,
        body: 1.0,
        marked: 0.0,
        mask: 1.0,
        words: true,
        slide: 1.0,
        whole: 1.0,
    };
}

/// Milliseconds, as the pieces are timed in them: rounded once, from a
/// duration exact to the microsecond, so that the moment a piece is due at
/// (see [`Live::next_change`]) is never a moment before it.
fn ms(d: Duration) -> f32 {
    (d.as_secs_f64() * 1000.0) as f32
}

/// `span`, in fades of `fade` milliseconds, from `from`.
fn scaled((start, len): (f32, f32), fade: f32, from: f32) -> (f32, f32) {
    (from + start * fade, len * fade)
}

/// How far through `(start, len)` `t` is, eased in and out; all at once at
/// `start`, where it takes no time.
fn ramp(t: f32, (start, len): (f32, f32)) -> f32 {
    if len <= 0.0 {
        f32::from(u8::from(t >= start))
    } else {
        ease((t - start) / len)
    }
}

fn percent(x: f32) -> u8 {
    (x.clamp(0.0, 1.0) * 100.0).round() as u8
}

impl Live {
    fn new(card: Card, now: Instant) -> Self {
        Self {
            card,
            shown_at: now,
            drawn_at: None,
            dropped_at: None,
            superseded: false,
        }
    }

    /// nvmux's fade one way, in milliseconds.
    fn fade(&self) -> f32 {
        self.card.fade_ms as f32
    }

    /// What it is timed from, once it has been drawn: its first frame — or
    /// for one that was up on another screen first, as long before it came
    /// as it had been up there ([`Card::up_ms`]), so that it goes on as it
    /// was going rather than starting over.
    fn origin(&self) -> Option<Instant> {
        let drawn = self.drawn_at?;
        if self.card.up_ms == 0 {
            return Some(drawn);
        }
        let up = Duration::from_millis(self.card.up_ms);
        Some(self.shown_at.checked_sub(up).unwrap_or(self.shown_at))
    }

    /// How long it has been up at `now`, in milliseconds.
    fn t(&self, now: Instant) -> f32 {
        self.origin()
            .map_or(0.0, |at| ms(now.saturating_duration_since(at)))
    }

    /// When the life of news is over, and its words go.
    fn leave(&self) -> f32 {
        SAID * self.fade() + self.card.life_ms as f32
    }

    /// Whether it goes with its words and leaves no mark: one a newer card has
    /// taken the place of, or news that leaves none.
    fn passes(&self) -> bool {
        self.superseded || self.card.mark.is_none()
    }

    /// Whether it is a mark by now: news whose words have gone, or were never
    /// said.
    fn is_mark(&self, now: Instant) -> bool {
        self.card.kind != Kind::Arrival
            && (self.card.only_mark || self.t(now) >= self.leave() + UNSAID * self.fade())
    }

    /// How old the news is, by now.
    fn age(&self, now: Instant) -> Option<u64> {
        let since = now.saturating_duration_since(self.shown_at).as_secs();
        self.card.age.map(|age| age + since)
    }

    fn steps(&self) -> bool {
        self.card.row.as_ref().is_some_and(|r| r.step.is_some())
    }

    /// How far each piece of it is in at `now`.
    fn pieces(&self, now: Instant) -> Pieces {
        let u = self.fade();
        let t = self.t(now);
        let mut p = if self.card.kind == Kind::Arrival {
            let whole = 1.0 - ramp(t, (self.card.life_ms as f32, ARRIVAL_OUT * u));
            Pieces {
                slide: if self.steps() {
                    ramp(t, scaled(SLIDE, u, 0.0))
                } else {
                    1.0
                },
                whole,
                mask: whole,
                words: true,
                ..Pieces::default()
            }
        } else if self.card.only_mark {
            Pieces {
                sign: ramp(t, scaled(SIGN_IN, u, 0.0)),
                marked: 1.0,
                ..Pieces::default()
            }
        } else {
            let leave = self.leave();
            let head_out = ramp(t, scaled(HEAD_OUT, u, leave));
            let sign = ramp(t, scaled(SIGN_IN, u, 0.0));
            Pieces {
                sign: if self.passes() {
                    sign.min(1.0 - head_out)
                } else {
                    sign
                },
                lit: 1.0 - ramp(t, scaled(SETTLE, u, 0.0)),
                head: ramp(t, scaled(HEAD_IN, u, 0.0)) * (1.0 - head_out),
                body: ramp(t, scaled(BODY_IN, u, 0.0))
                    * (1.0 - ramp(t, scaled(BODY_OUT, u, leave))),
                marked: if self.passes() {
                    0.0
                } else {
                    ramp(t, scaled(TO_MARK, u, leave))
                },
                mask: ramp(t, scaled(MASK_IN, u, 0.0))
                    * (1.0 - ramp(t, scaled(MASK_OUT, u, leave))),
                words: t < leave + UNSAID * u,
                ..Pieces::default()
            }
        };
        if let Some(at) = self.dropped_at {
            let left = 1.0 - ramp(ms(now.saturating_duration_since(at)), (0.0, LET_GO * u));
            p.sign *= left;
            p.head *= left;
            p.body *= left;
            p.mask *= left;
            p.whole *= left;
        }
        p
    }

    /// Whether it has gone by `now`.
    fn gone(&self, now: Instant) -> bool {
        let u = self.fade();
        if let Some(at) = self.dropped_at {
            if ms(now.saturating_duration_since(at)) >= LET_GO * u {
                return true;
            }
        }
        if self.drawn_at.is_none() {
            return false;
        }
        let t = self.t(now);
        match self.card.kind {
            Kind::Arrival => t >= self.card.life_ms as f32 + ARRIVAL_OUT * u,
            _ => !self.card.only_mark && self.passes() && t >= self.leave() + UNSAID * u,
        }
    }

    /// When it next looks different from how a frame drawn at `drawn` showed
    /// it — `whole` saying whether that showed it whole, as the prefix held
    /// shows the card the key acts on: a step of a piece on the move, the
    /// moment one starts to, the next tick of an age that shows.
    fn next_change(&self, drawn: Instant, whole: bool) -> Option<Instant> {
        let origin = self.origin()?;
        let u = self.fade();
        let t = ms(drawn.saturating_duration_since(origin));
        let mut moves = Vec::new();
        if let Some(at) = self.dropped_at {
            moves.push((ms(at.saturating_duration_since(origin)), LET_GO * u));
        }
        match self.card.kind {
            Kind::Arrival => {
                if self.steps() {
                    moves.push(scaled(SLIDE, u, 0.0));
                }
                moves.push((self.card.life_ms as f32, ARRIVAL_OUT * u));
            }
            _ if whole => {}
            _ if self.card.only_mark => moves.push(scaled(SIGN_IN, u, 0.0)),
            _ => {
                moves.push((0.0, SAID * u));
                let going = (TO_MARK.0 + TO_MARK.1).max(UNSAID);
                moves.push((self.leave(), going * u));
            }
        }
        // The microsecond a move starts at, or the one after.
        let at = |when: f32| {
            origin + Duration::from_micros((f64::from(when.max(0.0)) * 1000.0).ceil() as u64)
        };
        let mut next: Vec<Instant> = moves
            .into_iter()
            .filter_map(|(start, len)| {
                if t >= start && t < start + len {
                    Some(drawn + FRAME)
                } else {
                    (t < start).then(|| at(start))
                }
            })
            .collect();
        let says_age = self.card.age.is_some()
            && (whole
                || (!self.card.only_mark && t < self.leave() + (HEAD_OUT.0 + HEAD_OUT.1) * u));
        if says_age {
            let since = drawn.saturating_duration_since(self.shown_at);
            next.push(self.shown_at + Duration::from_secs(since.as_secs() + 1));
        }
        next.into_iter().min()
    }
}

impl Cards {
    /// Do what the relay said. Says whether anything changed on the screen.
    pub fn apply(&mut self, cmd: Command, now: Instant) -> bool {
        match cmd {
            Command::Show(card) => {
                self.live.retain(|l| l.card.id != card.id);
                if let Some(about) = card.about.clone() {
                    self.live.retain_mut(|l| {
                        if l.card.about.as_deref() != Some(about.as_str()) {
                            return true;
                        }
                        l.superseded = true;
                        !l.is_mark(now)
                    });
                }
                self.live.insert(0, Live::new(card, now));
                true
            }
            Command::Drop { id } => {
                let mut changed = false;
                for l in self
                    .live
                    .iter_mut()
                    .filter(|l| l.card.id == id && l.dropped_at.is_none())
                {
                    l.dropped_at = Some(now);
                    changed = true;
                }
                changed
            }
            // A relay opening, with the prefix let go of whatever this client
            // last heard: the one before may have ended on a key the prefix
            // was held for.
            Command::Clear => {
                self.held = false;
                !std::mem::take(&mut self.live).is_empty()
            }
            Command::Keys { held } => {
                let was = std::mem::replace(&mut self.held, held);
                was != held && self.keyed().is_some()
            }
            // The client's, not the cards': see `App::reconfigure`.
            Command::Settings(_) => false,
        }
    }

    /// Let go of the cards that are gone by `now`.
    pub fn expire(&mut self, now: Instant) {
        self.live.retain(|l| !l.gone(now));
    }

    /// Whether any card is up.
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// The card the key acts on: the newest that goes somewhere, and has not
    /// been acted on or had its place taken.
    fn keyed(&self) -> Option<usize> {
        self.live
            .iter()
            .position(|l| l.card.footer.is_some() && l.dropped_at.is_none() && !l.superseded)
    }

    /// The card shown whole, its key under it: the one the key acts on, while
    /// the prefix is held.
    fn whole(&self) -> Option<usize> {
        self.held.then(|| self.keyed()).flatten()
    }

    /// When the cards next look different from how a frame drawn at `drawn`
    /// showed them, and so when a frame is wanted: due once that is past.
    pub fn next_frame(&self, drawn: Instant) -> Option<Instant> {
        let whole = self.whole();
        self.live
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.next_change(drawn, whole == Some(i)))
            .min()
    }

    /// Draw the cards up at `now` over `frame`: on `base`, the editor's own
    /// colours, resolved through `colors`, with the signs in `signs`.
    pub fn paint(
        &mut self,
        frame: &mut Frame,
        base: Style,
        colors: &Colors,
        signs: &Signs,
        now: Instant,
    ) {
        // Timed from this frame on, whether or not the screen has room for
        // them: one that grows to have room does not show what is old by then.
        for l in &mut self.live {
            l.drawn_at.get_or_insert(now);
        }
        if self.live.is_empty() || frame.width < MIN_COLS || frame.height < MIN_ROWS {
            return;
        }
        let pens = Pens::new(base, colors, signs);
        let at = Margin::of(frame.width);
        let bottom = frame.height - BOTTOM;
        let whole = self.whole();
        let mut sheet = Sheet::new(frame.width);
        // The words first, newest on top, a row between one and the next; the
        // marks after them, one to a row.
        let mut row = TOP;
        let mut room = true;
        let mut marks = Vec::new();
        for (i, l) in self.live.iter().enumerate() {
            let keyed = whole == Some(i);
            let pieces = if keyed { Pieces::WHOLE } else { l.pieces(now) };
            if l.card.kind != Kind::Arrival && !pieces.words {
                marks.push((l, pieces));
                continue;
            }
            let height = height(&l.card, keyed);
            if !room || row + height > bottom {
                room = false;
                continue;
            }
            if l.card.kind == Kind::Arrival {
                arrival(&mut sheet, l, pieces, row, at, &pens);
            } else {
                news(&mut sheet, l, pieces, row, at, &pens, now);
                if keyed {
                    key(&mut sheet, &l.card, row, at, &pens);
                }
            }
            row += height + 1;
        }
        if room {
            for (l, pieces) in marks {
                if row >= bottom {
                    break;
                }
                let from = head(&l.card, l.age(now), at).sign;
                sign(&mut sheet, &l.card, pieces, row, from, at.end, &pens);
                row += 1;
            }
        }
        sheet.lay(frame, bottom, &pens, colors);
    }
}

/// Where a card's lines end — the column of their last character — and the
/// widest one may be.
#[derive(Debug, Clone, Copy)]
struct Margin {
    end: usize,
    widest: usize,
}

impl Margin {
    /// On a screen `cols` wide.
    fn of(cols: usize) -> Self {
        Self {
            end: cols - MARGIN - 1,
            widest: WIDTH.min(cols - MARGIN),
        }
    }
}

/// The rows `card` takes while its words are up: the arrival's row of names
/// or the note's first line, what it says under that, and where `keyed`, a
/// row and the key.
fn height(card: &Card, keyed: bool) -> usize {
    let key = if keyed && card.footer.is_some() { 2 } else { 0 };
    1 + usize::from(card.body.is_some()) + key
}

/// Where a note's first line goes, ending at the margin: its sign's column,
/// then the session, cut to fit, and its age, which never gives way.
struct Head {
    sign: usize,
    title: String,
    age: String,
}

impl Head {
    fn title_at(&self) -> usize {
        self.sign + 2
    }

    fn age_at(&self) -> usize {
        self.title_at() + self.title.width() + 2
    }
}

fn head(card: &Card, age: Option<u64>, at: Margin) -> Head {
    let age = age
        .map(|a| crate::ui::note::age(Duration::from_secs(a)))
        .unwrap_or_default();
    let for_age = if age.is_empty() { 0 } else { 2 + age.width() };
    let title = fit(&card.title, at.widest.saturating_sub(2 + for_age).max(1));
    let width = 2 + title.width() + for_age;
    Head {
        sign: (at.end + 1).saturating_sub(width),
        title,
        age,
    }
}

/// A note with news on `row`: its sign, who, its age and what — over all of
/// what it covers taken back as one, however far its words have come.
fn news(sheet: &mut Sheet, l: &Live, p: Pieces, row: usize, at: Margin, pens: &Pens, now: Instant) {
    let card = &l.card;
    let h = head(card, l.age(now), at);
    sign(sheet, card, p, row, h.sign, at.end, pens);
    sheet.text(row, h.title_at(), &h.title, pens.name(card), p.head);
    sheet.text(row, h.age_at(), &h.age, pens.faint, p.head);
    sheet.part(row, h.sign, at.end, p.mask, PEAK, REACH);
    if let Some(body) = &card.body {
        let body = fit(body, at.widest);
        let from = at.end + 1 - body.width();
        sheet.text(row + 1, from, &body, pens.soft, p.body);
        sheet.part(row + 1, from, at.end, p.mask, PEAK, REACH);
    }
}

/// The key that acts on `card`, a row under its note on `row`.
fn key(sheet: &mut Sheet, card: &Card, row: usize, at: Margin, pens: &Pens) {
    let Some(key) = &card.footer else {
        return;
    };
    let below = row + 2 + usize::from(card.body.is_some());
    let key = fit(key, at.widest);
    let from = at.end + 1 - key.width();
    sheet.text(below, from, &key, pens.dim, 1.0);
    sheet.part(below, from, at.end, 1.0, PEAK, REACH);
}

/// `card`'s sign on `row`: from column `from`, gone `p.marked` of the way to
/// the edge, `end`, and to being a mark — and from its own kind's to the one
/// it leaves.
fn sign(
    sheet: &mut Sheet,
    card: &Card,
    p: Pieces,
    row: usize,
    from: usize,
    end: usize,
    pens: &Pens,
) {
    let kind = if p.marked > 0.0 {
        card.mark.unwrap_or(card.kind)
    } else {
        card.kind
    };
    let Some(glyph) = kind.sign() else {
        return;
    };
    let colour = pens.sign(card, kind);
    let lit = if card.plain {
        colour
    } else {
        mix(LIT, colour, WHITE)
    };
    let colour = mix(percent(1.0 - p.lit), colour, lit);
    let strength = 1.0 - (1.0 - MARKED) * p.marked;
    let col = from + (end.saturating_sub(from) as f32 * p.marked).round() as usize;
    let pen = Pen::plain(mix(percent(strength), colour, pens.bg));
    sheet.text(row, col, &glyph.to_string(), pen, p.sign);
    sheet.part(row, col, col, p.sign * strength, SIGN_PEAK, SIGN_REACH);
}

/// One name on an arrival's row: where it is before the row moves and after,
/// whether it is the session the switch left and the one it landed in, and
/// how much of it there is before and after — one going off the end of the
/// row, or coming on at the other.
struct Name<'a> {
    text: &'a str,
    cols: (isize, isize),
    left: bool,
    landed: bool,
    there: (f32, f32),
}

impl<'a> Name<'a> {
    fn new(
        text: &'a str,
        cols: (isize, isize),
        left: bool,
        landed: bool,
        there: (f32, f32),
    ) -> Self {
        Self {
            text,
            cols,
            left,
            landed,
            there,
        }
    }
}

/// An arrival laid out on its row: its names, each from where it lies
/// before a step to where it lies after, and the columns it covers — for a
/// step, all the way its names go, the one going off and the one coming on
/// among them, out to the edge of the screen; the one span while it moves —
/// and the line under it, that the session is waiting for a key, and where.
struct Landing<'a> {
    names: Vec<Name<'a>>,
    covers: (usize, usize),
    waiting: Option<(String, usize, usize)>,
}

/// `card`'s names, the one landed in cut to `title`, laid out flush right in
/// the margin, a gap between: the one landed in alone where they are too
/// wide for it.
fn landing<'a>(card: &'a Card, title: &'a str, at: Margin) -> Landing<'a> {
    // Flush right, a gap between: `None` where the names are too wide.
    let lay = |names: &[&str]| -> Option<Vec<isize>> {
        let width = names.iter().map(|n| n.width()).sum::<usize>() + GAP * (names.len() - 1);
        let mut col = (at.end + 1)
            .checked_sub(width)
            .filter(|_| width <= at.widest)? as isize;
        Some(
            names
                .iter()
                .map(|n| {
                    let here = col;
                    col += (n.width() + GAP) as isize;
                    here
                })
                .collect(),
        )
    };
    let side = card.row.as_deref();
    let prev = side.and_then(|s| s.prev.as_deref());
    let next = side.and_then(|s| s.next.as_deref());
    let mut after_names: Vec<&str> = Vec::new();
    after_names.extend(prev);
    after_names.push(title);
    after_names.extend(next);
    let mut landed_at = usize::from(prev.is_some());
    let after = match lay(&after_names) {
        Some(cols) => cols,
        None => {
            after_names = vec![title];
            landed_at = 0;
            vec![(at.end + 1 - title.width()) as isize]
        }
    };
    let (mut covers, mut right) = (after[0], at.end);
    let mut names: Vec<Name> = after_names
        .iter()
        .zip(&after)
        .enumerate()
        .map(|(i, (&text, &col))| Name {
            text,
            cols: (col, col),
            left: i == landed_at,
            landed: i == landed_at,
            there: (1.0, 1.0),
        })
        .collect();
    let step = side.and_then(|s| s.step.as_ref());
    let moving = match (step, prev, next) {
        (Some(step), Some(prev), Some(next)) if after_names.len() == 3 => {
            let before_names: [&str; 3] = if step.forward {
                [&step.gone, prev, title]
            } else {
                [title, next, &step.gone]
            };
            lay(&before_names).map(|before| (step.forward, before, prev, next, step.gone.as_str()))
        }
        _ => None,
    };
    if let Some((forward, before, prev, next, gone)) = moving {
        // The row moves by what the name landed in moves by.
        let landed_before = if forward { before[2] } else { before[0] };
        let shift = after[1] - landed_before;
        let name = Name::new;
        // Going off the end, coming on at the other, the one left and the one
        // landed in, and the rest: each from where it was to where it goes.
        let (off, on) = ((1.0, 0.0), (0.0, 1.0));
        names = if forward {
            vec![
                name(gone, (before[0], before[0] + shift), false, false, off),
                name(prev, (before[1], after[0]), true, false, (1.0, 1.0)),
                name(title, (before[2], after[1]), false, true, (1.0, 1.0)),
                name(next, (after[2] - shift, after[2]), false, false, on),
            ]
        } else {
            vec![
                name(prev, (after[0] - shift, after[0]), false, false, on),
                name(title, (before[0], after[1]), false, true, (1.0, 1.0)),
                name(next, (before[1], after[2]), true, false, (1.0, 1.0)),
                name(gone, (before[2], before[2] + shift), false, false, off),
            ]
        };
        // All the way the names go, off one end and on at the other, and
        // past the margin there: a name half out of the patch would be half
        // gone with the screen, through a fade it stands in.
        covers = names
            .iter()
            .map(|n| n.cols.0.min(n.cols.1))
            .min()
            .unwrap_or(covers);
        right = at.end + MARGIN;
    }
    let waiting = card.body.as_ref().map(|waiting| {
        let end = after[landed_at] as usize + after_names[landed_at].width() - 1;
        let waiting = fit(waiting, end + 1);
        let from = end + 1 - waiting.width();
        (waiting, from, end)
    });
    Landing {
        names,
        covers: (covers.max(0) as usize, right),
        waiting,
    }
}

/// Where an arrival stands on a screen, as the one card up — as a relay opens
/// with it — on any frame it is drawn on, its row still or on the move.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Footprint {
    /// On each of its rows, the columns it covers: its row of names as wide as
    /// it lies either side of a step, and the line under it.
    pub spans: Vec<(usize, usize, usize)>,
    /// What it writes on them that stays where it is: each character, and
    /// the cell it starts on.
    pub glyphs: Vec<(usize, usize, char)>,
    /// And what moves with its row, for a step: the names on it all the
    /// while, each character on the cell it starts on before the step. After
    /// it they are `shift` columns along, and as the row moves, anywhere from
    /// the one to the other — all of them the same way.
    pub moving: Vec<(usize, usize, char)>,
    pub shift: isize,
}

/// Where `card`, an arrival, stands on a screen `cols` wide and `rows` high:
/// what a session's dissolve leaves standing (see [`crate::shadow::Kept`]), so
/// that it stays as it is while the screen under it comes and goes. Nothing
/// where it would not be drawn: not an arrival, or a screen too small for it.
pub fn arrival_footprint(card: &Card, cols: usize, rows: usize) -> Footprint {
    if card.kind != Kind::Arrival
        || cols < MIN_COLS
        || rows < MIN_ROWS
        || TOP + height(card, false) > rows - BOTTOM
    {
        return Footprint::default();
    }
    let at = Margin::of(cols);
    let title = fit(&card.title, at.widest);
    let landing = landing(card, &title, at);
    let write = |into: &mut Vec<(usize, usize, char)>, row: usize, col: usize, text: &str| {
        let mut at = col;
        for ch in text.chars() {
            let w = ch.width().unwrap_or(0);
            if w > 0 && ch != ' ' && at + w <= cols {
                into.push((row, at, ch));
            }
            at += w;
        }
    };
    let (mut glyphs, mut moving, mut shift) = (Vec::new(), Vec::new(), 0);
    for n in &landing.names {
        // One coming on to the row or going off it is drawn on some of its
        // frames and not on others.
        if n.there.0 < 1.0 || n.there.1 < 1.0 {
            continue;
        }
        let Ok(col) = usize::try_from(n.cols.0) else {
            continue;
        };
        let moves = n.cols.1 - n.cols.0;
        if moves == 0 {
            write(&mut glyphs, TOP, col, n.text);
        } else {
            write(&mut moving, TOP, col, n.text);
            shift = moves;
        }
    }
    let mut spans = vec![(TOP, landing.covers.0, landing.covers.1)];
    if let Some((waiting, from, end)) = &landing.waiting {
        write(&mut glyphs, TOP + 1, *from, waiting);
        spans.push((TOP + 1, *from, *end));
    }
    Footprint {
        spans,
        glyphs,
        moving,
        shift,
    }
}

/// Where a switch landed, on `row`: the row of names, moving if the switch
/// was a step along it, and under it that the session is waiting for a key.
fn arrival(sheet: &mut Sheet, l: &Live, p: Pieces, row: usize, at: Margin, pens: &Pens) {
    let card = &l.card;
    if p.whole <= 0.0 {
        return;
    }
    let title = fit(&card.title, at.widest);
    let landing = landing(card, &title, at);
    let s = p.slide;
    let bright = pens.name(card);
    let faint = pens.faint;
    for n in &landing.names {
        let there = p.whole * (n.there.0 + (n.there.1 - n.there.0) * s);
        if there <= 0.01 {
            continue;
        }
        let col = (n.cols.0 as f32 + (n.cols.1 - n.cols.0) as f32 * s).round() as isize;
        let (was, is) = (
            if n.left { bright } else { faint },
            if n.landed { bright } else { faint },
        );
        let pen = Pen {
            fg: mix(percent(s), is.fg, was.fg),
            bold: if s < 0.5 { n.left } else { n.landed },
            italic: false,
        };
        sheet.text_at(row, col, n.text, pen, there);
    }
    let (from, to) = landing.covers;
    sheet.part(row, from, to, p.mask, PEAK, REACH);
    if let Some((waiting, from, end)) = &landing.waiting {
        sheet.text(row + 1, *from, waiting, pens.soft, p.whole);
        sheet.part(row + 1, *from, *end, p.mask, PEAK, REACH);
    }
}

/// `s` in at most `width` columns, cut with `…` where it is wider.
fn fit(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// What a glyph is drawn in, at full strength.
#[derive(Debug, Clone, Copy)]
struct Pen {
    fg: Rgb,
    bold: bool,
    italic: bool,
}

impl Pen {
    fn plain(fg: Rgb) -> Self {
        Self {
            fg,
            bold: false,
            italic: false,
        }
    }
}

/// The pens the cards are drawn with.
struct Pens {
    /// The editor's text colour and its background, as they show.
    fg: Rgb,
    bg: Rgb,
    /// The background a card's cells have: the editor's own, and the
    /// terminal's where that is the editor's — a transparent terminal stays
    /// transparent under a card.
    back: Color,
    /// What a cell under a line is emptied to.
    blank: Style,
    faint: Pen,
    soft: Pen,
    dim: Pen,
    signs: Signs,
}

impl Pens {
    fn new(base: Style, colors: &Colors, signs: &Signs) -> Self {
        let fg = colors.visual_fg(&base);
        let bg = colors.visual_bg(&base);
        let back = if base.reverse {
            Color::Rgb(bg)
        } else {
            base.bg
        };
        let grey = |keep: u8| Pen::plain(mix(keep, fg, bg));
        Self {
            fg,
            bg,
            back,
            blank: Style {
                bg: back,
                ..Style::default()
            },
            faint: grey(FAINT),
            soft: Pen {
                italic: true,
                ..grey(SOFT)
            },
            dim: grey(DIM),
            signs: *signs,
        }
    }

    /// A session's name on `card`: bold, in its theme colour or the text's.
    fn name(&self, card: &Card) -> Pen {
        Pen {
            fg: ink(card).unwrap_or(self.fg),
            bold: true,
            italic: false,
        }
    }

    /// The colour of `kind`'s sign on `card`: the theme's where there is one,
    /// the text's under `NO_COLOR`, and otherwise the editor's for the kind.
    fn sign(&self, card: &Card, kind: Kind) -> Rgb {
        if let Some(ink) = ink(card) {
            return ink;
        }
        if card.plain {
            return self.fg;
        }
        match kind {
            Kind::Notified => self.signs.notified,
            Kind::Failed => self.signs.failed,
            Kind::Finished => self.signs.finished,
            Kind::Arrival => self.fg,
        }
    }
}

fn ink(card: &Card) -> Option<Rgb> {
    card.ink.map(|(r, g, b)| Rgb(r, g, b))
}

/// What the cards put on a frame: their glyphs, and the lines the dimming
/// under them is measured from.
struct Sheet {
    width: usize,
    glyphs: Vec<Glyph>,
    parts: Vec<Part>,
}

/// One character of a card, and how far it has come in.
struct Glyph {
    row: usize,
    col: usize,
    ch: char,
    wide: bool,
    pen: Pen,
    there: f32,
}

/// A line of a card, from one column to another, and how much of it there
/// is: what the dimming is measured from.
struct Part {
    row: usize,
    from: usize,
    to: usize,
    there: f32,
    peak: f32,
    reach: f32,
}

impl Sheet {
    fn new(width: usize) -> Self {
        Self {
            width,
            glyphs: Vec::new(),
            parts: Vec::new(),
        }
    }

    fn text(&mut self, row: usize, col: usize, s: &str, pen: Pen, there: f32) {
        self.text_at(row, col as isize, s, pen, there);
    }

    /// `s` from column `col` of `row`, a character to a cell or two, as much
    /// of it as is on the screen: a name on the move may be part way off it.
    fn text_at(&mut self, row: usize, col: isize, s: &str, pen: Pen, there: f32) {
        if there <= 0.0 {
            return;
        }
        let mut at = col;
        for ch in s.chars() {
            let w = ch.width().unwrap_or(0);
            if w == 0 {
                continue;
            }
            if ch != ' ' && at >= 0 && at as usize + w <= self.width {
                self.glyphs.push(Glyph {
                    row,
                    col: at as usize,
                    ch,
                    wide: w == 2,
                    pen,
                    there,
                });
            }
            at += w as isize;
        }
    }

    fn part(&mut self, row: usize, from: usize, to: usize, there: f32, peak: f32, reach: f32) {
        if there > 0.0 {
            self.parts.push(Part {
                row,
                from,
                to,
                there,
                peak,
                reach,
            });
        }
    }

    /// Put it all on `frame`: what is above `bottom` dimmed, by the most any
    /// line near a cell asks of it, and the glyphs over that.
    fn lay(&self, frame: &mut Frame, bottom: usize, pens: &Pens, colors: &Colors) {
        let width = frame.width;
        let mut amounts = vec![0.0f32; width * bottom];
        for part in &self.parts {
            let rows = (part.reach / ASPECT).ceil() as usize;
            let cols = part.reach.ceil() as usize;
            let last_row = (part.row + rows).min(bottom.saturating_sub(1));
            let last_col = (part.to + cols).min(width - 1);
            for r in part.row.saturating_sub(rows)..=last_row {
                let dy = (r as f32 - part.row as f32) * ASPECT;
                for c in part.from.saturating_sub(cols)..=last_col {
                    let dx = if c < part.from {
                        (part.from - c) as f32
                    } else {
                        c.saturating_sub(part.to) as f32
                    };
                    let d = (dx * dx + dy * dy).sqrt();
                    let amount = if d == 0.0 {
                        part.there
                    } else {
                        part.there * falloff::around(d, part.peak, part.reach)
                    };
                    let cell = &mut amounts[r * width + c];
                    *cell = cell.max(amount);
                }
            }
        }
        for (i, &amount) in amounts.iter().enumerate() {
            if amount > 0.01 {
                if let Some(cell) = frame.get_mut(i / width, i % width) {
                    hush(cell, amount, pens, colors);
                }
            }
        }
        for g in self.glyphs.iter().filter(|g| g.row < bottom) {
            put(frame, g, pens, colors);
        }
    }
}

/// `cell` taken `amount` of the way to the background, and emptied all the
/// way. A cell on the terminal's own background keeps it.
fn hush(cell: &mut Out, amount: f32, pens: &Pens, colors: &Colors) {
    if amount >= EMPTIED {
        *cell = Out::blank(pens.blank);
        return;
    }
    let keep = percent(1.0 - amount);
    let s = cell.style;
    let toward = |c: Rgb| Color::Rgb(mix(keep, c, pens.bg));
    cell.style = Style {
        fg: toward(colors.visual_fg(&s)),
        bg: if !s.reverse && s.bg == Color::Default {
            Color::Default
        } else {
            toward(colors.visual_bg(&s))
        },
        sp: if s.underline == Underline::None {
            s.sp
        } else {
            toward(colors.visual_sp(&s))
        },
        reverse: false,
        ..s
    };
}

/// `g` over what is under it: that going to the background over the first
/// half of the glyph's coming, the glyph coming up out of it over the second.
fn put(frame: &mut Frame, g: &Glyph, pens: &Pens, colors: &Colors) {
    let cells = if g.wide { 2 } else { 1 };
    if g.there < 0.5 {
        for c in g.col..g.col + cells {
            if let Some(cell) = frame.get_mut(g.row, c) {
                hush(cell, g.there * 2.0, pens, colors);
            }
        }
        return;
    }
    let style = Style {
        fg: Color::Rgb(mix(percent((g.there - 0.5) * 2.0), g.pen.fg, pens.bg)),
        bg: pens.back,
        bold: g.pen.bold,
        italic: g.pen.italic,
        ..Style::default()
    };
    if let Some(cell) = frame.get_mut(g.row, g.col) {
        *cell = Out {
            text: Text::Char(g.ch),
            style,
            wide: g.wide,
        };
    }
    if g.wide {
        if let Some(cell) = frame.get_mut(g.row, g.col + 1) {
            *cell = Out {
                text: Text::Half,
                style,
                wide: false,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Row, Step};
    use crate::palette::Palette;

    const OK: Rgb = Rgb(0xb3, 0xf6, 0xc0);

    fn colors() -> Colors {
        Colors {
            rgb: true,
            defaults: Default::default(),
            term: Palette {
                fg: Rgb(0xe0, 0xe2, 0xea),
                bg: Rgb(0x14, 0x16, 0x1b),
                ansi: crate::palette::XTERM_ANSI,
            },
        }
    }

    fn signs() -> Signs {
        Signs {
            notified: Rgb(0xfc, 0xe0, 0x94),
            failed: Rgb(0xff, 0xc0, 0xb9),
            finished: OK,
        }
    }

    /// News as the notifier sends it, with no fade: up at once, and gone at
    /// once.
    fn news(id: u64) -> Card {
        Card {
            fade_ms: 0,
            ink: None,
            plain: false,
            ..Card::news(
                id,
                Kind::Finished,
                "api-server",
                "cargo test: exited with code 0",
                3,
                "Ctrl-Space ⏎ go there",
            )
        }
    }

    fn landed(name: &str, prev: Option<&str>, next: Option<&str>, step: Option<Step>) -> Card {
        Card {
            fade_ms: 0,
            ink: None,
            plain: false,
            ..Card::arrival(1, name, false).in_row(Row {
                prev: prev.map(str::to_string),
                next: next.map(str::to_string),
                step,
            })
        }
    }

    fn frame(w: usize, h: usize) -> Frame {
        let mut f = Frame::new(w, h, Style::default());
        for cell in &mut f.cells {
            cell.text = Text::Char('x');
        }
        f
    }

    fn rows(f: &Frame) -> Vec<String> {
        (0..f.height)
            .map(|r| {
                (0..f.width)
                    .filter_map(|c| match &f.get(r, c)?.text {
                        Text::Char(ch) => Some(ch.to_string()),
                        Text::Cluster(s) => Some(s.to_string()),
                        Text::Half => None,
                    })
                    .collect()
            })
            .collect()
    }

    /// The frame the client draws at `now`, the cards gone by then let go of
    /// first, as it does.
    fn drawn(cards: &mut Cards, w: usize, h: usize, now: Instant) -> Frame {
        let mut f = frame(w, h);
        cards.expire(now);
        cards.paint(&mut f, Style::default(), &colors(), &signs(), now);
        f.mend();
        f
    }

    fn painted(cards: &mut Cards, w: usize, h: usize, now: Instant) -> Vec<String> {
        rows(&drawn(cards, w, h, now))
    }

    /// `s` flush right on a row of `w` x's, the margin's two after it.
    fn right(w: usize, s: &str) -> String {
        format!("{}{s}xx", "x".repeat(w - 2 - s.width()))
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// F as drawn: flush right, two columns in and a row down, the sign, the
    /// session and its age, and under them what happened; nothing of the
    /// editor's left under the lines, and the rest of it where it was.
    #[test]
    fn a_note_is_set_flush_right_in_the_margin() {
        let now = Instant::now();
        let mut cards = Cards::default();
        cards.apply(Command::Show(news(1)), now);
        let screen = painted(&mut cards, 100, 30, now);
        assert_eq!(screen[0], "x".repeat(100), "the tab line is left alone");
        assert_eq!(screen[1], right(100, "✓ api-server  3s"));
        assert_eq!(screen[2], right(100, "cargo test: exited with code 0"));
        assert_eq!(screen[3], "x".repeat(100), "the key only with the prefix");
        for row in &screen {
            assert_eq!(row.width(), 100, "{row:?}");
        }
    }

    /// The sign in the colour the editor has for its kind, the session bold
    /// in the text's, the age faint and what happened soft and in italics; a
    /// theme colour takes the sign's and the session's place, and `NO_COLOR`
    /// leaves the sign the text's.
    #[test]
    fn a_note_is_drawn_in_the_editors_colours() {
        let now = Instant::now();
        let c = colors();
        let cell = |card: Card, col: usize, row: usize| {
            let mut cards = Cards::default();
            cards.apply(Command::Show(card), now);
            let f = drawn(&mut cards, 100, 30, now);
            f.get(row, col).expect("a cell").style
        };
        let sign = 82;
        assert_eq!(cell(news(1), sign, 1).fg, Color::Rgb(OK));
        let title = cell(news(1), sign + 2, 1);
        assert_eq!((title.fg, title.bold), (Color::Rgb(c.term.fg), true));
        let age = cell(news(1), 96, 1);
        assert_eq!(age.fg, Color::Rgb(mix(FAINT, c.term.fg, c.term.bg)));
        let body = cell(news(1), 70, 2);
        assert_eq!(
            (body.fg, body.italic),
            (Color::Rgb(mix(SOFT, c.term.fg, c.term.bg)), true)
        );

        let ink = (0x7a, 0xa2, 0xf7);
        let themed = || Card {
            ink: Some(ink),
            ..news(1)
        };
        let inked = Color::Rgb(Rgb(ink.0, ink.1, ink.2));
        assert_eq!(cell(themed(), sign, 1).fg, inked, "the sign");
        assert_eq!(cell(themed(), sign + 2, 1).fg, inked, "the session");
        assert_eq!(
            cell(themed(), 96, 1).fg,
            age.fg,
            "what is faint stays faint"
        );
        let plain = Card {
            plain: true,
            ..news(1)
        };
        assert_eq!(cell(plain, sign, 1).fg, Color::Rgb(c.term.fg));
    }

    /// The newest on top, a row between one and the next, as many as fit
    /// above the status line and the command line.
    #[test]
    fn notes_stack_newest_first_a_row_apart() {
        let now = Instant::now();
        let mut cards = Cards::default();
        for id in 1..=4 {
            let mut card = news(id);
            card.title = format!("s{id}");
            cards.apply(Command::Show(card), now);
        }
        let screen = painted(&mut cards, 80, 12, now);
        assert_eq!(screen[1], right(80, "✓ s4  3s"));
        assert_eq!(screen[3], "x".repeat(80));
        assert_eq!(screen[4], right(80, "✓ s3  3s"));
        assert_eq!(screen[7], right(80, "✓ s2  3s"));
        assert!(
            screen.iter().all(|r| !r.contains("s1")),
            "a note that does not fit above the last two rows is left off: {screen:#?}"
        );
        assert_eq!(screen[10], "x".repeat(80));
        assert_eq!(screen[11], "x".repeat(80));
    }

    /// While the prefix is held, the card the key acts on says so under it —
    /// the newest that names it, past a newer one that goes nowhere; once it
    /// is acted on, the next; and once the prefix is let go of, none.
    #[test]
    fn the_key_is_said_while_the_prefix_is_held() {
        let now = Instant::now();
        let mut cards = Cards::default();
        for id in 1..=2 {
            let mut card = news(id);
            card.title = format!("s{id}");
            cards.apply(Command::Show(card), now);
        }
        let mut nowhere = news(3);
        nowhere.title = "here".into();
        nowhere.footer = None;
        cards.apply(Command::Show(nowhere), now);
        assert!(cards.apply(Command::Keys { held: true }, now));
        let screen = painted(&mut cards, 100, 30, now);
        assert_eq!(screen[1], right(100, "✓ here  3s"));
        assert_eq!(screen[4], right(100, "✓ s2  3s"));
        assert_eq!(screen[6], "x".repeat(100));
        assert_eq!(screen[7], right(100, "Ctrl-Space ⏎ go there"));
        assert_eq!(screen[9], right(100, "✓ s1  3s"));

        assert!(cards.apply(Command::Drop { id: 2 }, now));
        let screen = painted(&mut cards, 100, 30, now);
        assert_eq!(screen[4], right(100, "✓ s1  3s"), "{screen:#?}");
        assert_eq!(screen[7], right(100, "Ctrl-Space ⏎ go there"));

        assert!(cards.apply(Command::Keys { held: false }, now));
        let screen = painted(&mut cards, 100, 30, now);
        assert!(
            screen.iter().all(|r| !r.contains("go there")),
            "{screen:#?}"
        );
        assert!(
            !cards.apply(Command::Keys { held: false }, now),
            "nothing to say"
        );
    }

    /// The sign comes first, then who, then what; and the words go, and the
    /// sign stays, at the edge and dimmer, the session's mark.
    #[test]
    fn the_sign_comes_first_and_stays() {
        let now = Instant::now();
        let c = colors();
        let mut cards = Cards::default();
        let card = Card {
            fade_ms: 100,
            ..news(1)
        };
        let life = card.life_ms;
        cards.apply(Command::Show(card), now);
        let screen = painted(&mut cards, 100, 30, now);
        assert_eq!(screen[1], "x".repeat(100), "nothing yet");
        let screen = painted(&mut cards, 100, 30, now + ms(160));
        assert_eq!(
            screen[1],
            format!("{}✓{}xx", "x".repeat(82), " ".repeat(15)),
            "the sign, on all the note will cover taken back"
        );
        assert_eq!(screen[2], format!("{}{}xx", "x".repeat(68), " ".repeat(30)));
        let f = drawn(&mut cards, 100, 30, now + ms(160));
        let lit = mix(LIT, OK, WHITE);
        assert_eq!(f.get(1, 82).expect("a cell").style.fg, Color::Rgb(lit));
        assert_eq!(
            cards.next_frame(now + ms(160)),
            Some(now + ms(160) + FRAME),
            "on the move"
        );
        let screen = painted(&mut cards, 100, 30, now + ms(500));
        assert_eq!(screen[1], right(100, "✓ api-server  3s"));
        assert_eq!(
            screen[2],
            format!("{}{}xx", "x".repeat(68), " ".repeat(30)),
            "what happened comes last"
        );
        let screen = painted(&mut cards, 100, 30, now + ms(700));
        assert_eq!(screen[2], right(100, "cargo test: exited with code 0"));
        let f = drawn(&mut cards, 100, 30, now + ms(700));
        assert_eq!(
            f.get(1, 82).expect("a cell").style.fg,
            Color::Rgb(OK),
            "settled"
        );

        let leave = now + ms(690 + life);
        let screen = painted(&mut cards, 100, 30, leave + ms(600));
        assert_eq!(screen[1], format!("{}✓xx", "x".repeat(97)), "{screen:#?}");
        assert_eq!(screen[2], "x".repeat(100));
        let f = drawn(&mut cards, 100, 30, leave + ms(600));
        assert_eq!(
            f.get(1, 97).expect("a cell").style.fg,
            Color::Rgb(mix(50, OK, c.term.bg)),
            "a mark is the sign, dimmer"
        );
        cards.expire(leave + Duration::from_secs(3600));
        assert!(!cards.is_empty(), "and it stays");
        assert_eq!(cards.next_frame(leave + ms(600)), None, "still");
    }

    /// What is under a note is taken back all at once, for every line it
    /// has, and all of it before any of its words come: half way at half the
    /// sign's coming, emptied by its end. And it is given back only once the
    /// words have all gone.
    #[test]
    fn what_is_under_a_note_is_taken_back_before_its_words_come() {
        let now = Instant::now();
        let c = colors();
        let mut cards = Cards::default();
        let card = Card {
            fade_ms: 100,
            ..news(1)
        };
        let leave = now + ms(690 + card.life_ms);
        cards.apply(Command::Show(card), now);
        drawn(&mut cards, 100, 30, now);
        let half = mix(50, c.term.fg, c.term.bg);
        let f = drawn(&mut cards, 100, 30, now + ms(80));
        for (row, col) in [(1, 90), (1, 96), (2, 68), (2, 90)] {
            let cell = f.get(row, col).expect("a cell");
            assert_eq!(
                (&cell.text, cell.style.fg),
                (&Text::Char('x'), Color::Rgb(half)),
                "{row},{col}: the whole of it, half way"
            );
        }
        let f = drawn(&mut cards, 100, 30, now + ms(160));
        for (row, col) in [(1, 90), (1, 96), (2, 68), (2, 90)] {
            assert_eq!(
                f.get(row, col).expect("a cell").text,
                Text::Char(' '),
                "{row},{col}: emptied, and nothing written on it yet"
            );
        }
        assert_eq!(cards.next_frame(now + ms(160)), Some(now + ms(160) + FRAME));

        // Its words gone, what they were on is still taken back, and given
        // back after.
        let f = drawn(&mut cards, 100, 30, leave + ms(340));
        assert_eq!(f.get(2, 68).expect("a cell").text, Text::Char(' '));
        assert_eq!(f.get(1, 90).expect("a cell").text, Text::Char(' '));
        let f = drawn(&mut cards, 100, 30, leave + ms(420));
        let cell = f.get(2, 68).expect("a cell");
        assert_eq!(
            (&cell.text, cell.style.fg),
            (&Text::Char('x'), Color::Rgb(half)),
            "half given back"
        );
        let f = drawn(&mut cards, 100, 30, leave + ms(500));
        assert_eq!(f.get(2, 68).expect("a cell").style, Style::default());
    }

    /// The marks are one to a row, under the notes, newest first.
    #[test]
    fn marks_are_a_row_each_under_the_notes() {
        let now = Instant::now();
        let mut cards = Cards::default();
        cards.apply(Command::Show(news(1).as_mark()), now);
        let mut failed = news(2).as_mark();
        failed.kind = Kind::Failed;
        failed.mark = Some(Kind::Failed);
        cards.apply(Command::Show(failed), now);
        let screen = painted(&mut cards, 100, 30, now);
        assert_eq!(screen[1], format!("{}!xx", "x".repeat(97)));
        assert_eq!(screen[2], format!("{}✓xx", "x".repeat(97)));

        let mut note = news(3);
        note.title = "blog".into();
        cards.apply(Command::Show(note), now);
        let screen = painted(&mut cards, 100, 30, now);
        assert_eq!(screen[1], right(100, "✓ blog  3s"));
        assert_eq!(screen[4], format!("{}!xx", "x".repeat(97)), "{screen:#?}");
        assert_eq!(screen[5], format!("{}✓xx", "x".repeat(97)));
    }

    /// News leaves the sign it is told to — a notification's, say, that is
    /// still waiting under a command's that has finished since — and a mark
    /// alone, held up with the prefix, says its words again.
    #[test]
    fn a_mark_is_the_sign_the_session_has() {
        let now = Instant::now();
        let mut cards = Cards::default();
        cards.apply(Command::Show(news(1).leaving(Some(Kind::Notified))), now);
        painted(&mut cards, 100, 30, now);
        let gone = now + ms(news(1).life_ms + 1);
        let screen = painted(&mut cards, 100, 30, gone);
        assert_eq!(screen[1], format!("{}∗xx", "x".repeat(97)), "{screen:#?}");

        assert!(cards.apply(Command::Keys { held: true }, gone));
        let screen = painted(&mut cards, 100, 30, gone);
        assert!(screen[1].ends_with("✓ api-server  9sxx"), "{screen:#?}");
        assert_eq!(screen[2], right(100, "cargo test: exited with code 0"));
        assert_eq!(screen[4], right(100, "Ctrl-Space ⏎ go there"));
    }

    /// A newer card about a session takes the older one's place: a mark at
    /// once, and a note with its words up once they go — leaving the one mark.
    #[test]
    fn the_newer_card_about_a_session_takes_the_olders_place() {
        let now = Instant::now();
        let mut cards = Cards::default();
        cards.apply(Command::Show(news(1).about("aaaaaaaa")), now);
        painted(&mut cards, 100, 30, now);
        let later = now + ms(news(1).life_ms + 1);
        cards.apply(Command::Show(news(2).about("aaaaaaaa")), later);
        assert_eq!(cards.live.len(), 1, "the mark went at once");

        cards.apply(Command::Show(news(3).about("aaaaaaaa")), later);
        let mut other = news(4).about("bbbbbbbb");
        other.title = "blog".into();
        cards.apply(Command::Show(other), later);
        let screen = painted(&mut cards, 100, 30, later);
        assert!(screen[1].contains("✓ blog"), "{screen:#?}");
        assert!(screen[4].contains("✓ api-server"), "{screen:#?}");
        assert!(
            screen[7].contains("✓ api-server"),
            "the older still says it"
        );
        assert!(cards.apply(Command::Keys { held: true }, later));
        let screen = painted(&mut cards, 100, 30, later);
        assert_eq!(
            screen.iter().filter(|r| r.contains("go there")).count(),
            1,
            "{screen:#?}"
        );

        let marks = later + ms(news(1).life_ms + 1);
        cards.apply(Command::Keys { held: false }, marks);
        cards.expire(marks);
        let screen = painted(&mut cards, 100, 30, marks);
        assert_eq!(screen[1], format!("{}✓xx", "x".repeat(97)), "{screen:#?}");
        assert_eq!(screen[2], format!("{}✓xx", "x".repeat(97)));
        assert_eq!(screen[3], "x".repeat(100), "one mark a session");
    }

    /// Where a switch landed: the session, between the one `<prefix> p` goes
    /// to and the one `<prefix> n` does; with two, the other after it; alone,
    /// its name; and under it, that it is waiting for a key.
    #[test]
    fn an_arrival_is_where_you_are_in_the_row() {
        let now = Instant::now();
        let screen = |card: Card| {
            let mut cards = Cards::default();
            cards.apply(Command::Show(card), now);
            painted(&mut cards, 100, 30, now)
        };
        let three = screen(landed("api-server", Some("nvmux"), Some("blog"), None));
        assert_eq!(three[1], right(100, "nvmux   api-server   blog"));
        assert_eq!(three[2], "x".repeat(100));
        let two = screen(landed("api-server", None, Some("blog"), None));
        assert_eq!(two[1], right(100, "api-server   blog"));
        let alone = screen(landed("api-server", None, None, None));
        assert_eq!(alone[1], right(100, "api-server"));

        let mut waiting = landed("api-server", Some("nvmux"), Some("blog"), None);
        waiting.body = Some(crate::card::WAITING.into());
        let rows = screen(waiting);
        assert_eq!(
            rows[2],
            format!("{}waiting for a key{}", "x".repeat(74), "x".repeat(9))
        );

        let mut cards = Cards::default();
        cards.apply(
            Command::Show(landed("api-server", Some("nvmux"), Some("blog"), None)),
            now,
        );
        let f = drawn(&mut cards, 100, 30, now);
        let c = colors();
        let at = |col| f.get(1, col).expect("a cell").style;
        assert_eq!((at(81).fg, at(81).bold), (Color::Rgb(c.term.fg), true));
        assert_eq!(
            (at(73).fg, at(73).bold),
            (Color::Rgb(mix(FAINT, c.term.fg, c.term.bg)), false)
        );
    }

    /// A step moves the row a name the way it went, from the moment it is up:
    /// forward, a name off the left and the next on at the right; back, the
    /// other way.
    #[test]
    fn a_step_moves_the_row_the_way_it_went() {
        let now = Instant::now();
        let step = |forward, gone: &str| {
            Some(Step {
                forward,
                gone: gone.into(),
            })
        };
        // `row` flush right, on the patch the row's names go over: from
        // `from` out to the edge of the screen.
        let patch = |from: usize, row: &str| {
            format!(
                "{}{}{row}  ",
                "x".repeat(from),
                " ".repeat(98 - from - row.width())
            )
        };
        let mut cards = Cards::default();
        let mut card = landed(
            "api-server",
            Some("nvmux"),
            Some("blog"),
            step(true, "notes"),
        );
        card.fade_ms = 100;
        cards.apply(Command::Show(card), now);
        let before = painted(&mut cards, 100, 30, now);
        assert_eq!(before[1], patch(65, "notes   nvmux   api-server"));
        assert_eq!(
            cards.next_frame(now),
            Some(now + FRAME),
            "the row moves from its first frame"
        );
        let moving = painted(&mut cards, 100, 30, now + ms(50));
        assert_eq!(
            moving[1],
            patch(65, "notes   nvmux   api-server "),
            "a column along by the next"
        );
        let after = painted(&mut cards, 100, 30, now + ms(380));
        assert_eq!(
            after[1],
            patch(65, "nvmux   api-server   blog"),
            "what it covers is all the way the names went"
        );

        let mut cards = Cards::default();
        let mut card = landed(
            "api-server",
            Some("nvmux"),
            Some("blog"),
            step(false, "notes"),
        );
        card.fade_ms = 100;
        cards.apply(Command::Show(card), now);
        let before = painted(&mut cards, 100, 30, now);
        assert_eq!(before[1], patch(65, "api-server   blog   notes"));
        let after = painted(&mut cards, 100, 30, now + ms(380));
        assert_eq!(after[1], patch(65, "nvmux   api-server   blog"));
    }

    /// A card up on another screen first goes on from where it was there: the
    /// card a switch lands with has its row as far along as it has come since
    /// the key, on its first frame on the screen it lands on as on every one
    /// after — and drawn first a while after it came, further along by that
    /// while.
    #[test]
    fn a_card_up_elsewhere_first_goes_on_from_there() {
        let now = Instant::now();
        let mut card = landed(
            "api-server",
            Some("nvmux"),
            Some("blog"),
            Some(Step {
                forward: true,
                gone: "notes".into(),
            }),
        );
        card.fade_ms = 100;
        let landed_at = |cards: &mut Cards, at| painted(cards, 100, 30, at)[1].find("api-server");

        let mut first = Cards::default();
        first.apply(Command::Show(card.clone()), now);
        assert_eq!(
            landed_at(&mut first, now),
            Some(88),
            "where the step starts"
        );

        let mut handed = Cards::default();
        handed.apply(Command::Show(card.clone().up_for(ms(100))), now);
        assert_eq!(landed_at(&mut handed, now), Some(86), "two columns along");

        let mut late = Cards::default();
        late.apply(Command::Show(card.up_for(ms(100))), now);
        assert_eq!(
            landed_at(&mut late, now + ms(80)),
            Some(83),
            "and five, drawn first 80 ms after it came"
        );
    }

    /// Where an arrival stands is where it is drawn, on every frame: what
    /// stays where it is on its cell, and the names on the row all the while
    /// all the same way along, from none of its shift on its first frame to
    /// all of it once the row has moved — the names going off and coming on
    /// not among them. The span is all the way the row's names go, out to the
    /// edge of the screen; and the line that says the session is waiting has
    /// its own.
    #[test]
    fn an_arrival_stands_where_it_is_drawn() {
        let now = Instant::now();
        let step = Some(Step {
            forward: true,
            gone: "notes".into(),
        });
        let mut card = landed("api-server", Some("nvmux"), Some("blog"), step);
        card.fade_ms = 100;
        card.body = Some(crate::card::WAITING.into());
        let footprint = arrival_footprint(&card, 100, 30);
        assert_eq!(footprint.spans, [(1, 65, 99), (2, 74, 90)]);
        assert_eq!(footprint.glyphs.len(), "waitingforakey".len());
        assert_eq!(footprint.moving.len(), 5 + 10);
        assert_eq!(footprint.moving.first(), Some(&(1, 80, 'n')));
        assert_eq!(footprint.shift, -7);
        let mut cards = Cards::default();
        cards.apply(Command::Show(card), now);
        // How far along the moving names are on a frame, if they are all on
        // their cells the same way along.
        let along = |f: &Frame| {
            (footprint.shift..=0).find(|k| {
                footprint.moving.iter().all(|&(row, col, ch)| {
                    let col = usize::try_from(col as isize + k).expect("a column");
                    f.get(row, col).expect("a cell").text == Text::Char(ch)
                })
            })
        };
        for (t, was) in [(0, Some(0)), (150, Some(-4)), (380, Some(-7))] {
            let f = drawn(&mut cards, 100, 30, now + ms(t));
            for &(row, col, ch) in &footprint.glyphs {
                assert_eq!(
                    f.get(row, col).expect("a cell").text,
                    Text::Char(ch),
                    "{row},{col} at {t} ms"
                );
            }
            assert_eq!(along(&f), was, "at {t} ms");
        }

        assert_eq!(
            arrival_footprint(&news(1), 100, 30),
            Footprint::default(),
            "news stands nowhere"
        );
        assert_eq!(
            arrival_footprint(&landed("blog", None, None, None), 20, 30),
            Footprint::default(),
            "nor on a screen too small for it"
        );
    }

    /// A name or a line too wide is cut to fit, with `…`, and the age is never
    /// what gives way; a row of names too wide is the one landed in alone.
    #[test]
    fn what_is_too_wide_is_cut_to_fit() {
        let now = Instant::now();
        let mut cards = Cards::default();
        let mut card = news(1);
        card.title = "a-session-with-a-very-long-name-indeed-and-more".into();
        card.body = Some("x".repeat(100));
        cards.apply(Command::Show(card), now);
        let screen = painted(&mut cards, 60, 10, now);
        assert!(
            screen[1].contains('…') && screen[1].ends_with("  3sxx"),
            "{screen:#?}"
        );
        assert!(screen[2].contains('…'), "{screen:#?}");
        assert_eq!(screen[1].width(), 60);
        assert_eq!(screen[2].width(), 60);

        let mut cards = Cards::default();
        let long = "a-session-with-a-long-name";
        cards.apply(
            Command::Show(landed("here", Some(long), Some(long), None)),
            now,
        );
        let screen = painted(&mut cards, 60, 10, now);
        assert_eq!(screen[1], right(60, "here"));
    }

    /// Wide characters take their two cells, and what they would cut in half
    /// is mended.
    #[test]
    fn a_wide_name_takes_two_cells_a_character() {
        let now = Instant::now();
        let mut cards = Cards::default();
        cards.apply(Command::Show(landed("日本語", None, None, None)), now);
        let screen = painted(&mut cards, 30, 8, now);
        assert!(screen[1].ends_with("日本語xx"), "{screen:#?}");
        assert_eq!(screen[1].width(), 30);
    }

    /// The age counts on from what the relay said, and a frame is asked for
    /// when it next ticks.
    #[test]
    fn the_age_counts_on() {
        let now = Instant::now();
        let mut cards = Cards::default();
        cards.apply(Command::Show(news(1)), now);
        painted(&mut cards, 100, 30, now);
        let later = now + ms(2100);
        let screen = painted(&mut cards, 100, 30, later);
        assert!(screen[1].ends_with(" 5sxx"), "{screen:#?}");
        assert_eq!(cards.next_frame(later), Some(now + Duration::from_secs(3)));
    }

    /// The editor's text under a note dims the nearer it is to one of its
    /// lines, a row counting as two columns; a reach away it is as it was,
    /// and so are the status line and the command line.
    #[test]
    fn what_is_under_a_note_dims_the_nearer_it_is() {
        let now = Instant::now();
        let c = colors();
        let mut cards = Cards::default();
        cards.apply(Command::Show(news(1)), now);
        let f = drawn(&mut cards, 100, 30, now);
        let fg = |row, col| match f.get(row, col).expect("a cell").style.fg {
            Color::Rgb(Rgb(r, _, _)) => r,
            Color::Default => c.term.fg.0,
            other => panic!("{other:?}"),
        };
        assert!(fg(2, 67) < fg(2, 62), "nearer is dimmer");
        assert!(fg(2, 62) < fg(2, 58));
        assert_eq!(
            f.get(2, 50).expect("a cell").style,
            Style::default(),
            "out of reach"
        );
        assert!(
            fg(2, 66) <= fg(3, 80) && fg(3, 80) <= fg(2, 65),
            "a row down is about two columns along: {} {} {}",
            fg(2, 66),
            fg(3, 80),
            fg(2, 65)
        );
        assert_eq!(
            f.get(1, 40).expect("a cell").style,
            Style::default(),
            "out of reach"
        );
        assert_eq!(
            f.get(28, 97).expect("a cell").style,
            Style::default(),
            "the status line"
        );
        assert_eq!(f.get(29, 97).expect("a cell").style, Style::default());
        assert_eq!(
            fg(1, 99),
            mix(
                percent(1.0 - PEAK * (1.0 - ease(2.0 / REACH))),
                c.term.fg,
                c.term.bg
            )
            .0
        );
        assert_eq!(
            f.get(1, 83).expect("a cell").text,
            Text::Char(' '),
            "emptied under the line"
        );
    }

    /// A terminal's own background — a transparent one — stays its own under
    /// the dimming and under the note.
    #[test]
    fn a_transparent_background_stays_transparent() {
        let now = Instant::now();
        let mut cards = Cards::default();
        cards.apply(Command::Show(news(1)), now);
        let f = drawn(&mut cards, 100, 30, now);
        for col in [70, 81, 82, 90, 99] {
            assert_eq!(
                f.get(1, col).expect("a cell").style.bg,
                Color::Default,
                "{col}"
            );
        }
    }

    /// An arrival stays its life and goes; one let go of goes at once; a clear
    /// takes them all, and lets the prefix go.
    #[test]
    fn a_card_goes_when_its_time_is_up_or_it_is_let_go_of() {
        let now = Instant::now();
        let mut cards = Cards::default();
        let card = landed("blog", None, None, None);
        let life = ms(card.life_ms);
        cards.apply(Command::Show(card), now);
        painted(&mut cards, 100, 30, now);
        assert!(painted(&mut cards, 100, 30, now + life - ms(1))[1].contains("blog"));
        cards.expire(now + life);
        assert!(cards.is_empty(), "past its life");

        cards.apply(Command::Show(news(2)), now);
        assert!(cards.apply(Command::Drop { id: 2 }, now + Duration::from_secs(1)));
        cards.expire(now + Duration::from_secs(1));
        assert!(cards.is_empty(), "let go of");

        cards.apply(Command::Show(news(3)), now);
        cards.apply(Command::Keys { held: true }, now);
        assert!(cards.apply(Command::Clear, now));
        assert!(cards.is_empty());
        assert!(!cards.apply(Command::Clear, now), "nothing left to clear");
        cards.apply(Command::Show(news(4)), now);
        let screen = painted(&mut cards, 100, 30, now);
        assert!(
            screen.iter().all(|r| !r.contains("go there")),
            "the prefix went with the clear: {screen:#?}"
        );
    }

    /// Timed from when it is first drawn, which may be a while after it came:
    /// the editor drawing its first screen.
    #[test]
    fn a_card_is_timed_from_when_it_is_first_drawn() {
        let now = Instant::now();
        let mut cards = Cards::default();
        let card = landed("blog", None, None, None);
        let life = ms(card.life_ms);
        cards.apply(Command::Show(card), now);
        assert_eq!(cards.next_frame(now), None, "drawn when the frame comes");
        let first = now + Duration::from_secs(5);
        assert!(painted(&mut cards, 100, 30, first)[1].contains("blog"));
        cards.expire(first + life - ms(1));
        assert!(!cards.is_empty());
    }

    /// No card on a screen too small to hold one.
    #[test]
    fn no_card_on_a_screen_too_small() {
        let now = Instant::now();
        let mut cards = Cards::default();
        cards.apply(Command::Show(news(1)), now);
        let screen = painted(&mut cards, 20, 10, now);
        assert!(screen.iter().all(|r| r == &"x".repeat(20)));
    }
}
