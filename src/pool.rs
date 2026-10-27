//! Clients kept for the sessions not in front.
//!
//! With `[client] per_session` (the default), a client leaving the front is
//! parked, one per session, and resumed when its session returns instead of
//! respawned. Each stays a UI of its server until nvmux exits, which is why
//! it can be turned off; then [`Pool::set_aside`] hands every client back.
//!
//! Clients started ahead of a session's first visit (`[client] lazy = false`)
//! are held here too, whatever `per_session` says.
//!
//! The floating picker borrows one to show its session ([`Pool::lend`]) and
//! gives it back as it was, or switches to it in place.

use crate::pty::{Attachment, Parked, Probe};
use crate::session::Session;

/// What [`Pool::take`] found for a session.
#[derive(Debug)]
pub enum Taken {
    /// A live client: resume it, or relay it for the first time if it was
    /// started ahead.
    Kept(Box<Attachment>),
    /// The parked client exited: its session ended or its link dropped. The
    /// latter must be recovered (see `reconnect`) before the link is reused.
    Left,
    Absent,
}

#[derive(Debug)]
pub struct Pool {
    /// `[client] per_session`.
    keeps: bool,
    /// At most one per session; few enough that a `Vec` serves as the map.
    parked: Vec<Parked>,
    /// The session whose client is lent (see [`Pool::lend`]), held still: a
    /// second client for it, one revived say, is turned away.
    lent: Option<String>,
}

impl Pool {
    pub fn new(keeps: bool) -> Self {
        Self {
            keeps,
            parked: Vec::new(),
            lent: None,
        }
    }

    pub fn configured() -> Self {
        Self::new(crate::config::get().client.per_session)
    }

    /// Park a client that just left the front and return `None`, or hand it
    /// back if clients are not kept or its startup was cut short (see
    /// `Attachment::is_parkable`). Any client already parked for the session
    /// is retired: two UIs from one nvmux would split its screen.
    pub fn set_aside(&mut self, held: Option<Attachment>) -> Option<Attachment> {
        let attachment = held?;
        if !self.keeps || !attachment.is_parkable() {
            return Some(attachment);
        }
        let id = attachment.session_id.clone();
        self.retire(&id);
        if let Some(parked) = attachment.park() {
            tracing::debug!(id = %id, parked = self.parked.len() + 1, "client parked");
            self.parked.push(parked);
        }
        None
    }

    /// Hold a client started ahead of its session's first visit, with its
    /// attach probe if still unanswered. Held even when clients are not kept:
    /// `per_session` only governs clients leaving the front.
    pub fn start_ahead(&mut self, attachment: Attachment, probe: Option<Probe>) {
        let id = attachment.session_id.clone();
        self.retire(&id);
        if let Some(held) = attachment.start_ahead(probe) {
            tracing::debug!(id = %id, held = self.parked.len() + 1, "client started ahead");
            self.parked.push(held);
        }
    }

    /// Hold a client restarted elsewhere after a dropped link, in place of the
    /// dead one for its session. A live one there (visited since) wins and
    /// the newcomer is handed back.
    pub fn adopt(&mut self, held: Parked) -> Option<Parked> {
        if self.holds(held.session_id()) {
            return Some(held);
        }
        self.retire(held.session_id());
        tracing::debug!(id = %held.session_id(), held = self.parked.len() + 1, "client started ahead again");
        self.parked.push(held);
        None
    }

    /// Drop every client that has exited and return their session ids.
    pub fn take_left(&mut self) -> Vec<String> {
        let mut left = Vec::new();
        self.parked.retain(|p| {
            if p.is_alive() {
                return true;
            }
            left.push(p.session_id().to_string());
            false
        });
        left
    }

    /// Take back the client parked for this session.
    pub fn take(&mut self, session_id: &str) -> Taken {
        let Some(at) = self
            .parked
            .iter()
            .position(|p| p.session_id() == session_id)
        else {
            return Taken::Absent;
        };
        match self.parked.remove(at).unpark() {
            Some(attachment) => Taken::Kept(Box::new(attachment)),
            None => {
                tracing::debug!(id = %session_id, "the parked client had left");
                Taken::Left
            }
        }
    }

    /// Lend the live client held for a session, for the floating picker to
    /// show its screen, until [`Pool::give_back`] or [`Pool::took`]. One that
    /// exited stays, to be taken as [`Taken::Left`].
    pub fn lend(&mut self, session_id: &str) -> Option<Attachment> {
        let at = self
            .parked
            .iter()
            .position(|p| p.session_id() == session_id && p.is_alive())?;
        match self.parked.remove(at).unpark() {
            Some(attachment) => {
                self.lent = Some(session_id.to_string());
                Some(attachment)
            }
            // It left as it was asked for.
            None => {
                self.parked.push(Parked::gone(session_id));
                None
            }
        }
    }

    /// Hold a lent client again as it was held: started ahead, with all it
    /// said and its probe, or parked.
    pub fn give_back(&mut self, mut attachment: Attachment) {
        self.lent = None;
        if attachment.started_ahead() {
            let probe = attachment.take_probe();
            self.start_ahead(attachment, probe);
            return;
        }
        // Not kept now, it is retired, as `set_aside` would.
        if !self.keeps {
            return;
        }
        let id = attachment.session_id.clone();
        self.retire(&id);
        if let Some(parked) = attachment.park_again() {
            tracing::debug!(id = %id, parked = self.parked.len() + 1, "client given back");
            self.parked.push(parked);
        }
    }

    /// The lent client went to the front instead of back.
    pub fn took(&mut self, session_id: &str) {
        if self.lent.as_deref() == Some(session_id) {
            self.lent = None;
        }
    }

    /// Retire clients of sessions missing from a fresh listing: they can
    /// never be returned to, and where the host could not inspect a session,
    /// its client may still hold the session's screen to its size.
    pub fn keep_only(&mut self, listing: &[Session]) {
        self.parked
            .retain(|p| listing.iter().any(|s| s.id == p.session_id()));
    }

    /// Whether a live client is parked, or lent, for this session.
    pub fn holds(&self, session_id: &str) -> bool {
        self.lent.as_deref() == Some(session_id)
            || self
                .parked
                .iter()
                .any(|p| p.session_id() == session_id && p.is_alive())
    }

    /// Whether anything is parked for this session, alive or not, or lent.
    pub fn has(&self, session_id: &str) -> bool {
        self.lent.as_deref() == Some(session_id)
            || self.parked.iter().any(|p| p.session_id() == session_id)
    }

    pub fn len(&self) -> usize {
        self.parked.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parked.is_empty()
    }

    /// Apply a changed `[client] per_session`; turning it off retires every
    /// held client, those started ahead included.
    pub fn keep(&mut self, keeps: bool) {
        if self.keeps && !keeps {
            self.retire_all();
        }
        self.keeps = keeps;
    }

    /// Tells every client to go before dropping (and so waiting on) any, so
    /// they exit in parallel.
    pub fn retire_all(&mut self) {
        for parked in &mut self.parked {
            parked.tell_to_retire();
        }
        self.parked.clear();
    }

    fn retire(&mut self, session_id: &str) {
        self.parked.retain(|p| p.session_id() != session_id);
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.retire_all();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::pty::kept_standin;

    /// Whether `pid` exits within 5 s.
    fn gone(pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            // Signal 0 only checks that the pid exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn a_pool_that_does_not_keep_hands_every_client_back() {
        let mut pool = Pool::new(false);
        let a = kept_standin("s1", "exec sleep 30");
        let pid = a.pid_for_test();
        let back = pool.set_aside(Some(a)).expect("handed back");
        assert_eq!(back.pid_for_test(), pid);
        assert!(pool.is_empty());
        assert!(pool.set_aside(None).is_none());
    }

    #[test]
    fn a_pool_turned_off_retires_what_it_keeps_and_on_keeps_again() {
        let mut pool = Pool::new(true);
        let a = kept_standin("s1", "exec sleep 30");
        let pid = a.pid_for_test();
        assert!(pool.set_aside(Some(a)).is_none(), "parked");
        pool.keep(false);
        assert!(pool.is_empty());
        assert!(gone(pid), "the kept client was not retired");
        let b = kept_standin("s2", "exec sleep 30");
        assert!(pool.set_aside(Some(b)).is_some(), "handed back");
        pool.keep(true);
        let c = kept_standin("s3", "exec sleep 30");
        assert!(pool.set_aside(Some(c)).is_none(), "kept again");
        assert!(pool.holds("s3"));
        pool.retire_all();
        assert!(pool.is_empty());
    }

    #[test]
    fn a_kept_client_is_parked_and_taken_back_by_its_session() {
        let mut pool = Pool::new(true);
        let a = kept_standin("s1", "exec sleep 30");
        let pid = a.pid_for_test();
        assert!(
            pool.set_aside(Some(a)).is_none(),
            "the client was not parked"
        );
        assert!(pool.holds("s1"));
        assert!(!pool.holds("s2"));
        assert!(matches!(pool.take("s2"), Taken::Absent));
        let Taken::Kept(back) = pool.take("s1") else {
            panic!("not taken back");
        };
        assert_eq!(back.pid_for_test(), pid);
        assert!(!pool.holds("s1"), "taken back, and still held");
        assert!(pool.is_empty());
    }

    #[test]
    fn a_client_started_ahead_is_held_either_way_until_its_first_visit() {
        for keeps in [true, false] {
            let mut pool = Pool::new(keeps);
            let a = kept_standin("s1", "exec sleep 30");
            let pid = a.pid_for_test();
            pool.start_ahead(a, None);
            assert!(pool.holds("s1"), "keeps = {keeps}: not held");
            let Taken::Kept(back) = pool.take("s1") else {
                panic!("keeps = {keeps}: not taken back");
            };
            assert_eq!(back.pid_for_test(), pid);
            assert!(back.started_ahead());
            assert!(pool.is_empty());
        }
    }

    #[test]
    fn a_client_cut_short_in_its_startup_is_not_parked() {
        let mut pool = Pool::new(true);
        let mut a = kept_standin("s1", "exec sleep 30");
        a.cut_short_for_test();
        assert!(pool.set_aside(Some(a)).is_some());
        assert!(pool.is_empty());
    }

    #[test]
    fn a_second_client_for_a_session_retires_the_first() {
        let mut pool = Pool::new(true);
        let first = kept_standin("s1", "exec sleep 30");
        let first_pid = first.pid_for_test();
        pool.set_aside(Some(first));
        let second = kept_standin("s1", "exec sleep 30");
        let second_pid = second.pid_for_test();
        pool.set_aside(Some(second));
        assert_eq!(pool.len(), 1);
        assert!(gone(first_pid), "the first client was not retired");
        let Taken::Kept(back) = pool.take("s1") else {
            panic!("not taken back");
        };
        assert_eq!(back.pid_for_test(), second_pid);
    }

    #[test]
    fn a_client_that_left_is_not_held() {
        let mut pool = Pool::new(true);
        pool.set_aside(Some(kept_standin("s1", "sleep 0.1; exit 0")));
        let deadline = Instant::now() + Duration::from_secs(5);
        while pool.holds("s1") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!pool.holds("s1"));
        assert!(
            pool.has("s1"),
            "a client that left is forgotten before it is asked for"
        );
        assert!(
            matches!(pool.take("s1"), Taken::Left),
            "a client that left is not told apart from none at all"
        );
        assert!(matches!(pool.take("s1"), Taken::Absent));
    }

    #[test]
    fn the_clients_that_left_are_let_go_of_and_named() {
        let mut pool = Pool::new(true);
        pool.set_aside(Some(kept_standin("s1", "sleep 0.1; exit 0")));
        pool.set_aside(Some(kept_standin("s2", "exec sleep 30")));
        pool.start_ahead(kept_standin("s3", "sleep 0.1; exit 0"), None);
        let deadline = Instant::now() + Duration::from_secs(5);
        while (pool.holds("s1") || pool.holds("s3")) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut left = pool.take_left();
        left.sort();
        assert_eq!(left, ["s1", "s3"], "parked and started ahead alike");
        assert!(!pool.has("s1") && !pool.has("s3"), "named, and still held");
        assert!(pool.holds("s2"), "one still there was let go of");
        assert!(pool.take_left().is_empty(), "named twice");
    }

    #[test]
    fn a_client_started_again_replaces_one_that_left_and_not_one_alive() {
        let mut pool = Pool::new(true);
        pool.set_aside(Some(kept_standin("s1", "sleep 0.1; exit 0")));
        let deadline = Instant::now() + Duration::from_secs(5);
        while pool.holds("s1") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let again = kept_standin("s1", "exec sleep 30");
        let again_pid = again.pid_for_test();
        let held = again.start_ahead(None).expect("held");
        assert!(
            pool.adopt(held).is_none(),
            "turned away in place of one that left"
        );
        assert_eq!(pool.len(), 1, "the one that left is still held beside it");
        let Taken::Kept(back) = pool.take("s1") else {
            panic!("not taken back");
        };
        assert_eq!(back.pid_for_test(), again_pid);
        assert!(back.started_ahead());

        let visited = kept_standin("s2", "exec sleep 30");
        let visited_pid = visited.pid_for_test();
        pool.set_aside(Some(visited));
        let late = kept_standin("s2", "exec sleep 30");
        let late_pid = late.pid_for_test();
        let held = late.start_ahead(None).expect("held");
        let turned_away = pool.adopt(held).expect("held in place of one alive");
        drop(turned_away);
        assert!(gone(late_pid), "the newcomer turned away lived on");
        let Taken::Kept(back) = pool.take("s2") else {
            panic!("the one visited was let go of");
        };
        assert_eq!(back.pid_for_test(), visited_pid);
    }

    /// Lent, it still counts as held: a client revived meanwhile is turned
    /// away rather than kept beside it.
    #[test]
    fn a_lent_client_is_held_again_once_given_back() {
        let mut pool = Pool::new(true);
        let a = kept_standin("s1", "exec sleep 30");
        let pid = a.pid_for_test();
        assert!(pool.set_aside(Some(a)).is_none(), "parked");
        let lent = pool.lend("s1").expect("lent");
        assert_eq!(lent.pid_for_test(), pid);
        assert!(pool.holds("s1") && pool.has("s1"));
        let revived = kept_standin("s1", "exec sleep 30")
            .start_ahead(None)
            .expect("held");
        assert!(pool.adopt(revived).is_some(), "kept beside the lent one");
        pool.give_back(lent);
        let Taken::Kept(back) = pool.take("s1") else {
            panic!("not held again");
        };
        assert_eq!(back.pid_for_test(), pid);
        assert!(!back.started_ahead());
        assert!(!pool.has("s1"));
    }

    #[test]
    fn a_lent_client_taken_to_the_front_is_held_no_more() {
        let mut pool = Pool::new(true);
        pool.set_aside(Some(kept_standin("s1", "exec sleep 30")));
        let lent = pool.lend("s1").expect("lent");
        pool.took("s2");
        assert!(pool.holds("s1"), "another session's taken");
        pool.took("s1");
        assert!(!pool.holds("s1") && !pool.has("s1"));
        drop(lent);
    }

    /// As one whose thread saw it go.
    #[test]
    fn a_client_gone_as_it_was_lent_is_taken_as_left() {
        let mut pool = Pool::new(true);
        assert!(pool.adopt(Parked::gone("s1")).is_none());
        assert!(!pool.holds("s1") && pool.has("s1"));
        assert!(pool.lend("s1").is_none());
        assert_eq!(pool.take_left(), ["s1"]);
        assert!(pool.adopt(Parked::gone("s1")).is_none());
        assert!(matches!(pool.take("s1"), Taken::Left));
    }

    #[test]
    fn a_client_given_back_once_clients_are_not_kept_is_retired() {
        let mut pool = Pool::new(true);
        let a = kept_standin("s1", "exec sleep 30");
        let pid = a.pid_for_test();
        pool.set_aside(Some(a));
        let lent = pool.lend("s1").expect("lent");
        pool.keep(false);
        pool.give_back(lent);
        assert!(!pool.has("s1"));
        assert!(gone(pid), "the client lived on");
    }

    /// Lent, it would be forgotten: its session's next visit must recover the
    /// link first.
    #[test]
    fn a_client_that_left_is_not_lent() {
        let mut pool = Pool::new(true);
        pool.set_aside(Some(kept_standin("s1", "sleep 0.1; exit 0")));
        let deadline = Instant::now() + Duration::from_secs(5);
        while pool.holds("s1") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(pool.lend("s1").is_none());
        assert!(matches!(pool.take("s1"), Taken::Left));
        assert!(pool.lend("s2").is_none(), "none held");
    }

    #[test]
    fn a_client_started_ahead_is_given_back_started_ahead() {
        for keeps in [true, false] {
            let mut pool = Pool::new(keeps);
            let a = kept_standin("s1", "exec sleep 30");
            let pid = a.pid_for_test();
            pool.start_ahead(a, None);
            let lent = pool.lend("s1").expect("lent");
            assert!(lent.started_ahead());
            pool.give_back(lent);
            let Taken::Kept(back) = pool.take("s1") else {
                panic!("keeps = {keeps}: not held again");
            };
            assert_eq!(back.pid_for_test(), pid);
            assert!(back.started_ahead(), "keeps = {keeps}: given back as shown");
        }
    }

    #[test]
    fn a_session_gone_from_the_listing_has_its_client_retired() {
        let mut pool = Pool::new(true);
        let a = kept_standin("s1", "exec sleep 30");
        let pid = a.pid_for_test();
        pool.set_aside(Some(a));
        pool.set_aside(Some(kept_standin("s2", "exec sleep 30")));
        let listing = vec![Session::new("s2".into(), "two".into(), 1, 1)];
        pool.keep_only(&listing);
        assert!(!pool.holds("s1"));
        assert!(pool.holds("s2"));
        assert!(
            gone(pid),
            "the client of a session gone from the listing lived on"
        );
    }

    /// Each stand-in takes 80 ms (under `RETIRE_WAIT`) to exit once hung up:
    /// eight in series take at least 640 ms, in parallel up to 250 ms on a
    /// busy runner.
    #[test]
    fn dropping_the_pool_retires_every_client_side_by_side() {
        let mut pool = Pool::new(true);
        let pids: Vec<i32> = (0..8)
            .map(|i| {
                let a = kept_standin(
                    &format!("s{i}"),
                    "trap 'sleep 0.08; exit 0' HUP; printf r; while :; do sleep 0.01; done",
                );
                assert!(a.ready_for_test(), "stand-in {i} never got ready");
                let pid = a.pid_for_test();
                pool.set_aside(Some(a));
                pid
            })
            .collect();
        let start = Instant::now();
        drop(pool);
        let took = start.elapsed();
        for pid in pids {
            assert!(gone(pid), "client {pid} outlived the pool");
        }
        assert!(
            took < Duration::from_millis(500),
            "retiring eight took {took:?}: one after another, not side by side"
        );
    }
}
