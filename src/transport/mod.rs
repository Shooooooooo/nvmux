//! The local/remote seam.
//!
//! The picker and attach path use [`Transport`] without knowing where sessions
//! run. The scripts are the protocol, so [`list`], [`create`] and [`kill`] are
//! shared over a [`Host`] supplying what differs: how the host is reached, how
//! liveness is settled and where metadata lives.

pub mod local;
pub mod protocol;
pub mod remote;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::error::{NvmuxError, Result, SessionError};
use crate::launch::Launch;
use crate::proc::{Output, Shell};
use crate::rpc;
use crate::session::Session;
use crate::shell;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Local,
    /// Passed to `ssh` verbatim, so a hostname, `user@host` or any
    /// `~/.ssh/config` alias works.
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

/// Object-safe: the picker holds a `dyn Transport`.
pub trait Transport {
    fn location(&self) -> &Location;

    /// Every session on the host, liveness determined, in one script run.
    fn list_sessions(&self) -> Result<Vec<Session>>;

    /// Spawn a session running `launch`, with `{sock}` replaced by its socket.
    /// `directory` is absolute, on the transport's host; the spawn script
    /// checks it exists, avoiding a round trip and a race.
    fn create_session(&self, name: &str, launch: &Launch, directory: &str) -> Result<Session>;

    /// Terminate unconditionally; nvmux never asks about unsaved buffers.
    fn kill_session(&self, s: &Session) -> Result<()>;

    /// Metadata only: the socket path is the session's stable identity.
    fn rename_session(&self, s: &Session, new_name: &str) -> Result<()>;

    /// Persist the picker's order: write each record's [`Session::num`] and
    /// nothing else, in one batch.
    ///
    /// Locally the file is re-read and only `num` changed, keeping a concurrent
    /// rename. Over ssh the record is written back whole: safe because
    /// `list.sh` ships each entire `<id>.json` and `SessionState` is
    /// `#[serde(skip)]`.
    ///
    /// Not atomic; [`finish_listing`] turns half-written state into a valid
    /// order. Sessions without metadata are skipped, not given a record.
    fn renumber(&self, sessions: &[Session]) -> Result<()>;

    /// The session host's home, for the default directory and `~`. Empty if
    /// the host could not say.
    fn home(&self) -> &str;

    /// For the create prompt's completion worker (see [`crate::dirs`]).
    fn dir_source(&self) -> crate::dirs::DirSource;

    /// A socket path on this machine for `nvim --server`: locally the session
    /// socket, over SSH the local end of a forward.
    fn local_socket_for(&self, s: &Session) -> Result<PathBuf>;

    /// Batch [`Self::local_socket_for`]'s work (over ssh, one `-O forward`).
    /// Failures are left for `local_socket_for` to retry and report.
    fn reach_all(&self, _sessions: &[Session]) {}

    /// Restore the link if it dropped. Called when a client exits on its own,
    /// which is also what a dropped ssh link looks like; an error means the
    /// link stays down. One bounded attempt: [`crate::reconnect`] retries.
    fn reconnect(&self) -> Result<Reconnect> {
        Ok(Reconnect::Unneeded)
    }
}

/// What [`Transport::reconnect`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconnect {
    /// The link never dropped: the session itself ended.
    Unneeded,
    /// The link dropped and is back; its forwards must be remade.
    Restored,
}

/// Lent to the picker's background listing thread (see
/// [`crate::ui::Listing`]); the shell lock makes other scripts wait for it.
pub type Shared = std::sync::Arc<dyn Transport + Send + Sync>;

pub fn open(location: Location) -> Result<Shared> {
    match location {
        Location::Local => Ok(std::sync::Arc::new(local::LocalTransport::new()?)),
        Location::Ssh(host) => Ok(std::sync::Arc::new(remote::SshTransport::new(host)?)),
    }
}

// --- the bodies both transports share ---------------------------------------

/// What a transport supplies to [`list`], [`create`] and [`kill`].
pub(crate) trait Host {
    fn location(&self) -> &Location;

    fn dir(&self) -> &str;

    /// A failing script is `Ok` with a status; `Err` means the shell or its
    /// link failed.
    fn run(&self, script: &str, args: &[&str]) -> Result<Output>;

    /// A listing already in hand (over ssh, from the greeting), taken once.
    fn take_listing(&self) -> Option<String> {
        None
    }

    fn settle(&self, listed: Vec<Session>) -> Result<Vec<Session>>;

    /// The `--listen` path, checked against the length limit before spawning:
    /// Neovim silently truncates an overlong path.
    fn host_sock(&self, id: &str) -> Result<PathBuf>;

    fn reach(&self, id: &str, host_sock: &Path) -> Result<PathBuf>;

    /// Undo [`Host::reach`]. Always safe to call.
    fn unreach(&self, _id: &str) {}

    /// Remove what the kill script could not, which would otherwise resurrect
    /// the session in the next listing.
    fn sweep(&self, _id: &str) {}

    fn write_meta(&self, session: &Session) -> Result<()>;

    fn log_path(&self, id: &str) -> PathBuf;

    fn log_tail(&self, _id: &str) -> Option<String> {
        None
    }
}

/// Every session on the host, liveness settled, numbered and sorted.
pub(crate) fn list(h: &impl Host) -> Result<Vec<Session>> {
    let stdout = match h.take_listing() {
        Some(stdout) => stdout,
        None => {
            // Timed apart from settling: the two have different fixes.
            let t_script = std::time::Instant::now();
            let out = h.run(shell::LIST_SCRIPT, &[h.dir()])?;
            tracing::debug!(
                ms = t_script.elapsed().as_secs_f64() * 1000.0,
                "timing: listing script"
            );
            if !out.ok() {
                tracing::warn!(status = out.status, stderr = %out.stderr, "list.sh failed");
            }
            out.stdout
        }
    };
    let listed = protocol::rows_to_sessions(protocol::parse_listing(&stdout)?);
    Ok(finish_listing(h.settle(listed)?))
}

/// Spawn a session and wait for it to answer (see
/// [`Transport::create_session`]).
pub(crate) fn create(
    h: &(impl Host + Sync),
    name: &str,
    launch: &Launch,
    directory: &str,
) -> Result<Session> {
    let existing = list(h)?;
    let (id, rank) = plan_create(&existing, name)?;
    let host_sock = h.host_sock(&id)?;

    // Substituted here, where the path's length was checked.
    let argv = launch.argv_for(&host_sock.to_string_lossy());
    let mut args: Vec<&str> = vec![h.dir(), &id, directory];
    args.extend(argv.iter().map(String::as_str));

    tracing::info!(host = %h.location(), %id, name, command = launch.line(), directory, "spawning session");
    let out = h.run(shell::SPAWN_SCRIPT, &args)?;
    let spawned = protocol::parse_spawn(&out.stdout)?;

    // Failures end the session via the kill script, which removes files only
    // after the process exits (unlinking on a timeout would orphan a slow
    // Neovim). The log is read first, since the kill removes it.
    let not_ready = |detail: String| -> NvmuxError {
        let log_tail = h.log_tail(&id).unwrap_or(detail);
        discard(h, &id, spawned.pid);
        SessionError::NotReady {
            name: name.to_string(),
            timeout: REACHABLE_TIMEOUT,
            log: h.log_path(&id),
            log_tail,
        }
        .into()
    };

    if !spawned.socket_appeared {
        return Err(not_ready(out.stderr.trim().to_string()));
    }

    let mut session = Session::new(id.clone(), name.to_string(), spawned.pid.unwrap_or(0), rank)
        .launched_with(launch.line())
        .started_in(directory);

    // The metadata write (its own round trip) runs in parallel with reaching
    // the socket; a session that never answers is killed after both finish.
    let (local, reached, written) = std::thread::scope(|scope| {
        let writing = std::thread::Builder::new()
            .name("nvmux-meta".into())
            .spawn_scoped(scope, || h.write_meta(&session));
        let local = h.reach(&id, &host_sock);
        let reached = local
            .as_ref()
            .is_ok_and(|local| reachable(local, REACHABLE_TIMEOUT));
        let written = match writing {
            Ok(thread) => thread
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
            Err(_) => h.write_meta(&session),
        };
        (local, reached, written)
    });
    if let Err(e) = local {
        discard(h, &id, spawned.pid);
        return Err(e);
    }

    if !reached {
        // `ssh -O forward` to a missing remote socket still succeeds, so only
        // an answer proves the session.
        return Err(not_ready("the session never answered".into()));
    }
    written?;
    // The caller attaches without re-listing, so give it the number a
    // listing would.
    session.state.num = numbered_in(existing, &session);

    tracing::info!(host = %h.location(), id = %session.id, name, pid = session.pid, "session ready");
    Ok(session)
}

/// Falls back to the rank, which is never zero (zero cannot be dialled).
fn numbered_in(existing: Vec<Session>, session: &Session) -> u32 {
    let mut listing = existing;
    listing.push(session.clone());
    finish_listing(listing)
        .iter()
        .find(|s| s.id == session.id)
        .map_or(session.num, |s| s.state.num)
}

/// End a session that never came up.
fn discard(h: &impl Host, id: &str, pid: Option<u32>) {
    h.unreach(id);
    let pid = opt_pid_arg(pid);
    match h.run(shell::KILL_SCRIPT, &[h.dir(), id, &pid]) {
        Ok(out) => match protocol::parse_kill(&out.stdout) {
            Ok(outcome) => tracing::debug!(%id, ?outcome, "cleaned up a failed create"),
            Err(e) => tracing::warn!(%id, error = %e, "cleanup after a failed create"),
        },
        Err(e) => tracing::warn!(%id, error = %e, "cleanup after a failed create"),
    }
}

/// Terminate a session; see [`Transport::kill_session`].
pub(crate) fn kill(h: &impl Host, s: &Session) -> Result<()> {
    // The script sends SIGTERM first, so nvim writes ShaDa and unlinks its
    // socket. The pid is used only if it still owns the socket (pids are
    // reused).
    let pid = pid_arg(s.pid);
    let out = h.run(shell::KILL_SCRIPT, &[h.dir(), &s.id, &pid])?;
    if !out.ok() {
        tracing::warn!(status = out.status, stderr = %out.stderr, "kill.sh failed");
    }
    let outcome = protocol::parse_kill(&out.stdout)?;

    h.unreach(&s.id);

    kill_outcome(outcome, &s.name)?;
    h.sweep(&s.id);
    tracing::info!(host = %h.location(), id = %s.id, name = %s.name, "session killed");
    Ok(())
}

// --- behind list/create/kill above; the transports also call plan_rename and
// --- claim_shell directly ---------------------------------------------------

/// Claim a transport's shell slot for one script run, waiting for another
/// thread's run. A poisoned lock is taken anyway: callers replace a dead shell
/// before running. A re-entrant claim is an error, not a deadlock.
pub(crate) fn claim_shell(slot: &Mutex<Option<Shell>>) -> Result<Claimed<'_>> {
    if CLAIMED.with(Cell::get) {
        return Err(NvmuxError::Io(std::io::Error::other(
            "the shell is already running a script",
        )));
    }
    let slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
    CLAIMED.with(|c| c.set(true));
    Ok(Claimed(slot))
}

thread_local! {
    static CLAIMED: Cell<bool> = const { Cell::new(false) };
}

pub(crate) struct Claimed<'a>(MutexGuard<'a, Option<Shell>>);

impl Drop for Claimed<'_> {
    fn drop(&mut self) {
        CLAIMED.with(|c| c.set(false));
    }
}

impl std::ops::Deref for Claimed<'_> {
    type Target = Option<Shell>;

    fn deref(&self) -> &Option<Shell> {
        &self.0
    }
}

impl std::ops::DerefMut for Claimed<'_> {
    fn deref_mut(&mut self) -> &mut Option<Shell> {
        &mut self.0
    }
}

/// Reachability, not readiness: a config cloning plugins can take far longer.
const REACHABLE_TIMEOUT: Duration = Duration::from_secs(5);

const REACHABLE_POLL: Duration = Duration::from_millis(25);

/// Wait until a real Neovim answers `nvim_get_mode` (not a deferred call; see
/// [`crate::rpc`]). The `:Detach` alias is sent behind it on the same
/// connection; being deferred, it is not waited for while the editor waits for
/// a key.
fn reachable(sock: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(mut client) = rpc::Client::connect(sock, rpc::PROBE_TIMEOUT) {
            if let Ok(mode) = client.ask_mode() {
                let alias = client.ask("nvim_command", vec![rmpv::Value::from(DETACH_ALIAS)]);
                if let Ok(mode) = client.mode_reply(mode) {
                    match alias {
                        Ok(alias) if !mode.blocking => {
                            let answered = client
                                .set_read_timeout(rpc::CONNECT_TIMEOUT)
                                .and_then(|()| client.reply_to(alias));
                            if let Err(e) = answered {
                                tracing::debug!(error = %e, "could not install the :Detach alias");
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::debug!(error = %e, "could not ask for the :Detach alias");
                        }
                    }
                    return true;
                }
            }
        }
        std::thread::sleep(REACHABLE_POLL);
    }
    false
}

/// Case-insensitive. `except` is the session being renamed.
fn ensure_name_free(existing: &[Session], name: &str, except: Option<&str>) -> Result<()> {
    let taken = existing
        .iter()
        .any(|s| except != Some(s.id.as_str()) && s.name.eq_ignore_ascii_case(name));
    if taken {
        return Err(SessionError::Exists(name.to_string()).into());
    }
    Ok(())
}

/// Validate the name and allocate an id and a rank (see [`finish_listing`]).
fn plan_create(existing: &[Session], name: &str) -> Result<(String, u32)> {
    crate::session::validate_name(name)?;
    ensure_name_free(existing, name, None)?;
    Ok((crate::ids::nonce()?, next_rank(existing)))
}

pub(crate) fn plan_rename(existing: &[Session], new_name: &str, id: &str) -> Result<()> {
    crate::session::validate_name(new_name)?;
    ensure_name_free(existing, new_name, Some(id))
}

/// Empty for 0 (unknown) and 1 (init): the pid is only a hint.
fn pid_arg(pid: u32) -> String {
    if pid > 1 {
        pid.to_string()
    } else {
        String::new()
    }
}

fn opt_pid_arg(pid: Option<u32>) -> String {
    pid.map(pid_arg).unwrap_or_default()
}

/// Reporting success unless the session is gone would hide a running Neovim
/// from the picker.
fn kill_outcome(outcome: protocol::KillOutcome, name: &str) -> Result<()> {
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

/// `command!` requires an uppercase name, so this cannot shadow `:q`.
const DETACH_ALIAS: &str = "command! -bar Detach detach";

/// Sort sessions by rank and number them by position.
///
/// Displayed numbers are dense and recomputed per listing. The stored
/// [`Session::num`] is a rank, used only to sort; ranks may have gaps. Missing
/// or duplicate ranks are resolved in memory, never written back (that would
/// cost an SSH round trip per listing).
fn finish_listing(mut sessions: Vec<Session>) -> Vec<Session> {
    // `created` then `id` give unranked sessions and ties a stable order.
    sessions.sort_by(|a, b| {
        let key = |s: &Session| if s.num == 0 { u32::MAX } else { s.num };
        key(a)
            .cmp(&key(b))
            .then_with(|| a.created.cmp(&b.created))
            .then_with(|| a.id.cmp(&b.id))
    });

    for (i, s) in sessions.iter_mut().enumerate() {
        s.state.num = i as u32 + 1;
    }

    sessions
}

/// One past the highest stored rank (not displayed number). Unranked sessions
/// still sort after it.
fn next_rank(existing: &[Session]) -> u32 {
    existing
        .iter()
        .map(|s| s.num)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

/// The session `<prefix> n` / `<prefix> p` moves to from number `from`,
/// wrapping. `from` need not exist (a session killed elsewhere): it lands on
/// the nearest number left. With one session, returns that session.
pub fn neighbour(sessions: &[Session], from: u32, dir: crate::keys::Direction) -> Option<&Session> {
    use crate::keys::Direction;
    // Zero is "unnumbered".
    let numbered = || sessions.iter().filter(|s| s.state.num != 0);
    // By id too, so duplicates give one deterministic answer.
    let order = |a: &&Session, b: &&Session| (a.state.num, &a.id).cmp(&(b.state.num, &b.id));
    match dir {
        Direction::Next => numbered()
            .filter(|s| s.state.num > from)
            .min_by(order)
            .or_else(|| numbered().min_by(order)),
        Direction::Prev => numbered()
            .filter(|s| s.state.num < from)
            .max_by(order)
            .or_else(|| numbered().max_by(order)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::NvmuxError;
    use crate::keys::Direction;

    #[test]
    fn a_taken_name_is_refused_case_insensitively_except_for_its_own_session() {
        let existing = vec![session("aaa", 1, 1), session("bbb", 2, 2)];
        assert!(ensure_name_free(&existing, "name-aaa", None).is_err());
        assert!(ensure_name_free(&existing, "NAME-AAA", None).is_err());
        assert!(ensure_name_free(&existing, "name-ccc", None).is_ok());
        // Renaming to its own name, in any case, is fine.
        assert!(ensure_name_free(&existing, "Name-aaa", Some("aaa")).is_ok());
        assert!(ensure_name_free(&existing, "name-bbb", Some("aaa")).is_err());
    }

    #[test]
    fn a_create_is_refused_before_anything_is_spawned_if_the_name_is_taken() {
        let existing = vec![session("aaaaaaaa", 1, 1)];
        let mut taken = existing.clone();
        taken[0].name = "notes".into();
        let err = plan_create(&taken, "NOTES").expect_err("case-insensitively taken");
        assert!(matches!(
            err,
            NvmuxError::Session(SessionError::Exists(ref n)) if n == "NOTES"
        ));
        assert!(plan_create(&taken, "").is_err(), "an empty name is invalid");
    }

    #[test]
    fn a_create_takes_a_rank_past_the_end() {
        let all = finish_listing(vec![
            session("aaaaaaaa", 1, 1),
            session("bbbbbbbb", 2, 2),
            session("cccccccc", 3, 3),
        ]);

        let (id, rank) = plan_create(&all, "fresh").expect("planned");
        assert_eq!(rank, 4);
        assert!(crate::ids::is_valid_id(&id), "{id:?}");

        // Killing the middle session leaves a hole in the ranks. The next
        // create must not fill it, or it would sort between the survivors.
        let gapped: Vec<_> = all.into_iter().filter(|s| s.num != 2).collect();
        let (_, rank) = plan_create(&gapped, "fresh").expect("planned");
        assert_eq!(rank, 4, "the freed rank is not reused");
    }

    #[test]
    fn a_new_session_is_numbered_by_where_it_lands() {
        let listed = finish_listing(vec![session("aaaaaaaa", 1, 1), session("cccccccc", 3, 3)]);
        assert_eq!(numbers(&listed), vec![1, 2], "the hole is already closed");

        let (_, rank) = plan_create(&listed, "fresh").expect("planned");
        let mut fresh = session("ffffffff", rank, 9);
        assert_eq!(numbered_in(listed, &fresh), 3, "it shows as the third row");

        // An orphan has no rank and stays last.
        let with_orphan =
            finish_listing(vec![session("aaaaaaaa", 1, 1), session("zzzzzzzz", 0, 2)]);
        fresh.num = next_rank(&with_orphan);
        assert_eq!(numbered_in(with_orphan, &fresh), 2);
    }

    #[test]
    fn stepping_wraps_at_both_ends() {
        let all = finish_listing(vec![
            session("aaaaaaaa", 1, 1),
            session("bbbbbbbb", 2, 2),
            session("cccccccc", 3, 3),
        ]);
        let step = |from, dir| neighbour(&all, from, dir).map(|s| s.state.num);

        assert_eq!(step(1, Direction::Next), Some(2));
        assert_eq!(step(2, Direction::Next), Some(3));
        assert_eq!(step(3, Direction::Next), Some(1), "forwards off the end");
        assert_eq!(step(1, Direction::Prev), Some(3), "backwards off the start");
        assert_eq!(step(2, Direction::Prev), Some(1));
    }

    #[test]
    fn stepping_crosses_a_gap_in_the_numbering() {
        let all = numbered(&[
            session("aaaaaaaa", 1, 1),
            session("bbbbbbbb", 2, 2),
            session("cccccccc", 5, 3),
        ]);
        assert_eq!(
            neighbour(&all, 2, Direction::Next).map(|s| s.state.num),
            Some(5)
        );
        assert_eq!(
            neighbour(&all, 5, Direction::Prev).map(|s| s.state.num),
            Some(2)
        );
    }

    #[test]
    fn stepping_from_a_number_that_is_gone_lands_on_the_nearest_one_left() {
        let all = numbered(&[
            session("aaaaaaaa", 1, 1),
            session("bbbbbbbb", 2, 2),
            session("dddddddd", 4, 3),
        ]);
        // 3 was killed underneath us.
        assert_eq!(
            neighbour(&all, 3, Direction::Next).map(|s| s.state.num),
            Some(4)
        );
        assert_eq!(
            neighbour(&all, 3, Direction::Prev).map(|s| s.state.num),
            Some(2)
        );
        // Past either end it still wraps.
        assert_eq!(
            neighbour(&all, 9, Direction::Next).map(|s| s.state.num),
            Some(1)
        );
        assert_eq!(
            neighbour(&all, 0, Direction::Prev).map(|s| s.state.num),
            Some(4)
        );
    }

    #[test]
    fn an_unnumbered_session_is_not_somewhere_to_land() {
        let mut orphan = session("zzzzzzzz", 0, 9);
        orphan.state.num = 0;
        let mut one = session("aaaaaaaa", 1, 1);
        one.state.num = 1;
        let all = vec![one, orphan];
        assert_eq!(
            neighbour(&all, 1, Direction::Next).map(|s| s.id.clone()),
            Some("aaaaaaaa".to_string())
        );
    }

    #[test]
    fn stepping_with_one_session_stays_put_and_with_none_goes_nowhere() {
        let one = finish_listing(vec![session("aaaaaaaa", 1, 1)]);
        for dir in [Direction::Next, Direction::Prev] {
            assert_eq!(neighbour(&one, 1, dir).map(|s| s.state.num), Some(1));
            assert!(neighbour(&[], 1, dir).is_none());
        }
    }

    #[test]
    fn only_a_signallable_pid_is_passed_to_the_kill_script() {
        assert_eq!(pid_arg(0), "");
        assert_eq!(pid_arg(1), "");
        assert_eq!(pid_arg(4242), "4242");
        assert_eq!(opt_pid_arg(None), "");
        assert_eq!(opt_pid_arg(Some(1)), "");
        assert_eq!(opt_pid_arg(Some(4242)), "4242");
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

    fn session(id: &str, num: u32, created: u64) -> Session {
        let mut s = Session::new(id.to_string(), format!("name-{id}"), 0, num);
        s.created = created;
        s
    }

    /// Displayed numbers equal to stored ones, for `neighbour` on any slice.
    fn numbered(sessions: &[Session]) -> Vec<Session> {
        sessions
            .iter()
            .map(|s| {
                let mut s = s.clone();
                s.state.num = s.num;
                s
            })
            .collect()
    }

    fn numbers(sessions: &[Session]) -> Vec<u32> {
        sessions.iter().map(|s| s.state.num).collect()
    }

    fn resolved(sessions: Vec<Session>) -> Vec<(String, u32)> {
        finish_listing(sessions)
            .into_iter()
            .map(|s| (s.id, s.state.num))
            .collect()
    }

    #[test]
    fn the_column_is_dense_however_gapped_the_ranks_are() {
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
                ("ccc".to_string(), 3),
            ],
            "ranks order the list; the numbers are positions in it"
        );
    }

    #[test]
    fn killing_a_session_renumbers_the_ones_below_it() {
        let all = vec![
            session("aaa", 1, 1),
            session("bbb", 2, 2),
            session("ccc", 3, 3),
        ];
        assert_eq!(numbers(&finish_listing(all.clone())), vec![1, 2, 3]);

        let survivors: Vec<Session> = all.into_iter().filter(|s| s.id != "bbb").collect();
        let listed = finish_listing(survivors);
        assert_eq!(
            listed
                .iter()
                .map(|s| (s.id.as_str(), s.state.num))
                .collect::<Vec<_>>(),
            vec![("aaa", 1), ("ccc", 2)],
            "the third session is now the second"
        );
        assert_eq!(
            listed.iter().map(|s| s.num).collect::<Vec<_>>(),
            vec![1, 3],
            "and nothing on disk moved to make that true"
        );
    }

    #[test]
    fn unranked_sessions_sort_last_and_are_still_numbered() {
        let out = resolved(vec![
            session("aaa", 0, 1),
            session("bbb", 2, 2),
            session("ccc", 0, 3),
        ]);
        // Only "bbb" has a rank, so it leads; the unranked follow oldest first.
        assert_eq!(
            out,
            vec![
                ("bbb".to_string(), 1),
                ("aaa".to_string(), 2),
                ("ccc".to_string(), 3),
            ]
        );
    }

    #[test]
    fn duplicate_stored_numbers_are_broken_apart() {
        let out = resolved(vec![
            session("aaa", 1, 1),
            session("bbb", 1, 2),
            session("ccc", 1, 3),
        ]);
        let nums: Vec<u32> = out.iter().map(|(_, n)| *n).collect();
        assert_eq!(nums, vec![1, 2, 3]);
        assert_eq!(out[0].0, "aaa", "the earliest of them leads");
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
        assert!(finish_listing(vec![]).is_empty());
    }

    #[test]
    fn the_next_rank_starts_at_one_and_always_climbs() {
        assert_eq!(next_rank(&[]), 1, "the first session ranks 1");

        let listed = finish_listing(vec![session("aaa", 1, 1), session("bbb", 2, 2)]);
        assert_eq!(next_rank(&listed), 3, "one past the highest");

        // A hole left by killing the middle session is stepped over.
        let listed = finish_listing(vec![session("aaa", 1, 1), session("ccc", 3, 3)]);
        assert_eq!(next_rank(&listed), 4);
    }

    #[test]
    fn the_next_rank_reads_stored_ranks_and_not_the_column() {
        let listed = finish_listing(vec![session("aaa", 1, 1), session("ccc", 9, 3)]);
        assert_eq!(numbers(&listed), vec![1, 2], "two rows on screen");
        assert_eq!(next_rank(&listed), 10, "but the rank clears the highest");

        // Nothing has a rank: ranking starts over at 1.
        let listed = finish_listing(vec![session("aaa", 0, 1), session("bbb", 0, 2)]);
        assert_eq!(listed[0].num, 0, "still unranked on disk");
        assert_eq!(next_rank(&listed), 1);
    }

    /// With two sessions sharing a stored number, writing only the changed
    /// rows would leave a stale number colliding with a fresh one.
    #[test]
    fn the_order_a_reorder_wrote_is_the_order_the_next_listing_reads() {
        // "bbb" and "ccc" both store 2, and "ddd" has no number.
        let listed = finish_listing(vec![
            session("aaa", 1, 1),
            session("bbb", 2, 2),
            session("ccc", 2, 3),
            session("ddd", 0, 4),
        ]);
        let shown: Vec<(String, u32)> =
            listed.iter().map(|s| (s.id.clone(), s.state.num)).collect();
        assert_eq!(
            shown,
            vec![
                ("aaa".to_string(), 1),
                ("bbb".to_string(), 2),
                ("ccc".to_string(), 3),
                ("ddd".to_string(), 4),
            ]
        );

        // The whole arrangement, "aaa" dragged to the end; each row takes its
        // new position's number.
        let arranged = ["bbb", "ccc", "ddd", "aaa"];
        let written: Vec<Session> = arranged
            .iter()
            .zip(shown.iter().map(|(_, n)| *n))
            .map(|(id, num)| session(id, num, 0))
            .collect();

        let back: Vec<String> = finish_listing(written).into_iter().map(|s| s.id).collect();
        assert_eq!(back, arranged, "the arrangement did not survive a listing");
    }

    #[test]
    fn a_claim_waits_for_another_threads_run() {
        let slot = Mutex::new(None);
        let held = claim_shell(&slot).expect("the first claim");
        std::thread::scope(|s| {
            let waiting = s.spawn(|| claim_shell(&slot).map(|_| ()));
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                !waiting.is_finished(),
                "the second thread did not wait for the first"
            );
            drop(held);
            waiting
                .join()
                .expect("the second thread")
                .expect("refused once the first had let go");
        });
    }

    #[test]
    fn a_claim_inside_a_claim_is_refused() {
        let slot = Mutex::new(None);
        let held = claim_shell(&slot).expect("the first claim");
        assert!(claim_shell(&slot).is_err(), "claimed twice by one thread");
        drop(held);
        assert!(claim_shell(&slot).is_ok(), "not had again once let go");
    }
}
