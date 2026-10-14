//! The cards nvmux shows in the corner of the screen (see [`crate::card`]):
//! kept here as they come, and drawn over the frame the client composes.
//!
//! Over it, and last — after the windows, the floats and the cursor's effects
//! — so that nothing of the editor's is ever drawn over a card. And drawn from
//! what the client keeps rather than written once: the editor clearing its
//! screen (`:mode`, a resize) and drawing it again is a frame like any other,
//! and the card is on it.
//!
//! # Where, and what
//!
//! Top right, a column in from the edge and a row down, off the tab line;
//! the newest on top and the others below it, as many as there is room for
//! above the status line and the command line, which a card never covers. One
//! with news:
//!
//! ```text
//! ╭─ ✓ api-server ──────────────────────── 3s ─╮
//! │ cargo test: exited with code 0             │
//! │ Ctrl-Space ⏎ go there                      │
//! ╰────────────────────────────────────────────╯
//! ```
//!
//! the border dim; on it, the sign and the session, in `[theme] color` or the
//! editor's text colour, and its age counting on beside them, dim; what
//! happened in the editor's text colour; and the key that acts on it dim. A
//! theme colour leaves the border dim, as nvmux's own screens leave what is
//! dim on them. The key is on one card only:
//! the newest that goes somewhere, which is the one it acts on (see
//! [`crate::card::Desk::go`]); those below it go without, a row shorter, and
//! the next has it once that one is acted on. One that says where a switch
//! landed is the session's name and nothing else — and that the session is
//! waiting for a key, where it is.
//!
//! # Fading
//!
//! As nvmux's own screens fade, and for as long (see [`crate::fade`]): what is
//! under the card fades to the card's background over the first half, and the
//! card fades up out of it over the second; going, the other way round. A card
//! that says where a switch landed does not fade in: it is drawn before the
//! session's screen dissolves in, and comes in with it.

use std::time::{Duration, Instant};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::compose::{Frame, Out};
use super::grid::Text;
use super::style::{mix, Color, Colors, Style};
use crate::card::{Card, Command, Kind};
use crate::palette::Rgb;

/// The widest a card is: the width the design was drawn at, which takes a
/// command and its exit status on one line.
const WIDTH: usize = 46;

/// The smallest screen a card goes up on.
const MIN_COLS: usize = 24;
const MIN_ROWS: usize = 6;

/// The row the first card's top is on: one down, off the tab line.
const TOP: usize = 1;

/// Columns between a card and the right edge of the screen.
const MARGIN: usize = 1;

/// Rows at the bottom no card covers: the status line and the command line.
const BOTTOM: usize = 2;

/// How often a fading card is drawn again: the client's own frame rate.
const FRAME: Duration = Duration::from_millis(16);

/// How much of the text colour dim text keeps, the rest the background: the
/// grey the design was drawn in, between Neovim's default foreground and
/// background.
const DIM: u8 = 58;

/// The cards up, newest first.
#[derive(Debug, Default)]
pub struct Cards {
    live: Vec<Live>,
}

#[derive(Debug)]
struct Live {
    card: Card,
    shown_at: Instant,
    /// Let go of before its time — acted on — and fading out from then.
    dropped_at: Option<Instant>,
}

impl Live {
    fn fade(&self) -> Duration {
        Duration::from_millis(self.card.fade_ms)
    }

    fn fade_in(&self) -> Duration {
        if self.card.fade_in {
            self.fade()
        } else {
            Duration::ZERO
        }
    }

    /// When it starts to go.
    fn leaving_at(&self) -> Instant {
        self.dropped_at.unwrap_or_else(|| {
            self.shown_at + self.fade_in() + Duration::from_millis(self.card.life_ms)
        })
    }

    fn gone_at(&self) -> Instant {
        self.leaving_at() + self.fade()
    }

    /// How much of it is drawn, from 0 to 1.
    fn strength(&self, now: Instant) -> f32 {
        let fraction = |since: Instant, over: Duration| {
            if over.is_zero() {
                1.0
            } else {
                (now.saturating_duration_since(since).as_secs_f32() / over.as_secs_f32()).min(1.0)
            }
        };
        let coming = fraction(self.shown_at, self.fade_in());
        let leaving = self.leaving_at();
        if now < leaving {
            return coming;
        }
        let going = if self.fade().is_zero() {
            0.0
        } else {
            1.0 - fraction(leaving, self.fade())
        };
        coming.min(going).max(0.0)
    }

    /// Whether it is part way through a fade at `now`.
    fn fading(&self, now: Instant) -> bool {
        now < self.shown_at + self.fade_in() || (now >= self.leaving_at() && now < self.gone_at())
    }

    /// How old the news is, by now.
    fn age(&self, now: Instant) -> Option<u64> {
        let since = now.saturating_duration_since(self.shown_at).as_secs();
        self.card.age.map(|age| age + since)
    }
}

impl Cards {
    /// Do what the relay said. Says whether anything changed on the screen.
    pub fn apply(&mut self, cmd: Command, now: Instant) -> bool {
        match cmd {
            Command::Show(card) => {
                self.live.retain(|l| l.card.id != card.id);
                self.live.insert(
                    0,
                    Live {
                        card,
                        shown_at: now,
                        dropped_at: None,
                    },
                );
                true
            }
            Command::Drop { id } => {
                let mut changed = false;
                for l in self.live.iter_mut().filter(|l| l.card.id == id) {
                    if now < l.leaving_at() {
                        l.dropped_at = Some(now);
                        changed = true;
                    }
                }
                changed
            }
            Command::Clear => !std::mem::take(&mut self.live).is_empty(),
        }
    }

    /// Let go of the cards that are gone by `now`.
    pub fn expire(&mut self, now: Instant) {
        self.live.retain(|l| l.gone_at() > now);
    }

    /// Whether any card is up.
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// When the cards next look different from how a frame drawn at `drawn`
    /// showed them — a fade's next step, the moment one starts to go or is
    /// gone, the next tick of an age — and so when a frame is wanted: due once
    /// that is past.
    pub fn next_frame(&self, drawn: Instant) -> Option<Instant> {
        self.live
            .iter()
            .flat_map(|l| {
                let fading = l.fading(drawn).then_some(drawn + FRAME);
                let leaving = Some(l.leaving_at()).filter(|&at| at > drawn);
                let gone = Some(l.gone_at()).filter(|&at| at > drawn);
                let tick = l.card.age.map(|_| {
                    let since = drawn.saturating_duration_since(l.shown_at);
                    l.shown_at + Duration::from_secs(since.as_secs() + 1)
                });
                [fading, leaving, gone, tick]
            })
            .flatten()
            .min()
    }

    /// Draw the cards up at `now` over `frame`: on `base`, the editor's own
    /// colours, resolved through `colors`.
    pub fn paint(&self, frame: &mut Frame, base: Style, colors: &Colors, now: Instant) {
        if frame.width < MIN_COLS || frame.height < MIN_ROWS {
            return;
        }
        let pens = Pens::new(base, colors);
        let widest = WIDTH.min(frame.width - MARGIN - 1);
        let bottom = frame.height - BOTTOM;
        // The one card the key is drawn on: the newest that names it and has
        // not been acted on, which is the one it acts on.
        let keyed = self
            .live
            .iter()
            .position(|l| l.card.footer.is_some() && l.dropped_at.is_none());
        let mut top = TOP;
        for (i, l) in self.live.iter().enumerate() {
            let strength = l.strength(now);
            if strength <= 0.0 {
                continue;
            }
            let rows = render(&l.card, l.age(now), widest, &pens, keyed == Some(i));
            if top + rows.len() > bottom {
                break;
            }
            let width = rows.first().map_or(0, Vec::len);
            let left = frame.width - MARGIN - width;
            for (r, row) in rows.iter().enumerate() {
                for (c, cell) in row.iter().enumerate() {
                    if let Some(under) = frame.get_mut(top + r, left + c) {
                        *under = blend(under, cell, strength, &pens, colors);
                    }
                }
            }
            top += rows.len();
        }
    }
}

/// The styles a card is drawn in.
struct Pens {
    border: Style,
    title: Style,
    text: Style,
    dim: Style,
    /// The card's background, as it shows.
    bg: Rgb,
}

impl Pens {
    fn new(base: Style, colors: &Colors) -> Self {
        let fg = colors.visual_fg(&base);
        let bg = colors.visual_bg(&base);
        // The editor's own background, the terminal's where that is the
        // editor's: a transparent terminal stays transparent under a card.
        let back = if base.reverse {
            Color::Rgb(bg)
        } else {
            base.bg
        };
        let plain = Style {
            bg: back,
            ..Style::default()
        };
        let dim = Style {
            fg: Color::Rgb(mix(DIM, fg, bg)),
            ..plain
        };
        Self {
            // Dim, as the age and the key are: the frame is not what it says.
            border: dim,
            title: Style {
                bold: true,
                ..plain
            },
            text: Style {
                fg: Color::Rgb(fg),
                ..plain
            },
            dim,
            bg,
        }
    }

    /// `style` in the card's ink, where it has one.
    fn inked(style: Style, card: &Card) -> Style {
        match card.ink {
            Some((r, g, b)) => Style {
                fg: Color::Rgb(Rgb(r, g, b)),
                ..style
            },
            None => style,
        }
    }
}

/// `card`'s rows, each `width` cells or narrower for an arrival, which is as
/// wide as its name; with the key that acts on it only where `keyed`.
fn render(card: &Card, age: Option<u64>, widest: usize, pens: &Pens, keyed: bool) -> Vec<Vec<Out>> {
    // Under a theme colour too, as what is dim on nvmux's own screens stays
    // dim there: the colour is the sign's and the session's.
    let border = pens.border;
    let title = Pens::inked(pens.title, card);
    if card.kind == Kind::Arrival {
        let waiting = card.body.as_deref().map(|b| format!("  {b}"));
        let want = card.title.width() + waiting.as_deref().map_or(0, str::width) + 4;
        let width = want.clamp(8, widest);
        let inner = width - 4;
        let name = fit(&card.title, inner);
        let mut middle = Row::new(width);
        middle.text("│ ", border);
        middle.text(&name, title);
        if let Some(w) = &waiting {
            middle.text(&fit(w, inner - name.width()), pens.dim);
        }
        middle.pad_to(width - 2, pens.text);
        middle.text(" │", border);
        return vec![
            edge(width, '╭', '╮', border),
            middle.cells,
            edge(width, '╰', '╯', border),
        ];
    }

    let width = widest;
    let inner = width - 4;
    // `╭─ ✓ name ───── 3s ─╮`: the name gives way to the age.
    let sign = card
        .kind
        .sign()
        .map(|s| format!("{s} "))
        .unwrap_or_default();
    let age = age.map(|a| format!(" {} ", crate::ui::note::age(Duration::from_secs(a))));
    let room = width - 4 - 2 - age.as_deref().map_or(0, str::width) - 1;
    let head = fit(&format!("{sign}{}", card.title), room.max(1));
    let mut top = Row::new(width);
    top.text("╭─ ", border);
    top.text(&head, title);
    top.text(" ", border);
    let tail = 2 + age.as_deref().map_or(0, str::width);
    top.fill_to(width - tail, '─', border);
    if let Some(a) = &age {
        top.text(a, pens.dim);
    }
    top.text("─╮", border);

    let mut rows = vec![top.cells];
    let footer = card.footer.as_ref().filter(|_| keyed);
    for (line, pen) in [(card.body.as_ref(), pens.text), (footer, pens.dim)] {
        if let Some(line) = line {
            let mut row = Row::new(width);
            row.text("│ ", border);
            row.text(&fit(line, inner), pen);
            row.pad_to(width - 2, pens.text);
            row.text(" │", border);
            rows.push(row.cells);
        }
    }
    rows.push(edge(width, '╰', '╯', border));
    rows
}

/// A border row: `left`, a run of `─`, `right`.
fn edge(width: usize, left: char, right: char, pen: Style) -> Vec<Out> {
    let mut row = Row::new(width);
    row.text(&left.to_string(), pen);
    row.fill_to(width - 1, '─', pen);
    row.text(&right.to_string(), pen);
    row.cells
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

/// A row of a card being written, a cell at a time.
struct Row {
    cells: Vec<Out>,
    width: usize,
}

impl Row {
    fn new(width: usize) -> Self {
        Self {
            cells: Vec::with_capacity(width),
            width,
        }
    }

    /// `s`, a character to a cell or two, as far as the row goes.
    fn text(&mut self, s: &str, style: Style) {
        for c in s.chars() {
            let w = c.width().unwrap_or(0);
            if w == 0 {
                continue;
            }
            if self.cells.len() + w > self.width {
                break;
            }
            self.cells.push(Out {
                text: Text::Char(c),
                style,
                wide: w == 2,
            });
            if w == 2 {
                self.cells.push(Out {
                    text: Text::Half,
                    style,
                    wide: false,
                });
            }
        }
    }

    fn fill_to(&mut self, col: usize, c: char, style: Style) {
        while self.cells.len() < col.min(self.width) {
            self.cells.push(Out {
                text: Text::Char(c),
                style,
                wide: false,
            });
        }
    }

    fn pad_to(&mut self, col: usize, style: Style) {
        self.fill_to(col, ' ', style);
    }
}

/// What a cell of the frame shows with `card` over `under` at `strength`.
fn blend(under: &Out, card: &Out, strength: f32, pens: &Pens, colors: &Colors) -> Out {
    if strength >= 1.0 {
        return card.clone();
    }
    let percent = |x: f32| (x.clamp(0.0, 1.0) * 100.0).round() as u8;
    if strength < 0.5 {
        // What is under it, going to the card's background.
        let keep = percent(1.0 - strength * 2.0);
        let style = Style {
            fg: Color::Rgb(mix(keep, colors.visual_fg(&under.style), pens.bg)),
            bg: Color::Rgb(mix(keep, colors.visual_bg(&under.style), pens.bg)),
            reverse: false,
            sp: Color::Default,
            link: 0,
            ..under.style
        };
        return Out {
            style,
            ..under.clone()
        };
    }
    // The card, coming up out of its background.
    let show = percent((strength - 0.5) * 2.0);
    let style = Style {
        fg: Color::Rgb(mix(show, colors.visual_fg(&card.style), pens.bg)),
        ..card.style
    };
    Out {
        style,
        ..card.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Card;
    use crate::palette::Palette;

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

    fn news(id: u64) -> Card {
        Card {
            fade_ms: 0,
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

    fn painted(cards: &Cards, w: usize, h: usize, now: Instant) -> Vec<String> {
        let mut f = frame(w, h);
        cards.paint(&mut f, Style::default(), &colors(), now);
        f.mend();
        rows(&f)
    }

    /// Option A as drawn: top right, a row down and a column in, the sign,
    /// the session and the age in the border, the news, and the key.
    #[test]
    fn a_card_with_news_is_drawn_in_the_corner() {
        let now = Instant::now();
        let mut cards = Cards::default();
        cards.apply(Command::Show(news(1)), now);
        let screen = painted(&cards, 100, 30, now);
        assert_eq!(screen[0], "x".repeat(100), "the tab line is left alone");
        let left = "x".repeat(53);
        assert_eq!(
            screen[1],
            format!("{left}╭─ ✓ api-server {} 3s ─╮x", "─".repeat(24))
        );
        assert_eq!(
            screen[2],
            format!("{left}│ {:<42} │x", "cargo test: exited with code 0")
        );
        assert_eq!(
            screen[3],
            format!("{left}│ {:<42} │x", "Ctrl-Space ⏎ go there")
        );
        assert_eq!(screen[4], format!("{left}╰{}╯x", "─".repeat(44)));
        assert_eq!(screen[5], "x".repeat(100));
        for row in &screen[1..5] {
            assert_eq!(row.width(), 100, "{row:?}");
        }
    }

    /// The newest on top, the others below it, as many as fit above the
    /// status line and the command line.
    #[test]
    fn cards_stack_newest_first_above_the_bottom_rows() {
        let now = Instant::now();
        let mut cards = Cards::default();
        for id in 1..=4 {
            let mut card = news(id);
            card.title = format!("s{id}");
            cards.apply(Command::Show(card), now);
        }
        let screen = painted(&cards, 80, 12, now);
        assert!(screen[1].contains("✓ s4 "), "{screen:#?}");
        assert!(screen[5].contains("✓ s3 "), "{screen:#?}");
        assert!(
            screen
                .iter()
                .all(|r| !r.contains("s2") && !r.contains("s1")),
            "a card that does not fit above the last two rows is left off: {screen:#?}"
        );
        assert_eq!(screen[10], "x".repeat(80));
        assert_eq!(screen[11], "x".repeat(80));
    }

    /// The key is on one card only: the newest that names it, past a newer
    /// one that goes nowhere — the card `<prefix> Enter` acts on. Those below
    /// go without it, a row shorter; and once it is acted on, it goes without
    /// too as it fades, and the next card has the key.
    #[test]
    fn only_the_card_the_key_acts_on_names_it() {
        let now = Instant::now();
        let mut cards = Cards::default();
        for id in 1..=3 {
            let mut card = news(id);
            card.title = format!("s{id}");
            card.fade_ms = 100;
            cards.apply(Command::Show(card), now);
        }
        let mut nowhere = news(4);
        nowhere.title = "here".into();
        nowhere.footer = None;
        cards.apply(Command::Show(nowhere), now);
        let keyed = |screen: &[String]| -> Vec<usize> {
            (0..screen.len())
                .filter(|&r| screen[r].contains("go there"))
                .collect()
        };

        let up = now + Duration::from_millis(200);
        let screen = painted(&cards, 100, 30, up);
        assert!(screen[1].contains("✓ here "), "{screen:#?}");
        assert!(screen[4].contains("✓ s3 "), "{screen:#?}");
        assert!(screen[8].contains("✓ s2 "), "{screen:#?}");
        assert!(screen[11].contains("✓ s1 "), "{screen:#?}");
        assert_eq!(keyed(&screen), [6], "{screen:#?}");
        assert_eq!(
            screen[6],
            format!("{}│ {:<42} │x", "x".repeat(53), "Ctrl-Space ⏎ go there")
        );

        assert!(cards.apply(Command::Drop { id: 3 }, up));
        let screen = painted(&cards, 100, 30, up + Duration::from_millis(25));
        assert!(screen[4].contains("✓ s3 "), "still fading: {screen:#?}");
        assert!(screen[7].contains("✓ s2 "), "{screen:#?}");
        assert_eq!(keyed(&screen), [9], "{screen:#?}");
    }

    /// Where a switch landed: the name, boxed, as wide as it is; and that the
    /// session waits for a key, where it does.
    #[test]
    fn an_arrival_is_a_boxed_name() {
        let now = Instant::now();
        let mut cards = Cards::default();
        cards.apply(Command::Show(Card::arrival(1, "blog", false)), now);
        let screen = painted(&cards, 40, 10, now);
        let left = "x".repeat(40 - 1 - 8);
        assert_eq!(screen[1], format!("{left}╭──────╮x"));
        assert_eq!(screen[2], format!("{left}│ blog │x"));
        assert_eq!(screen[3], format!("{left}╰──────╯x"));

        cards.apply(Command::Show(Card::arrival(2, "blog", true)), now);
        let screen = painted(&cards, 40, 10, now);
        assert!(screen[2].contains("│ blog  press a key │"), "{screen:#?}");
    }

    /// A name or a line too wide for the card is cut to fit, with `…`; the age
    /// is never what gives way.
    #[test]
    fn what_is_too_wide_is_cut_to_fit() {
        let now = Instant::now();
        let mut cards = Cards::default();
        let mut card = news(1);
        card.title = "a-session-with-a-very-long-name-indeed-and-more".into();
        card.body = Some("x".repeat(100));
        cards.apply(Command::Show(card), now);
        let screen = painted(&cards, 60, 10, now);
        assert!(
            screen[1].contains("…") && screen[1].contains(" 3s ─╮"),
            "{screen:#?}"
        );
        assert!(screen[2].contains("…"), "{screen:#?}");
        assert_eq!(screen[1].width(), 60);
        assert_eq!(screen[2].width(), 60);
    }

    /// Wide characters take their two cells, and what they would cut in half
    /// is mended.
    #[test]
    fn a_wide_name_takes_two_cells_a_character() {
        let now = Instant::now();
        let mut cards = Cards::default();
        cards.apply(Command::Show(Card::arrival(1, "日本語", false)), now);
        let screen = painted(&cards, 30, 8, now);
        assert!(screen[2].contains("│ 日本語 │"), "{screen:#?}");
        assert_eq!(screen[2].width(), 30);
    }

    /// The age counts on from what the relay said, and a frame is asked for
    /// when it next ticks.
    #[test]
    fn the_age_counts_on() {
        let now = Instant::now();
        let mut cards = Cards::default();
        cards.apply(Command::Show(news(1)), now);
        let later = now + Duration::from_millis(2100);
        let screen = painted(&cards, 100, 30, later);
        assert!(screen[1].contains(" 5s ─╮"), "{screen:#?}");
        assert_eq!(cards.next_frame(later), Some(now + Duration::from_secs(3)));
    }

    /// A card stays its life and goes; one let go of goes at once; a clear
    /// takes them all.
    #[test]
    fn a_card_goes_when_its_time_is_up_or_it_is_let_go_of() {
        let now = Instant::now();
        let mut cards = Cards::default();
        cards.apply(Command::Show(news(1)), now);
        let life = Duration::from_millis(news(1).life_ms);
        assert!(painted(&cards, 100, 30, now + life - Duration::from_millis(1))[1].contains('╭'));
        cards.expire(now + life);
        assert!(cards.is_empty(), "past its life");

        cards.apply(Command::Show(news(2)), now);
        assert!(cards.apply(Command::Drop { id: 2 }, now + Duration::from_secs(1)));
        cards.expire(now + Duration::from_secs(1));
        assert!(cards.is_empty(), "let go of");

        cards.apply(Command::Show(news(3)), now);
        assert!(cards.apply(Command::Clear, now));
        assert!(cards.is_empty());
        assert!(!cards.apply(Command::Clear, now), "nothing left to clear");
    }

    /// Fading in, what is under the card goes to the card's background over
    /// the first half, and the card comes up out of it over the second.
    #[test]
    fn a_card_fades_up_out_of_what_is_under_it() {
        let now = Instant::now();
        let mut cards = Cards::default();
        let card = Card {
            fade_ms: 100,
            ..news(1)
        };
        cards.apply(Command::Show(card), now);
        let quarter = painted(&cards, 100, 30, now + Duration::from_millis(25));
        assert_eq!(quarter[1], "x".repeat(100), "the text under it, fading");
        let three = painted(&cards, 100, 30, now + Duration::from_millis(75));
        assert!(three[1].contains('╭'), "the card, coming up");
        assert_eq!(
            cards.next_frame(now + Duration::from_millis(25)),
            Some(now + Duration::from_millis(25) + FRAME)
        );

        let mut f = frame(100, 30);
        cards.paint(
            &mut f,
            Style::default(),
            &colors(),
            now + Duration::from_millis(75),
        );
        let Some(Out { style, .. }) = f.get(2, 55) else {
            panic!("no cell");
        };
        let c = colors();
        assert_eq!(
            style.fg,
            Color::Rgb(mix(50, c.term.fg, c.term.bg)),
            "half way up"
        );
    }

    /// The border is dim, as the age and the key are, and stays dim under a
    /// theme colour, which goes to the sign and the session on it instead.
    #[test]
    fn the_border_is_dim_and_the_name_on_it_is_not() {
        let now = Instant::now();
        let c = colors();
        let dim = Color::Rgb(mix(DIM, c.term.fg, c.term.bg));
        let at = |cards: &Cards, r: usize, col: usize| {
            let mut f = frame(100, 30);
            cards.paint(&mut f, Style::default(), &c, now);
            let cell = f.get(r, col).expect("a cell");
            (cell.style.fg, cell.style.bold)
        };

        let mut cards = Cards::default();
        cards.apply(Command::Show(news(1)), now);
        for (r, col) in [(1, 53), (1, 54), (1, 98), (2, 53), (2, 98), (4, 60)] {
            assert_eq!(at(&cards, r, col).0, dim, "the border at {r},{col}");
        }
        let (fg, bold) = at(&cards, 1, 58);
        assert!(bold && fg != dim, "the session's name is not dim: {fg:?}");

        let ink = (0x7a, 0xa2, 0xf7);
        cards.apply(
            Command::Show(Card {
                ink: Some(ink),
                ..news(2)
            }),
            now,
        );
        assert_eq!(at(&cards, 1, 53).0, dim, "a theme colour leaves it dim");
        assert_eq!(
            at(&cards, 1, 58).0,
            Color::Rgb(Rgb(ink.0, ink.1, ink.2)),
            "and is the name's"
        );
    }

    /// No card on a screen too small to hold one.
    #[test]
    fn no_card_on_a_screen_too_small() {
        let now = Instant::now();
        let mut cards = Cards::default();
        cards.apply(Command::Show(news(1)), now);
        let screen = painted(&cards, 20, 10, now);
        assert!(screen.iter().all(|r| r == &"x".repeat(20)));
    }
}
