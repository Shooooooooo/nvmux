//! The cards nvmux shows in the margin of the session in front: which session
//! you have just landed in, and what the others report while you are in one —
//! a command that finished (`✓`) or failed (`!`) in one of their terminals, a
//! notification a program there asked for (`∗`) — and of the session in front,
//! what a terminal of its own off the screen says. A card with news says it
//! and goes, and leaves its sign behind as the session's mark, which stays
//! until the session is opened.
//!
//! # Drawn by the client
//!
//! A card is nvmux's, and drawn on this side of the link: nothing about it
//! goes to the session's server, so it is up as soon as nvmux knows what it
//! says, over a slow link as over none. nvmux's own client draws it
//! ([`crate::client::cards`]) — over the frame it composes from the editor's
//! grids, so that the editor redrawing its screen, a `:mode` included, draws
//! the card again with it rather than over it. Neovim's own client
//! (`[client] ui = "nvim"`) draws no card: it has no way to be asked.
//!
//! # On the client's input
//!
//! The relay tells its client what to show the way it tells it its size when
//! that changed while it was parked: in the client's input, as a sequence no
//! terminal sends — an APC opened by [`OPEN`] — which the client keeps for
//! itself rather than typing (see [`crate::client::input`]). One write, and
//! never inside a paste or a sequence of the terminal's own the relay is
//! passing on (see [`Forwarded`]). What it carries is a [`Command`] as JSON,
//! hex-encoded: plain ASCII, so that nothing in a name or a notification —
//! an `ESC`, a `BEL`, a UTF-8 byte that is a C1 control on its own — can end
//! the sequence early or reach the terminal.
//!
//! # From the notifier
//!
//! What the other sessions report is read on a thread of its own
//! ([`crate::tell::Notifier`]), which posts its cards on the [`board`] for the
//! relay in front to take: the relay's loop wakes on the board's pipe, and
//! takes what was posted for its own session, the one the card was written
//! for, and drops anything left over from one before it.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// What opens the sequence a command goes to the client in: an APC, `ESC _`,
/// and a byte no terminal's APC starts with — kitty's graphics replies, the
/// only APCs a terminal sends, start with `G`.
pub const OPEN: &[u8] = b"\x1b_~";

/// The byte after `ESC _` that says the APC is nvmux's.
pub const OPENER: u8 = b'~';

/// What ends it: the string terminator.
pub const CLOSE: &[u8] = b"\x1b\\";

/// How long the card that says where a switch landed stays at full strength.
///
/// Long enough to catch a name out of the corner of the eye, which is where it
/// is, in the margin: unlike the box that said it before
/// ([`crate::announce`]), it is not over the text you have just switched to,
/// so a little longer than that box's second costs nothing.
pub const ARRIVAL_LIFE: Duration = Duration::from_millis(1500);

/// How long a card with news stays at full strength: long enough to read two
/// lines and decide whether to go.
pub const NEWS_LIFE: Duration = Duration::from_secs(6);

/// What an arrival says under the name of a session waiting for a key, which
/// nothing else on its blank screen would say.
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
    /// The picker's sign for it (see [`crate::ui::note`]); none for an arrival,
    /// which is a name and nothing else.
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
    /// The session's name; for the session in front, `here`, and where its
    /// terminal is.
    pub title: String,
    /// What happened, for news; for an arrival, that the session is waiting
    /// for a key ([`WAITING`]), which nothing else on the screen would say.
    pub body: Option<String>,
    /// The key that acts on the card and what it does, or nothing for one that
    /// goes nowhere. The client draws it while the prefix is held
    /// ([`Command::Keys`]), on the newest card up that has one, which is the
    /// one the key acts on ([`Desk::go`]), and on no other.
    pub footer: Option<String>,
    /// How old the news was when the card was sent, in seconds; the client
    /// counts on from there. `None` for an arrival.
    pub age: Option<u64>,
    /// How long its words stay at full strength, in milliseconds.
    pub life_ms: u64,
    /// nvmux's own fade one way, in milliseconds (see [`crate::fade`]), and 0
    /// where that is off: what the card's comings and goings are timed in.
    pub fade_ms: u64,
    /// `[theme] color` (see [`crate::theme`]): the colour of its sign and its
    /// session's name, in place of the editor's; `None` for the editor's own.
    pub ink: Option<(u8, u8, u8)>,
    /// No colour of nvmux's own, under `NO_COLOR`: its sign in the editor's
    /// text colour rather than the colour the editor has for its kind.
    #[serde(default)]
    pub plain: bool,
    /// The session the news is about, by its id, where it is another's — and
    /// for a terminal of the session in front, which one: a newer card about
    /// the same takes this one's place, as its news has. `None` for an
    /// arrival.
    #[serde(default)]
    pub about: Option<String>,
    /// The sign the news leaves once its words have gone, which stays until
    /// the card is let go of: the session's mark. The one the picker has
    /// beside the session, which is a notification's or a failure's over a
    /// command's that finished since; `None` for a card that leaves none.
    #[serde(default)]
    pub mark: Option<Kind>,
    /// Whether it is the mark alone, its words never said: for news that came
    /// before the screen it is on, and was told on another. The prefix held
    /// says them, on the one the key acts on.
    #[serde(default)]
    pub only_mark: bool,
    /// For an arrival: where the session is among the ones `<prefix> n` and
    /// `<prefix> p` step through. Boxed, as only an arrival has one.
    #[serde(default)]
    pub row: Option<Box<Row>>,
    /// How long it has been up already, in milliseconds, where it was up on
    /// another screen first: the card a switch lands with, which the client
    /// of the session it leaves draws from the key on (see
    /// [`crate::pty::relay`]). The client of the session it lands in takes it
    /// up from there, its row as far along as it has come, rather than from
    /// the start. 0 for a card on its first screen, which is timed from when
    /// it is first drawn there.
    #[serde(default)]
    pub up_ms: u64,
    /// For an arrival drawn again on the screen a switch lands on: the column
    /// its lines end at on the screen it left, where the client there laid
    /// it — against its own editor, which with `[client] centre` need not be
    /// where this one's is — so that it stands where it stood through both
    /// fades. `None` for one laid against this client's own editor.
    #[serde(default)]
    pub end: Option<u16>,
}

/// Where a switch landed among the sessions `<prefix> n` and `<prefix> p`
/// step through, for the arrival that says so: the names either side of it,
/// and for a step, which way it went.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// The session `<prefix> p` goes to from here, where that is not the one
    /// `<prefix> n` goes to as well: with two sessions, the other is `next`.
    pub prev: Option<String>,
    /// The session `<prefix> n` goes to from here.
    pub next: Option<String>,
    /// For a switch by `<prefix> n` or `<prefix> p` among three sessions or
    /// more, which moves the row one name the way it went.
    pub step: Option<Step>,
}

/// A step along the [`Row`]: which way it went, and the name that goes off
/// the end of the row as it moves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// Forward, as `<prefix> n` goes; back, as `<prefix> p` does.
    pub forward: bool,
    /// The session beyond the one left, on its far side from the one landed
    /// in: in the row before the step, and not after it.
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
            end: None,
        }
    }

    /// A card with news: `kind`, from `title`'s session, saying `body`, `age`
    /// seconds old, with `footer` naming the key that acts on it. It leaves
    /// its own sign, and is about nobody in particular.
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
            end: None,
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

    /// This arrival, its lines ending at column `end`, as it stood on the
    /// screen it was up on first (see [`Card::end`]).
    pub fn ending_at(self, end: u16) -> Self {
        Self {
            end: Some(end),
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

    /// Its text made safe, as it is on the way in: for a card that came off
    /// the wire, which says what nvmux sent but is read as though it might not.
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
    /// Let this one go: it has been acted on, or what it is about has nothing
    /// more to say.
    Drop { id: u64 },
    /// Let them all go: this client has just come back to the front, and
    /// whatever it was showing when it left is old.
    Clear,
    /// The prefix is held, or it is not any more: while it is, the card the
    /// key acts on says so.
    Keys { held: bool },
    /// What to do with a terminal wider than the windows need (see
    /// [`crate::config::Centre`]): said when `<prefix> w` changes it, and as
    /// a client comes back to the front, which may have been parked through
    /// a change. Nothing to do with a card, but told the same way, for the
    /// same reasons.
    Centre { centre: crate::config::Centre },
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

/// The command a sequence's body — what comes after [`OPEN`], without its
/// terminator — carries, or `None` for one that carries none.
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

/// `s` fit to be drawn in a cell: control characters, C1 ones included, as
/// spaces, the ends trimmed, and no longer than any card is wide.
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

/// A number for a new card, never one another card had: what
/// [`Command::Drop`] names it by, whichever thread made it.
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
    /// The session in front it was written for: of its own terminals it says
    /// `here`, which is nobody else's.
    pub front: String,
    pub what: Posted,
}

/// What a [`Post`] says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Posted {
    /// Show this card; acting on it goes here.
    Card(Card, Option<Go>),
    /// Let go of what is up about this session (see [`Card::about`]): it has
    /// nothing to tell any more — what it kept has been seen, or it is gone.
    Forget(String),
}

impl Post {
    /// The card it shows, if it shows one.
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
    /// Written a byte per post, so that the relay's `poll` wakes; the relay
    /// reads them off again with what was posted.
    wake: Option<(UnixStream, UnixStream)>,
}

/// The most cards that wait on the board for a relay to take them; past it the
/// oldest go, which no relay was there to show while they were new.
const BOARD_MAX: usize = 32;

/// The one board there is: one relay is in front at a time.
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

    /// Leave `post` for the relay in front.
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

    /// The descriptor the relay polls for posts, if there is one.
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

/// How long the relay's input to its client must have been quiet before a
/// card goes in where the stream is not known to be between keys: a sequence
/// of the terminal's that never ended, an `Alt` chord that only looked like
/// one. A paste is given longer: a card inside one would be pasted.
const QUIET: Duration = Duration::from_millis(150);
const QUIET_PASTE: Duration = Duration::from_secs(1);

/// Where the stream the relay passes on to its client stands — between keys,
/// inside a bracketed paste, or inside a string of the terminal's (an OSC, a
/// DCS or an APC reply) — by the client's own rules for telling (see
/// [`crate::client::input`]), so that a card goes in only where the client
/// will read it as one.
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

    /// Whether a card may go in now: between keys, or after long enough with
    /// nothing passed on that whatever was open has stalled.
    pub fn clear(&self, now: Instant) -> bool {
        self.retry_at().is_none_or(|at| now >= at)
    }

    /// When a card may go in, if it may not go in at any time.
    pub fn retry_at(&self) -> Option<Instant> {
        let quiet = match self.inside {
            Inside::Nothing if self.tail.is_empty() => return None,
            Inside::Paste => QUIET_PASTE,
            Inside::Nothing | Inside::String(_) => QUIET,
        };
        Some(self.last? + quiet)
    }

    /// A card went in, whatever stood open: what was open has stalled, and
    /// the client, reading the card, has given up on it.
    fn reset(&mut self) {
        self.inside = Inside::Nothing;
        self.tail.clear();
    }
}

/// The relay's side of the cards: what it has told its client to show, which
/// `<prefix> Enter` acts on, and what waits for a moment between keys to go
/// in (see [`Forwarded`]). One a relay, for its session only.
#[derive(Debug)]
pub struct Desk {
    front: String,
    up: Vec<Up>,
    queue: VecDeque<(Command, Option<Go>)>,
    /// Whether the client was last told the prefix is held.
    held: bool,
    pub forwarded: Forwarded,
}

/// A card sent, and up until it is let go of: news stays, as the mark it
/// leaves, until its session is opened.
#[derive(Debug)]
struct Up {
    id: u64,
    go: Option<Go>,
    about: Option<String>,
}

impl Desk {
    /// The desk of a relay of session `front`.
    pub fn new(front: &str) -> Self {
        Self {
            front: front.to_string(),
            up: Vec::new(),
            queue: VecDeque::new(),
            held: false,
            forwarded: Forwarded::default(),
        }
    }

    /// What a relay opens with, before the client has been passed a key: off
    /// with whatever it was showing when it was last in front, and on with
    /// the card that says where the switch landed, if it says anything.
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

    /// Take what was posted on the board: this relay's, to go in as soon as
    /// it can; anybody else's dropped — written for a session that is not in
    /// front any more.
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

    /// Let go of everything about `about`: what is up, and what waits to go
    /// up.
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

    /// What may go in to the client now: everything waiting, if the stream is
    /// between keys, and nothing otherwise. A card about what one up is about
    /// takes its place, here as on the screen: the key acts on the newer.
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

    /// When to look again for a moment to put what waits in: `None` when
    /// nothing waits, or it can go in now.
    pub fn retry_at(&self) -> Option<Instant> {
        if self.queue.is_empty() {
            return None;
        }
        self.forwarded.retry_at()
    }

    /// Whether [`Desk::go`] would find a card to act on: what the hint bar asks
    /// before it offers `<prefix> Enter` (see [`crate::hint`]). One stays up
    /// until it is acted on or what it is about has nothing more to say, as
    /// news leaves its mark.
    pub fn can_go(&self) -> bool {
        self.up.iter().any(|u| u.go.is_some())
    }

    /// The newest card up that goes somewhere, taken down: what
    /// `<prefix> Enter` acts on.
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

    /// What goes on the wire comes back off it as it went, every command.
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
            Command::Keys { held: true },
        ] {
            let wire = encode(&cmd);
            let body = &wire[OPEN.len()..wire.len() - CLOSE.len()];
            assert_eq!(decode(body), Some(cmd));
        }
    }

    /// Plain ASCII between the opener and the terminator, whatever the card
    /// says: nothing in a name ends the sequence, or reaches the terminal.
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

    /// What the card says is drawn as text: controls are spaces, on the way
    /// out and again on the way in.
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

    /// Anything that is not a command is not one.
    #[test]
    fn a_body_that_is_not_a_command_is_ignored() {
        assert_eq!(decode(b"zz"), None);
        assert_eq!(decode(b"7"), None);
        assert_eq!(decode(b""), None);
        assert_eq!(decode(b"7b7d"), None, "{{}}");
    }

    /// An arrival is a name, and whether the session is waiting for a key;
    /// it leaves no mark, is about nobody, and stays the shorter while.
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

    /// News leaves its own sign, unless it is told to leave another; it is
    /// the mark alone only when asked.
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

    /// The board hands each post over once, oldest first, and wakes whoever
    /// polls it.
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

    /// A card goes in between keys, never inside a paste — where it would be
    /// pasted — nor inside a string of the terminal's, however the writes
    /// cut them; and a string that never ends holds cards back only a while.
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
                Command::Keys { held } => format!("keys {held}"),
                Command::Centre { centre } => format!("centre {}", centre.name()),
            })
            .collect()
    }

    /// A relay opens with a clear and where the switch landed; takes what was
    /// posted for its own session only; and lets it in when the stream is
    /// between keys.
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

    /// `<prefix> Enter` takes the newest card that goes somewhere down, and
    /// says where — however long it has been up, as a mark stays up until
    /// its session is opened.
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

    /// A card about what an older one is about takes its place, for the key
    /// as on the screen; and what is forgotten is let go of, up or waiting.
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

    /// The client is told when the prefix is held and when it is let go of,
    /// and not again while it stays either way.
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

    /// `can_go` is `go` asked without acting: what the hint bar offers
    /// `<prefix> Enter` on, so the two must never disagree — a mark included,
    /// and none once what it is about is let go of.
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
