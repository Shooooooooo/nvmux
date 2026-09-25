//! Directory completion for the create prompt, off the keypress path.
//!
//! Typing a path must never wait for a directory to be listed. Locally that
//! listing is a millisecond; over ssh it is a round trip, measured at ~230ms
//! even to localhost (see `scripts/list.sh`), and a prompt that spent that on
//! every keystroke would be unusable on the connection this feature exists for.
//! So `Completer::ask` is what a keystroke calls, it answers from memory or
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
//! The thread is never joined. Dropping the `Completer` drops the sending end,
//! which ends the worker's loop after its current listing returns — and a
//! wedged ssh must not hang the prompt on the way out. What is left running
//! holds a shell on the host and a dead channel; the loop ends, the shell goes
//! with it (its stdin closes, and it gets a moment to leave before it is
//! killed), and the thread exits on its own.
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

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::dirs::{self, DirSource, Lister};
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
}

/// What the host said, and which question it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reply {
    seq: u64,
    dir: String,
    listing: Listing,
}

/// A listing kept for later. Keyed by directory alone: it holds every child,
/// so it answers every query anyone could type inside that directory.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cached {
    dir: String,
    listing: Listing,
}

/// What is on screen right now.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Shown {
    dir: String,
    names: Vec<String>,
    truncated: bool,
}

/// Directory completion for one prompt.
pub(super) struct Completer {
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
    /// The session host's home, for the `~` in a typed path. Held here rather
    /// than reached for per question because it is what [`DirSource`] is about
    /// — the host — and because the expansion happens on this thread, before a
    /// request is made, not in the worker.
    home: String,
    /// Scratch buffers for the scorer, reused across queries. `Matcher` is
    /// stateful for that reason and scoring takes `&mut`, which is why ranking
    /// lives here beside the names rather than in the prompt.
    matcher: Matcher,
}

impl Completer {
    /// Start completing against `source`, which the worker takes with it.
    ///
    /// `home` is that same host's home directory, which is what a leading `~`
    /// in the field means — see [`crate::session::expand_tilde`]. Empty is
    /// allowed and means the host never said: a `~` is then left as the
    /// literal text it is, and lists nothing.
    pub(super) fn new(source: DirSource, home: &str) -> Self {
        let (ask_tx, ask_rx) = mpsc::channel::<Request>();
        let (reply_tx, reply_rx) = mpsc::channel::<Reply>();

        // Not joined, and nothing waits on it: see the module docs.
        std::thread::Builder::new()
            .name("nvmux-complete".into())
            .spawn(move || serve(source, &ask_rx, &reply_tx))
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
            home: home.to_string(),
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    /// What has been typed has changed. Never blocks, never does I/O.
    pub(super) fn ask(&mut self, input: &str) {
        let Some((dir, _query)) = dirs::split(input) else {
            // Nothing to list: no `/` in what has been typed, and a working
            // directory is absolute, so there is no relative one to resolve.
            self.shown = Shown::default();
            self.awaiting = false;
            return;
        };
        // The `~` is expanded here, on the directory half and after the split,
        // which is what keeps it to the one shape that has a directory in it.
        // A bare `~` names the home but does not yet say "inside it", and it
        // has no `/`, so `split` has already returned above and left it alone —
        // exactly as `/home/shu` is left alone. Typing the `/` is what asks.
        //
        // The expansion is the same one `enter` will apply (see
        // `session::validate_directory`), so the menu cannot offer what
        // creating the session would not use. A `~` that will not expand —
        // `~user`, or a host that reported no home — stays the literal text,
        // and the host lists nothing for it: at a prompt `~r` is `~root` half
        // typed, not an error to report.
        let dir = crate::session::expand_tilde(dir, &self.home).unwrap_or_else(|_| dir.to_string());
        let dir = dir.as_str();
        // Only the directory is a question for the host. Everything after the
        // last `/` is a query, and this side answers those from what it already
        // holds — which is why typing inside a directory costs nothing at all.
        if self.shown.dir == dir && !self.awaiting {
            return;
        }
        if self.take_from_cache(dir) {
            return;
        }

        self.shown = Shown {
            dir: dir.to_string(),
            ..Shown::default()
        };
        self.seq += 1;
        self.awaiting = true;
        let request = Request {
            seq: self.seq,
            dir: dir.to_string(),
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
    pub(super) fn poll(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.rx.try_recv() {
                Ok(reply) => {
                    let current = reply.seq == self.seq;
                    self.remember(reply.dir.clone(), reply.listing.clone());
                    if current {
                        self.shown = Shown {
                            dir: reply.dir,
                            names: reply.listing.names,
                            truncated: reply.listing.truncated,
                        };
                        self.awaiting = false;
                        changed = true;
                    } else if self.awaiting {
                        // An answer to a question that has moved on can still
                        // settle the current one, when both are about the same
                        // directory — which is now the only thing a question is.
                        let dir = self.shown.dir.clone();
                        changed |= self.take_from_cache(&dir);
                    }
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return changed,
            }
        }
    }

    /// Is an answer still outstanding? The prompt polls faster while it is, and
    /// says it is waiting rather than saying there is nothing there.
    pub(super) fn waiting(&self) -> bool {
        self.awaiting
    }

    /// Every child of the directory being completed, unranked and unfiltered,
    /// in the host's order. What [`Completer::matches`] scores.
    #[cfg(test)]
    fn names(&self) -> &[String] {
        &self.shown.names
    }

    /// The directories `query` could mean, best first.
    ///
    /// Fuzzy, so `nvmx` finds `nvmux-rs` — the whole reason the host stopped
    /// filtering. An empty query is every child in the host's order, which is
    /// what a freshly typed `/` shows.
    ///
    /// Dotted directories are hidden until the query asks for one. That rule
    /// used to come free from the shell's globbing, where `*` skips a leading
    /// dot and `.*` does not; it is spelled out here because there is no glob
    /// left to carry it, and it is worth keeping because a home directory is
    /// mostly dotted and none of it is what anyone is looking for.
    ///
    /// Ties are broken by the host's order rather than left to the sort, so a
    /// list of equally good matches does not shuffle as the query grows.
    pub(super) fn matches(&mut self, query: &str) -> Vec<String> {
        let wants_dotted = query.contains('.');
        let visible = |name: &String| wants_dotted || !name.starts_with('.');

        if query.is_empty() {
            return self
                .shown
                .names
                .iter()
                .filter(|n| visible(n))
                .cloned()
                .collect();
        }

        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(u32, usize, &String)> = self
            .shown
            .names
            .iter()
            .enumerate()
            .filter(|(_, name)| visible(name))
            .filter_map(|(i, name)| {
                let score = pattern.score(Utf32Str::new(name, &mut buf), &mut self.matcher)?;
                Some((score, i, name))
            })
            .collect();

        // Best score first; the host's order within a score. Sorted explicitly
        // rather than through `Pattern::match_list`, whose tie order is its own
        // business and not something a screen should inherit.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored
            .into_iter()
            .map(|(_, _, name)| name.clone())
            .collect()
    }

    /// The host stopped short of listing everything. Shown, so a row that is
    /// missing entries does not read as one that is complete.
    pub(super) fn truncated(&self) -> bool {
        self.shown.truncated
    }

    /// Answer from a listing already in hand, if one covers this question.
    fn take_from_cache(&mut self, dir: &str) -> bool {
        let Some(cached) = self.cache.iter().find(|c| c.dir == dir) else {
            return false;
        };
        self.shown = Shown {
            dir: dir.to_string(),
            names: cached.listing.names.clone(),
            truncated: cached.listing.truncated,
        };
        self.awaiting = false;
        true
    }

    fn remember(&mut self, dir: String, listing: Listing) {
        self.cache.retain(|c| c.dir != dir);
        self.cache.insert(0, Cached { dir, listing });
        self.cache.truncate(CACHE);
    }
}

/// The worker: one listing at a time, always the newest question asked.
fn serve(source: DirSource, rx: &Receiver<Request>, tx: &Sender<Reply>) {
    let mut lister = Lister::new(source);
    // Brought up now, while the user is still typing the first characters,
    // rather than by the first question — which over ssh would otherwise pay
    // for a login shell on the host.
    lister.warm();
    while let Ok(first) = rx.recv() {
        let request = newest(first, rx);
        let listing = match lister.children(&request.dir) {
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

    /// A completer with no worker behind it: the test *is* the worker, so it can
    /// answer late, out of order, or not at all, and can see exactly which
    /// directories were asked about. No thread, no filesystem, no ssh.
    fn detached() -> (Completer, Receiver<Request>, Sender<Reply>) {
        detached_at("/home/you")
    }

    /// The same, for a host whose home is `home` — which is what a `~` in the
    /// field means, and the only thing the tilde tests need to vary.
    fn detached_at(home: &str) -> (Completer, Receiver<Request>, Sender<Reply>) {
        let (ask_tx, ask_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let c = Completer {
            tx: Some(ask_tx),
            rx: reply_rx,
            seq: 0,
            awaiting: false,
            cache: Vec::new(),
            shown: Shown::default(),
            home: home.to_string(),
            matcher: Matcher::new(Config::DEFAULT),
        };
        (c, ask_rx, reply_tx)
    }

    fn asked(rx: &Receiver<Request>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(r) = rx.try_recv() {
            out.push(r.dir);
        }
        out
    }

    /// Answer the question currently being asked, as a worker would.
    fn answer(c: &mut Completer, rx: &Receiver<Request>, tx: &Sender<Reply>, l: Listing) {
        while rx.try_recv().is_ok() {}
        assert!(c.waiting(), "nothing was outstanding to answer");
        tx.send(Reply {
            seq: c.seq,
            dir: c.shown.dir.clone(),
            listing: l,
        })
        .expect("send");
        c.poll();
    }

    /// A completer already holding one directory's children, which is the state
    /// every matching test below wants.
    fn holding(dir: &str, names: &[&str]) -> Completer {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask(&format!("{dir}/"));
        answer(&mut c, &ask_rx, &reply_tx, listing(names));
        c
    }

    /// The whole point: a keystroke does no I/O and does not wait. All `ask` may
    /// do is look in memory and post a question.
    #[test]
    fn asking_posts_a_question_and_shows_nothing_until_it_is_answered() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/home/you/pro");
        assert_eq!(asked(&ask_rx), ["/home/you"]);
        assert!(c.waiting(), "an answer is outstanding");
        assert!(c.names().is_empty(), "nothing to show yet");

        answer(
            &mut c,
            &ask_rx,
            &reply_tx,
            listing(&["projects", "prototypes"]),
        );
        assert!(!c.waiting());
        assert_eq!(c.names(), ["projects", "prototypes"]);
    }

    /// The question is the *directory*, and nothing else. Everything after the
    /// last `/` is a query this side answers from what it already holds, which
    /// is what makes typing inside a directory free.
    #[test]
    fn typing_within_one_directory_never_asks_again() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/home/you/");
        answer(
            &mut c,
            &ask_rx,
            &reply_tx,
            listing(&["projects", "prototypes", "public"]),
        );

        for typed in [
            "/home/you/p",
            "/home/you/pro",
            "/home/you/xyz",
            "/home/you/",
        ] {
            c.ask(typed);
            assert!(asked(&ask_rx).is_empty(), "{typed:?} asked the host again");
            assert!(!c.waiting());
            assert_eq!(c.names().len(), 3, "the whole directory is still in hand");
        }
    }

    /// A new directory is a new question, however much is cached about the old
    /// one.
    #[test]
    fn a_different_directory_is_always_asked_about() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/home/you/p");
        answer(&mut c, &ask_rx, &reply_tx, listing(&["projects"]));

        c.ask("/home/you/projects/s");
        assert_eq!(asked(&ask_rx), ["/home/you/projects"]);
        assert!(
            c.names().is_empty(),
            "the old directory's children are not this one's"
        );
    }

    /// The classic autocomplete bug: a slow answer landing after a fast one and
    /// replacing the right list with a stale one.
    #[test]
    fn a_stale_answer_never_overwrites_a_fresh_one() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/a/");
        c.ask("/b/");
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
                listing: listing(&["xbeta"]),
            })
            .expect("send");
        assert!(c.poll());
        assert_eq!(c.names(), ["xbeta"]);

        // ...and the first arrives late. It must be kept, and not shown.
        reply_tx
            .send(Reply {
                seq: questions[0].seq,
                dir: "/a".into(),
                listing: listing(&["xalpha"]),
            })
            .expect("send");
        c.poll();
        assert_eq!(c.names(), ["xbeta"], "the stale answer must not win");

        // Kept, though: going back to it costs nothing.
        c.ask("/a/");
        assert!(
            asked(&ask_rx).is_empty(),
            "the late answer was worth keeping"
        );
        assert_eq!(c.names(), ["xalpha"]);
    }

    /// Coalescing: everything queued behind a request is discarded in favour of
    /// the newest. Typing faster than a round trip must cost one listing.
    #[test]
    fn only_the_newest_of_a_run_of_questions_is_served() {
        let (tx, rx) = mpsc::channel();
        for (i, dir) in ["/a", "/a/b", "/a/b/c", "/a/b/c/d"].iter().enumerate() {
            tx.send(Request {
                seq: i as u64 + 1,
                dir: (*dir).into(),
            })
            .expect("send");
        }
        let first = rx.recv().expect("recv");
        let served = newest(first, &rx);
        assert_eq!(served.dir, "/a/b/c/d");
        assert_eq!(served.seq, 4);
        assert!(
            rx.try_recv().is_err(),
            "the run was drained, not left queued"
        );
    }

    /// Nothing to list, so nothing is asked: a working directory is absolute, so
    /// there is no relative one to resolve a bare word against.
    #[test]
    fn a_path_with_no_slash_asks_nothing_and_shows_nothing() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/home/");
        answer(&mut c, &ask_rx, &reply_tx, listing(&["projects"]));
        assert!(!c.names().is_empty());

        c.ask("home");
        assert!(asked(&ask_rx).is_empty());
        assert!(c.names().is_empty());
        assert!(!c.waiting());
    }

    // --- the tilde -----------------------------------------------------------

    /// A `~` is the *session host's* home, so what gets listed is the home the
    /// completer was built with — never this machine's `$HOME`, which over ssh
    /// would be a different computer's path.
    #[test]
    fn a_tilde_is_the_session_hosts_home_directory() {
        let (mut c, ask_rx, reply_tx) = detached_at("/home/them");
        c.ask("~/");
        assert_eq!(asked(&ask_rx), ["/home/them"]);

        answer(&mut c, &ask_rx, &reply_tx, listing(&["src", "docs"]));
        assert_eq!(c.matches(""), ["src", "docs"]);
    }

    /// The expansion is of the directory half, so it survives being deep in a
    /// path rather than only working at the front of one.
    #[test]
    fn a_tilde_expands_anywhere_the_directory_half_starts_with_one() {
        let (mut c, ask_rx, _reply_tx) = detached_at("/home/them");
        c.ask("~/src/nv");
        assert_eq!(asked(&ask_rx), ["/home/them/src"]);
    }

    /// A trailing slash on the host's home must not become a doubled one:
    /// `//` is the prompt's "start again from the root", so `/home/them//src`
    /// would be read as `/src` by everything downstream.
    #[test]
    fn a_home_with_a_trailing_slash_does_not_make_a_double_one() {
        let (mut c, ask_rx, _reply_tx) = detached_at("/home/them/");
        c.ask("~/src/nv");
        assert_eq!(asked(&ask_rx), ["/home/them/src"]);
    }

    /// The same directory by two spellings is one question. Worth pinning
    /// because the cache is keyed on the *expanded* path: walking to `~/src`
    /// and then typing it out in full must not cost a second round trip.
    #[test]
    fn a_tilde_path_and_the_spelled_out_one_share_a_listing() {
        let (mut c, ask_rx, reply_tx) = detached_at("/home/them");
        c.ask("~/");
        answer(&mut c, &ask_rx, &reply_tx, listing(&["src"]));

        c.ask("/home/them/s");
        assert!(
            asked(&ask_rx).is_empty(),
            "the same directory was asked about twice"
        );
        assert_eq!(c.names(), ["src"]);
    }

    /// A bare `~` names the home but does not yet say "inside it", and it has
    /// no `/`, so it is left alone exactly as `/home/them` is: the menu offers
    /// what is *beside* it, and typing the `/` is what steps in. Pinned because
    /// expanding it would make `accept` write `~/them` for a directory reached
    /// as `/home/them`.
    #[test]
    fn a_bare_tilde_is_left_alone_like_any_other_directory_name() {
        let (mut c, ask_rx, _reply_tx) = detached_at("/home/them");
        c.ask("~");
        assert!(asked(&ask_rx).is_empty(), "nothing to list yet");
        assert!(!c.waiting());
        assert!(c.names().is_empty());
    }

    /// `~user` is somebody else's home and only the host could resolve it.
    /// `session::validate_directory` refuses it; here it is simply listed as
    /// the literal text, which finds nothing — because at a prompt `~r` is
    /// `~root` half typed, and a keystroke on the way somewhere must not be an
    /// error.
    #[test]
    fn a_tilde_user_is_never_expanded_and_never_an_error() {
        let (mut c, ask_rx, reply_tx) = detached_at("/home/them");
        c.ask("~root/s");
        assert_eq!(asked(&ask_rx), ["~root"], "the literal text, not the home");

        // And the host's honest answer for it is nothing at all.
        answer(&mut c, &ask_rx, &reply_tx, Listing::default());
        assert!(c.matches("s").is_empty());
    }

    /// A host that never said where home is has no `~` to offer. The literal
    /// text again, rather than a confident wrong guess at this machine's own
    /// home — which is the whole reason the expansion takes a `home` at all.
    #[test]
    fn a_host_that_reported_no_home_expands_nothing() {
        let (mut c, ask_rx, _reply_tx) = detached_at("");
        c.ask("~/src");
        assert_eq!(asked(&ask_rx), ["~"]);
    }

    /// The menu and enter must not mean two different directories. This is the
    /// claim that `session::validate_directory` and the completer share one
    /// expansion, checked against the real thing rather than restated.
    #[test]
    fn what_the_menu_lists_is_what_enter_would_create_in() {
        const HOME: &str = "/home/them";
        for (typed, inside) in [("~/", "~/"), ("~/src/nv", "~/src"), ("~/a/b/c", "~/a/b")] {
            let (mut c, ask_rx, _reply_tx) = detached_at(HOME);
            c.ask(typed);
            let listed = asked(&ask_rx);
            let created = crate::session::validate_directory(inside, HOME)
                .expect("a directory the prompt would accept");
            assert_eq!(
                listed,
                [created.trim_end_matches('/').to_string()],
                "{typed:?} listed one directory and would create in another"
            );
        }
    }

    /// The cache is bounded, or a long session at a prompt would hold every
    /// directory it ever passed through.
    #[test]
    fn the_cache_holds_a_bounded_number_of_directories() {
        let (mut c, _ask_rx, _reply_tx) = detached();
        for i in 0..CACHE * 2 {
            c.remember(format!("/d{i}"), listing(&["x"]));
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
        c.remember("/d".into(), listing(&["a1"]));
        c.remember("/d".into(), listing(&["a1", "a2"]));
        assert_eq!(c.cache.len(), 1);
        assert_eq!(c.cache[0].listing.names, ["a1", "a2"]);
    }

    // --- ranking -------------------------------------------------------------

    /// The whole reason the host stopped filtering: a query need not be a prefix
    /// of the name, or even contiguous within it.
    #[test]
    fn a_query_matches_a_subsequence_not_just_a_prefix() {
        let mut c = holding("/src", &["nvmux-rs", "notes", "vendor"]);
        assert_eq!(c.matches("nvmx"), ["nvmux-rs"]);
        assert_eq!(c.matches("mux"), ["nvmux-rs"], "not anchored at the start");
        assert_eq!(c.matches("nts"), ["notes"]);
    }

    /// The first row is what enter takes, so which one it is matters more than
    /// anything else the scorer does.
    #[test]
    fn the_best_match_is_first() {
        let mut c = holding("/d", &["a-long-name-with-src-inside", "src"]);
        assert_eq!(
            c.matches("src").first().map(String::as_str),
            Some("src"),
            "an exact name beats a scattered hit"
        );

        let mut c = holding("/d", &["prototypes", "projects"]);
        assert_eq!(
            c.matches("proj").first().map(String::as_str),
            Some("projects")
        );
    }

    /// An empty query is the whole directory, in the host's order — what a
    /// freshly typed `/` shows.
    #[test]
    fn an_empty_query_is_every_child_in_the_hosts_order() {
        let mut c = holding("/d", &["b", "a", "c"]);
        assert_eq!(c.matches(""), ["b", "a", "c"], "not re-sorted");
    }

    /// The other half of that claim, and the one the prompt leans on: a lone
    /// `/` is a question about the root, not a path with no directory in it. It
    /// is what an emptied working directory field reaches the root by, so the
    /// route is pinned here rather than left to follow from `dirs::split`.
    #[test]
    fn a_lone_slash_asks_about_the_root() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/");
        assert_eq!(asked(&ask_rx), ["/"], "the root, not nothing to list");

        answer(&mut c, &ask_rx, &reply_tx, listing(&["etc", "usr", "var"]));
        assert_eq!(c.matches(""), ["etc", "usr", "var"]);
    }

    /// A home directory is mostly dotted and none of it is what anyone is
    /// looking for. The rule used to come free from the shell's globbing; it is
    /// spelled out here because there is no glob left to carry it.
    #[test]
    fn dotted_directories_are_hidden_until_the_query_asks_for_one() {
        let mut c = holding("/home", &[".config", ".local", "src", "docs"]);
        assert_eq!(c.matches(""), ["src", "docs"], "hidden by default");
        // `oc` is a subsequence of both `docs` and `.local`, so this is the
        // dotted one being excluded rather than simply not matching.
        assert_eq!(c.matches("oc"), ["docs"], "and hidden from a plain query");
        assert_eq!(c.matches(".co"), [".config"], "a dot asks for them");
        assert!(
            c.matches(".").len() == 2,
            "a bare dot shows every dotted directory: {:?}",
            c.matches(".")
        );
    }

    /// A list of equally good matches must not shuffle as the query grows, so
    /// the tiebreak is the host's order rather than whatever the sort does.
    #[test]
    fn equal_scores_keep_the_hosts_order() {
        let names = ["xa", "xb", "xc", "xd"];
        let mut c = holding("/d", &names);
        // Every candidate scores identically on a query matching only the shared
        // first character.
        assert_eq!(c.matches("x"), names);
    }

    /// A query nothing matches is an empty list, not every directory.
    #[test]
    fn a_query_that_matches_nothing_matches_nothing() {
        let mut c = holding("/d", &["alpha", "beta"]);
        assert!(c.matches("zzzz").is_empty());
    }
}
