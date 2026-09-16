//! The local/remote seam.
//!
//! Everything above this module — the picker, the attach path — is written once
//! against [`Transport`] and does not know whether the sessions it is listing
//! live on this machine or on the far end of an SSH connection.
//!
//! Below it, most of the work is written once too. The scripts are the
//! protocol and [`crate::proc::Shell`] runs them the same way on either host,
//! so listing, creating and killing a session are one body each — [`list`],
//! [`create`], [`kill`] — over a [`Host`] that supplies what differs: how the
//! host is reached, how a session's liveness is settled, and where its
//! metadata lives. The differences that do not fold are kept in the open, as
//! each transport's own methods: [`Transport::local_socket_for`], which is the
//! seam that makes a remote session attachable at all, and the metadata edits
//! (`rename_session`, `renumber`), which the local transport does in place on
//! files it can re-read and the remote one does by writing the picker's record
//! back through a script.

pub mod local;
pub mod protocol;
pub mod remote;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{NvmuxError, Result, SessionError};
use crate::launch::Launch;
use crate::proc::Output;
use crate::rpc;
use crate::session::Session;
use crate::shell;

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

    /// Spawn a session running `launch`, whose `{sock}` becomes this session's
    /// socket — on this machine or on the far end, wherever the sessions live.
    ///
    /// `directory` is where it starts, absolute and on *this* host, as
    /// [`crate::session::validate_directory`] left it. Whether it exists is the
    /// spawn script's question and not asked here: it is the side that can
    /// answer it without a round trip and without a gap between the answer and
    /// the launch.
    fn create_session(&self, name: &str, launch: &Launch, directory: &str) -> Result<Session>;

    /// Terminate a session, unconditionally: nvmux never asks about unsaved
    /// buffers. The ordinary way out is `:q` in the session itself, which ends
    /// it because the editor *is* the session.
    fn kill_session(&self, s: &Session) -> Result<()>;

    /// Rename is a metadata edit and nothing more. The socket is never renamed
    /// or moved: its path is the session's stable identity and the display name
    /// is only data, so no SSH forward has to be rebuilt.
    fn rename_session(&self, s: &Session, new_name: &str) -> Result<()>;

    /// Persist the order the user arranged in the picker: write each of these
    /// records' [`Session::num`], and change nothing else.
    ///
    /// Whole records rather than `(id, number)` pairs, so there is one source of
    /// truth for the number a session should store. Locally only `id` and `num`
    /// are read — the file is right here, so it is re-read and edited, which
    /// keeps a rename another nvmux landed in between. Over ssh the record is
    /// written back whole, the same trade [`Transport::rename_session`] already
    /// makes and safe for the same reason: `list.sh` ships each session's entire
    /// `<id>.json`, so the picker's copy is complete, and `SessionState` is
    /// `#[serde(skip)]`, so no resolved number can reach disk.
    ///
    /// Batched. Over ssh this is one round trip whatever the list length, where
    /// a row dragged the length of a long list would otherwise cost one per row
    /// it passed — see [`crate::shell`].
    ///
    /// Not atomic across sessions: the files are written one at a time on the
    /// host that owns them. A failure part way is reported and the picker
    /// re-lists straight afterwards, so what is on screen is what is on disk;
    /// [`finish_listing`] resolves any half-written state into a valid,
    /// duplicate-free ordering by construction.
    ///
    /// A session with no metadata — an orphan, or one killed since the listing —
    /// is skipped rather than having a record conjured for it.
    fn renumber(&self, sessions: &[Session]) -> Result<()>;

    /// The home directory on the host that runs the sessions: what a new
    /// session starts in unless the user says otherwise, and what a leading `~`
    /// expands against.
    ///
    /// Empty only if the host could not say — over ssh, a `hello.sh` from a
    /// newer nvmux than the far end has ever seen. The prompt then offers no
    /// default rather than a wrong one.
    fn home(&self) -> &str;

    /// A handle for listing directories on this session's host, owned and
    /// `Send` so the create prompt's completion worker can hold one — see
    /// [`crate::dirs`], which explains why it cannot simply borrow this.
    fn dir_source(&self) -> crate::dirs::DirSource;

    /// A socket path on **this** machine that `nvim --server` can use.
    ///
    /// The single seam that makes remote sessions work: locally the session
    /// socket itself, over SSH the local end of a forward. Everything downstream
    /// is identical in both cases.
    fn local_socket_for(&self, s: &Session) -> Result<PathBuf>;

    /// Bring the link to the host back if it has gone, and say whether it had.
    ///
    /// Asked once a client has exited on its own, which over ssh is what a
    /// dropped connection looks like: the forward closes under the client and
    /// it leaves, exactly as it would have if the session had been quit. The
    /// link is what tells the two apart — a session that ended leaves the
    /// master standing — so the answer is not a bool but three: the link was
    /// fine ([`Reconnect::Unneeded`]) and the session is what went; it was
    /// down and is back ([`Reconnect::Restored`]), so the same session can be
    /// attached again; or it was down and stays down, which is the error.
    ///
    /// One attempt, bounded: this is the step [`crate::reconnect`] repeats, so
    /// it must not itself wait for the system's TCP timeout. Locally there is
    /// no link to lose, and the default says so.
    fn reconnect(&self) -> Result<Reconnect> {
        Ok(Reconnect::Unneeded)
    }
}

/// What [`Transport::reconnect`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconnect {
    /// The link never went: whatever exited, exited for its own reasons.
    Unneeded,
    /// The link had gone and is back. Every forward went with it, so a
    /// session is reached afresh — [`Transport::local_socket_for`] knows.
    Restored,
}

/// Build the transport for a location.
pub fn open(location: Location) -> Result<Box<dyn Transport>> {
    match location {
        Location::Local => Ok(Box::new(local::LocalTransport::new()?)),
        Location::Ssh(host) => Ok(Box::new(remote::SshTransport::new(host)?)),
    }
}

// --- the bodies both transports share ---------------------------------------

/// What a transport supplies to [`list`], [`create`] and [`kill`].
///
/// Everything here is about *where* the host is. The scripts run there, in a
/// shell of the host's, and speak the same protocol wherever they run; what
/// each transport knows is how a socket is spelled over there and reached from
/// here, what a listed session's liveness is worth, and where a session's
/// metadata is kept. The three defaults are the local answer, where a socket
/// needs no reaching and nothing is left to clean up but a file.
pub(crate) trait Host {
    fn location(&self) -> &Location;

    /// The runtime directory on the session host, as the scripts take it.
    fn dir(&self) -> &str;

    /// Run one of the shared scripts in the host's shell.
    ///
    /// A script that fails is an `Ok` with a status, which each body reads its
    /// own way; `Err` is the shell, or the link it runs over.
    fn run(&self, script: &str, args: &[&str]) -> Result<Output>;

    /// A listing already in hand, taken once. Over ssh the greeting carries
    /// one, so the picker's first listing costs no round trip.
    fn take_listing(&self) -> Option<String> {
        None
    }

    /// Settle what the listing script said about each session's liveness:
    /// which to keep, which to drop, and what to clean up on the way.
    fn settle(&self, listed: Vec<Session>) -> Result<Vec<Session>>;

    /// The socket path as the session host sees it — what `--listen` gets —
    /// checked against the path budget *before* anything is spawned: an
    /// overlong path makes Neovim silently truncate and bind somewhere that
    /// could never be reached.
    fn host_sock(&self, id: &str) -> Result<PathBuf>;

    /// Make a new session's socket reachable from this machine, and say where.
    /// Locally that is the socket itself; over ssh, a forward onto it.
    fn reach(&self, id: &str, host_sock: &Path) -> Result<PathBuf>;

    /// Undo [`Host::reach`] for a session that is going or gone. A session
    /// that comes back is reached again, so this is never wrong to call.
    fn unreach(&self, _id: &str) {}

    /// After the kill script has seen a session go: whatever it could not
    /// remove itself, which would otherwise resurrect the session in the next
    /// listing. Only the local transport has anything to do here.
    fn sweep(&self, _id: &str) {}

    /// Write a new session's metadata where the host keeps it.
    fn write_meta(&self, session: &Session) -> Result<()>;

    /// Where the session's log is, in the words of the error that names it.
    fn log_path(&self, id: &str) -> PathBuf;

    /// The last lines of that log, where it can be read from here.
    fn log_tail(&self, _id: &str) -> Option<String> {
        None
    }
}

/// Every session on the host, liveness settled, numbered and sorted.
pub(crate) fn list(h: &impl Host) -> Result<Vec<Session>> {
    let stdout = match h.take_listing() {
        Some(stdout) => stdout,
        None => {
            // Timed apart from the settling below because the two have
            // different fixes: the script is the host's process table and a
            // shell, while settling is round trips to editors that may be
            // busy. See the `timing:` records in `main` and `pty`.
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

/// Spawn a session and wait for it to answer; see [`Transport::create_session`].
pub(crate) fn create(
    h: &impl Host,
    name: &str,
    launch: &Launch,
    directory: &str,
) -> Result<Session> {
    // The listing is also what the new session's rank is allocated from, and
    // what its displayed number is read back out of once it exists, so
    // numbering costs no extra work here.
    let existing = list(h)?;
    let (id, rank) = plan_create(&existing, name)?;
    let host_sock = h.host_sock(&id)?;

    // The socket is substituted here rather than in the script: this side
    // already computed the path and bounded its length, which the script has
    // no way to do. Every word travels as its own argument.
    let argv = launch.argv_for(&host_sock.to_string_lossy());
    let mut args: Vec<&str> = vec![h.dir(), &id, directory];
    args.extend(argv.iter().map(String::as_str));

    tracing::info!(host = %h.location(), %id, name, command = launch.line(), directory, "spawning session");
    let out = h.run(shell::SPAWN_SCRIPT, &args)?;
    let spawned = protocol::parse_spawn(&out.stdout)?;

    // Every way out of a half-created session ends it through the kill
    // script, which removes the files only once it has seen the process go —
    // unlinking the socket here on the strength of "it did not answer in
    // time" would orphan a Neovim that was merely slow to start — and reports
    // why, naming the log on the host that has it. The log is read before the
    // kill, which removes it.
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

    // Over ssh this is the forward, and proves it works before the user tries
    // to attach; a forward that cannot be made leaves no session behind either.
    let local = h
        .reach(&id, &host_sock)
        .inspect_err(|_| discard(h, &id, spawned.pid))?;

    if !wait_until_reachable(&local, REACHABLE_TIMEOUT) {
        // A forward succeeding proves nothing on its own: `ssh -O forward` to
        // a nonexistent remote socket still exits 0 and creates a working
        // local socket. Only an answer through it does.
        return Err(not_ready("the session never answered".into()));
    }

    let mut session = Session::new(id, name.to_string(), spawned.pid.unwrap_or(0), rank)
        .launched_with(launch.line())
        .started_in(directory);
    h.write_meta(&session)?;
    // The caller attaches to this without re-listing, so give it the same
    // resolved number a listing would have. Numbers are positions now, so that
    // is a question only the whole list can answer: hand `finish_listing` the
    // listing this create was planned from with the new session in it, and
    // read back the place it takes.
    session.state.num = numbered_in(existing, &session);

    install_detach_alias(&local);

    tracing::info!(host = %h.location(), id = %session.id, name, pid = session.pid, "session ready");
    Ok(session)
}

/// The number `session` will show once it joins `existing`: its position in
/// the listing the two of them make.
///
/// Its own rank as the fallback, for the impossible case of a listing that
/// does not contain the session just put into it — a number that is at least
/// never zero, which is the one value [`crate::keys`] cannot dial.
fn numbered_in(existing: Vec<Session>, session: &Session) -> u32 {
    let mut listing = existing;
    listing.push(session.clone());
    finish_listing(listing)
        .iter()
        .find(|s| s.id == session.id)
        .map_or(session.num, |s| s.state.num)
}

/// End a session that never became one. An empty pid is fine: the script
/// finds the process by its socket, and the pid is only a hint.
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
    // Straight to signals — SIGTERM first, inside the script, so nvim still
    // runs VimLeavePre, writes its ShaDa file and unlinks its own socket. The
    // recorded pid is only a starting guess: the script uses it only if it
    // still owns this session's socket, since pids get reused.
    let pid = pid_arg(s.pid);
    let out = h.run(shell::KILL_SCRIPT, &[h.dir(), &s.id, &pid])?;
    if !out.ok() {
        tracing::warn!(status = out.status, stderr = %out.stderr, "kill.sh failed");
    }
    let outcome = protocol::parse_kill(&out.stdout)?;

    // Whatever the outcome: re-attaching would reach it again anyway.
    h.unreach(&s.id);

    // Files are removed only once the session is genuinely gone; see
    // `kill_outcome`.
    kill_outcome(outcome, &s.name)?;
    h.sweep(&s.id);
    tracing::info!(host = %h.location(), id = %s.id, name = %s.name, "session killed");
    Ok(())
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

/// Everything a create needs decided before anything is spawned: the name is
/// legal and free, the new session has an id and a rank.
///
/// A rank, not the number the picker will show — see [`finish_listing`] for
/// the difference and [`create`] for where the number comes from.
///
/// Pure, and shared by both transports. The listing it works from is **not**
/// taken here: fetching one is where local and remote differ (one reaps dead
/// sessions, the other sweeps orphaned forwards), so each caller passes its own.
pub(crate) fn plan_create(existing: &[Session], name: &str) -> Result<(String, u32)> {
    crate::session::validate_name(name)?;
    ensure_name_free(existing, name, None)?;
    Ok((crate::ids::new_id()?, next_rank(existing)))
}

/// The same checks for a rename, which allocates nothing.
pub(crate) fn plan_rename(existing: &[Session], new_name: &str, id: &str) -> Result<()> {
    crate::session::validate_name(new_name)?;
    ensure_name_free(existing, new_name, Some(id))
}

/// The `<pid>` argument `kill.sh` takes.
///
/// Empty unless the recorded pid is worth signalling: 0 is "unknown" and 1 is
/// init. The script finds the process by its socket anyway, so the pid is only
/// ever a hint, and a wrong one must not become a signal to something else.
pub(crate) fn pid_arg(pid: u32) -> String {
    if pid > 1 {
        pid.to_string()
    } else {
        String::new()
    }
}

/// The same argument, from what `spawn.sh` reported.
pub(crate) fn opt_pid_arg(pid: Option<u32>) -> String {
    pid.map(pid_arg).unwrap_or_default()
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

/// Shared by both transports: put the sessions in order, then number them by
/// the position each one landed in.
///
/// The column is **dense and recalculated on every listing**. Kill the second
/// of three and the third becomes the second: there is no hole to explain, the
/// last row of a list of *n* is always *n*, and a session's number is just the
/// count of rows above it plus one.
///
/// That is a trade, and this is the side of it that costs: a number is no
/// longer a name a session keeps for life, because killing a row shifts every
/// row below it up one. What is stable instead is the *order*, and the stored
/// [`Session::num`] is what holds it — a **rank**, read here only to sort by
/// and never shown. Ranks need not be dense and their gaps never reach the
/// screen: a create takes one past the end ([`next_rank`]), a reorder writes
/// the arrangement back ([`Transport::renumber`]), and a kill leaves the
/// survivors' ranks exactly as they were, because nothing downstream reads a
/// rank as a number to display.
///
/// Ordering is by rank rather than by name because the number is what the user
/// reads off the screen and presses; a name sort would scramble the column and
/// make it noise. It also stops the list reshuffling on every rename.
///
/// Resolution still never writes to disk. The three things that arrive without
/// a usable rank — metadata written before numbering existed, orphans with no
/// metadata at all, and the two-clients-created-at-once duplicate — are settled
/// in memory by the sort below. A read path that wrote would cost an SSH round
/// trip per listing, and would persist a derived number as if it had been
/// assigned.
pub(crate) fn finish_listing(mut sessions: Vec<Session>) -> Vec<Session> {
    // Rank first, so sessions with a claim on a place in the list keep it;
    // `created` then `id` puts the rankless ones — and any pair sharing a rank
    // — in a stable order rather than whatever the directory happened to yield.
    sessions.sort_by(|a, b| {
        let key = |s: &Session| if s.num == 0 { u32::MAX } else { s.num };
        key(a)
            .cmp(&key(b))
            .then_with(|| a.created.cmp(&b.created))
            .then_with(|| a.id.cmp(&b.id))
    });

    // Position, not rank. Every session gets a number, they start at 1, and no
    // two can collide however broken the ranks that arrived.
    for (i, s) in sessions.iter_mut().enumerate() {
        s.state.num = i as u32 + 1;
    }

    sessions
}

/// The rank to store for a session being created now: one past the highest
/// rank on the host, so it sorts after everything that already exists and the
/// new row appears at the end of the list.
///
/// Stored ranks, not displayed numbers — those are positions now, and the
/// number this session will show is whatever place it lands in once it joins
/// the listing. [`create`] reads that back out of [`finish_listing`] rather
/// than assuming it is the last one, because a session with no rank at all
/// (legacy metadata, an orphan) sorts after every ranked one and so keeps the
/// bottom of the list.
pub(crate) fn next_rank(existing: &[Session]) -> u32 {
    existing
        .iter()
        .map(|s| s.num)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

/// The session `<prefix> n` / `<prefix> p` moves to from the one numbered
/// `from`, wrapping at both ends.
///
/// By number rather than by position in the slice. The listing a caller hands
/// in happens to be sorted ([`finish_listing`]), but nothing here leans on
/// that, so the rule is the same whatever order it arrives in.
///
/// `from` is a pivot, not a member, and that is what makes the awkward case
/// right without a special arm: press `n` in a session someone killed from
/// another window and you land on the nearest number that still exists, rather
/// than nowhere or back at the start.
///
/// `None` only for an empty listing. With one session it names that session,
/// which the session loop then reuses the client for — nothing changes, and
/// nothing is announced.
pub fn neighbour(sessions: &[Session], from: u32, dir: crate::keys::Direction) -> Option<&Session> {
    use crate::keys::Direction;
    // Zero is "unnumbered", which `finish_listing` never leaves behind. Nobody
    // can be sitting on one or have typed one, so it is not somewhere to land.
    let numbered = || sessions.iter().filter(|s| s.state.num != 0);
    // On the id as well as the number, so a duplicate — which only a listing
    // built by hand can contain — still gives one deterministic answer.
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
        // A rename to its own name (or its own name in another case) is fine.
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

    /// A create takes a rank past the end, so the new row lands at the bottom
    /// of the list rather than in the first hole — which is the whole point of
    /// ranks being an ordering key and not the column on screen.
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

        // Killing the middle session leaves a hole in the *ranks*. The next
        // create must step over it rather than fill it, or the new session
        // would come up between the two survivors.
        let gapped: Vec<_> = all.into_iter().filter(|s| s.num != 2).collect();
        let (_, rank) = plan_create(&gapped, "fresh").expect("planned");
        assert_eq!(rank, 4, "the freed rank is not reused");
    }

    /// The number a create hands back, which the caller attaches to without
    /// re-listing, is the place the new session takes in the list — not its
    /// rank, and the two part company as soon as anything has been killed.
    #[test]
    fn a_new_session_is_numbered_by_where_it_lands() {
        let listed = finish_listing(vec![session("aaaaaaaa", 1, 1), session("cccccccc", 3, 3)]);
        assert_eq!(numbers(&listed), vec![1, 2], "the hole is already closed");

        let (_, rank) = plan_create(&listed, "fresh").expect("planned");
        let mut fresh = session("ffffffff", rank, 9);
        assert_eq!(numbered_in(listed, &fresh), 3, "it shows as the third row");

        // An orphan has no rank and keeps the bottom of the list, so the new
        // session is numbered above it rather than after it.
        let with_orphan =
            finish_listing(vec![session("aaaaaaaa", 1, 1), session("zzzzzzzz", 0, 2)]);
        fresh.num = next_rank(&with_orphan);
        assert_eq!(numbered_in(with_orphan, &fresh), 2);
    }

    /// `<prefix> n` and `<prefix> p` must round the list and come back, or the
    /// two keys would dead-end at either edge and the user would have to know a
    /// number after all.
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

    /// A listing has no gaps to cross any more, but `neighbour` must not be the
    /// place that assumes it: it takes whatever slice it is handed, and the
    /// numbers in one built by hand are nobody's promise. From 2 of {1, 2, 5}
    /// the next session is 5, not 3.
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

    /// A session killed from another window while this one was attached to it:
    /// the number the user is on is no longer in the listing. Stepping must
    /// still land somewhere, and the nearest number is a far better answer than
    /// the start of the list.
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
        // Past either end it still wraps rather than giving up.
        assert_eq!(
            neighbour(&all, 9, Direction::Next).map(|s| s.state.num),
            Some(1)
        );
        assert_eq!(
            neighbour(&all, 0, Direction::Prev).map(|s| s.state.num),
            Some(4)
        );
    }

    /// An unnumbered session is one `finish_listing` never produces, and not
    /// somewhere anyone could have typed their way to either.
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

    /// One session is its own neighbour, which the session loop then reuses as
    /// the client it already has: a no-op, not a reattach.
    #[test]
    fn stepping_with_one_session_stays_put_and_with_none_goes_nowhere() {
        let one = finish_listing(vec![session("aaaaaaaa", 1, 1)]);
        for dir in [Direction::Next, Direction::Prev] {
            assert_eq!(neighbour(&one, 1, dir).map(|s| s.state.num), Some(1));
            assert!(neighbour(&[], 1, dir).is_none());
        }
    }

    /// `kill.sh` finds the process by its socket; a pid of 0 or 1 is never a
    /// hint worth passing, and 1 would name init.
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

    /// `created` is what orders the unranked ones, so it is set explicitly.
    fn session(id: &str, num: u32, created: u64) -> Session {
        let mut s = Session::new(id.to_string(), format!("name-{id}"), 0, num);
        s.created = created;
        s
    }

    /// A listing whose displayed numbers are the stored ones, which
    /// `finish_listing` no longer produces. For the cases that are about what
    /// `neighbour` does with an arbitrary slice rather than about resolution.
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

    /// The change this whole model exists for: the stored ranks order the list
    /// and then stop mattering, so the column is 1, 2, 3 and not 1, 2, 5.
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

    /// Kill the second of three and the third becomes the second, with nothing
    /// written to disk: the survivors' ranks are untouched, and only the
    /// listing they are resolved into changed.
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

    /// Metadata written before numbering existed, and orphans, have no stored
    /// rank. They must still be reachable by keystroke.
    #[test]
    fn unranked_sessions_sort_last_and_are_still_numbered() {
        let out = resolved(vec![
            session("aaa", 0, 1),
            session("bbb", 2, 2),
            session("ccc", 0, 3),
        ]);
        // "bbb" is the only one with a rank, so it heads the list; the unranked
        // pair follow it oldest first, and all three get a number.
        assert_eq!(
            out,
            vec![
                ("bbb".to_string(), 1),
                ("aaa".to_string(), 2),
                ("ccc".to_string(), 3),
            ]
        );
    }

    /// Two clients creating at the same moment can store the same rank. Both
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

        // What killing the middle session leaves behind: a hole in the ranks
        // that the next create steps over rather than fills.
        let listed = finish_listing(vec![session("aaa", 1, 1), session("ccc", 3, 3)]);
        assert_eq!(next_rank(&listed), 4);
    }

    /// It reads the *stored* rank, not the number on screen — which is exactly
    /// the pair that come apart after a kill. Ranking a new session by the
    /// number of rows would collide with a survivor and put the two in
    /// `created` order, which is not necessarily the end of the list.
    #[test]
    fn the_next_rank_reads_stored_ranks_and_not_the_column() {
        let listed = finish_listing(vec![session("aaa", 1, 1), session("ccc", 9, 3)]);
        assert_eq!(numbers(&listed), vec![1, 2], "two rows on screen");
        assert_eq!(next_rank(&listed), 10, "but the rank clears the highest");

        // Nothing has a rank at all: ranking starts over at 1.
        let listed = finish_listing(vec![session("aaa", 0, 1), session("bbb", 0, 2)]);
        assert_eq!(listed[0].num, 0, "still unranked on disk");
        assert_eq!(next_rank(&listed), 1);
    }

    /// What a reorder writes is what the next listing reads back. Reordering is
    /// only worth anything if this holds, and it is the round trip nothing else
    /// covers: the picker arranges `state.num`, the transport stores those as
    /// `num`, and `finish_listing` has to resolve them to the same order again.
    ///
    /// The fixture is deliberately the hard one — two sessions storing the
    /// *same* number, which `duplicate_stored_numbers_are_broken_apart` says is
    /// a real condition. A reorder that wrote only the rows whose displayed
    /// number changed would leave one of these two storing a stale number that
    /// collides with a freshly written one, and this is where that shows up.
    #[test]
    fn the_order_a_reorder_wrote_is_the_order_the_next_listing_reads() {
        // As listed: "bbb" and "ccc" both claim 2, so one of them is showing a
        // number it does not store, and "ddd" has none at all.
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

        // The picker sends the whole arrangement, with "aaa" dragged to the end:
        // each row keeps the number of the position it now sits in.
        let arranged = ["bbb", "ccc", "ddd", "aaa"];
        let written: Vec<Session> = arranged
            .iter()
            .zip(shown.iter().map(|(_, n)| *n))
            .map(|(id, num)| session(id, num, 0))
            .collect();

        let back: Vec<String> = finish_listing(written).into_iter().map(|s| s.id).collect();
        assert_eq!(back, arranged, "the arrangement did not survive a listing");
    }
}
