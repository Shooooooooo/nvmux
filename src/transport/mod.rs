//! The local/remote seam.
//!
//! Everything above this module — the picker, the attach path — is written once
//! against [`Transport`] and does not know whether the sessions it is listing
//! live on this machine or on the far end of an SSH connection.

pub mod exec;
pub mod local;
pub mod protocol;
pub mod remote;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{NvmuxError, Result, SessionError};
use crate::rpc;
use crate::session::Session;

/// Where a set of sessions lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Local,
    /// Passed to `ssh` **verbatim**, so a hostname, `user@host` or any
    /// `~/.ssh/config` alias works without nvmux understanding it.
    Ssh(String),
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Location::Local => f.write_str("local"),
            Location::Ssh(host) => f.write_str(host),
        }
    }
}

/// Session management, independent of where the sessions run.
///
/// Object-safe on purpose: the picker holds a `Box<dyn Transport>` and never
/// branches on which one it has.
pub trait Transport {
    fn location(&self) -> &Location;

    /// Every session on the host, with liveness already determined. One call,
    /// not one per session — see [`crate::shell::LIST_SCRIPT`].
    fn list_sessions(&self) -> Result<Vec<Session>>;

    fn create_session(&self, name: &str) -> Result<Session>;

    /// Terminate a session, unconditionally: nvmux never asks about unsaved
    /// buffers. The ordinary way out is `:q` in the session itself, which ends
    /// it because the editor *is* the session.
    fn kill_session(&self, s: &Session) -> Result<()>;

    /// Rename is a metadata edit and nothing more. The socket is never renamed
    /// or moved: its path is the session's stable identity and the display name
    /// is only data, so no SSH forward has to be rebuilt.
    fn rename_session(&self, s: &Session, new_name: &str) -> Result<()>;

    /// A socket path on **this** machine that `nvim --server` can use.
    ///
    /// The single seam that makes remote sessions work: locally the session
    /// socket itself, over SSH the local end of a forward. Everything downstream
    /// is identical in both cases.
    fn local_socket_for(&self, s: &Session) -> Result<PathBuf>;
}

/// Build the transport for a location.
pub fn open(location: Location) -> Result<Box<dyn Transport>> {
    match location {
        Location::Local => Ok(Box::new(local::LocalTransport::new()?)),
        Location::Ssh(host) => Ok(Box::new(remote::SshTransport::new(host)?)),
    }
}

// --- shared between the two transports -------------------------------------

/// How long to wait for a new session's socket to appear and accept a
/// connection. A *reachability* budget, not a readiness one: a config that
/// clones plugins on first run can take far longer than any timeout worth
/// having here, and a session that is still starting is a good session.
pub(crate) const REACHABLE_TIMEOUT: Duration = Duration::from_secs(5);

const REACHABLE_POLL: Duration = Duration::from_millis(25);

/// Wait until the socket accepts a connection and a real Neovim answers.
/// Deliberately *not* a deferred call — see [`crate::rpc`]. Over SSH `sock` is
/// the local end of the forward, which also proves the forward works.
pub(crate) fn wait_until_reachable(sock: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(mut client) = rpc::Client::connect(sock, rpc::PROBE_TIMEOUT) {
            if client.api_info().is_ok() {
                return true;
            }
        }
        std::thread::sleep(REACHABLE_POLL);
    }
    false
}

/// Names carry no identity, but two identical ones in a picker is a usability
/// trap, so a name is refused if another session already has it — compared
/// case-insensitively. `except` is the session being renamed, which may of
/// course keep its own name.
pub(crate) fn ensure_name_free(
    existing: &[Session],
    name: &str,
    except: Option<&str>,
) -> Result<()> {
    let taken = existing
        .iter()
        .any(|s| except != Some(s.id.as_str()) && s.name.eq_ignore_ascii_case(name));
    if taken {
        return Err(SessionError::Exists(name.to_string()).into());
    }
    Ok(())
}

/// What a kill script outcome means for the caller. Files are removed by the
/// script only once the session is genuinely gone; claiming success on the
/// other outcomes would drop it from the picker while its Neovim kept running,
/// with no socket left to ever find it by.
pub(crate) fn kill_outcome(outcome: protocol::KillOutcome, name: &str) -> Result<()> {
    use protocol::KillOutcome;
    match outcome {
        KillOutcome::Killed | KillOutcome::Absent => Ok(()),
        KillOutcome::Orphaned => Err(SessionError::NotKilled {
            name: name.to_string(),
            reason: "it did not exit, even after SIGKILL",
        }
        .into()),
        KillOutcome::Unknown => Err(SessionError::NotKilled {
            name: name.to_string(),
            reason: "its socket is still present but no usable `ps` or /proc was \
                     available to find the process, so nothing was signalled",
        }
        .into()),
    }
}

/// Give a new session a way out that is not `:q`. `command!` requires an
/// uppercase name (`command! q` is E183), so this cannot shadow `:q` itself.
/// Failure is not worth failing the create over.
pub(crate) fn install_detach_alias(sock: &Path) {
    if let Ok(mut client) = rpc::Client::connect(sock, rpc::CONNECT_TIMEOUT) {
        if let Err(e) = client.command("command! -bar Detach detach") {
            tracing::debug!(error = %e, "could not install the :Detach alias");
        }
    }
}

/// Shared by both transports: resolve each session's display number, then sort
/// by it.
///
/// Ordering is by number rather than by name because the number is what the user
/// reads off the screen and presses; a name sort would scramble the column and
/// make it noise. It also stops the list reshuffling on every rename.
///
/// Resolution never writes to disk. The stored number is the authority whenever
/// it has an answer, and the gaps this fills — metadata written before numbering
/// existed, orphans with no metadata at all, and the two-clients-created-at-once
/// duplicate — are covered in memory. A read path that wrote would cost an SSH
/// round trip per listing, and would persist a derived number as if it had been
/// assigned.
pub(crate) fn finish_listing(mut sessions: Vec<Session>) -> Result<Vec<Session>> {
    // Stored number first, so the sessions with a claim on a number get to keep
    // it; `created` then `id` puts the unnumbered ones in a stable order rather
    // than whatever the directory happened to yield.
    sessions.sort_by(|a, b| {
        let key = |s: &Session| if s.num == 0 { u32::MAX } else { s.num };
        key(a)
            .cmp(&key(b))
            .then_with(|| a.created.cmp(&b.created))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut taken: Vec<u32> = Vec::with_capacity(sessions.len());
    for s in &mut sessions {
        s.state.num = if s.num != 0 && !taken.contains(&s.num) {
            s.num
        } else {
            smallest_free(&taken)
        };
        taken.push(s.state.num);
    }

    sessions.sort_by_key(|s| s.state.num);
    Ok(sessions)
}

/// The number to give a session being created now: the smallest not already on
/// screen, so killing 3 and creating again refills the hole rather than climbing.
///
/// Resolved numbers, not stored ones — a legacy session displaying as 3 must not
/// have 3 taken out from under it.
pub(crate) fn next_free_num(existing: &[Session]) -> u32 {
    let taken: Vec<u32> = existing.iter().map(|s| s.state.num).collect();
    smallest_free(&taken)
}

/// The smallest positive integer not in `taken`. Linear in a list that is a
/// handful of sessions long.
fn smallest_free(taken: &[u32]) -> u32 {
    (1..)
        .find(|n| !taken.contains(n))
        .expect("u32 is not exhausted")
}

impl From<NvmuxError> for std::io::Error {
    fn from(e: NvmuxError) -> Self {
        std::io::Error::other(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_taken_name_is_refused_case_insensitively_except_for_its_own_session() {
        let existing = vec![session("aaa", 1, 1), session("bbb", 2, 2)];
        assert!(ensure_name_free(&existing, "name-aaa", None).is_err());
        assert!(ensure_name_free(&existing, "NAME-AAA", None).is_err());
        assert!(ensure_name_free(&existing, "name-ccc", None).is_ok());
        // A rename to its own name (or its own name in another case) is fine.
        assert!(ensure_name_free(&existing, "Name-aaa", Some("aaa")).is_ok());
        assert!(ensure_name_free(&existing, "name-bbb", Some("aaa")).is_err());
    }

    #[test]
    fn only_killed_and_absent_count_as_killed() {
        use protocol::KillOutcome::*;
        assert!(kill_outcome(Killed, "x").is_ok());
        assert!(kill_outcome(Absent, "x").is_ok());
        for outcome in [Orphaned, Unknown] {
            let err = kill_outcome(outcome, "x").expect_err("not killed");
            assert!(matches!(
                err,
                NvmuxError::Session(SessionError::NotKilled { .. })
            ));
        }
    }

    /// `created` is what orders the unnumbered ones, so it is set explicitly.
    fn session(id: &str, num: u32, created: u64) -> Session {
        let mut s = Session::new(id.to_string(), format!("name-{id}"), 0, num);
        s.created = created;
        s
    }

    fn resolved(sessions: Vec<Session>) -> Vec<(String, u32)> {
        finish_listing(sessions)
            .expect("listing")
            .into_iter()
            .map(|s| (s.id, s.state.num))
            .collect()
    }

    #[test]
    fn stored_numbers_are_kept_and_the_list_reads_in_number_order() {
        let out = resolved(vec![
            session("ccc", 5, 3),
            session("aaa", 1, 1),
            session("bbb", 2, 2),
        ]);
        assert_eq!(
            out,
            vec![
                ("aaa".to_string(), 1),
                ("bbb".to_string(), 2),
                ("ccc".to_string(), 5),
            ],
            "gaps are kept; the list reads in number order"
        );
    }

    /// Metadata written before numbering existed, and orphans, have no stored
    /// number. They must still be reachable by keystroke.
    #[test]
    fn unnumbered_sessions_are_given_the_smallest_free_numbers() {
        let out = resolved(vec![
            session("aaa", 0, 1),
            session("bbb", 2, 2),
            session("ccc", 0, 3),
        ]);
        // "bbb" keeps the 2 it stored; the unnumbered pair take 1 and 3 around
        // it, oldest first, and the list comes back ordered by number.
        assert_eq!(
            out,
            vec![
                ("aaa".to_string(), 1),
                ("bbb".to_string(), 2),
                ("ccc".to_string(), 3),
            ]
        );
    }

    /// Two clients creating at the same moment can store the same number. Both
    /// sessions must stay pressable.
    #[test]
    fn duplicate_stored_numbers_are_broken_apart() {
        let out = resolved(vec![
            session("aaa", 1, 1),
            session("bbb", 1, 2),
            session("ccc", 1, 3),
        ]);
        let nums: Vec<u32> = out.iter().map(|(_, n)| *n).collect();
        assert_eq!(nums, vec![1, 2, 3]);
        assert_eq!(out[0].0, "aaa", "the earliest keeps the number it claimed");
    }

    #[test]
    fn every_session_gets_a_distinct_number_however_broken_the_input() {
        let out = resolved(vec![
            session("aaa", 0, 4),
            session("bbb", 7, 1),
            session("ccc", 7, 2),
            session("ddd", 0, 3),
        ]);
        let mut nums: Vec<u32> = out.iter().map(|(_, n)| *n).collect();
        nums.sort_unstable();
        nums.dedup();
        assert_eq!(nums.len(), 4, "no two sessions share a number");
        assert!(nums.iter().all(|&n| n >= 1), "numbering starts at 1");
    }

    #[test]
    fn an_empty_listing_is_not_a_problem() {
        assert!(finish_listing(vec![]).expect("listing").is_empty());
    }

    #[test]
    fn the_next_free_number_starts_at_one_and_fills_gaps() {
        assert_eq!(next_free_num(&[]), 1, "sessions are numbered from 1");

        let listed =
            finish_listing(vec![session("aaa", 1, 1), session("bbb", 2, 2)]).expect("listing");
        assert_eq!(next_free_num(&listed), 3, "appends when there is no gap");

        // What killing the middle session leaves behind.
        let listed =
            finish_listing(vec![session("aaa", 1, 1), session("ccc", 3, 3)]).expect("listing");
        assert_eq!(next_free_num(&listed), 2, "refills the hole");
    }

    /// It reads the *resolved* number, or a legacy session displaying as 3 would
    /// have 3 taken out from under it.
    #[test]
    fn the_next_free_number_respects_numbers_that_were_only_resolved() {
        let listed =
            finish_listing(vec![session("aaa", 0, 1), session("bbb", 0, 2)]).expect("listing");
        assert_eq!(listed[0].num, 0, "still unnumbered on disk");
        assert_eq!(next_free_num(&listed), 3);
    }
}
