//! Sessions on this machine. The socket is reachable directly, liveness is
//! probed over it (the script's process-table lookup only vetoes a reap), and
//! metadata is a local file edited in place. The rest is shared with
//! [`crate::transport`].

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{NvmuxError, Result, SessionError};
use crate::launch::Launch;
use crate::paths::{self, SessionPaths};
use crate::proc::{self, Shell};
use crate::rpc;
use crate::session::{Liveness, Session};
use crate::transport::{self, plan_rename, Host, Location, Transport};

pub struct LocalTransport {
    location: Location,
    dir: PathBuf,
    /// `dir` as the scripts take it.
    dir_arg: String,
    /// `$HOME`, or empty if unset or relative.
    home: String,
    /// The `sh` every script runs in. Started lazily, so building a transport
    /// does no I/O, and replaced if it dies.
    shell: Mutex<Option<Shell>>,
}

impl LocalTransport {
    pub fn new() -> Result<Self> {
        Self::with_dir(paths::ensure_runtime_dir()?)
    }

    /// Use an explicit runtime directory (for tests). The same security check
    /// applies.
    pub fn with_dir(dir: PathBuf) -> Result<Self> {
        paths::ensure_dir_secure(&dir)?;
        Ok(Self {
            dir_arg: dir.to_string_lossy().into_owned(),
            location: Location::Local,
            dir,
            home: home_dir(),
            shell: Mutex::new(None),
        })
    }

    fn paths(&self, id: &str) -> Result<SessionPaths> {
        Ok(SessionPaths::new(&self.dir, id)?)
    }

    /// Whether a stale socket file is safe to delete: `lstat` says it is a
    /// socket and we own it. On `AF_UNIX`, `connect()` returns `ECONNREFUSED`
    /// for any non-listening inode, files and directories included, so the
    /// errno alone is not enough.
    fn is_reapable(&self, sock: &Path) -> bool {
        match std::fs::symlink_metadata(sock) {
            Ok(meta) => {
                if !meta.file_type().is_socket() {
                    tracing::warn!(
                        path = %sock.display(),
                        "refusing to reap: not a socket"
                    );
                    return false;
                }
                if meta.uid() != nix::unistd::geteuid().as_raw() {
                    tracing::warn!(path = %sock.display(), "refusing to reap: not ours");
                    return false;
                }
                true
            }
            // A dangling symlink does not land here: `symlink_metadata`
            // succeeds on it and `is_socket()` rejects it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => {
                tracing::warn!(path = %sock.display(), error = %e, "refusing to reap");
                false
            }
        }
    }

    /// Parse `<id>.json`, or `None` if missing: a session reaped since the
    /// listing, or an orphan (live socket, no metadata). Not an error: each
    /// caller handles `None` its own way.
    fn read_meta(&self, json: &Path) -> Result<Option<Session>> {
        let bytes = match std::fs::read(json) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(NvmuxError::Io(e)),
        };
        Ok(Some(Session::from_json(&bytes, json)?))
    }

    fn reap(&self, paths: &SessionPaths) {
        tracing::info!(sock = %paths.sock.display(), "reaping dead session");
        for p in [&paths.sock, &paths.json, &paths.log] {
            if let Err(e) = std::fs::remove_file(p) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %p.display(), error = %e, "could not remove");
                }
            }
        }
    }
}

impl Host for LocalTransport {
    fn location(&self) -> &Location {
        &self.location
    }

    fn dir(&self) -> &str {
        &self.dir_arg
    }

    /// A failing script is `Ok` with its status; `Err` means the shell died
    /// mid-run or could not start. A dead shell is replaced before a run, never
    /// after, so a half-run script is not run twice.
    fn run(&self, script: &str, args: &[&str]) -> Result<proc::Output> {
        let mut slot = transport::claim_shell(&self.shell)?;
        if slot.as_mut().is_none_or(|shell| !shell.is_alive()) {
            *slot = Some(Shell::local()?);
        }
        let shell = slot.as_mut().expect("just started");
        shell.run(script, args).map_err(|died| {
            *slot = None;
            NvmuxError::Io(std::io::Error::other(died))
        })
    }

    /// Replaces the script's liveness hint with a probe over the socket, and
    /// reaps sessions found dead that the script did not see served. Probes
    /// run in parallel: a busy session uses its whole budget
    /// ([`rpc::QUICK_PROBE_TIMEOUT`]), so the listing waits one budget in
    /// total rather than one per session.
    fn settle(&self, listed: Vec<Session>) -> Result<Vec<Session>> {
        let t_probe = std::time::Instant::now();
        let budget = rpc::QUICK_PROBE_TIMEOUT;
        let probed = listed.len();
        let paths = listed
            .iter()
            .map(|s| self.paths(&s.id))
            .collect::<Result<Vec<_>>>()?;
        let found: Vec<Liveness> = std::thread::scope(|scope| {
            let probes: Vec<_> = paths
                .iter()
                .map(|p| scope.spawn(move || rpc::probe_within(&p.sock, budget)))
                .collect();
            // A panicked probe proves nothing; do not reap on it.
            probes
                .into_iter()
                .map(|p| p.join().unwrap_or(Liveness::Busy))
                .collect()
        });
        let mut alive = Vec::with_capacity(listed.len());
        for ((mut s, paths), liveness) in listed.into_iter().zip(paths).zip(found) {
            // macOS refuses a connect to a full accept queue, as a stalled
            // session's is, so a refusal does not outweigh a serving process.
            let served = s.state.liveness != Liveness::Dead;
            s.state.liveness = match liveness {
                Liveness::Dead if served => {
                    tracing::debug!(id = %s.id, "refused, but served; kept as busy");
                    Liveness::Busy
                }
                probed => probed,
            };
            match s.state.liveness {
                // Never reap a busy session: blocked in `:!make` it is
                // reachable but cannot answer a deferred call.
                Liveness::Alive | Liveness::Busy => alive.push(s),
                Liveness::Dead => {
                    if self.is_reapable(&paths.sock) {
                        self.reap(&paths);
                    } else {
                        // Not ours to delete: hide it, leave it on disk.
                        tracing::debug!(id = %s.id, "dead session left in place");
                    }
                }
            }
        }
        tracing::debug!(
            ms = t_probe.elapsed().as_secs_f64() * 1000.0,
            sessions = probed,
            "timing: listing probes"
        );
        Ok(alive)
    }

    /// `SessionPaths::new` bounds the length.
    fn host_sock(&self, id: &str) -> Result<PathBuf> {
        Ok(self.paths(id)?.sock)
    }

    fn reach(&self, _id: &str, host_sock: &Path) -> Result<PathBuf> {
        Ok(host_sock.to_path_buf())
    }

    fn sweep(&self, id: &str) {
        if let Ok(paths) = self.paths(id) {
            if paths.sock.exists() && self.is_reapable(&paths.sock) {
                self.reap(&paths);
            }
        }
    }

    fn write_meta(&self, session: &Session) -> Result<()> {
        Ok(session.write_atomic(&self.paths(&session.id)?.json)?)
    }

    fn log_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.log"))
    }

    /// The last 15 lines of a session's log, for start-up errors.
    fn log_tail(&self, id: &str) -> Option<String> {
        let Ok(body) = std::fs::read_to_string(self.log_path(id)) else {
            return Some("(no log file)".to_string());
        };
        let lines: Vec<&str> = body.lines().collect();
        let start = lines.len().saturating_sub(15);
        Some(if lines.is_empty() {
            "(log is empty)".to_string()
        } else {
            lines[start..].join("\n")
        })
    }
}

impl Transport for LocalTransport {
    fn location(&self) -> &Location {
        &self.location
    }

    fn list_sessions(&self) -> Result<Vec<Session>> {
        transport::list(self)
    }

    fn home(&self) -> &str {
        &self.home
    }

    fn dir_source(&self) -> crate::dirs::DirSource {
        crate::dirs::DirSource::Local
    }

    fn create_session(&self, name: &str, launch: &Launch, directory: &str) -> Result<Session> {
        transport::create(self, name, launch, directory)
    }

    fn kill_session(&self, s: &Session) -> Result<()> {
        transport::kill(self, s)
    }

    fn rename_session(&self, s: &Session, new_name: &str) -> Result<()> {
        plan_rename(&self.list_sessions()?, new_name, &s.id)?;

        let paths = self.paths(&s.id)?;
        let Some(mut session) = self.read_meta(&paths.json)? else {
            // Killed or reaped since the picker listed it.
            return Err(NvmuxError::Session(SessionError::NotFound(s.name.clone())));
        };
        session.name = new_name.to_string();
        session.write_atomic(&paths.json)?;

        tracing::info!(id = %s.id, from = %s.name, to = new_name, "renamed");
        Ok(())
    }

    /// Read, edit, write rather than writing the picker's copy, so a rename by
    /// another nvmux since the listing is kept.
    fn renumber(&self, sessions: &[Session]) -> Result<()> {
        for s in sessions {
            let paths = self.paths(&s.id)?;
            let Some(mut on_disk) = self.read_meta(&paths.json)? else {
                // An orphan (its name is a placeholder; writing would make it
                // real) or reaped since the listing. Skip it, do not abort.
                tracing::debug!(id = %s.id, "no metadata to renumber");
                continue;
            };
            on_disk.num = s.num;
            on_disk.write_atomic(&paths.json)?;
        }
        Ok(())
    }

    fn local_socket_for(&self, s: &Session) -> Result<PathBuf> {
        Ok(self.paths(&s.id)?.sock)
    }
}

/// `$HOME` rather than the password database, as `scripts/hello.sh` reads on
/// a remote host: it is what the user's shell would say. Empty if unset or
/// relative.
fn home_dir() -> String {
    match std::env::var_os("HOME") {
        Some(h) if Path::new(&h).is_absolute() => h.to_string_lossy().into_owned(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::DirBuilderExt;

    use super::*;

    fn transport(tag: &str) -> (LocalTransport, PathBuf) {
        let dir = crate::test_support::scratch_path(&format!("local-{tag}"));
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("runtime dir");
        (
            LocalTransport::with_dir(dir.clone()).expect("transport"),
            dir,
        )
    }

    fn shell_pid(t: &LocalTransport) -> u32 {
        t.shell
            .lock()
            .expect("lock")
            .as_ref()
            .expect("a shell")
            .pid()
    }

    /// macOS refuses a connect to a stalled session's full accept queue, so
    /// its probe reads Dead; reaping on that would orphan a running nvim.
    #[test]
    fn a_refused_session_the_script_saw_served_is_kept() {
        let (t, dir) = transport("refused");
        let row = |id: &str, liveness| {
            let paths = t.paths(id).expect("paths");
            crate::test_support::dead_socket(&paths.sock);
            std::fs::write(&paths.json, b"{}").expect("json");
            std::fs::write(&paths.log, b"").expect("log");
            let mut s = Session::new(id.into(), id.into(), 1, 1);
            s.state.liveness = liveness;
            (s, paths)
        };
        // As `rows_to_sessions` sets them from the script's process check.
        let (served, kept_paths) = row("served22", Liveness::Busy);
        let (unseen, reaped_paths) = row("unseen22", Liveness::Dead);

        let listed = t.settle(vec![served, unseen]).expect("settle");

        let ids: Vec<(&str, Liveness)> = listed
            .iter()
            .map(|s| (s.id.as_str(), s.state.liveness))
            .collect();
        assert_eq!(ids, [("served22", Liveness::Busy)]);
        for p in [&kept_paths.sock, &kept_paths.json, &kept_paths.log] {
            assert!(p.exists(), "{} was reaped", p.display());
        }
        assert!(
            !reaped_paths.sock.exists(),
            "the probe did not find it Dead"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failing script keeps the shell; one that died while idle is replaced
    /// by the next script, not reported forever.
    #[test]
    fn one_shell_runs_every_script_and_a_dead_one_is_replaced() {
        let (t, dir) = transport("shell");
        assert!(
            t.shell.lock().expect("lock").is_none(),
            "no I/O before the first script"
        );

        assert!(t.list_sessions().expect("list").is_empty());
        let pid = shell_pid(&t);
        for _ in 0..3 {
            assert!(t.list_sessions().expect("list").is_empty());
            assert_eq!(shell_pid(&t), pid, "a listing started a new shell");
        }

        // spawn.sh fails on a missing program; the shell must survive.
        let hopeless = Launch::parse("nvmux-no-such-editor --listen {sock}").expect("parses");
        assert!(t.create_session("hopeless", &hopeless, "/").is_err());
        assert_eq!(shell_pid(&t), pid, "a failed script cost the shell");
        assert!(t.list_sessions().expect("list").is_empty());

        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        assert!(
            crate::test_support::wait_until(std::time::Duration::from_secs(2), || !t
                .shell
                .lock()
                .expect("lock")
                .as_mut()
                .expect("held")
                .is_alive()),
            "the shell did not die"
        );
        assert!(t.list_sessions().expect("list").is_empty());
        assert_ne!(shell_pid(&t), pid, "the dead shell was not replaced");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Opened up after the transport checked it. The script's refusal must
    /// reach the picker, not read as no sessions.
    #[test]
    fn a_runtime_directory_others_can_reach_fails_the_listing_with_the_reason() {
        use std::os::unix::fs::PermissionsExt;

        let (t, dir) = transport("opened");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let listed = t.list_sessions();
        let _ = std::fs::remove_dir_all(&dir);

        let err = listed.expect_err("listed a directory other users can reach");
        assert!(
            err.to_string().contains("accessible to other users"),
            "{err}"
        );
    }
}
