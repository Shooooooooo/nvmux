//! What the other sessions report while you are in one, told as it happens: a
//! notification a program in one of their terminal buffers asked for (`∗`), a
//! command there that failed (`!`) or finished (`✓`) — the picker's signs,
//! while the picker is down — as cards in the corner of the session in front
//! (see [`crate::card`]). Of the session in front, only what a terminal of its
//! own off the screen says: the rest it was showing.
//!
//! The news is read off each session's watcher ([`crate::notes`]), and its
//! cards are posted for the relay in front ([`crate::card::board`]), which has
//! its client draw them. Nothing is asked of the session in front to show
//! them, so they are up as soon as the news is read, whatever the editor is
//! doing and however far away it is; and they are nvmux's, with nothing of
//! them left in the editor once they go.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::card::{self, Card, Go, Post};
use crate::notes::{self, Heard, Kind, News};
use crate::ui::note;
use crate::{ids, keys};

/// How long the notifier waits, once a session is in front, before its first
/// round of reads: long enough for the sessions handed to it at the same
/// moment to be read in that round.
const SETTLE: Duration = Duration::from_millis(30);

/// The card that tells `news`, from the session with this id and name — or,
/// with `None`, from the session in front itself — and where acting on it
/// goes: that session, or the terminal it came from. `prefix` is the prefix
/// key, which the card names. `None` for what is not news: what is busy.
pub fn card_for(news: &News, from: Option<(&str, &str)>, prefix: u8) -> Option<(Card, Option<Go>)> {
    let kind = match news.kind {
        Kind::Notified => card::Kind::Notified,
        Kind::Failed => card::Kind::Failed,
        Kind::Finished => card::Kind::Finished,
        Kind::Busy(_) => return None,
    };
    let what = note::what(news.kind, &news.title, &news.text);
    let key = format!("{} {}", keys::prefix_label(prefix), keys::ENTER_GLYPH);
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
    Some((card, go))
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
}

/// One session the notifier reads.
struct Watched {
    id: String,
    name: String,
    sock: PathBuf,
    /// The highest number among what it has said, by the last read; `None`
    /// until it has been read since nvmux's own screens were last up.
    heard: Option<u64>,
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
        let prefix = crate::config::get().keys.prefix;
        let (commands, inbox) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("nvmux-notifier".into())
            .spawn(move || run(&inbox, prefix));
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

fn run(inbox: &Receiver<Command>, prefix: u8) {
    let mut watched: Vec<Watched> = Vec::new();
    let mut front: Option<String> = None;
    let mut due: Option<Instant> = None;
    loop {
        let next = match due {
            Some(at) => inbox.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match next {
            Ok(Command::Watch { id, name, sock }) => {
                match watched.iter_mut().find(|w| w.id == id) {
                    Some(w) => {
                        w.name = name;
                        w.sock = sock;
                    }
                    None => watched.push(Watched {
                        id,
                        name,
                        sock,
                        heard: None,
                    }),
                }
            }
            Ok(Command::Front(id)) => {
                if id == front {
                    continue;
                }
                // Back from one of nvmux's own screens: what was said while it
                // was up was on it, or is old by now.
                if front.is_none() {
                    for w in &mut watched {
                        w.heard = None;
                    }
                }
                front = id;
                due = front.is_some().then(|| Instant::now() + SETTLE);
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(id) = front.as_deref() {
                    round(&mut watched, id, prefix);
                }
                due = front.is_some().then(|| Instant::now() + notes::POLL);
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Read every session, all at a time, and post a card for the session in
/// front, `front`, of what is news.
fn round(watched: &mut [Watched], front: &str, prefix: u8) {
    let heard: Vec<(usize, Heard)> = std::thread::scope(|s| {
        let reads: Vec<_> = watched
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let sock = &w.sock;
                s.spawn(move || (i, notes::hear(sock)))
            })
            .collect();
        reads.into_iter().filter_map(|r| r.join().ok()).collect()
    });
    for (i, heard) in heard {
        let Heard::Said(news) = heard else {
            continue;
        };
        let w = &mut watched[i];
        let here = w.id == front;
        let from = (!here).then_some((w.id.as_str(), w.name.as_str()));
        for n in to_tell(w.heard, &news, here) {
            if let Some((card, go)) = card_for(n, from, prefix) {
                card::board().post(Post {
                    front: front.to_string(),
                    card,
                    go,
                });
            }
        }
        let top = news.iter().map(|n| n.seq).max().unwrap_or(0);
        w.heard = Some(w.heard.map_or(top, |h| h.max(top)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (card, go) = card_for(&finished, Some(("abcd2345", "api-server")), 0).expect("news");
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
        let (card, _) = card_for(&notified, Some(("abcd2345", "blog")), 0).expect("news");
        assert_eq!(card.kind, card::Kind::Notified);
        assert_eq!(
            card.body.as_deref(),
            Some("Claude Code · Claude needs your permission")
        );

        let failed = news(Kind::Failed, "make", "exited with code 2", 3, None);
        let (card, go) = card_for(&failed, Some(("not an id", "x")), 0).expect("news");
        assert_eq!(card.kind, card::Kind::Failed);
        assert_eq!((card.footer, go), (None, None), "nowhere to go, so no key");
        assert!(card_for(&news(Kind::Busy(None), "", "", 4, None), None, 0).is_none());
    }

    /// Of the session in front: `here`, and where its terminal is, and the
    /// key that shows it — where it can be shown.
    #[test]
    fn a_card_from_the_session_in_front_is_here() {
        let mut own = news(Kind::Failed, "sh", "exited with code 3", 1, Some(false));
        own.buf = Some(5);
        own.tab = Some(2);
        let (card, go) = card_for(&own, None, 0x14).expect("news");
        assert_eq!(card.title, "here · tab 2");
        assert_eq!(card.footer.as_deref(), Some("Ctrl-t ⏎ show it"));
        assert_eq!(go, Some(Go::Buffer(5)));

        own.tab = None;
        assert_eq!(card_for(&own, None, 0).expect("news").0.title, "here");
        own.buf = None;
        let (card, go) = card_for(&own, None, 0).expect("news");
        assert_eq!((card.footer, go), (None, None));
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
}
