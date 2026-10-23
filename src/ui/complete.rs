//! Directory completion for the create prompt, off the keypress path.
//!
//! A keystroke must never wait on a listing: over ssh each is a round trip
//! (~230 ms even to localhost, see `scripts/list.sh`). `Completer::ask`
//! answers from memory or posts a request to a worker thread; it does no I/O.
//!
//! A thread is safe despite [`crate::pty`]'s single-thread rule: it reads no
//! terminal. The screens poll via crossterm's timeout-only `event::poll`, so
//! no self-pipe is possible (as [`crate::winch`] uses); replies arrive on a
//! channel drained each tick, which [`super::poll_key_for`] shortens while
//! waiting. The thread is never joined, so a wedged ssh cannot hang the
//! prompt; dropping the `Completer` ends its loop after the current listing.
//!
//! - Coalescing: the worker serves only the newest queued request.
//! - Sequence numbers: only the answer to the current request is shown, so a
//!   slow stale answer cannot replace a fresh (often cached) one.
//! - Cache: a directory's listing answers every query typed inside it by
//!   filtering in memory, so a path costs about one round trip per `/`.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::dirs::{self, DirSource, Lister};
use crate::transport::protocol::Listing;

/// Directories to remember: a few trees' worth of path components. A miss
/// costs one round trip.
const CACHE: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Request {
    seq: u64,
    dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Reply {
    seq: u64,
    dir: String,
    listing: Listing,
}

/// Keyed by directory alone: it holds every child, so it answers any query
/// typed inside that directory.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cached {
    dir: String,
    listing: Listing,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Shown {
    dir: String,
    names: Vec<String>,
    truncated: bool,
}

pub(super) struct Completer {
    /// Dropping it ends the worker.
    tx: Option<Sender<Request>>,
    rx: Receiver<Reply>,
    /// The current request; a reply with another `seq` is stale.
    seq: u64,
    /// A request is outstanding: the prompt polls faster and shows waiting.
    awaiting: bool,
    cache: Vec<Cached>,
    shown: Shown,
    /// The session host's home, for expanding `~` before a request is made.
    home: String,
    /// Scorer scratch buffers, reused across queries. Scoring takes `&mut`,
    /// which is why ranking lives here rather than in the prompt.
    matcher: Matcher,
}

impl Completer {
    /// `home` is the host's home for a leading `~` (see
    /// [`crate::session::expand_tilde`]); empty means unknown, and `~` stays
    /// literal.
    pub(super) fn new(source: DirSource, home: &str) -> Self {
        let (ask_tx, ask_rx) = mpsc::channel::<Request>();
        let (reply_tx, reply_rx) = mpsc::channel::<Reply>();

        std::thread::Builder::new()
            .name("nvmux-complete".into())
            .spawn(move || serve(source, &ask_rx, &reply_tx))
            // A failed spawn drops the closure, closing the channel; the field
            // then works without completion.
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

    /// The typed text changed. Never blocks or does I/O.
    pub(super) fn ask(&mut self, input: &str) {
        let Some((dir, _query)) = dirs::split(input) else {
            // No `/` typed: nothing to list, as working directories are
            // absolute.
            self.shown = Shown::default();
            self.awaiting = false;
            return;
        };
        // Expand `~` on the directory half only (a bare `~` returned above),
        // as `enter` does (see `session::validate_directory`), so the menu
        // lists where the session would be created. An unexpandable `~`
        // stays literal and lists nothing: `~r` is `~root` half typed.
        let dir = crate::session::expand_tilde(dir, &self.home).unwrap_or_else(|_| dir.to_string());
        let dir = dir.as_str();
        // Only the directory goes to the host; the query after the last `/`
        // is answered from memory.
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
        // Unbounded, so this never blocks. Closed means no worker, so no
        // completions.
        if let Some(tx) = &self.tx {
            if tx.send(request).is_err() {
                self.awaiting = false;
            }
        }
    }

    /// Drain worker replies; returns whether the screen changed.
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
                        // A stale reply for the same directory still answers
                        // the current request.
                        let dir = self.shown.dir.clone();
                        changed |= self.take_from_cache(&dir);
                    }
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return changed,
            }
        }
    }

    pub(super) fn waiting(&self) -> bool {
        self.awaiting
    }

    #[cfg(test)]
    fn names(&self) -> &[String] {
        &self.shown.names
    }

    /// The directories `query` fuzzily matches, best first; all children for
    /// an empty query. Dot directories need a `.` in the query, since a home
    /// is mostly dotfiles. Ties keep the host's order, so the list does not
    /// shuffle as the query grows.
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

        // Best score first, then host order. Not `Pattern::match_list`, whose
        // tie order is its own.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored
            .into_iter()
            .map(|(_, _, name)| name.clone())
            .collect()
    }

    /// Whether the host listed only part of the directory.
    pub(super) fn truncated(&self) -> bool {
        self.shown.truncated
    }

    /// Show the cached listing for `dir`; returns whether there was one.
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

/// The worker: one listing at a time, for the newest request.
fn serve(source: DirSource, rx: &Receiver<Request>, tx: &Sender<Reply>) {
    let mut lister = Lister::new(source);
    // Warm up while the user types, so the first request does not pay for
    // an ssh login shell.
    lister.warm();
    while let Ok(first) = rx.recv() {
        let request = newest(first, rx);
        let listing = match lister.children(&request.dir) {
            Ok(listing) => listing,
            // A failure only costs completions. Empty is also right for the
            // usual cause: a half-typed directory that does not exist yet.
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
        if tx.send(reply).is_err() {
            return;
        }
    }
}

/// The newest of the queued requests, discarding older ones (coalescing).
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

    /// A completer whose worker is the test.
    fn detached() -> (Completer, Receiver<Request>, Sender<Reply>) {
        detached_at("/home/you")
    }

    /// [`detached`] for a host whose home is `home`.
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

    /// Answer the current request, as a worker would.
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

    /// A completer already holding `dir`'s children.
    fn holding(dir: &str, names: &[&str]) -> Completer {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask(&format!("{dir}/"));
        answer(&mut c, &ask_rx, &reply_tx, listing(names));
        c
    }

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

        // ...and the first arrives late: kept, but not shown.
        reply_tx
            .send(Reply {
                seq: questions[0].seq,
                dir: "/a".into(),
                listing: listing(&["xalpha"]),
            })
            .expect("send");
        c.poll();
        assert_eq!(c.names(), ["xbeta"], "the stale answer must not win");

        // Going back to it costs nothing.
        c.ask("/a/");
        assert!(
            asked(&ask_rx).is_empty(),
            "the late answer was worth keeping"
        );
        assert_eq!(c.names(), ["xalpha"]);
    }

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

    /// Never this machine's `$HOME`, which over ssh is another computer's.
    #[test]
    fn a_tilde_is_the_session_hosts_home_directory() {
        let (mut c, ask_rx, reply_tx) = detached_at("/home/them");
        c.ask("~/");
        assert_eq!(asked(&ask_rx), ["/home/them"]);

        answer(&mut c, &ask_rx, &reply_tx, listing(&["src", "docs"]));
        assert_eq!(c.matches(""), ["src", "docs"]);
    }

    #[test]
    fn a_tilde_expands_anywhere_the_directory_half_starts_with_one() {
        let (mut c, ask_rx, _reply_tx) = detached_at("/home/them");
        c.ask("~/src/nv");
        assert_eq!(asked(&ask_rx), ["/home/them/src"]);
    }

    /// `//` restarts from the root at the prompt, so `/home/them//src` would
    /// mean `/src`.
    #[test]
    fn a_home_with_a_trailing_slash_does_not_make_a_double_one() {
        let (mut c, ask_rx, _reply_tx) = detached_at("/home/them/");
        c.ask("~/src/nv");
        assert_eq!(asked(&ask_rx), ["/home/them/src"]);
    }

    /// The cache is keyed on the expanded path.
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

    /// Expanding it would make `accept` write `~/them` for a directory reached
    /// as `/home/them`.
    #[test]
    fn a_bare_tilde_is_left_alone_like_any_other_directory_name() {
        let (mut c, ask_rx, _reply_tx) = detached_at("/home/them");
        c.ask("~");
        assert!(asked(&ask_rx).is_empty(), "nothing to list yet");
        assert!(!c.waiting());
        assert!(c.names().is_empty());
    }

    #[test]
    fn a_tilde_user_is_never_expanded_and_never_an_error() {
        let (mut c, ask_rx, reply_tx) = detached_at("/home/them");
        c.ask("~root/s");
        assert_eq!(asked(&ask_rx), ["~root"], "the literal text, not the home");

        answer(&mut c, &ask_rx, &reply_tx, Listing::default());
        assert!(c.matches("s").is_empty());
    }

    #[test]
    fn a_host_that_reported_no_home_expands_nothing() {
        let (mut c, ask_rx, _reply_tx) = detached_at("");
        c.ask("~/src");
        assert_eq!(asked(&ask_rx), ["~"]);
    }

    /// Checked against `session::validate_directory` itself, not restated.
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

    #[test]
    fn remembering_a_directory_again_replaces_the_older_answer() {
        let (mut c, _ask_rx, _reply_tx) = detached();
        c.remember("/d".into(), listing(&["a1"]));
        c.remember("/d".into(), listing(&["a1", "a2"]));
        assert_eq!(c.cache.len(), 1);
        assert_eq!(c.cache[0].listing.names, ["a1", "a2"]);
    }

    // --- ranking -------------------------------------------------------------

    #[test]
    fn a_query_matches_a_subsequence_not_just_a_prefix() {
        let mut c = holding("/src", &["nvmux-rs", "notes", "vendor"]);
        assert_eq!(c.matches("nvmx"), ["nvmux-rs"]);
        assert_eq!(c.matches("mux"), ["nvmux-rs"], "not anchored at the start");
        assert_eq!(c.matches("nts"), ["notes"]);
    }

    /// The first row is what enter takes.
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

    #[test]
    fn an_empty_query_is_every_child_in_the_hosts_order() {
        let mut c = holding("/d", &["b", "a", "c"]);
        assert_eq!(c.matches(""), ["b", "a", "c"], "not re-sorted");
    }

    /// How an emptied working-directory field reaches the root.
    #[test]
    fn a_lone_slash_asks_about_the_root() {
        let (mut c, ask_rx, reply_tx) = detached();
        c.ask("/");
        assert_eq!(asked(&ask_rx), ["/"], "the root, not nothing to list");

        answer(&mut c, &ask_rx, &reply_tx, listing(&["etc", "usr", "var"]));
        assert_eq!(c.matches(""), ["etc", "usr", "var"]);
    }

    #[test]
    fn dotted_directories_are_hidden_until_the_query_asks_for_one() {
        let mut c = holding("/home", &[".config", ".local", "src", "docs"]);
        assert_eq!(c.matches(""), ["src", "docs"], "hidden by default");
        // `oc` matches both `docs` and `.local`, so this tests the exclusion.
        assert_eq!(c.matches("oc"), ["docs"], "and hidden from a plain query");
        assert_eq!(c.matches(".co"), [".config"], "a dot asks for them");
        assert!(
            c.matches(".").len() == 2,
            "a bare dot shows every dotted directory: {:?}",
            c.matches(".")
        );
    }

    #[test]
    fn equal_scores_keep_the_hosts_order() {
        let names = ["xa", "xb", "xc", "xd"];
        let mut c = holding("/d", &names);
        // All score the same on their shared first character.
        assert_eq!(c.matches("x"), names);
    }

    #[test]
    fn a_query_that_matches_nothing_matches_nothing() {
        let mut c = holding("/d", &["alpha", "beta"]);
        assert!(c.matches("zzzz").is_empty());
    }
}
