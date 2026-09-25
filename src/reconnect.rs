//! Getting back into a session after the link to its host has dropped.
//!
//! A session over ssh is reached through one master connection, and when that
//! goes — the laptop slept, the Wi-Fi changed, a VPN renewed — the forward
//! closes under the `--remote-ui` client and the client leaves, exactly as it
//! would have if the session had been quit. The session itself is untouched:
//! the editor runs on the far side and never noticed. So the client's exit is
//! not the end of anything, and this module is what turns it back into the
//! session the user was in.
//!
//! # The link tells the cases apart
//!
//! The relay cannot say why its client left, and does not try: it reports
//! [`crate::pty::Outcome::ChildExited`] for a `:q` and for a dropped link
//! alike. What separates them is the master. A session that ended leaves the
//! master standing; a dropped link takes the master, the shell and every
//! forward with it. [`Transport::reconnect`] looks, and brings the master back
//! if it can — and locally there is no link to lose, so the answer there is
//! always [`Reconnect::Unneeded`] and nothing here ever fires.
//!
//! # Bounded, and only for the errors waiting can fix
//!
//! The first attempt is made at once. A link that is coming back — the usual
//! case after a sleep — is often not back on the very second the master gave up
//! on it, so a failure is retried on the schedule in [`DELAYS`]: about a minute
//! in all, which covers a machine finding its network again and does not leave
//! an unattended terminal spawning `ssh` for the rest of the day. After that
//! `main` gives up and exits with the reason — not back to the picker, whose
//! first listing would go over the same dead link (see the `GaveUp` arm in
//! main.rs) — and the session is still there for the next `nvmux <host>`.
//!
//! Only an error that time can change is retried — the host not answering, or
//! the connection to it dying on the way up. Anything else is reported at
//! once: a refused key is not going to be accepted on the third try, and a
//! passphrase prompt (which ssh puts on the terminal, as it did the first time)
//! must not be asked six times over.
//!
//! # Pure, given its two effects
//!
//! Sleeping and telling the user are the caller's, passed in, so the policy —
//! which errors, how many times, how long between — is tested here without a
//! clock or a terminal. `main` supplies `std::thread::sleep` and a line on
//! stderr.

use std::time::Duration;

use crate::error::{NvmuxError, SshError};
use crate::transport::{Reconnect, Transport};

/// How long to wait before each retry, in order. A failure after the last one
/// is where the series ends. Doubling, and capped: the first retries are quick
/// because a link that is a second late is common, the later ones are spaced
/// because a link that is thirty seconds late is a link that is being fixed.
pub const DELAYS: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(30),
];

/// How the recovery ended.
#[derive(Debug)]
pub enum Verdict {
    /// The link was up all along: the client left for reasons of its own — the
    /// session was quit, or the editor died — and the picker will show which.
    Unneeded,
    /// The link is back, so the same session can be attached again.
    Restored,
    /// The link is down and is staying down: after every retry, or for a reason
    /// waiting would not change. The picker gets the error.
    GaveUp(NvmuxError),
}

/// One failed attempt, on its way to the terminal, and what happens next.
#[derive(Debug)]
pub struct Retry<'a> {
    /// The attempt that just failed, counting from 1.
    pub attempt: usize,
    /// How many retries the series allows in all.
    pub of: usize,
    pub error: &'a NvmuxError,
    /// How long until the next attempt.
    pub wait: Duration,
}

/// Try to get the link back, on the schedule above.
///
/// `report` is told about each failure *before* the wait it announces, so the
/// user is never looking at a silent terminal; `sleep` is the wait itself.
pub fn recover<R, S>(transport: &dyn Transport, mut report: R, mut sleep: S) -> Verdict
where
    R: FnMut(&Retry<'_>),
    S: FnMut(Duration),
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        let error = match transport.reconnect() {
            // On the first try this is the whole answer: nothing was wrong
            // with the link. On a later one it means the master is back
            // without our doing — another nvmux on the same host brought it
            // up between two tries — and back is back.
            Ok(Reconnect::Unneeded) if attempt == 1 => return Verdict::Unneeded,
            Ok(Reconnect::Unneeded | Reconnect::Restored) => return Verdict::Restored,
            Err(e) => e,
        };
        if !worth_retrying(&error) || attempt > DELAYS.len() {
            return Verdict::GaveUp(error);
        }
        let wait = DELAYS[attempt - 1];
        report(&Retry {
            attempt,
            of: DELAYS.len(),
            error: &error,
            wait,
        });
        sleep(wait);
    }
}

/// Whether time might fix this. The host not answering and the connection
/// dying on the way up are what a network coming back looks like from here;
/// everything else — a refused key, no `ssh`, a forward the master rejected —
/// will fail the same way in a second, and is better reported now.
fn worth_retrying(error: &NvmuxError) -> bool {
    matches!(
        error,
        NvmuxError::Ssh(SshError::Unreachable(_) | SshError::MasterDied(_) | SshError::NoMaster(_))
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::PathBuf;

    use super::*;
    use crate::error::Result;
    use crate::launch::Launch;
    use crate::session::Session;
    use crate::transport::Location;

    /// A transport that answers `reconnect` from a script and nothing else:
    /// recovery asks it one question, and the test is about what recovery does
    /// with the answers.
    struct Scripted {
        answers: RefCell<VecDeque<Result<Reconnect>>>,
        asked: RefCell<usize>,
    }

    impl Scripted {
        fn new(answers: Vec<Result<Reconnect>>) -> Self {
            Self {
                answers: RefCell::new(answers.into()),
                asked: RefCell::new(0),
            }
        }
    }

    impl Transport for Scripted {
        fn location(&self) -> &Location {
            unimplemented!()
        }
        fn list_sessions(&self) -> Result<Vec<Session>> {
            unimplemented!()
        }
        fn create_session(&self, _: &str, _: &Launch, _: &str) -> Result<Session> {
            unimplemented!()
        }
        fn kill_session(&self, _: &Session) -> Result<()> {
            unimplemented!()
        }
        fn rename_session(&self, _: &Session, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn renumber(&self, _: &[Session]) -> Result<()> {
            unimplemented!()
        }
        fn home(&self) -> &str {
            unimplemented!()
        }
        fn dir_source(&self) -> crate::dirs::DirSource {
            unimplemented!()
        }
        fn local_socket_for(&self, _: &Session) -> Result<PathBuf> {
            unimplemented!()
        }
        fn reconnect(&self) -> Result<Reconnect> {
            *self.asked.borrow_mut() += 1;
            self.answers
                .borrow_mut()
                .pop_front()
                .expect("asked more often than the script allows")
        }
    }

    fn unreachable() -> NvmuxError {
        NvmuxError::Ssh(SshError::Unreachable("myhost".into()))
    }

    /// Run recovery, collecting the waits it asked for and the attempts it
    /// reported.
    fn run(transport: &Scripted) -> (Verdict, Vec<Duration>, Vec<usize>) {
        let mut slept = Vec::new();
        let mut reported = Vec::new();
        let verdict = recover(
            transport,
            |retry| {
                reported.push(retry.attempt);
                assert_eq!(retry.of, DELAYS.len());
                assert_eq!(retry.wait, DELAYS[retry.attempt - 1]);
            },
            |wait| slept.push(wait),
        );
        (verdict, slept, reported)
    }

    /// The `:q` case: the link was up, so this was never a reconnection, and
    /// nobody is told anything.
    #[test]
    fn a_link_that_never_went_is_left_alone() {
        let t = Scripted::new(vec![Ok(Reconnect::Unneeded)]);
        let (verdict, slept, reported) = run(&t);
        assert!(matches!(verdict, Verdict::Unneeded), "{verdict:?}");
        assert!(slept.is_empty());
        assert!(reported.is_empty());
        assert_eq!(*t.asked.borrow(), 1);
    }

    /// The common case after a sleep: the link is back by the time the master
    /// noticed it had gone.
    #[test]
    fn a_link_restored_first_time_costs_no_wait() {
        let t = Scripted::new(vec![Ok(Reconnect::Restored)]);
        let (verdict, slept, reported) = run(&t);
        assert!(matches!(verdict, Verdict::Restored), "{verdict:?}");
        assert!(slept.is_empty());
        assert!(reported.is_empty());
    }

    /// A host that is not there yet is waited for, each failure announced
    /// before its wait, and the series stops the moment it is.
    #[test]
    fn an_unreachable_host_is_retried_on_the_schedule_until_it_answers() {
        let t = Scripted::new(vec![
            Err(unreachable()),
            Err(unreachable()),
            Ok(Reconnect::Restored),
        ]);
        let (verdict, slept, reported) = run(&t);
        assert!(matches!(verdict, Verdict::Restored), "{verdict:?}");
        assert_eq!(slept, &DELAYS[..2]);
        assert_eq!(reported, [1, 2]);
        assert_eq!(*t.asked.borrow(), 3);
    }

    /// Another nvmux on the same host can bring the master up between two of
    /// our tries. That is not "unneeded" — ours was needed and is done.
    #[test]
    fn a_master_someone_else_brought_back_counts_as_restored() {
        let t = Scripted::new(vec![Err(unreachable()), Ok(Reconnect::Unneeded)]);
        let (verdict, slept, _) = run(&t);
        assert!(matches!(verdict, Verdict::Restored), "{verdict:?}");
        assert_eq!(slept, &DELAYS[..1]);
    }

    /// The series is finite: every delay is used once, and the failure after
    /// the last of them is the one handed back.
    #[test]
    fn a_host_that_never_answers_is_given_up_on_after_every_delay() {
        let answers = (0..=DELAYS.len()).map(|_| Err(unreachable())).collect();
        let t = Scripted::new(answers);
        let (verdict, slept, reported) = run(&t);
        assert!(matches!(verdict, Verdict::GaveUp(_)), "{verdict:?}");
        assert_eq!(slept, DELAYS);
        assert_eq!(reported, (1..=DELAYS.len()).collect::<Vec<_>>());
        assert_eq!(*t.asked.borrow(), DELAYS.len() + 1);
    }

    /// A refused key will be refused again, and retrying it would mean asking
    /// for a passphrase once per delay.
    #[test]
    fn an_error_waiting_cannot_fix_is_reported_at_once() {
        let t = Scripted::new(vec![Err(NvmuxError::Ssh(SshError::AuthFailed(
            "myhost".into(),
        )))]);
        let (verdict, slept, reported) = run(&t);
        match verdict {
            Verdict::GaveUp(NvmuxError::Ssh(SshError::AuthFailed(host))) => {
                assert_eq!(host, "myhost");
            }
            other => panic!("expected the auth failure back, got {other:?}"),
        }
        assert!(slept.is_empty(), "no wait was worth having");
        assert!(reported.is_empty(), "nothing to retry, nothing to announce");
    }

    /// The whole series is about a minute: long enough for a machine to find
    /// its network again after a sleep, short enough that a terminal left
    /// unattended is not spawning `ssh` for the rest of the day.
    #[test]
    fn the_schedule_adds_up_to_about_a_minute() {
        let total: Duration = DELAYS.iter().sum();
        assert!(total >= Duration::from_secs(45), "{total:?}");
        assert!(total <= Duration::from_secs(90), "{total:?}");
        assert!(
            DELAYS.windows(2).all(|w| w[0] <= w[1]),
            "the waits do not get shorter"
        );
    }
}
