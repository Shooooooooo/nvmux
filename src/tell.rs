//! What the other sessions report while you are in one, told as it happens: a
//! notification a program in one of their terminal buffers asked for (`∗`), a
//! command there that failed (`!`) or finished (`✓`) — the picker's signs,
//! while the picker is down — as cards in the margin of the session in front
//! (see [`crate::card`]). Of the session in front, only what a terminal of its
//! own off the screen says: the rest it was showing.
//!
//! What a session keeps until it is opened stays as its mark, the sign the
//! picker has beside it, once the card's words have gone; and a session
//! brought to the front while another still keeps something is told that
//! session's mark alone, the words having been said already.
//!
//! The news is read off each session's watcher ([`crate::notes`]), and its
//! cards are posted for the relay in front ([`crate::card::board`]), which has
//! its client draw them. Nothing is asked of the session in front to show
//! them, so they are up as soon as the news is read, whatever the editor is
//! doing and however far away it is; and they are nvmux's, with nothing of
//! them left in the editor once they go.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::card::{self, Card, Go, Post, Posted};
use crate::config::KeySettings;
use crate::notes::{self, Heard, Kind, News, Pushed};
use crate::ui::note;
use crate::{ids, keys, rpc};

/// How long the notifier waits, once a session is in front, before its first
/// round of reads: long enough for the sessions handed to it at the same
/// moment to be read in that round. And how long it waits to read a session
/// that has said its news changed: long enough for the rest of what one event
/// brings — a command's end, said by its shell and by its terminal closing —
/// to be read with it.
const SETTLE: Duration = Duration::from_millis(30);

/// How long a session that tells the notifier its news goes between reads all
/// the same: for what a push could miss, over a connection gone quiet without
/// closing.
const SAFETY: Duration = Duration::from_secs(30);

/// The least time between two reads of a session that its pushes bring on: a
/// program that sends notification after notification has its session read
/// four times a second, not as often as it sends.
const SPACING: Duration = Duration::from_millis(250);

/// How long a session whose listener stopped, though the session may still be
/// there, waits for another: it is read every [`notes::POLL`] meanwhile.
const RELISTEN: Duration = Duration::from_secs(10);

/// The card that tells `news`, from the session with this id and name — or,
/// with `None`, from the session in front itself — and where acting on it
/// goes: that session, or the terminal it came from. `settings` are the keys
/// as the user has them, of which the card names the two that go there:
/// `<prefix> Enter` by default. `None` for what is not news: what is busy.
///
/// It is about the session, or the terminal, it came from (see
/// [`Card::about`]), and leaves its own sign — or, going nowhere, none.
pub fn card_for(
    news: &News,
    from: Option<(&str, &str)>,
    settings: KeySettings,
) -> Option<(Card, Option<Go>)> {
    let kind = sign(news.kind)?;
    let what = note::what(news.kind, &news.title, &news.text);
    // The prefix, and the key `go` is on — which every row of the table has,
    // so the second is never missing.
    let key = format!(
        "{} {}",
        keys::prefix_label(settings.prefix),
        settings
            .bind
            .key(keys::Action::Go)
            .map(keys::key_glyph)
            .unwrap_or_default()
    );
    let (title, go, does) = match from {
        Some((id, name)) => (
            name.to_string(),
            ids::Id::new(id).map(Go::Session),
            "go there",
        ),
        None => (
            match news.tab {
                Some(tab) => format!("here · tab {tab}"),
                None => "here".to_string(),
            },
            news.buf.map(Go::Buffer),
            "show it",
        ),
    };
    let footer = if go.is_some() {
        format!("{key} {does}")
    } else {
        String::new()
    };
    let card = Card::news(card::next_id(), kind, &title, &what, news.age, &footer);
    let card = match (from, news.buf) {
        (Some((id, _)), _) => card.about(id),
        (None, Some(buf)) => card.about(&format!("here {buf}")),
        (None, None) => card,
    };
    // Nowhere to go is nothing to open, and a mark would wait for that.
    let card = if go.is_some() {
        card
    } else {
        card.leaving(None)
    };
    Some((card, go))
}

/// The sign on a card for news of `kind`; none for what is not news.
fn sign(kind: Kind) -> Option<card::Kind> {
    match kind {
        Kind::Notified => Some(card::Kind::Notified),
        Kind::Failed => Some(card::Kind::Failed),
        Kind::Finished => Some(card::Kind::Finished),
        Kind::Busy(_) => None,
    }
}

/// Of what a session keeps, what its mark says: a notification or a failure,
/// which wait for you, over a command that has finished — the sign the picker
/// has beside it, and the news that sign is for.
fn marking(news: &[News]) -> Option<&News> {
    let rank = |n: &&News| match n.kind {
        Kind::Notified | Kind::Failed => 0,
        Kind::Finished => 1,
        Kind::Busy(_) => 2,
    };
    news.iter()
        .filter(|n| sign(n.kind).is_some())
        .min_by_key(rank)
}

/// Of what a session said, what to tell, oldest first: what it numbered past
/// `heard` — the highest it had said by the last read — and nothing while
/// `heard` is `None`, a first read, which is there to find out how far the
/// session had got. Of the session in front, `here`, only what came from a
/// terminal that was off the screen: the rest it was showing.
fn to_tell(heard: Option<u64>, news: &[News], here: bool) -> Vec<&News> {
    let Some(heard) = heard else {
        return Vec::new();
    };
    news.iter()
        .filter(|n| n.seq > heard)
        .filter(|n| !here || n.shown == Some(false))
        .collect()
}

/// What the notifier is handed.
enum Command {
    /// Read this session, called this, through this socket, from now on.
    Watch {
        id: String,
        name: String,
        sock: PathBuf,
    },
    /// This session is in front now; `None`, one of nvmux's own screens is.
    Front(Option<String>),
    /// What the listener with this number heard from its session.
    Pushed(u64, Pushed),
    /// The listener with this number has stopped: its session went, or its
    /// connection did, or the session could not be listened to.
    Stopped(u64),
    /// The notifier was dropped: stop, and every listener with it.
    Quit,
}

/// One session the notifier reads.
struct Watched {
    id: String,
    name: String,
    sock: PathBuf,
    /// The highest number among what it has said, by the last read; `None`
    /// until it has been read since nvmux's own screens were last up.
    heard: Option<u64>,
    /// Whether a card about it is up in front, which leaves its mark: to be
    /// let go of once it has nothing to say.
    marked: bool,
    /// When it is to be read next, while a session is in front.
    next: Instant,
    /// When it was last read: a read a push brings on comes no sooner than
    /// [`SPACING`] after.
    last: Option<Instant>,
    /// The listener on it.
    ear: Option<Ear>,
    /// Whether its watcher tells the listener its news, by the last read, and
    /// has not been found to have missed any: read every [`SAFETY`] while it
    /// does, and every [`notes::POLL`] otherwise.
    told: bool,
    /// Whether it has said its news changed since it was last read.
    pushed: bool,
    /// When a listener may be started on it again, after one stopped.
    relisten: Option<Instant>,
}

impl Watched {
    fn new(id: String, name: String, sock: PathBuf, next: Instant) -> Self {
        Self {
            id,
            name,
            sock,
            heard: None,
            marked: false,
            next,
            last: None,
            ear: None,
            told: false,
            pushed: false,
            relisten: None,
        }
    }
}

/// A listener on a session ([`notes::listen`]), on a thread of its own, and
/// numbered: what an earlier one on the session says is not taken for what
/// this one does. Dropped, it stops.
struct Ear {
    number: u64,
    /// Its channel, and the way to stop it, once it is connected.
    on: Option<(u64, rpc::Interrupt)>,
}

impl Ear {
    /// The channel a read names for the watcher to tell this listener.
    fn chan(&self) -> Option<u64> {
        self.on.as_ref().map(|(chan, _)| *chan)
    }
}

impl Drop for Ear {
    fn drop(&mut self) {
        // One still connecting stops as soon as it says it is listening, to
        // a notifier that no longer has it.
        if let Some((_, stop)) = &self.on {
            stop.fire();
        }
    }
}

/// Posts, as it happens, cards with what the other sessions have to report —
/// and of the session in front, what its terminals off the screen do — for
/// the relay in front to have its client draw.
///
/// It reads every session it is handed every [`notes::POLL`] while a session
/// is in front, and none while one of nvmux's own screens is: the picker has
/// a reader of its own, and shows it all. So coming back from one of those,
/// what was said meanwhile counts as told, and the first round only finds out
/// how far each session has got.
///
/// The reads are [`notes::hear`]'s, which ask nothing of an editor waiting for
/// a key — this runs for as long as nvmux does, and a session may wait for its
/// key all night.
///
/// It listens to every session as well ([`notes::listen`]), each on a thread
/// of its own, and reads one as soon as
/// it says its news has changed, though no sooner than [`SPACING`] after the
/// last read of it. A session whose watcher a read has found telling its
/// listener is read every [`SAFETY`] besides, rather than every
/// [`notes::POLL`]: the rest — one not listened to yet, or whose listener
/// stopped, or whose watcher was put in afresh — are read as often as ever,
/// and a read names the listener to the watcher. A read the timer brought on
/// that finds news a told listener never heard of starts the listener again.
///
/// With `[client] ui = "nvim"` there is no client to draw a card, and nothing
/// is read. Dropping it stops the thread without waiting for it, as with
/// [`notes::Poller`]: a read in flight finishes to nobody.
pub struct Notifier {
    commands: Option<Sender<Command>>,
}

impl Notifier {
    /// Start reading, with no session handed over yet and none in front.
    pub fn start() -> Self {
        if crate::config::get().client.ui == crate::config::Ui::Nvim {
            return Self { commands: None };
        }
        let settings = crate::config::get().keys;
        let (commands, inbox) = mpsc::channel();
        let ears = commands.clone();
        let spawned = std::thread::Builder::new()
            .name("nvmux-notifier".into())
            .spawn(move || run(&inbox, &ears, settings));
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "tell: no thread to read the sessions on");
        }
        Self {
            commands: Some(commands),
        }
    }

    /// Read session `id`, called `name`, through `sock` from now on — or, if it
    /// is read already, by this name and through this socket.
    pub fn watch(&self, id: &str, name: &str, sock: PathBuf) {
        self.send(Command::Watch {
            id: id.to_string(),
            name: name.to_string(),
            sock,
        });
    }

    /// Session `id` is in front now — handed over already, with [`watch`] —
    /// or, with `None`, one of nvmux's own screens is.
    ///
    /// [`watch`]: Notifier::watch
    pub fn front(&self, id: Option<&str>) {
        self.send(Command::Front(id.map(str::to_string)));
    }

    fn send(&self, command: Command) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(command);
        }
    }
}

impl Drop for Notifier {
    /// The thread hands its listeners what to say back on, so it never finds
    /// itself alone on its channel: it is told to stop.
    fn drop(&mut self) {
        self.send(Command::Quit);
    }
}

/// The notifier's thread: `inbox` is what it is handed, and `ears` where its
/// listeners say what they hear.
fn run(inbox: &Receiver<Command>, ears: &Sender<Command>, settings: KeySettings) {
    let mut watched: Vec<Watched> = Vec::new();
    let mut front: Option<String> = None;
    let mut numbered: u64 = 0;
    loop {
        // Nothing is read while nvmux's own screens are up.
        let due = front
            .as_ref()
            .and_then(|_| watched.iter().map(|w| w.next).min());
        let next = match due {
            Some(at) => inbox.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let now = Instant::now();
        match next {
            Ok(Command::Watch { id, name, sock }) => {
                match watched.iter_mut().find(|w| w.id == id) {
                    Some(w) => {
                        w.name = name;
                        if w.sock != sock {
                            // A listener on the old socket hears nothing of
                            // the new one.
                            w.sock = sock;
                            w.ear = None;
                            w.told = false;
                            w.relisten = None;
                            w.next = w.next.min(now + notes::POLL);
                        }
                    }
                    None => watched.push(Watched::new(id, name, sock, now + SETTLE)),
                }
            }
            Ok(Command::Front(id)) => {
                if id == front {
                    continue;
                }
                // Back from one of nvmux's own screens: what was said while it
                // was up was on it, or is old by now. And whatever is in front
                // now has come up with nothing on it.
                for w in &mut watched {
                    if front.is_none() {
                        w.heard = None;
                    }
                    w.marked = false;
                    w.next = now + SETTLE;
                }
                front = id;
            }
            Ok(Command::Pushed(number, pushed)) => {
                let w = watched
                    .iter_mut()
                    .find(|w| w.ear.as_ref().is_some_and(|e| e.number == number));
                match (w, pushed) {
                    (Some(w), Pushed::Listening { chan, stop }) => {
                        if let Some(ear) = w.ear.as_mut() {
                            ear.on = Some((chan, stop));
                        }
                    }
                    (Some(w), Pushed::News) => {
                        w.pushed = true;
                        let spaced = w.last.map_or(now, |at| at + SPACING);
                        w.next = w.next.min((now + SETTLE).max(spaced));
                    }
                    // An earlier listener's, or one on a session no longer
                    // read.
                    (None, Pushed::Listening { stop, .. }) => stop.fire(),
                    (None, Pushed::News) => {}
                }
            }
            Ok(Command::Stopped(number)) => {
                let w = watched
                    .iter_mut()
                    .find(|w| w.ear.as_ref().is_some_and(|e| e.number == number));
                if let Some(w) = w {
                    w.ear = None;
                    w.told = false;
                    w.relisten = Some(now + RELISTEN);
                    // Gone, perhaps: read soon, to let go of its mark.
                    w.next = w.next.min(now + SETTLE);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(id) = front.as_deref() {
                    round(&mut watched, id, settings, now);
                }
            }
            Ok(Command::Quit) | Err(RecvTimeoutError::Disconnected) => return,
        }
        listeners(&mut watched, ears, &mut numbered, now);
    }
}

/// A listener on every session without one, but for one whose listener
/// stopped a moment ago. Each is numbered from `numbered`, and says what it
/// hears on `ears`.
fn listeners(watched: &mut [Watched], ears: &Sender<Command>, numbered: &mut u64, now: Instant) {
    for w in watched {
        if w.ear.is_some() || w.relisten.is_some_and(|at| at > now) {
            continue;
        }
        *numbered += 1;
        if listen(*numbered, w.sock.clone(), ears.clone()) {
            w.ear = Some(Ear {
                number: *numbered,
                on: None,
            });
        } else {
            w.relisten = Some(now + RELISTEN);
        }
    }
}

/// Start listener `number` on the session on `sock`, on a thread of its own,
/// saying what it hears, and when it stops, on `ears`. Whether there was a
/// thread for it.
fn listen(number: u64, sock: PathBuf, ears: Sender<Command>) -> bool {
    let spawned = std::thread::Builder::new()
        .name("nvmux-listener".into())
        .spawn(move || {
            let heard = |pushed| ears.send(Command::Pushed(number, pushed)).is_ok();
            if let Err(e) = notes::listen(&sock, heard) {
                tracing::debug!(sock = %sock.display(), error = %e, "tell: stopped listening");
            }
            let _ = ears.send(Command::Stopped(number));
        });
    if let Err(e) = &spawned {
        tracing::debug!(error = %e, "tell: no thread to listen on");
    }
    spawned.is_ok()
}

/// Read every session due by `now`, all at a time, and post a card for the
/// session in front, `front`, of what is news — oldest first, whichever
/// session said it ([`oldest_first`]). On a first read, which says only how
/// far a session has got, what another keeps is its mark alone; and of one
/// that keeps nothing any more, or is gone, the mark is let go of. One gone
/// for good, its socket with it ([`vanished`]), is read no more. Each read
/// names the session's listener, where it has one, and says when it is to be
/// read next.
fn round(watched: &mut Vec<Watched>, front: &str, settings: KeySettings, now: Instant) {
    let heard: Vec<(usize, Heard)> = std::thread::scope(|s| {
        let reads: Vec<_> = watched
            .iter()
            .enumerate()
            .filter(|(_, w)| w.next <= now)
            .map(|(i, w)| {
                let sock = &w.sock;
                let listener = w.ear.as_ref().and_then(Ear::chan);
                s.spawn(move || (i, notes::hear(sock, listener)))
            })
            .collect();
        reads.into_iter().filter_map(|r| r.join().ok()).collect()
    });
    let read_at = Instant::now();
    let post = |what| Post {
        front: front.to_string(),
        what,
    };
    let mut told = Vec::new();
    let mut forgot = Vec::new();
    let mut ended = Vec::new();
    for (i, heard) in heard {
        let w = &mut watched[i];
        w.last = Some(read_at);
        let pushed = std::mem::take(&mut w.pushed);
        let (news, listening) = match heard {
            Heard::Said { news, listening } => (news, listening),
            Heard::Gone => {
                w.told = false;
                w.next = read_at + notes::POLL;
                if std::mem::take(&mut w.marked) {
                    forgot.push(w.id.clone());
                }
                if vanished(&w.sock) {
                    ended.push(w.id.clone());
                }
                continue;
            }
            Heard::Unknown => {
                // What it said it has is still to be read, and soon: it will
                // not say so again.
                w.pushed = pushed;
                let wait = if w.told && !pushed {
                    SAFETY
                } else {
                    notes::POLL
                };
                w.next = read_at + wait;
                continue;
            }
        };
        // News that a listener being told since the last read never heard
        // of: its connection is not bringing what the watcher says. Started
        // again, in `listeners`.
        let missed =
            listening && !pushed && w.heard.is_some_and(|h| news.iter().any(|n| n.seq > h));
        if missed {
            tracing::debug!(id = %w.id, "tell: news came that the listener did not hear");
            w.ear = None;
        }
        w.told = listening && !missed;
        w.next = read_at + if w.told { SAFETY } else { notes::POLL };
        let here = w.id == front;
        let from = (!here).then_some((w.id.as_str(), w.name.as_str()));
        let marks = marking(&news);
        let mark = marks.and_then(|n| sign(n.kind));
        let mut cards = Vec::new();
        if w.heard.is_none() {
            if let Some(n) = marks.filter(|_| !here) {
                cards.extend(card_for(n, from, settings).map(|(c, go)| (n.age, c.as_mark(), go)));
            }
        }
        for n in to_tell(w.heard, &news, here) {
            cards.extend(card_for(n, from, settings).map(|(c, go)| (n.age, c, go)));
        }
        for (age, card, go) in cards {
            // What another session keeps leaves the one sign the picker has
            // for it, whichever news came last.
            let card = if here || card.mark.is_none() {
                card
            } else {
                w.marked = true;
                card.leaving(mark)
            };
            told.push((age, post(Posted::Card(card, go))));
        }
        if mark.is_none() && std::mem::take(&mut w.marked) {
            forgot.push(w.id.clone());
        }
        let top = news.iter().map(|n| n.seq).max().unwrap_or(0);
        w.heard = Some(w.heard.map_or(top, |h| h.max(top)));
    }
    for id in forgot {
        card::board().post(post(Posted::Forget(id)));
    }
    for p in oldest_first(told) {
        card::board().post(p);
    }
    watched.retain(|w| !ended.contains(&w.id));
}

/// Whether nothing is at `sock` any more, which of a session read as gone
/// means gone for good, and not worth a connect every two seconds for as long
/// as nvmux runs. A socket goes only with its session: the editor's own as it
/// quits or is reaped, and a forward as nvmux takes it down, for a session
/// killed or no longer listed. One still there that refuses may yet answer:
/// a forward that a dropped link left behind does, once it is made again,
/// and the picker floating over a session makes it again for its own reads
/// of notes, without handing the session here.
fn vanished(sock: &Path) -> bool {
    matches!(
        std::fs::symlink_metadata(sock),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
    )
}

/// What a round tells, `(age, post)` as the sessions were read, in the order
/// to post it: oldest first, so that the newest card lands on top whichever
/// session said it. What one session said, and what several said the same
/// second, keeps the order it came in.
fn oldest_first(mut told: Vec<(u64, Post)>) -> Vec<Post> {
    told.sort_by_key(|&(age, _)| std::cmp::Reverse(age));
    told.into_iter().map(|(_, post)| post).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The keys a card names, as they are by default but for the prefix.
    fn prefixed(prefix: u8) -> KeySettings {
        KeySettings {
            prefix,
            ..KeySettings::default()
        }
    }

    fn news(kind: Kind, title: &str, text: &str, seq: u64, shown: Option<bool>) -> News {
        News {
            kind,
            title: title.to_string(),
            text: text.to_string(),
            seq,
            shown,
            age: 3,
            buf: None,
            tab: None,
        }
    }

    /// A card with news from another session: its sign, its name, what
    /// happened in the picker's words, how long ago, and the key that goes
    /// there — which goes there.
    #[test]
    fn a_card_from_another_session_names_it_and_goes_there() {
        let finished = news(Kind::Finished, "cargo test", "exited with code 0", 1, None);
        let (card, go) =
            card_for(&finished, Some(("abcd2345", "api-server")), prefixed(0)).expect("news");
        assert_eq!(card.kind, card::Kind::Finished);
        assert_eq!(card.title, "api-server");
        assert_eq!(card.body.as_deref(), Some("cargo test: exited with code 0"));
        assert_eq!(card.age, Some(3));
        assert_eq!(card.footer.as_deref(), Some("Ctrl-Space ⏎ go there"));
        assert_eq!(go, Some(Go::Session(ids::Id::new("abcd2345").expect("id"))));

        let notified = news(
            Kind::Notified,
            "Claude Code",
            "Claude needs your permission",
            2,
            None,
        );
        let (card, _) = card_for(&notified, Some(("abcd2345", "blog")), prefixed(0)).expect("news");
        assert_eq!(card.kind, card::Kind::Notified);
        assert_eq!(
            card.body.as_deref(),
            Some("Claude Code · Claude needs your permission")
        );

        let failed = news(Kind::Failed, "make", "exited with code 2", 3, None);
        let (card, go) = card_for(&failed, Some(("not an id", "x")), prefixed(0)).expect("news");
        assert_eq!(card.kind, card::Kind::Failed);
        assert_eq!((card.footer, go), (None, None), "nowhere to go, so no key");
        assert_eq!(card.mark, None, "and no mark to wait for it");
        assert!(card_for(&news(Kind::Busy(None), "", "", 4, None), None, prefixed(0)).is_none());
    }

    /// A card is about the session it came from, which a newer one about it
    /// replaces; of the session in front, about the terminal; and leaves its
    /// own sign.
    #[test]
    fn a_card_is_about_where_it_came_from() {
        let finished = news(Kind::Finished, "cargo test", "exited with code 0", 1, None);
        let (card, _) =
            card_for(&finished, Some(("abcd2345", "api-server")), prefixed(0)).expect("news");
        assert_eq!(card.about.as_deref(), Some("abcd2345"));
        assert_eq!(card.mark, Some(card::Kind::Finished));
        let mut own = news(Kind::Failed, "sh", "exited with code 3", 1, Some(false));
        own.buf = Some(5);
        let (card, _) = card_for(&own, None, prefixed(0)).expect("news");
        assert_eq!(card.about.as_deref(), Some("here 5"));
    }

    /// What a session's mark says is what the picker's sign does: a
    /// notification or a failure, waiting, over a command that has finished
    /// since; and nothing, where it keeps nothing.
    #[test]
    fn a_mark_is_the_pickers_sign() {
        let finished = news(Kind::Finished, "cargo test", "exited with code 0", 2, None);
        let notified = news(Kind::Notified, "Claude Code", "waiting", 1, None);
        let kept = [notified.clone(), finished.clone()];
        assert_eq!(marking(&kept).map(|n| n.kind), Some(Kind::Notified));
        assert_eq!(marking(&[finished]).map(|n| n.kind), Some(Kind::Finished));
        assert_eq!(marking(&[]), None);
        assert_eq!(
            marking(&[news(Kind::Busy(Some(3)), "lua_ls", "", 1, None)]),
            None
        );
    }

    /// Of the session in front: `here`, and where its terminal is, and the
    /// key that shows it — where it can be shown.
    #[test]
    fn a_card_from_the_session_in_front_is_here() {
        let mut own = news(Kind::Failed, "sh", "exited with code 3", 1, Some(false));
        own.buf = Some(5);
        own.tab = Some(2);
        let (card, go) = card_for(&own, None, prefixed(0x14)).expect("news");
        assert_eq!(card.title, "here · tab 2");
        assert_eq!(card.footer.as_deref(), Some("Ctrl-t ⏎ show it"));
        assert_eq!(go, Some(Go::Buffer(5)));

        own.tab = None;
        assert_eq!(
            card_for(&own, None, prefixed(0)).expect("news").0.title,
            "here"
        );
        own.buf = None;
        let (card, go) = card_for(&own, None, prefixed(0)).expect("news");
        assert_eq!((card.footer, go), (None, None));
    }

    /// A card names the key that goes there as the user has it: with `go`
    /// moved off Enter, the key it was moved to, drawn the way the hint bar
    /// draws it.
    #[test]
    fn a_card_names_the_key_go_was_moved_to() {
        let mut settings = prefixed(0);
        assert!(settings.bind.bind("go", b'g'));
        let finished = news(Kind::Finished, "cargo test", "exited with code 0", 1, None);
        let (card, _) =
            card_for(&finished, Some(("abcd2345", "api-server")), settings).expect("news");
        assert_eq!(card.footer.as_deref(), Some("Ctrl-Space g go there"));

        let mut settings = prefixed(0);
        assert!(settings.bind.bind("go", b' '));
        assert!(
            settings.bind.bind("picker", b'\r'),
            "trading Enter for Space"
        );
        let (card, _) =
            card_for(&finished, Some(("abcd2345", "api-server")), settings).expect("news");
        assert_eq!(card.footer.as_deref(), Some("Ctrl-Space ␣ go there"));
    }

    /// The first read only finds out how far a session has got; after it,
    /// what is numbered past that is news, oldest first, once.
    #[test]
    fn news_is_what_is_numbered_past_the_last_read() {
        let said = [
            news(Kind::Finished, "make", "exited with code 0", 4, Some(true)),
            news(Kind::Failed, "make", "exited with code 2", 7, Some(true)),
        ];
        assert!(to_tell(None, &said, false).is_empty(), "a first read");
        let told: Vec<u64> = to_tell(Some(4), &said, false)
            .iter()
            .map(|n| n.seq)
            .collect();
        assert_eq!(told, [7]);
        assert!(to_tell(Some(7), &said, false).is_empty(), "told already");
    }

    /// In front, a session is told only what came from a terminal off the
    /// screen: not one it was showing, and not a progress message, which it
    /// shows itself.
    #[test]
    fn the_session_in_front_is_told_only_of_a_terminal_off_the_screen() {
        let said = [
            news(Kind::Finished, "sh", "exited with code 0", 1, Some(true)),
            news(Kind::Failed, "sh", "exited with code 3", 2, Some(false)),
            news(Kind::Failed, "claude", "failed", 3, None),
        ];
        let told: Vec<u64> = to_tell(Some(0), &said, true)
            .iter()
            .map(|n| n.seq)
            .collect();
        assert_eq!(told, [2]);
        assert_eq!(
            to_tell(Some(0), &said, false).len(),
            3,
            "from elsewhere, all"
        );
    }

    /// A session read as gone with its socket gone too is read no more. One
    /// whose socket is still there, refusing, is kept: so is a forward that a
    /// dropped link left behind, which answers again once it is made anew.
    #[test]
    fn a_session_gone_with_its_socket_is_read_no_more() {
        let left = crate::test_support::scratch_sock("tell-left");
        drop(std::os::unix::net::UnixListener::bind(&left).expect("bind"));
        let now = Instant::now();
        let watched = |id: &str, sock| Watched::new(id.to_string(), id.to_string(), sock, now);
        let mut all = vec![
            watched("quit2345", crate::test_support::scratch_sock("tell-quit")),
            watched("left2345", left.clone()),
        ];
        round(&mut all, "front234", prefixed(0), now);
        let ids: Vec<&str> = all.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["left2345"], "the one with its socket still there");
        let _ = std::fs::remove_file(&left);
    }

    /// What a round tells is posted oldest first, so that the newest card is
    /// the one on top, though another session was read before the one that
    /// said it; and what one session said in a second keeps its order.
    #[test]
    fn what_a_round_tells_is_posted_oldest_first_whichever_session_said_it() {
        let told = |name: &str, text: &str, age: u64| {
            let said = News {
                age,
                ..news(Kind::Finished, text, "exited with code 0", 1, None)
            };
            let (card, go) = card_for(&said, Some(("abcd2345", name)), prefixed(0)).expect("news");
            let post = Post {
                front: "front".to_string(),
                what: Posted::Card(card, go),
            };
            (age, post)
        };
        let posted = oldest_first(vec![
            told("blog", "npm run build", 1),
            told("notes", "git push", 2),
            told("api-server", "cargo test", 4),
            told("api-server", "cargo clippy", 4),
        ]);
        let card = |p: &Post| p.card().cloned().expect("a card");
        assert_eq!(
            posted
                .iter()
                .map(|p| (card(p).title, card(p).age))
                .collect::<Vec<_>>(),
            [
                ("api-server".to_string(), Some(4)),
                ("api-server".to_string(), Some(4)),
                ("notes".to_string(), Some(2)),
                ("blog".to_string(), Some(1)),
            ]
        );
        assert_eq!(
            card(&posted[0]).body.as_deref(),
            Some("cargo test: exited with code 0"),
            "{posted:?}"
        );
    }
}
