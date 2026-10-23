//! Reattaching to a session after its ssh link drops.
//!
//! When the master connection drops (sleep, network change, VPN renewal) the
//! forward closes and the client exits as if the session were quit, but the
//! remote editor is untouched. The relay reports
//! [`crate::pty::Outcome::ChildExited`] in both cases; [`Transport::reconnect`]
//! tells them apart by whether the master survived, and restores it if not.
//! Locally it always returns [`Reconnect::Unneeded`].
//!
//! The first attempt is immediate; failures are retried per [`DELAYS`], then
//! `main` exits with the reason rather than returning to the picker, whose
//! listing would use the same dead link. Only errors that time can fix are
//! retried: a refused key will not succeed later, and an ssh passphrase
//! prompt must not repeat once per retry.
//!
//! Sleeping and reporting are passed in, so the policy is tested without a
//! clock or a terminal.

use std::time::Duration;

use crate::error::{NvmuxError, SshError};
use crate::transport::{Reconnect, Transport};

/// Wait before each retry; a failure after the last ends the series. About a
/// minute in all: enough for a machine to regain its network after a sleep,
/// without an unattended terminal spawning `ssh` all day.
pub const DELAYS: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(30),
];

#[derive(Debug)]
pub enum Verdict {
    /// The link was up: the client exited on its own (session quit or editor
    /// died).
    Unneeded,
    /// The link is back; the same session can be reattached.
    Restored,
    /// Retries exhausted, or the error is not one that waiting fixes.
    GaveUp(NvmuxError),
}

/// A failed attempt, as reported to the user.
#[derive(Debug)]
pub struct Retry<'a> {
    /// 1-based.
    pub attempt: usize,
    /// Total retries allowed.
    pub of: usize,
    pub error: &'a NvmuxError,
    /// Time until the next attempt.
    pub wait: Duration,
}

/// Retry [`Transport::reconnect`] on the [`DELAYS`] schedule.
///
/// `report` is called for each failure before its wait, so the terminal is
/// never silent; `sleep` performs the wait.
pub fn recover<R, S>(transport: &dyn Transport, mut report: R, mut sleep: S) -> Verdict
where
    R: FnMut(&Retry<'_>),
    S: FnMut(Duration),
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        let error = match transport.reconnect() {
            // First try: the link was fine. Later: another nvmux on the host
            // restored the master between our tries, which counts as restored.
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

/// Whether time might fix this: an unanswering host or a connection dying
/// during setup is what a returning network looks like. Anything else (refused
/// key, no `ssh`, rejected forward) would fail the same way again.
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

    /// A transport that only answers `reconnect`, from a script.
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

    /// Runs recovery, returning the verdict, the waits and the reported
    /// attempts.
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

    /// The `:q` case.
    #[test]
    fn a_link_that_never_went_is_left_alone() {
        let t = Scripted::new(vec![Ok(Reconnect::Unneeded)]);
        let (verdict, slept, reported) = run(&t);
        assert!(matches!(verdict, Verdict::Unneeded), "{verdict:?}");
        assert!(slept.is_empty());
        assert!(reported.is_empty());
        assert_eq!(*t.asked.borrow(), 1);
    }

    #[test]
    fn a_link_restored_first_time_costs_no_wait() {
        let t = Scripted::new(vec![Ok(Reconnect::Restored)]);
        let (verdict, slept, reported) = run(&t);
        assert!(matches!(verdict, Verdict::Restored), "{verdict:?}");
        assert!(slept.is_empty());
        assert!(reported.is_empty());
    }

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

    /// Another nvmux on the host may restore the master between our tries.
    #[test]
    fn a_master_someone_else_brought_back_counts_as_restored() {
        let t = Scripted::new(vec![Err(unreachable()), Ok(Reconnect::Unneeded)]);
        let (verdict, slept, _) = run(&t);
        assert!(matches!(verdict, Verdict::Restored), "{verdict:?}");
        assert_eq!(slept, &DELAYS[..1]);
    }

    /// Each delay is used once; the failure after the last is returned.
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

    /// Retrying a refused key would prompt for a passphrase once per delay.
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
