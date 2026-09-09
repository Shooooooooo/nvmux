//! Directory completion for the create prompt, off the keypress path.
//!
//! Typing a path must never wait for a directory to be listed. Locally that
//! listing is a millisecond; over ssh it is a round trip, measured at ~230ms
//! even to localhost (see `scripts/list.sh`), and a prompt that spent that on
//! every keystroke would be unusable on the connection this feature exists for.
//! So [`Completer::ask`] is what a keystroke calls, it answers from memory or
//! sends a request, and it never does any I/O at all.
//!
//! # The first thread in this crate, and why
//!
//! nvmux is otherwise one thread and one `libc::poll`, and [`crate::pty`] is
//! emphatic about why: a second reader of the same stream splits it, and a
//! thread parked in a blocking `read` cannot be cancelled. Neither applies here
//! — this thread reads no terminal and owns nothing the UI needs back.
//!
//! What does apply is that the screens do not poll a set of file descriptors at
//! all. They poll through crossterm's `event::poll`, which takes a timeout and
//! nothing else, so there is no fd for a self-pipe to join — the mechanism
//! [`crate::winch`] uses to keep SIGWINCH off a thread is simply not available
//! here. A channel drained on the existing tick is what is left, and
//! [`super::poll_key_for`] shortens that tick while an answer is outstanding so
//! the wait is a frame rather than a quarter of a second.
//!
//! The thread is never joined. Dropping the [`Completer`] drops the sending end,
//! which ends the worker's loop after its current listing returns — and a
//! wedged ssh must not hang the prompt on the way out. What is left running
//! holds a hostname, a path and a dead channel, and exits on its own.
//!
//! # Three properties, and what each one is for
//!
//! **Coalescing.** The worker takes a request and then drains everything queued
//! behind it, serving only the newest. Typing `projects` faster than a round
//! trip costs one listing rather than eight.
//!
//! **Sequence numbers.** Only the answer to the question currently being asked
//! is displayed. Without this a slow answer landing after a fast one would
//! replace the right candidates with stale ones — the classic autocomplete bug,
//! and one that gets *more* likely the better the cache works, because a cached
//! answer is instant and an in-flight one is not.
//!
//! **A prefix-extension cache.** A listing of `/home/you` for `p` also answers
//! `pr`, `pro` and `proj`, by filtering in memory. So a path costs about one
//! round trip per `/` rather than one per keystroke, and backspacing inside a
//! component is usually free. The exception is a truncated listing, which is
//! only part of an answer and so can be shown but never narrowed: what it left
//! out may be exactly what the longer prefix wanted.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use crate::dirs::{self, DirSource};
use crate::transport::protocol::Listing;

/// How many directories to remember. A path is a handful of components and a
/// user moves between a handful of trees; past that, re-asking costs one round
/// trip and holding on costs memory for answers that have gone stale anyway.
const CACHE: usize = 16;

/// What the prompt asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Request {
    seq: u64,
    dir: String,
    prefix: String,
}

/// What the host said, and which question it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reply {
    seq: u64,
    dir: String,
    prefix: String,
    listing: Listing,
}

/// A listing kept for later.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cached {
    dir: String,
    prefix: String,
    listing: Listing,
}

/// What is on screen right now.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Shown {
    dir: String,
    prefix: String,
    names: Vec<String>,
    truncated: bool,
}

/// Directory completion for one prompt.
pub struct Completer {
    /// `None` only in tests, which drive the channels themselves. Dropping this
    /// is what ends the worker.
    tx: Option<Sender<Request>>,
    rx: Receiver<Reply>,
    /// The question being asked. A reply carrying anything else is an answer to
    /// a question that has moved on.
    seq: u64,
    /// Set while a request is outstanding, so the prompt can poll faster and
    /// say that it is waiting rather than that there is nothing.
    awaiting: bool,
    cache: Vec<Cached>,
    shown: Shown,
}

impl Completer {
    /// Start completing against `source`, which the worker takes with it.
    pub fn new(source: DirSource) -> Self {
        let (ask_tx, ask_rx) = mpsc::channel::<Request>();
        let (reply_tx, reply_rx) = mpsc::channel::<Reply>();

        // Not joined, and nothing waits on it: see the module docs.
        std::thread::Builder::new()
            .name("nvmux-complete".into())
            .spawn(move || serve(&source, &ask_rx, &reply_tx))
            // A thread that will not start is not worth failing a prompt over.
            // The sender is dropped with the closure, `ask` finds the channel
            // closed, and the field is simply one you type into unaided.
            .map_err(|e| tracing::debug!(error = %e, "no completion worker; typing is unaided"))
            .ok();

        Self {
            tx: Some(ask_tx),
            rx: reply_rx,
            seq: 0,
            awaiting: false,
            cache: Vec::new(),
            shown: Shown::default(),
        }
    }

    /// What has been typed has changed. Never blocks, never does I/O.
    pub fn ask(&mut self, input: &str) {
        let Some((dir, prefix)) = dirs::split(input) else {
            // Nothing to list: no `/` in what has been typed, and a working
            // directory is absolute, so there is no relative one to resolve.
            self.shown = Shown::default();
            self.awaiting = false;
            return;
        };
        if self.shown.dir == dir && self.shown.prefix == prefix && !self.awaiting {
            return;
        }
        if self.take_from_cache(dir, prefix) {
            return;
        }

        self.shown = Shown {
            dir: dir.to_string(),
            prefix: prefix.to_string(),
            ..Shown::default()
        };
        self.seq += 1;
        self.awaiting = true;
        let request = Request {
            seq: self.seq,
            dir: dir.to_string(),
            prefix: prefix.to_string(),
        };
        // An unbounded channel: this cannot block, and a closed one means the
        // worker is gone, which costs completions and nothing else.
        if let Some(tx) = &self.tx {
            if tx.send(request).is_err() {
                self.awaiting = false;
            }
        }
    }

    /// Take whatever the worker has sent. Returns whether the screen changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.rx.try_recv() {
                Ok(reply) => {
                    let current = reply.seq == self.seq;
                    self.remember(
                        reply.dir.clone(),
                        reply.prefix.clone(),
                        reply.listing.clone(),
                    );
                    if current {
                        self.shown = Shown {
                            dir: reply.dir,
                            prefix: reply.prefix,
                            names: reply.listing.names,
                            truncated: reply.listing.truncated,
                        };
                        self.awaiting = false;
                        changed = true;
                    } else if self.awaiting {
                        // An answer to an older question can still settle the
                        // current one: `/home/you` listed for `p` answers `pr`.
                        let (dir, prefix) = (self.shown.dir.clone(), self.shown.prefix.clone());
                        changed |= self.take_from_cache(&dir, &prefix);
                    }
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return changed,
            }
        }
    }

    /// Is an answer still outstanding? The prompt polls faster while it is, and
    /// says it is waiting rather than saying there is nothing there.
    pub fn waiting(&self) -> bool {
        self.awaiting
    }

    /// The dim text after the cursor: as much of the completion as is certain.
    ///
    /// The longest prefix every candidate shares, minus what has been typed. A
    /// single candidate is certain to its end, and gains a `/` so the next
    /// component can be typed without reaching for one.
    pub fn ghost(&self) -> String {
        let names = &self.shown.names;
        let Some(first) = names.first() else {
            return String::new();
        };
        let shared = if names.len() == 1 {
            format!("{first}/")
        } else {
            common_prefix(names)
        };
        // `strip_prefix` rather than slicing at the prefix's length: every
        // candidate *should* start with what was typed, because the host
        // matched them that way, but this reads a remote host's output and a
        // wrong answer must be nothing to show rather than a panic mid-glyph.
        shared
            .strip_prefix(self.shown.prefix.as_str())
            .unwrap_or("")
            .to_string()
    }

    /// The alternatives, for the row under the fields.
    pub fn candidates(&self) -> &[String] {
        &self.shown.names
    }

    /// The host stopped short of listing everything. Shown, so a row that is
    /// missing entries does not read as one that is complete.
    pub fn truncated(&self) -> bool {
        self.shown.truncated
    }

    /// Answer from a listing already in hand, if one covers this question.
    fn take_from_cache(&mut self, dir: &str, prefix: &str) -> bool {
        let Some(listing) = from_cache(&self.cache, dir, prefix) else {
            return false;
        };
        self.shown = Shown {
            dir: dir.to_string(),
            prefix: prefix.to_string(),
            names: listing.names,
            truncated: listing.truncated,
        };
        self.awaiting = false;
        true
    }

    fn remember(&mut self, dir: String, prefix: String, listing: Listing) {
        self.cache.retain(|c| c.dir != dir || c.prefix != prefix);
        self.cache.insert(
            0,
            Cached {
                dir,
                prefix,
                listing,
            },
        );
        self.cache.truncate(CACHE);
    }
}

/// The longest listing already held that answers this question.
///
/// A listing of `dir` for some prefix of `prefix` contains every name `prefix`
/// could match, so filtering it is a complete answer — unless it was truncated,
/// in which case it is only part of one and what it left out may be exactly what
/// was wanted. A truncated listing therefore answers its own prefix and nothing
/// longer.
///
/// Longest first, so the least filtering is done and, more to the point, so a
/// truncated exact match is preferred to a shorter complete one that would have
/// to be narrowed.
fn from_cache(cache: &[Cached], dir: &str, prefix: &str) -> Option<Listing> {
    cache
        .iter()
        .filter(|c| c.dir == dir && prefix.starts_with(&c.prefix))
        .filter(|c| !c.listing.truncated || c.prefix == prefix)
        .max_by_key(|c| c.prefix.len())
        .map(|c| Listing {
            names: c
                .listing
                .names
                .iter()
                .filter(|n| n.starts_with(prefix))
                .cloned()
                .collect(),
            truncated: c.listing.truncated,
        })
}

/// The longest prefix every name shares, cut to a character boundary so it can
/// never split a multi-byte glyph.
fn common_prefix(names: &[String]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut end = first.len();
    for name in &names[1..] {
        end = end.min(
            first
                .bytes()
                .zip(name.bytes())
                .take_while(|(a, b)| a == b)
                .count(),
        );
    }
    // Bytes agreeing is not characters agreeing: `é` and `ê` share their first
    // byte. Backing off to a boundary before slicing is what keeps this from
    // panicking on a directory named in anything but ASCII.
    while end > 0 && !first.is_char_boundary(end) {
        end -= 1;
    }
    first[..end].to_string()
}

/// The worker: one listing at a time, always the newest question asked.
fn serve(source: &DirSource, rx: &Receiver<Request>, tx: &Sender<Reply>) {
    while let Ok(first) = rx.recv() {
        let request = newest(first, rx);
        let listing = match source.children(&request.dir, &request.prefix) {
            Ok(listing) => listing,
            // A host that cannot answer costs completions, not a session. The
            // empty listing is also the honest answer for the commonest cause:
            // a directory half-typed and not there yet.
            Err(e) => {
                tracing::debug!(dir = %request.dir, error = %e, "could not list a directory");
                Listing::default()
            }
        };
        let reply = Reply {
            seq: request.seq,
            dir: request.dir,
            prefix: request.prefix,
            listing,
        };
        // A closed channel means the prompt is gone; so is the reason to keep
        // listing.
        if tx.send(reply).is_err() {
            return;
        }
    }
}

/// The newest of a run of requests, discarding the ones overtaken while the
/// previous listing was in flight. This is the coalescing.
fn newest(first: Request, rx: &Receiver<Request>) -> Request {
    let mut latest = first;
    while let Ok(next) = rx.try_recv() {
        latest = next;
    }
    latest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(names: &[&str]) -> Listing {
        Listing {
            names: names.iter().map(|n| n.to_string()).collect(),
            truncated: false,
        }
    }

    fn truncated(names: &[&str]) -> Listing {
        Listing {
            truncated: true,
            ..listing(names)
        }
    }

    /// A completer with no worker behind it: the test *is* the worker, so it
    /// can answer late, out of order, or not at all, and can see exactly which
    /// questions were asked. No thread, no filesystem, no ssh.
    fn detached() -> (Completer, Receiver<Request>, Sender<Reply>) {
        let (ask_tx, ask_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let c = Completer {
            tx: Some(ask_tx),
            rx: reply_rx,
            seq: 0,
            awaiting: false,
            cache: Vec::new(),
            shown: Shown::default(),
        };
        (c, ask_rx, reply_tx)
    }

    fn asked(rx: &Receiver<Request>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        while let Ok(r) = rx.try_recv() {
            out.push((r.dir, r.prefix));
        }
        out
    }

    /// Answer the question currently being asked, as a worker would.
    ///
    /// Taken from the completer rather than from the channel, so a test may
    /// inspect what was asked first without that counting as having served it.
    fn answer(c: &mut Completer, rx: &Receiver<Request>, tx: &Sender<Reply>, l: Listing) {
        while rx.try_recv().is_ok() {}
        assert!(c.waiting(), "nothing was outstanding to answer");
        tx.send(Reply {
            seq: c.seq,
            dir: c.shown.dir.clone(),
            prefix: c.shown.prefix.clone(),
            listing: l,
        })
        .expect("send");
        c.poll();
    }

    /// The whole point: a keystroke does no I/O and does not wait. All `ask`
    /// may do is look in memory and post a question.
    #[test]
    fn asking_posts_a_question_and_shows_nothing_until_it_is_answered() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/home/you/pro");
        assert_eq!(
            asked(&ask_rx),
            [("/home/you".to_string(), "pro".to_string())]
        );
        assert!(c.waiting(), "an answer is outstanding");
        assert!(c.candidates().is_empty(), "nothing to show yet");
        assert_eq!(c.ghost(), "");

        answer(
            &mut c,
            &ask_rx,
            &reply_tx,
            listing(&["projects", "prototypes"]),
        );
        assert!(!c.waiting());
        assert_eq!(c.candidates(), ["projects", "prototypes"]);
    }

    /// Typing forward inside one directory must not ask again: that is what
    /// makes a path cost about one round trip per `/` instead of one per key.
    #[test]
    fn a_cached_listing_answers_a_longer_prefix_without_asking_again() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/home/you/p");
        answer(
            &mut c,
            &ask_rx,
            &reply_tx,
            listing(&["projects", "prototypes", "public"]),
        );

        c.ask("/home/you/pro");
        assert!(asked(&ask_rx).is_empty(), "the answer was already in hand");
        assert!(!c.waiting());
        assert_eq!(c.candidates(), ["projects", "prototypes"]);

        c.ask("/home/you/proj");
        assert!(asked(&ask_rx).is_empty());
        assert_eq!(c.candidates(), ["projects"]);

        // Backspacing back inside the component is free too.
        c.ask("/home/you/pro");
        assert!(asked(&ask_rx).is_empty());
        assert_eq!(c.candidates(), ["projects", "prototypes"]);
    }

    /// A new directory is a new question, however much is cached about the old
    /// one.
    #[test]
    fn a_different_directory_is_always_asked_about() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/home/you/p");
        answer(&mut c, &ask_rx, &reply_tx, listing(&["projects"]));

        c.ask("/home/you/projects/s");
        assert_eq!(
            asked(&ask_rx),
            [("/home/you/projects".to_string(), "s".to_string())]
        );
        assert!(
            c.candidates().is_empty(),
            "the old directory's names are not this one's"
        );
    }

    /// A truncated listing is part of an answer. Showing it is honest; nar-
    /// rowing it is not, because what the host left out may be exactly what the
    /// longer prefix wanted — which would show "no such directory" for one that
    /// is there.
    #[test]
    fn a_truncated_listing_is_shown_but_never_narrowed() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/big/a");
        answer(&mut c, &ask_rx, &reply_tx, truncated(&["a1", "a2"]));
        assert_eq!(c.candidates(), ["a1", "a2"]);
        assert!(c.truncated(), "the row must say it is partial");

        c.ask("/big/a1");
        assert_eq!(
            asked(&ask_rx),
            [("/big".to_string(), "a1".to_string())],
            "a longer prefix must be asked afresh, not filtered out of a partial answer"
        );
    }

    /// The classic autocomplete bug: a slow answer landing after a fast one and
    /// replacing the right candidates with stale ones. It gets *likelier* the
    /// better the cache works, since a cached answer is instant.
    #[test]
    fn a_stale_answer_never_overwrites_a_fresh_one() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/a/x");
        c.ask("/b/x");
        let questions: Vec<Request> = {
            let mut v = Vec::new();
            while let Ok(r) = ask_rx.try_recv() {
                v.push(r);
            }
            v
        };
        assert_eq!(questions.len(), 2);

        // The *second* question is answered first...
        reply_tx
            .send(Reply {
                seq: questions[1].seq,
                dir: "/b".into(),
                prefix: "x".into(),
                listing: listing(&["xbeta"]),
            })
            .expect("send");
        assert!(c.poll());
        assert_eq!(c.candidates(), ["xbeta"]);

        // ...and the first arrives late. It must be kept, and not shown.
        reply_tx
            .send(Reply {
                seq: questions[0].seq,
                dir: "/a".into(),
                prefix: "x".into(),
                listing: listing(&["xalpha"]),
            })
            .expect("send");
        c.poll();
        assert_eq!(c.candidates(), ["xbeta"], "the stale answer must not win");

        // Kept, though: going back to it costs nothing.
        c.ask("/a/x");
        assert!(
            asked(&ask_rx).is_empty(),
            "the late answer was still worth keeping"
        );
        assert_eq!(c.candidates(), ["xalpha"]);
    }

    /// An answer to a question that has moved on can still settle the current
    /// one, when the current one is inside the same directory.
    #[test]
    fn an_overtaken_answer_can_still_settle_the_question_that_overtook_it() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/home/p");
        c.ask("/home/pr");
        let first = {
            let mut v = Vec::new();
            while let Ok(r) = ask_rx.try_recv() {
                v.push(r);
            }
            v.remove(0)
        };

        reply_tx
            .send(Reply {
                seq: first.seq,
                dir: "/home".into(),
                prefix: "p".into(),
                listing: listing(&["projects", "prototypes", "public"]),
            })
            .expect("send");
        assert!(
            c.poll(),
            "the screen changed even though this was not the answer asked for"
        );
        assert_eq!(c.candidates(), ["projects", "prototypes"]);
        assert!(!c.waiting(), "and there is nothing left to wait for");
    }

    /// Coalescing: everything queued behind a request is discarded in favour of
    /// the newest. Typing faster than a round trip must cost one listing.
    #[test]
    fn only_the_newest_of_a_run_of_questions_is_served() {
        let (tx, rx) = mpsc::channel();
        for (i, prefix) in ["p", "pr", "pro", "proj"].iter().enumerate() {
            tx.send(Request {
                seq: i as u64 + 1,
                dir: "/home".into(),
                prefix: (*prefix).into(),
            })
            .expect("send");
        }
        let first = rx.recv().expect("recv");
        let served = newest(first, &rx);
        assert_eq!(served.prefix, "proj");
        assert_eq!(served.seq, 4);
        assert!(
            rx.try_recv().is_err(),
            "the run was drained, not left queued"
        );
    }

    /// The ghost is what `→` will take, so it must never be more than is
    /// certain.
    #[test]
    fn the_ghost_is_the_longest_prefix_every_candidate_shares() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/home/pr");
        answer(
            &mut c,
            &ask_rx,
            &reply_tx,
            listing(&["projects", "prototypes"]),
        );
        assert_eq!(c.ghost(), "o", "only the shared `o` is certain");

        c.ask("/home/proj");
        assert_eq!(c.ghost(), "ects/", "one candidate is certain to its end");
    }

    /// A single candidate carries a `/`, so the next component can be typed
    /// without reaching for one — and so accepting it re-asks about the
    /// directory it names.
    #[test]
    fn a_single_candidate_completes_to_a_trailing_slash() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/home/you/");
        answer(&mut c, &ask_rx, &reply_tx, listing(&["only"]));
        assert_eq!(c.ghost(), "only/");
    }

    #[test]
    fn there_is_no_ghost_when_nothing_matches_or_nothing_is_certain() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/home/zz");
        answer(&mut c, &ask_rx, &reply_tx, listing(&[]));
        assert_eq!(c.ghost(), "");

        // Two candidates sharing nothing beyond what is typed.
        c.ask("/home/a");
        answer(&mut c, &ask_rx, &reply_tx, listing(&["ab", "ac"]));
        assert_eq!(c.ghost(), "");
    }

    /// Bytes agreeing is not characters agreeing. `é` and `ê` share their first
    /// byte, and a completer that sliced there would hand back half a glyph —
    /// or panic.
    #[test]
    fn a_shared_prefix_is_never_cut_through_a_character() {
        assert_eq!(common_prefix(&["café".into(), "cafè".into()]), "caf");
        assert_eq!(common_prefix(&["日本語".into(), "日本".into()]), "日本");
        assert_eq!(common_prefix(&["日本".into(), "中国".into()]), "");

        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/home/caf");
        answer(&mut c, &ask_rx, &reply_tx, listing(&["café", "cafè"]));
        assert_eq!(c.ghost(), "", "nothing beyond `caf` is certain");
    }

    /// A host that answers with something other than what was asked must cost
    /// a suggestion, not a crash. This reads a remote machine's output.
    #[test]
    fn a_candidate_that_does_not_start_with_what_was_typed_is_not_a_ghost() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/home/pr");
        answer(&mut c, &ask_rx, &reply_tx, listing(&["élan"]));
        assert_eq!(c.ghost(), "");
    }

    /// Nothing to list, so nothing is asked: a working directory is absolute,
    /// so there is no relative one to resolve a bare word against.
    #[test]
    fn a_path_with_no_slash_asks_nothing_and_shows_nothing() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/home/p");
        answer(&mut c, &ask_rx, &reply_tx, listing(&["projects"]));
        assert!(!c.candidates().is_empty());

        c.ask("home");
        assert!(asked(&ask_rx).is_empty());
        assert!(c.candidates().is_empty());
        assert!(!c.waiting());
    }

    /// The cache is bounded, or a long session at a prompt would hold every
    /// directory it ever passed through.
    #[test]
    fn the_cache_holds_a_bounded_number_of_directories() {
        let (mut c, _ask_rx, _reply_tx) = detached();
        for i in 0..CACHE * 2 {
            c.remember(format!("/d{i}"), String::new(), listing(&["x"]));
        }
        assert_eq!(c.cache.len(), CACHE);
        assert_eq!(
            c.cache[0].dir,
            format!("/d{}", CACHE * 2 - 1),
            "newest first"
        );
    }

    /// Re-listing a directory replaces what was held rather than shadowing it,
    /// so a stale answer cannot outlive a fresh one in the cache.
    #[test]
    fn remembering_a_directory_again_replaces_the_older_answer() {
        let (mut c, _ask_rx, _reply_tx) = detached();
        c.remember("/d".into(), "a".into(), listing(&["a1"]));
        c.remember("/d".into(), "a".into(), listing(&["a1", "a2"]));
        assert_eq!(c.cache.len(), 1);
        assert_eq!(c.cache[0].listing.names, ["a1", "a2"]);
    }
}
