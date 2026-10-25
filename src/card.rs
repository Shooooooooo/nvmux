//! Cards nvmux shows in the margin of the session in front: which session you
//! landed in, and news from the others (a command in one of their terminals
//! finished `✓` or failed `!`, or a program asked for a notification `∗`),
//! plus news from the front session's own off-screen terminals. A news card
//! shows its text, then leaves its sign as the session's mark until the
//! session is opened.
//!
//! # Drawn by the client
//!
//! Nothing goes to the session's server, so a card is up as soon as nvmux
//! knows it, whatever the link. nvmux's client draws it
//! ([`crate::client::cards`]) over the frame it composes from the editor's
//! grids, so editor redraws (`:mode` included) redraw it too. Neovim's own
//! client (`[client] ui = "nvim"`) cannot be asked and draws none.
//!
//! # On the client's input
//!
//! The relay sends commands in the client's input, as it does a size change
//! made while parked: an APC opened by [`OPEN`], which no terminal sends and
//! the client keeps for itself (see [`crate::client::input`]). In order with
//! the input passed on, never inside a paste or a terminal sequence (see
//! [`Forwarded`]). The pty holds input untouched until the client reads it,
//! so an early card waits (see `pty::take_input_as_written`). The payload is
//! a [`Command`] as hex-encoded JSON, so nothing in it (`ESC`, `BEL`, a lone
//! C1 byte) can end the sequence or reach the terminal.
//!
//! # From the notifier
//!
//! Other sessions' news is read on its own thread ([`crate::tell::Notifier`]),
//! which posts cards on the [`board`]. The relay in front wakes on the board's
//! pipe, takes the posts for its own session and drops the rest.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Opens a command's sequence: an APC (`ESC _`) and a byte no terminal's APC
/// starts with (kitty's graphics replies, the only APCs terminals send, start
/// with `G`).
pub const OPEN: &[u8] = b"\x1b_~";

/// The byte after `ESC _` that says the APC is nvmux's.
pub const OPENER: u8 = b'~';

/// The string terminator.
pub const CLOSE: &[u8] = b"\x1b\\";

/// How long the arrival card stays at full strength. It sits in the margin,
/// not over the text, so it can outlast [`crate::announce`]'s one second.
pub const ARRIVAL_LIFE: Duration = Duration::from_millis(1500);

/// How long a card with news stays at full strength: long enough to read two
/// lines and decide whether to go.
pub const NEWS_LIFE: Duration = Duration::from_secs(6);

/// An arrival's body for a session waiting for a key, which its blank screen
/// would not say.
pub const WAITING: &str = "waiting for a key";

/// The most characters of a title or a line that are kept: more than any card
/// is wide.
const MAX_TEXT: usize = 200;

/// What a card is about: its sign, and how long it stays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Which session you landed in.
    Arrival,
    /// A notification a program in a terminal asked for.
    Notified,
    /// A command in a terminal failed, or a progress message did.
    Failed,
    /// A command in a terminal finished.
    Finished,
}

impl Kind {
    /// The picker's sign for it (see [`crate::ui::note`]); none for an arrival.
    pub fn sign(self) -> Option<char> {
        match self {
            Kind::Arrival => None,
            Kind::Notified => Some(crate::ui::note::mark(crate::notes::Kind::Notified)),
            Kind::Failed => Some(crate::ui::note::mark(crate::notes::Kind::Failed)),
            Kind::Finished => Some(crate::ui::note::mark(crate::notes::Kind::Finished)),
        }
    }
}

/// One card: what the client draws, and for how long.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    /// Unique among the cards one relay sends: what [`Command::Drop`] names.
    pub id: u64,
    pub kind: Kind,
    /// The session's name; for the session in front, `here` and where its
    /// terminal is.
    pub title: String,
    /// What happened, for news; for an arrival, [`WAITING`] or nothing.
    pub body: Option<String>,
    /// The key that acts on the card and what it does, if any. Drawn while the
    /// prefix is held ([`Command::Keys`]), only on the newest card up that has
    /// one, which is the one the key acts on ([`Desk::go`]).
    pub footer: Option<String>,
    /// How old the news was when the card was sent, in seconds; the client
    /// counts on from there. `None` for an arrival.
    pub age: Option<u64>,
    /// How long its words stay at full strength, in milliseconds.
    pub life_ms: u64,
    /// nvmux's fade one way in milliseconds (see [`crate::fade`]), 0 if off;
    /// times the card's entry and exit.
    pub fade_ms: u64,
    /// `[style] color` (see [`crate::theme`]) for its sign and session name;
    /// `None` for the editor's colours.
    pub ink: Option<(u8, u8, u8)>,
    /// `NO_COLOR`: the sign in the editor's text colour, not its colour for
    /// the kind.
    #[serde(default)]
    pub plain: bool,
    /// The session the news is about, by id, if another's; for a terminal of
    /// the session in front, which one. A newer card about the same replaces
    /// this one. `None` for an arrival.
    #[serde(default)]
    pub about: Option<String>,
    /// The sign left once the words have gone, kept until the card is
    /// dropped: the session's mark, as the picker shows it (a notification or
    /// failure outranks a later finish). `None` leaves none.
    #[serde(default)]
    pub mark: Option<Kind>,
    /// Only the mark, words unsaid: for news older than this screen, already
    /// shown on another. Holding the prefix shows them on the card the key
    /// acts on.
    #[serde(default)]
    pub only_mark: bool,
    /// For an arrival: its place among the sessions `<prefix> n` and
    /// `<prefix> p` step through. Boxed, as only an arrival has one.
    #[serde(default)]
    pub row: Option<Box<Row>>,
    /// How long it has already been up on another screen, in milliseconds.
    /// The card a switch lands with is first drawn by the departing session's
    /// client (see [`crate::pty::relay`]), and the arriving client continues
    /// from there. 0 for a card on its first screen.
    #[serde(default)]
    pub up_ms: u64,
}

/// Where a switch landed among the sessions `<prefix> n` and `<prefix> p`
/// step through: the names either side and, for a step, its direction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// Where `<prefix> p` goes, unless that is also `next` (two sessions).
    pub prev: Option<String>,
    /// Where `<prefix> n` goes.
    pub next: Option<String>,
    /// For a `<prefix> n` or `<prefix> p` switch among three or more
    /// sessions, which shifts the row one name.
    pub step: Option<Step>,
}

/// A step along the [`Row`]: its direction and the name leaving the row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// True for `<prefix> n`, false for `<prefix> p`.
    pub forward: bool,
    /// The session beyond the one left, on the far side from the one landed
    /// in: in the row before the step, not after.
    pub gone: String,
}

impl Card {
    /// The card that says a switch landed in `name` — waiting for a key, if
    /// `waiting`.
    pub fn arrival(id: u64, name: &str, waiting: bool) -> Self {
        Self {
            id,
            kind: Kind::Arrival,
            title: clean(name),
            body: waiting.then(|| WAITING.to_string()),
            footer: None,
            age: None,
            life_ms: millis(ARRIVAL_LIFE),
            fade_ms: fade_ms(),
            ink: ink(),
            plain: plain(),
            about: None,
            mark: None,
            only_mark: false,
            row: None,
            up_ms: 0,
        }
    }

    /// A news card: `kind` from `title`'s session, saying `body`, `age`
    /// seconds old, with `footer` naming the key that acts on it. Leaves its
    /// own sign and is about nobody in particular.
    pub fn news(id: u64, kind: Kind, title: &str, body: &str, age: u64, footer: &str) -> Self {
        Self {
            id,
            kind,
            title: clean(title),
            body: Some(clean(body)).filter(|b| !b.is_empty()),
            footer: Some(clean(footer)).filter(|f| !f.is_empty()),
            age: Some(age),
            life_ms: millis(NEWS_LIFE),
            fade_ms: fade_ms(),
            ink: ink(),
            plain: plain(),
            about: None,
            mark: Some(kind),
            only_mark: false,
            row: None,
            up_ms: 0,
        }
    }

    /// This arrival, with the sessions either side of the one it names.
    pub fn in_row(self, row: Row) -> Self {
        let clean_name = |s: Option<String>| s.map(|s| clean(&s)).filter(|s| !s.is_empty());
        let step = row.step.and_then(|s| {
            let gone = clean(&s.gone);
            (!gone.is_empty()).then_some(Step {
                forward: s.forward,
                gone,
            })
        });
        Self {
            row: Some(Box::new(Row {
                prev: clean_name(row.prev),
                next: clean_name(row.next),
                step,
            })),
            ..self
        }
    }

    /// This card, up for `up` already on another screen (see [`Card::up_ms`]).
    pub fn up_for(self, up: Duration) -> Self {
        Self {
            up_ms: millis(up),
            ..self
        }
    }

    /// This news, about `about`: a newer card about the same takes its place.
    pub fn about(self, about: &str) -> Self {
        Self {
            about: Some(clean(about)),
            ..self
        }
    }

    /// This news, leaving `mark`'s sign once its words have gone, or none.
    pub fn leaving(self, mark: Option<Kind>) -> Self {
        Self { mark, ..self }
    }

    /// This news as the mark it leaves alone, its words unsaid.
    pub fn as_mark(self) -> Self {
        Self {
            only_mark: true,
            ..self
        }
    }

    /// Its text made safe again: a card off the wire is not trusted.
    fn cleaned(mut self) -> Self {
        self.title = clean(&self.title);
        self.body = self.body.as_deref().map(clean);
        self.footer = self.footer.as_deref().map(clean);
        self.about = self.about.as_deref().map(clean);
        match self.row.take() {
            Some(row) => self.in_row(*row),
            None => self,
        }
    }
}

/// What the relay tells its client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Command {
    /// Show this card, above the others.
    Show(Card),
    /// Drop this card: acted on, or its subject has nothing more to say.
    Drop { id: u64 },
    /// Drop all cards: this client is back in front and what it showed is
    /// stale. Also ends [`Command::Away`].
    Clear,
    /// This client is in the back (parked, or started before its first
    /// visit), unseen until [`Command::Clear`]: it stops animating and redraws
    /// changes only now and then, for nvmux's copy of its screen. On `Clear`
    /// it draws at once, even with nothing changed: the relay waits for that
    /// frame.
    Away,
    /// Whether the prefix is held; while it is, the card the key acts on says
    /// so.
    Keys { held: bool },
    /// Draw with these settings from now on: changed on the settings screen
    /// since the client started or was last told (see
    /// [`crate::config::Drawing`]). Not about cards; it shares their channel.
    Settings(crate::config::Drawing),
}

/// `cmd` as it goes into the client's input: see the module docs.
pub fn encode(cmd: &Command) -> Vec<u8> {
    let json = serde_json::to_vec(cmd).unwrap_or_default();
    let mut out = Vec::with_capacity(OPEN.len() + json.len() * 2 + CLOSE.len());
    out.extend_from_slice(OPEN);
    for b in json {
        out.push(HEX[usize::from(b >> 4)]);
        out.push(HEX[usize::from(b & 0x0f)]);
    }
    out.extend_from_slice(CLOSE);
    out
}

/// The command in a sequence's body (after [`OPEN`], without the
/// terminator), or `None`.
pub fn decode(body: &[u8]) -> Option<Command> {
    if !body.len().is_multiple_of(2) {
        return None;
    }
    let json: Vec<u8> = body
        .chunks(2)
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect::<Option<_>>()?;
    match serde_json::from_slice(&json).ok()? {
        Command::Show(card) => Some(Command::Show(card.cleaned())),
        other => Some(other),
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// `s` fit for a cell: control characters (C1 included) as spaces, ends
/// trimmed, and no longer than any card is wide.
pub fn clean(s: &str) -> String {
    let text: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_TEXT)
        .collect();
    text.trim().to_string()
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The fade each way, where nvmux fades at all.
fn fade_ms() -> u64 {
    if crate::fade::enabled() {
        millis(crate::config::get().effects.fade.one_way())
    } else {
        0
    }
}

fn ink() -> Option<(u8, u8, u8)> {
    crate::theme::ink().map(|rgb| (rgb.0, rgb.1, rgb.2))
}

fn plain() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}

/// A card id unique across threads, for [`Command::Drop`].
pub fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Where acting on a card goes: what `<prefix> Enter` does while it is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Go {
    /// Switch to this session.
    Session(crate::ids::Id),
    /// Show this terminal buffer of the session in front.
    Buffer(i64),
}

/// What the notifier has for the relay in front.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Post {
    /// The session in front it was written for: news of its own terminals
    /// says `here`, which is true only there.
    pub front: String,
    pub what: Posted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Posted {
    /// Show this card; acting on it goes here.
    Card(Card, Option<Go>),
    /// Drop what is up about this session (see [`Card::about`]): it was seen,
    /// or the session is gone.
    Forget(String),
}

impl Post {
    pub fn card(&self) -> Option<&Card> {
        match &self.what {
            Posted::Card(card, _) => Some(card),
            Posted::Forget(_) => None,
        }
    }
}

/// Where the notifier leaves cards for the relay: see the module docs.
pub struct Board {
    posts: Mutex<VecDeque<Post>>,
    /// A byte per post wakes the relay's `poll`; [`Board::take`] drains them.
    wake: Option<(UnixStream, UnixStream)>,
}

/// The most posts kept waiting; past it the oldest go.
const BOARD_MAX: usize = 32;

/// The one board: one relay is in front at a time.
pub fn board() -> &'static Board {
    static BOARD: OnceLock<Board> = OnceLock::new();
    BOARD.get_or_init(Board::new)
}

impl Board {
    fn new() -> Self {
        let wake = UnixStream::pair()
            .and_then(|(r, w)| {
                r.set_nonblocking(true)?;
                w.set_nonblocking(true)?;
                Ok((r, w))
            })
            .map_err(|e| tracing::warn!(error = %e, "card: no pipe to wake the relay on"))
            .ok();
        Self {
            posts: Mutex::new(VecDeque::new()),
            wake,
        }
    }

    pub fn post(&self, post: Post) {
        if let Ok(mut posts) = self.posts.lock() {
            posts.push_back(post);
            while posts.len() > BOARD_MAX {
                posts.pop_front();
            }
        }
        if let Some((_, w)) = &self.wake {
            // Full means a wake is already waiting to be read: one is enough.
            let _ = (&*w).write(&[1]);
        }
    }

    pub fn wake_fd(&self) -> Option<RawFd> {
        self.wake.as_ref().map(|(r, _)| r.as_raw_fd())
    }

    /// Everything posted, oldest first, taken off the board.
    pub fn take(&self) -> Vec<Post> {
        if let Some((r, _)) = &self.wake {
            let mut buf = [0u8; 64];
            while matches!((&*r).read(&mut buf), Ok(n) if n > 0) {}
        }
        self.posts
            .lock()
            .map(|mut posts| posts.drain(..).collect())
            .unwrap_or_default()
    }
}

/// How long the relay's input to its client must be quiet before a card goes
/// in when the stream is not known to be between keys (an unterminated
/// terminal sequence, or what only looked like an `Alt` chord). Longer for a
/// paste: a card inside one would be pasted.
const QUIET: Duration = Duration::from_millis(150);
const QUIET_PASTE: Duration = Duration::from_secs(1);

/// Where the stream passed to the client stands (between keys, in a
/// bracketed paste, or in an OSC, DCS or APC reply), by the client's own rules
/// ([`crate::client::input`]), so a card goes in only where the client will
/// read it as one.
#[derive(Debug, Default)]
pub struct Forwarded {
    inside: Inside,
    /// The start of a marker the last write ended in the middle of.
    tail: Vec<u8>,
    last: Option<Instant>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Inside {
    #[default]
    Nothing,
    Paste,
    /// A string, by its introducer: `]`, `P` or `_`.
    String(u8),
}

impl Forwarded {
    /// The relay passed `bytes` on to its client.
    pub fn saw(&mut self, bytes: &[u8], now: Instant) {
        use crate::client::input::{APC_OPENS, DCS_OPENS, OSC_OPENS, PASTE_END, PASTE_START};
        self.last = Some(now);
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(bytes);
        let mut i = 0;
        while i < buf.len() {
            let rest = &buf[i..];
            let (step, cut_short) = match self.inside {
                Inside::Nothing => {
                    let opens = |intro: u8, opens: &[u8]| {
                        rest.len() >= 3
                            && rest[0] == 0x1b
                            && rest[1] == intro
                            && opens.contains(&rest[2])
                    };
                    if rest.starts_with(PASTE_START) {
                        self.inside = Inside::Paste;
                        (PASTE_START.len(), false)
                    } else if let Some(intro) =
                        [(b']', OSC_OPENS), (b'P', DCS_OPENS), (b'_', APC_OPENS)]
                            .into_iter()
                            .find_map(|(intro, set)| opens(intro, set).then_some(intro))
                    {
                        self.inside = Inside::String(intro);
                        (3, false)
                    } else {
                        // `ESC`, `ESC ]`, `ESC [ 2 0`: what the next write
                        // may make an opener of.
                        let short = rest[0] == 0x1b
                            && (PASTE_START.starts_with(rest)
                                || rest.len() == 1
                                || rest.len() == 2 && matches!(rest[1], b']' | b'P' | b'_'));
                        (1, short)
                    }
                }
                Inside::Paste => {
                    if rest.starts_with(PASTE_END) {
                        self.inside = Inside::Nothing;
                        (PASTE_END.len(), false)
                    } else {
                        (1, rest[0] == 0x1b && PASTE_END.starts_with(rest))
                    }
                }
                Inside::String(intro) => match rest {
                    [0x07, ..] if intro == b']' => {
                        self.inside = Inside::Nothing;
                        (1, false)
                    }
                    [0x1b, b'\\', ..] => {
                        self.inside = Inside::Nothing;
                        (2, false)
                    }
                    [0x1b] => (1, true),
                    _ => (1, false),
                },
            };
            if cut_short {
                self.tail = rest.to_vec();
                return;
            }
            i += step;
        }
    }

    /// Whether a card may go in now: between keys, or after a quiet long
    /// enough that whatever was open has stalled.
    pub fn clear(&self, now: Instant) -> bool {
        self.retry_at().is_none_or(|at| now >= at)
    }

    /// When a card may go in, if not at any time.
    pub fn retry_at(&self) -> Option<Instant> {
        let quiet = match self.inside {
            Inside::Nothing if self.tail.is_empty() => return None,
            Inside::Paste => QUIET_PASTE,
            Inside::Nothing | Inside::String(_) => QUIET,
        };
        Some(self.last? + quiet)
    }

    /// A card went in regardless: whatever was open has stalled, and the
    /// client abandons it on reading the card.
    fn reset(&mut self) {
        self.inside = Inside::Nothing;
        self.tail.clear();
    }
}

/// The relay's side of the cards, one per relay: what its client shows, what
/// `<prefix> Enter` acts on, and what waits for a gap between keys (see
/// [`Forwarded`]).
#[derive(Debug)]
pub struct Desk {
    front: String,
    up: Vec<Up>,
    queue: VecDeque<(Command, Option<Go>)>,
    /// Whether the client was last told the prefix is held.
    held: bool,
    pub forwarded: Forwarded,
}

/// A card sent and still up; news stays up, as its mark, until its session
/// is opened.
#[derive(Debug)]
struct Up {
    id: u64,
    go: Option<Go>,
    about: Option<String>,
}

impl Desk {
    pub fn new(front: &str) -> Self {
        Self {
            front: front.to_string(),
            up: Vec::new(),
            queue: VecDeque::new(),
            held: false,
            forwarded: Forwarded::default(),
        }
    }

    /// A relay's opening commands, before the client gets any key: clear what
    /// it showed when last in front, and show the arrival card if any.
    pub fn open(&mut self, arrival: Option<Card>) -> Vec<Command> {
        let mut out = vec![Command::Clear];
        if let Some(card) = arrival {
            self.up.push(Up {
                id: card.id,
                go: None,
                about: None,
            });
            out.push(Command::Show(card));
        }
        out
    }

    /// Queue this relay's posts; drop others', written for a session no
    /// longer in front.
    pub fn take(&mut self, posts: Vec<Post>) {
        for post in posts {
            if post.front != self.front {
                continue;
            }
            match post.what {
                Posted::Card(card, go) => self.queue.push_back((Command::Show(card), go)),
                Posted::Forget(about) => self.forget(&about),
            }
        }
    }

    /// Drop everything about `about`, up or queued.
    fn forget(&mut self, about: &str) {
        let is_about = |card: &Card| card.about.as_deref() == Some(about);
        self.queue
            .retain(|(cmd, _)| !matches!(cmd, Command::Show(card) if is_about(card)));
        let (gone, kept): (Vec<Up>, Vec<Up>) = std::mem::take(&mut self.up)
            .into_iter()
            .partition(|u| u.about.as_deref() == Some(about));
        self.up = kept;
        for u in gone {
            self.queue.push_back((Command::Drop { id: u.id }, None));
        }
    }

    /// Tell the client whether the prefix is held, if that is news to it.
    pub fn keys(&mut self, held: bool) {
        if held != self.held {
            self.held = held;
            self.queue.push_back((Command::Keys { held }, None));
        }
    }

    /// Everything queued, if a card may go in now ([`Forwarded::clear`]);
    /// otherwise nothing. A card about the same as one up replaces it, for the
    /// key too.
    pub fn ready(&mut self, now: Instant) -> Vec<Command> {
        if self.queue.is_empty() || !self.forwarded.clear(now) {
            return Vec::new();
        }
        self.forwarded.reset();
        let queue = std::mem::take(&mut self.queue);
        queue
            .into_iter()
            .map(|(cmd, go)| {
                if let Command::Show(card) = &cmd {
                    if let Some(about) = card.about.as_deref() {
                        self.up.retain(|u| u.about.as_deref() != Some(about));
                    }
                    self.up.push(Up {
                        id: card.id,
                        go,
                        about: card.about.clone(),
                    });
                }
                cmd
            })
            .collect()
    }

    /// When to try again to send what is queued: `None` if nothing is, or it
    /// can go now.
    pub fn retry_at(&self) -> Option<Instant> {
        if self.queue.is_empty() {
            return None;
        }
        self.forwarded.retry_at()
    }

    /// Whether [`Desk::go`] would find a card: asked by the hint bar before
    /// offering `<prefix> Enter` (see [`crate::hint`]).
    pub fn can_go(&self) -> bool {
        self.up.iter().any(|u| u.go.is_some())
    }

    /// Take down the newest card up that goes somewhere: what `<prefix> Enter`
    /// acts on.
    pub fn go(&mut self) -> Option<(u64, Go)> {
        let at = self.up.iter().rposition(|u| u.go.is_some())?;
        let up = self.up.remove(at);
        self.queue.push_back((Command::Drop { id: up.id }, None));
        Some((up.id, up.go?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card() -> Card {
        Card::news(
            7,
            Kind::Finished,
            "api-server",
            "cargo test: exited with code 0",
            3,
            "Ctrl-Space ⏎ go there",
        )
    }

    #[test]
    fn a_command_comes_off_the_wire_as_it_went_on() {
        let row = Row {
            prev: Some("notes".into()),
            next: Some("blog".into()),
            step: Some(Step {
                forward: true,
                gone: "api-server".into(),
            }),
        };
        for cmd in [
            Command::Show(card()),
            Command::Show(card().about("abcd2345").leaving(Some(Kind::Notified))),
            Command::Show(card().as_mark()),
            Command::Show(Card::arrival(8, "blog", true).in_row(row)),
            Command::Drop { id: 7 },
            Command::Clear,
            Command::Away,
            Command::Keys { held: true },
        ] {
            let wire = encode(&cmd);
            let body = &wire[OPEN.len()..wire.len() - CLOSE.len()];
            assert_eq!(decode(body), Some(cmd));
        }
    }

    #[test]
    fn nothing_a_card_says_can_end_its_sequence() {
        let nasty = Card::news(
            1,
            Kind::Notified,
            "evil\x1b\\name",
            "bell\x07 and a C1 \u{9c} and ✓",
            0,
            "",
        );
        let wire = encode(&Command::Show(nasty));
        assert!(wire.starts_with(OPEN) && wire.ends_with(CLOSE));
        let body = &wire[OPEN.len()..wire.len() - CLOSE.len()];
        assert!(
            body.iter().all(|b| b.is_ascii_hexdigit()),
            "{:?}",
            String::from_utf8_lossy(body)
        );
    }

    /// Controls become spaces when built, and again when decoded.
    #[test]
    fn a_cards_text_holds_no_control_characters() {
        let card = Card::news(1, Kind::Failed, "a\x1bb", "c\u{9b}d", 0, "e\x07");
        assert_eq!(card.title, "a b");
        assert_eq!(card.body.as_deref(), Some("c d"));
        assert_eq!(card.footer.as_deref(), Some("e"));

        let mut raw = card.clone();
        raw.title = "x\x1by".to_string();
        let json = serde_json::to_vec(&Command::Show(raw)).expect("json");
        let body: Vec<u8> = json
            .iter()
            .flat_map(|b| [HEX[usize::from(b >> 4)], HEX[usize::from(b & 0x0f)]])
            .collect();
        let Some(Command::Show(back)) = decode(&body) else {
            panic!("did not decode");
        };
        assert_eq!(back.title, "x y");

        let mut raw = Card::arrival(2, "blog", false);
        raw.row = Some(Box::new(Row {
            prev: Some("a\x1bb".into()),
            next: Some("\x07".into()),
            step: Some(Step {
                forward: false,
                gone: "c\u{9b}d".into(),
            }),
        }));
        let json = serde_json::to_vec(&Command::Show(raw)).expect("json");
        let body: Vec<u8> = json
            .iter()
            .flat_map(|b| [HEX[usize::from(b >> 4)], HEX[usize::from(b & 0x0f)]])
            .collect();
        let Some(Command::Show(back)) = decode(&body) else {
            panic!("did not decode");
        };
        let row = back.row.expect("a row");
        assert_eq!(row.prev.as_deref(), Some("a b"));
        assert_eq!(row.next, None, "a name of nothing but a control is none");
        assert_eq!(row.step.map(|s| s.gone).as_deref(), Some("c d"));
    }

    #[test]
    fn a_body_that_is_not_a_command_is_ignored() {
        assert_eq!(decode(b"zz"), None);
        assert_eq!(decode(b"7"), None);
        assert_eq!(decode(b""), None);
        assert_eq!(decode(b"7b7d"), None, "{{}}");
    }

    /// It leaves no mark, is about nobody, and has the shorter life.
    #[test]
    fn an_arrival_is_a_name() {
        let named = Card::arrival(1, "blog", false);
        assert_eq!((named.kind, named.title.as_str()), (Kind::Arrival, "blog"));
        assert_eq!(named.body, None);
        assert_eq!((named.mark, named.about.as_deref()), (None, None));
        assert_eq!(named.kind.sign(), None);
        let waiting = Card::arrival(2, "blog", true);
        assert_eq!(waiting.body.as_deref(), Some(WAITING));
        assert!(Duration::from_millis(named.life_ms) < NEWS_LIFE);
    }

    #[test]
    fn news_leaves_its_sign() {
        let news = card();
        assert_eq!(news.mark, Some(Kind::Finished));
        assert!(!news.only_mark);
        let left = card().leaving(Some(Kind::Failed)).as_mark();
        assert_eq!((left.kind, left.mark), (Kind::Finished, Some(Kind::Failed)));
        assert!(left.only_mark);
    }

    #[test]
    fn news_wears_the_pickers_signs() {
        assert_eq!(Kind::Finished.sign(), Some('✓'));
        assert_eq!(Kind::Failed.sign(), Some('!'));
        assert_eq!(Kind::Notified.sign(), Some('∗'));
    }

    #[test]
    fn the_board_hands_over_what_was_posted_once() {
        let board = Board::new();
        let fd = board.wake_fd().expect("a pipe");
        let post = |n: u64| Post {
            front: "s".into(),
            what: Posted::Card(Card::arrival(n, "x", false), None),
        };
        board.post(post(1));
        board.post(post(2));
        let mut p = [crate::pty::pollfd(fd)];
        let n = unsafe { libc::poll(p.as_mut_ptr(), 1, 0) };
        assert_eq!(n, 1, "the board did not wake its reader");
        let ids: Vec<u64> = board
            .take()
            .iter()
            .filter_map(|p| Some(p.card()?.id))
            .collect();
        assert_eq!(ids, [1, 2]);
        assert!(board.take().is_empty());
        let n = unsafe { libc::poll(p.as_mut_ptr(), 1, 0) };
        assert_eq!(n, 0, "a wake was left behind");
    }

    /// Never inside a paste or a terminal string, however writes split them;
    /// an unterminated string holds cards back only for `QUIET`.
    #[test]
    fn a_card_waits_for_the_stream_to_be_between_keys() {
        let t0 = Instant::now();
        let mut f = Forwarded::default();
        assert!(f.clear(t0), "nothing passed on yet");
        f.saw(b"hello\x1b[A", t0);
        assert!(f.clear(t0), "keys, and a key's sequence");

        f.saw(b"\x1b[200~pasted", t0);
        assert!(!f.clear(t0), "inside a paste");
        assert!(!f.clear(t0 + QUIET), "a paste is waited for longer");
        f.saw(b" more\x1b[20", t0);
        assert!(!f.clear(t0));
        f.saw(b"1~", t0);
        assert!(f.clear(t0), "the paste's end, cut across two writes");

        f.saw(b"\x1b]11;rgb:0000/0000/0000", t0);
        assert!(!f.clear(t0), "inside an OSC reply");
        f.saw(b"\x1b\\", t0);
        assert!(f.clear(t0), "ended");

        f.saw(b"\x1b", t0);
        assert!(!f.clear(t0), "an ESC that may open anything");
        f.saw(b"]", t0);
        assert!(!f.clear(t0));
        f.saw(b"x", t0);
        assert!(f.clear(t0), "Alt-] and an x, as the client reads them");

        f.saw(b"\x1bP>|kitty", t0);
        assert!(!f.clear(t0 + QUIET / 2));
        assert!(f.clear(t0 + QUIET), "a string that never ends has stalled");
    }

    fn post(front: &str, id: u64, go: Option<Go>) -> Post {
        Post {
            front: front.into(),
            what: Posted::Card(Card::news(id, Kind::Finished, "s", "done", 0, "f"), go),
        }
    }

    fn about(front: &str, id: u64, about: &str, go: Option<Go>) -> Post {
        Post {
            front: front.into(),
            what: Posted::Card(
                Card::news(id, Kind::Finished, "s", "done", 0, "f").about(about),
                go,
            ),
        }
    }

    fn ids(cmds: &[Command]) -> Vec<String> {
        cmds.iter()
            .map(|c| match c {
                Command::Show(card) => format!("show {}", card.id),
                Command::Drop { id } => format!("drop {id}"),
                Command::Clear => "clear".to_string(),
                Command::Away => "away".to_string(),
                Command::Keys { held } => format!("keys {held}"),
                Command::Settings(_) => "settings".to_string(),
            })
            .collect()
    }

    #[test]
    fn settings_go_to_the_client_and_come_back_as_they_went() {
        let mut drawing = crate::config::Drawing::default();
        drawing.client.centre = crate::config::Centre::Fixed;
        drawing.client.predict_typing = crate::config::Predict::Off;
        drawing.client.window_width = 100;
        drawing.remote = true;
        drawing.effects.smear.enabled = true;
        drawing.effects.particles.mode = crate::config::ParticleMode::Ripple;
        let cmd = Command::Settings(drawing);
        let wire = encode(&cmd);
        let body = &wire[OPEN.len()..wire.len() - CLOSE.len()];
        assert_eq!(decode(body), Some(cmd));
    }

    #[test]
    fn a_desk_lets_its_own_cards_in_between_keys() {
        let t0 = Instant::now();
        let mut desk = Desk::new("front");
        let opened = desk.open(Some(Card::arrival(1, "front", false)));
        assert!(matches!(opened.as_slice(), [Command::Clear, Command::Show(c)] if c.id == 1));
        assert_eq!(desk.open(None), [Command::Clear]);

        desk.take(vec![post("front", 2, None), post("other", 3, None)]);
        desk.forwarded.saw(b"\x1b[200~", t0);
        assert!(desk.ready(t0).is_empty(), "inside a paste");
        assert_eq!(desk.retry_at(), Some(t0 + QUIET_PASTE));
        desk.forwarded.saw(b"\x1b[201~", t0);
        assert_eq!(
            ids(&desk.ready(t0)),
            ["show 2"],
            "another session's card was written for it"
        );
        assert!(desk.ready(t0).is_empty() && desk.retry_at().is_none());
    }

    /// However long it has been up: a mark stays up until its session is
    /// opened.
    #[test]
    fn going_takes_the_newest_card_that_goes_somewhere() {
        let t0 = Instant::now();
        let a = crate::ids::Id::new("aaaaaaaa").expect("id");
        let b = crate::ids::Id::new("bbbbbbbb").expect("id");
        let mut desk = Desk::new("front");
        desk.open(Some(Card::arrival(1, "front", false)));
        desk.take(vec![
            post("front", 2, Some(Go::Session(a))),
            post("front", 3, Some(Go::Session(b))),
            post("front", 4, None),
        ]);
        desk.ready(t0);
        assert_eq!(desk.go(), Some((3, Go::Session(b))));
        assert_eq!(
            desk.ready(t0),
            [Command::Drop { id: 3 }],
            "the card acted on is let go of"
        );
        assert_eq!(desk.go(), Some((2, Go::Session(a))));
        assert_eq!(desk.go(), None, "the arrival and the last go nowhere");

        desk.take(vec![post("front", 5, Some(Go::Buffer(7)))]);
        desk.ready(t0);
        assert_eq!(desk.go(), Some((5, Go::Buffer(7))), "an hour on or now");
    }

    /// Replaced for the key as on screen; forgetting drops cards up or queued.
    #[test]
    fn the_newer_card_about_a_session_takes_the_olders_place() {
        let t0 = Instant::now();
        let a = crate::ids::Id::new("aaaaaaaa").expect("id");
        let b = crate::ids::Id::new("bbbbbbbb").expect("id");
        let mut desk = Desk::new("front");
        desk.open(None);
        desk.take(vec![
            about("front", 2, "aaaaaaaa", Some(Go::Session(a))),
            about("front", 3, "bbbbbbbb", Some(Go::Session(b))),
            about("front", 4, "aaaaaaaa", Some(Go::Session(a))),
        ]);
        desk.ready(t0);
        assert_eq!(desk.go(), Some((4, Go::Session(a))), "the newer about a");
        desk.ready(t0);
        assert_eq!(desk.go(), Some((3, Go::Session(b))));
        desk.ready(t0);
        assert_eq!(desk.go(), None, "the older about a went with the newer");

        desk.take(vec![about("front", 5, "aaaaaaaa", Some(Go::Session(a)))]);
        desk.ready(t0);
        desk.take(vec![
            about("front", 6, "aaaaaaaa", Some(Go::Session(a))),
            Post {
                front: "front".into(),
                what: Posted::Forget("aaaaaaaa".into()),
            },
        ]);
        assert_eq!(
            ids(&desk.ready(t0)),
            ["drop 5"],
            "the one up let go of, the one waiting never sent"
        );
        assert_eq!(desk.go(), None);
    }

    #[test]
    fn the_client_is_told_of_the_prefix_as_it_changes() {
        let t0 = Instant::now();
        let mut desk = Desk::new("front");
        desk.open(None);
        desk.keys(false);
        assert!(desk.ready(t0).is_empty());
        desk.keys(true);
        desk.keys(true);
        assert_eq!(ids(&desk.ready(t0)), ["keys true"]);
        desk.keys(false);
        assert_eq!(ids(&desk.ready(t0)), ["keys false"]);
    }

    /// The hint bar relies on `can_go` agreeing with `go`, marks included.
    #[test]
    fn can_go_says_whether_going_would_go_anywhere() {
        let t0 = Instant::now();
        let a = crate::ids::Id::new("aaaaaaaa").expect("id");
        let mut desk = Desk::new("front");
        desk.open(Some(Card::arrival(1, "front", false)));
        assert!(!desk.can_go(), "an arrival card goes nowhere");

        desk.take(vec![post("front", 2, Some(Go::Session(a)))]);
        assert!(!desk.can_go(), "a card not yet put in is not up");
        desk.ready(t0);
        assert!(desk.can_go(), "up, for as long as it leaves a mark");
        assert!(desk.go().is_some());
        assert!(!desk.can_go(), "the card acted on is down");

        desk.take(vec![about("front", 3, "aaaaaaaa", Some(Go::Session(a)))]);
        desk.ready(t0);
        assert!(desk.can_go());
        desk.take(vec![Post {
            front: "front".into(),
            what: Posted::Forget("aaaaaaaa".into()),
        }]);
        assert!(!desk.can_go(), "what it was about has nothing more to say");
    }
}
