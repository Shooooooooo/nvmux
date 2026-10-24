//! Live cards in the front session's margin (see [`crate::card`]) for news
//! from other sessions' terminal buffers: a notification (`∗`), or a command
//! that failed (`!`) or finished (`✓`), as the picker's signs. For the front
//! session itself, only news from its terminals not on screen.
//!
//! After a card's text goes, unopened news stays as the session's mark (its
//! picker sign). After a switch, sessions with pending news show only their
//! mark, the text having been shown already.
//!
//! News is read from each session's watcher ([`crate::notes`]) and posted to
//! the front relay ([`crate::card::board`]), whose client draws it. Nothing is
//! asked of the front editor, so cards appear whatever it is doing and leave
//! nothing behind in it.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::card::{self, Card, Go, Post, Posted};
use crate::config::KeySettings;
use crate::notes::{self, Heard, Kind, News, Pushed};
use crate::ui::note;
use crate::{ids, keys, rpc};

/// Delay before the first round after a session comes to the front, so
/// sessions handed over together share it; and before reading after a push,
/// so the rest of one event (a command's end, reported by its shell and by
/// its terminal closing) is read with it.
const SETTLE: Duration = Duration::from_millis(30);

/// Read interval for sessions that push their news, to catch what a push
/// missed on a connection that went quiet without closing.
const SAFETY: Duration = Duration::from_secs(30);

/// Minimum gap between push-triggered reads of one session, so a flood of
/// notifications reads it at most four times a second.
const SPACING: Duration = Duration::from_millis(250);

/// Wait before restarting a stopped listener; the session is read every
/// [`notes::POLL`] meanwhile.
const RELISTEN: Duration = Duration::from_secs(10);

/// The card for `news` from session `(id, name)`, or from the front session
/// if `from` is `None`, and where acting on it goes (that session, or the
/// terminal). The footer names the user's `go` key (`<prefix> Enter` by
/// default). `None` for non-news (busy).
///
/// The card is about its session or terminal (see [`Card::about`]) and
/// leaves its own sign as a mark, or none if it goes nowhere.
pub fn card_for(
    news: &News,
    from: Option<(&str, &str)>,
    settings: KeySettings,
) -> Option<(Card, Option<Go>)> {
    let kind = sign(news.kind)?;
    let what = note::what(news.kind, &news.title, &news.text);
    // Every row of the bind table has a key, so `go`'s is never missing.
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
    // With nowhere to go nothing would ever clear a mark, so leave none.
    let card = if go.is_some() {
        card
    } else {
        card.leaving(None)
    };
    Some((card, go))
}

/// `None` for non-news.
fn sign(kind: Kind) -> Option<card::Kind> {
    match kind {
        Kind::Notified => Some(card::Kind::Notified),
        Kind::Failed => Some(card::Kind::Failed),
        Kind::Finished => Some(card::Kind::Finished),
        Kind::Busy(_) => None,
    }
}

/// The news a session's mark (its picker sign) shows: a notification or
/// failure outranks a finished command.
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

/// News to tell, oldest first: entries numbered past `heard` (the highest
/// seq at the last read). Nothing on a first read (`heard` is `None`), which
/// only sets the baseline. For the front session (`here`), only news from
/// terminals not on screen.
fn to_tell(heard: Option<u64>, news: &[News], here: bool) -> Vec<&News> {
    let Some(heard) = heard else {
        return Vec::new();
    };
    news.iter()
        .filter(|n| n.seq > heard)
        .filter(|n| !here || n.shown == Some(false))
        .collect()
}

/// Messages to the notifier thread.
enum Command {
    Watch {
        id: String,
        name: String,
        sock: PathBuf,
    },
    /// `None` when one of nvmux's own screens is up.
    Front(Option<String>),
    /// From the listener with this number.
    Pushed(u64, Pushed),
    /// That listener stopped: session or connection gone, or it could not
    /// listen.
    Stopped(u64),
    /// The notifier was dropped: stop, with every listener.
    Quit,
}

struct Watched {
    id: String,
    name: String,
    sock: PathBuf,
    /// Highest seq at the last read; `None` until read since nvmux's own
    /// screens were last up.
    heard: Option<u64>,
    /// Whether it has a mark up in front, to forget once it has no news.
    marked: bool,
    /// Next read, while a session is in front.
    next: Instant,
    /// Push-triggered reads wait [`SPACING`] after this.
    last: Option<Instant>,
    ear: Option<Ear>,
    /// Whether, at the last read, its watcher pushed to the listener and has
    /// not been caught missing news: read every [`SAFETY`] if so, otherwise
    /// every [`notes::POLL`].
    told: bool,
    /// Whether it pushed news since the last read.
    pushed: bool,
    /// When a listener may be restarted after one stopped.
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

/// A listener thread ([`notes::listen`]). Numbered so reports from an
/// earlier listener on the same session are ignored. Stops when dropped.
struct Ear {
    number: u64,
    /// Its channel and stop handle, once connected.
    on: Option<(u64, rpc::Interrupt)>,
}

impl Ear {
    /// The channel a read passes so the watcher pushes to this listener.
    fn chan(&self) -> Option<u64> {
        self.on.as_ref().map(|(chan, _)| *chan)
    }
}

impl Drop for Ear {
    fn drop(&mut self) {
        // One still connecting has no stop handle yet; `run` stops it when
        // it reports listening and is not found.
        if let Some((_, stop)) = &self.on {
            stop.fire();
        }
    }
}

/// Posts cards with other sessions' news (and the front session's
/// off-screen terminals) for the front relay's client to draw.
///
/// While a session is in front, every session is read on a timer;
/// while nvmux's own screens are up, none are (the picker has its own
/// reader). So on return, news from meanwhile counts as told and the first
/// round only sets each session's baseline. Reads use [`notes::hear`], which
/// asks nothing of an editor waiting for a key.
///
/// Each session also gets a listener thread ([`notes::listen`]); a push
/// triggers a read, at most once per [`SPACING`]. A session whose watcher is
/// found pushing to its listener is polled only every [`SAFETY`]; the rest
/// every [`notes::POLL`]. Each read passes the listener's channel to the
/// watcher. A timer read finding news the listener missed restarts it.
///
/// Does nothing with `[client] ui = "nvim"` (no client to draw cards).
/// Dropping it stops the thread without waiting, as with [`notes::Poller`].
pub struct Notifier {
    commands: Option<Sender<Command>>,
}

impl Notifier {
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

    /// Read session `id` through `sock` from now on, or update its name and
    /// socket if already read.
    pub fn watch(&self, id: &str, name: &str, sock: PathBuf) {
        self.send(Command::Watch {
            id: id.to_string(),
            name: name.to_string(),
            sock,
        });
    }

    /// Session `id` (already passed to [`watch`]) is now in front; `None` when
    /// one of nvmux's own screens is.
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
    /// The thread holds a sender to its own inbox (for its listeners), so the
    /// channel never disconnects: it must be told to stop.
    fn drop(&mut self) {
        self.send(Command::Quit);
    }
}

/// The notifier thread. Listeners report on `ears`.
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
                            // A listener on the old socket hears nothing
                            // from the new one.
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
                // Back from nvmux's own screens: news from meanwhile was shown
                // there or is stale. The new front starts with no cards up.
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
                    // From an earlier listener, or a session no longer read.
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
                    // Maybe gone: read soon to forget its mark.
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

/// Start a listener on each session lacking one, unless its last stopped
/// within [`RELISTEN`].
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

/// Spawn listener `number` on `sock`, reporting news and its stop on `ears`.
/// Returns whether the thread spawned.
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

/// Read every session due by `now` in parallel and post cards for new news to
/// `front`, oldest first across sessions ([`oldest_first`]). On a first read,
/// another session's pending news posts only its mark; a session with none
/// left, or gone, has its mark forgotten. One whose socket is gone too
/// ([`vanished`]) is dropped.
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
                // Keep the push pending: it will not be sent again.
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
        // News the listener should have been pushed but was not: its
        // connection is broken. Restarted in `listeners`.
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
            // Another session's cards leave its picker sign as the mark,
            // whichever news came last.
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

/// Whether `sock` no longer exists, so a session read as gone is gone for
/// good and not worth polling. Sockets vanish only with their session. One
/// that exists but refuses may answer again: a forward left by a dropped link
/// does once remade, which the floating picker may do without telling us.
fn vanished(sock: &Path) -> bool {
    matches!(
        std::fs::symlink_metadata(sock),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
    )
}

/// Sort oldest first, so the newest card lands on top whichever session
/// said it. Stable: equal ages keep their order.
fn oldest_first(mut told: Vec<(u64, Post)>) -> Vec<Post> {
    told.sort_by_key(|&(age, _)| std::cmp::Reverse(age));
    told.into_iter().map(|(_, post)| post).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Not news from a terminal on screen, nor a progress message, which the
    /// session shows itself.
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

    /// One whose socket remains (refusing) is kept: a forward left by a
    /// dropped link answers again once remade.
    #[test]
    fn a_session_gone_with_its_socket_is_read_no_more() {
        let left = crate::test_support::scratch_sock("tell-left");
        crate::test_support::dead_socket(&left);
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

    /// Equal ages keep their order.
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
