//! The clients kept for the sessions that are not in front.
//!
//! On — `[client] per_session`, the default — every client that leaves the
//! front is parked (see [`crate::pty::Parked`]), one per session, and a
//! return to that session — from the picker, the prompt, the help, or a
//! switch — takes it back out: a resume, not a spawn. The screen the client
//! last had goes straight back on the terminal, with what it told the
//! terminal, and its server's own is asked for on top (see `pty::relay`). The
//! cost is that each one stays a UI of its session's server until nvmux
//! leaves, which is why it can be turned off.
//!
//! Off, nvmux has one client at a time. A switch retires it and starts
//! another, and the client that `<prefix> Space`, `<prefix> c` and
//! `<prefix> ?` come back to is simply held, unread, until they do. That path
//! does not come through here at all: [`Pool::set_aside`] hands the client
//! straight back, and a `Pool` that has never been given one has nothing to
//! take.

use crate::pty::{Attachment, Parked};
use crate::session::Session;

/// What [`Pool::take`] found for a session.
#[derive(Debug)]
pub enum Taken {
    /// Its client, parked and alive: resume it. Boxed, being the one variant
    /// with anything in it.
    Kept(Box<Attachment>),
    /// A client was parked for it, and has left — its session ended, or the
    /// link it came over went. The caller cannot tell which from here, and
    /// the second is one to recover from (see `reconnect`) before anything
    /// else goes over that link.
    Left,
    /// Nothing was parked for it.
    Absent,
}

/// The parked clients, by session. See the module docs.
#[derive(Debug)]
pub struct Pool {
    /// Whether clients are kept at all (`[client] per_session`).
    keeps: bool,
    /// At most one per session, in the order they were parked. A handful at
    /// most — one per session the user has visited — so a list is the map.
    parked: Vec<Parked>,
}

impl Pool {
    /// A pool that keeps clients if `keeps`, and otherwise hands every one
    /// straight back.
    pub fn new(keeps: bool) -> Self {
        Self {
            keeps,
            parked: Vec::new(),
        }
    }

    /// The pool the configuration asks for.
    pub fn configured() -> Self {
        Self::new(crate::config::get().client.per_session)
    }

    /// Take a client that has just left the front.
    ///
    /// Kept clients are parked, and `None` comes back; anything else comes
    /// back as it went in, for the caller to hold as it always has — which is
    /// also where a kept client goes that left the front before its startup
    /// was over (see `pty::Attachment::is_parkable`), to be replaced on the
    /// next switch as a client that is not kept would be. A client already
    /// parked for the same session — there should never be one, since a
    /// session's client is taken out before another is started for it — is
    /// retired to make room: two UIs from one nvmux would only halve the
    /// session's screen between them.
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

    /// The client parked for this session, taken back to be resumed; see
    /// [`Taken`] for what else it can find.
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

    /// Let go of the clients of sessions a fresh listing no longer has: gone,
    /// or found dead by the host. A parked client of one is attached to
    /// nothing anybody can come back to, and on a host whose sessions could
    /// not be inspected it may still be holding the session's screen to its
    /// size.
    pub fn keep_only(&mut self, listing: &[Session]) {
        self.parked
            .retain(|p| listing.iter().any(|s| s.id == p.session_id()));
    }

    /// Whether a live client is parked for this session: what makes the
    /// picker's `Esc` a way back to it, and what spares a switch to it the
    /// early spawn a fresh attach would start.
    pub fn holds(&self, session_id: &str) -> bool {
        self.parked
            .iter()
            .any(|p| p.session_id() == session_id && p.is_alive())
    }

    /// Whether anything is parked for this session, alive or not: what a
    /// switch's early start must not run ahead of (see `session_loop`).
    pub fn has(&self, session_id: &str) -> bool {
        self.parked.iter().any(|p| p.session_id() == session_id)
    }

    /// How many clients are parked, live or not yet let go of.
    pub fn len(&self) -> usize {
        self.parked.len()
    }

    /// Whether nothing is parked.
    pub fn is_empty(&self) -> bool {
        self.parked.is_empty()
    }

    /// Retire the client parked for this session, if there is one.
    fn retire(&mut self, session_id: &str) {
        self.parked.retain(|p| p.session_id() != session_id);
    }
}

/// Every parked client is retired with the pool — on a quit, a detach, and
/// every error on the way out — each told first and then waited for, so that
/// they go side by side, each on its own thread, rather than one after
/// another. None of what they write on the way out reaches the terminal:
/// nothing reads it but `reap`, which throws it away.
impl Drop for Pool {
    fn drop(&mut self) {
        for parked in &mut self.parked {
            parked.tell_to_retire();
        }
        self.parked.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::pty::kept_standin;

    /// Whether the process has gone, within a few seconds.
    fn gone(pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            // Signal 0 checks for the pid without signalling it; a reaped one
            // is not there.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Off, the pool is not there at all: every client comes straight back,
    /// the very one that went in, and nothing is held.
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

    /// On, a kept client is parked under its session and taken back by it —
    /// the same process, and only for that session.
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

    /// A client that left the front before its startup was over goes back to
    /// the caller, to be held and replaced as one that is not kept would be.
    #[test]
    fn a_client_cut_short_in_its_startup_is_not_parked() {
        let mut pool = Pool::new(true);
        let mut a = kept_standin("s1", "exec sleep 30");
        a.cut_short_for_test();
        assert!(pool.set_aside(Some(a)).is_some());
        assert!(pool.is_empty());
    }

    /// One client per session: a second parked for the same session retires
    /// the first.
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

    /// A client that leaves while parked is not held: the picker's `Esc` and a
    /// switch both look it up, and must find nothing rather than a corpse.
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

    /// A listing that no longer has a session lets go of its client.
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

    /// Letting go of the pool retires every client in it, side by side: each
    /// stand-in takes 80 ms to leave once hung up, so four of them one after
    /// another would take over 300 ms.
    #[test]
    fn dropping_the_pool_retires_every_client_side_by_side() {
        let mut pool = Pool::new(true);
        let pids: Vec<i32> = (0..4)
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
            took < Duration::from_millis(250),
            "retiring four took {took:?}: one after another, not side by side"
        );
    }
}
