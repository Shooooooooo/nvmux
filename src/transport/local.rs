//! Sessions on this machine. No SSH anywhere in this file.

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use crate::error::{NvmuxError, Result, SessionError};
use crate::ids;
use crate::paths::{self, SessionPaths};
use crate::rpc;
use crate::session::{Liveness, Session};
use crate::shell;
use crate::transport::exec::{Executor, LocalExecutor};
use crate::transport::{
    ensure_name_free, finish_listing, install_detach_alias, kill_outcome, protocol,
    wait_until_reachable, Location, Transport, REACHABLE_TIMEOUT,
};

pub struct LocalTransport {
    location: Location,
    dir: PathBuf,
    exec: LocalExecutor,
}

impl LocalTransport {
    pub fn new() -> Result<Self> {
        Self::with_dir(paths::ensure_runtime_dir()?)
    }

    /// Use an explicit runtime directory instead of the default, so integration
    /// tests do not touch the user's real sessions. The same security check
    /// applies.
    pub fn with_dir(dir: PathBuf) -> Result<Self> {
        paths::ensure_dir_secure(&dir)?;
        Ok(Self {
            location: Location::Local,
            dir,
            exec: LocalExecutor,
        })
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.dir
    }

    fn paths(&self, id: &str) -> Result<SessionPaths> {
        Ok(SessionPaths::new(&self.dir, id)?)
    }

    /// The last few lines of a session's log, included in the error because it
    /// is the first thing anyone wants when a session will not start.
    fn log_tail(&self, log: &Path) -> String {
        let Ok(body) = std::fs::read_to_string(log) else {
            return "(no log file)".to_string();
        };
        let lines: Vec<&str> = body.lines().collect();
        let start = lines.len().saturating_sub(15);
        if lines.is_empty() {
            "(log is empty)".to_string()
        } else {
            lines[start..].join("\n")
        }
    }

    /// Whether a socket file is safe to delete as stale: nothing listening,
    /// `lstat` says it really is a socket, and we own it.
    ///
    /// The middle check is not paranoia. On `AF_UNIX`, `connect()` returns
    /// `ECONNREFUSED` for *any* non-listening inode — a regular file does, and
    /// so does a directory — so reaping on the errno alone would delete whatever
    /// happened to be sitting at that path.
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
            // Nothing to reap. A dangling symlink does NOT arrive here:
            // `symlink_metadata` succeeds on one, and the `is_socket()` check
            // above is what rejects it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => {
                tracing::warn!(path = %sock.display(), error = %e, "refusing to reap");
                false
            }
        }
    }

    /// Delete a dead session's files.
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

impl Transport for LocalTransport {
    fn location(&self) -> &Location {
        &self.location
    }

    fn list_sessions(&self) -> Result<Vec<Session>> {
        let dir = self.dir.to_string_lossy().into_owned();
        let out = self.exec.run_script(shell::LIST_SCRIPT, &[&dir])?;
        if !out.ok() {
            tracing::warn!(status = out.status, stderr = %out.stderr, "list.sh failed");
        }
        let listed = protocol::rows_to_sessions(protocol::parse_listing(&out.stdout)?);

        let mut alive = Vec::with_capacity(listed.len());
        for mut s in listed {
            let paths = self.paths(&s.id)?;

            // The script's `kill -0` result is only a hint; the socket is right
            // here, so replace it with the real answer.
            s.state.liveness = rpc::probe(&paths.sock);

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

        finish_listing(alive)
    }

    fn create_session(&self, name: &str) -> Result<Session> {
        crate::session::validate_name(name)?;

        // The listing is also what the new session's number is allocated
        // from, so numbering costs no extra work here.
        let existing = self.list_sessions()?;
        ensure_name_free(&existing, name, None)?;
        let num = crate::transport::next_free_num(&existing);

        let id = ids::new_id().map_err(|e| std::io::Error::other(e.to_string()))?;
        let paths = self.paths(&id)?;
        let dir = self.dir.to_string_lossy().into_owned();

        tracing::info!(%id, name, "spawning session");
        let out = self.exec.run_script(shell::SPAWN_SCRIPT, &[&dir, &id])?;
        let spawned = protocol::parse_spawn(&out.stdout)?;

        if !spawned.socket_appeared || !wait_until_reachable(&paths.sock, REACHABLE_TIMEOUT) {
            // Clean up rather than leave a half-created session behind — but
            // through the kill script, which removes the files only once it
            // has seen the process go. Unlinking the socket here on the
            // strength of "it did not answer in time" would orphan a Neovim
            // that was merely slow to start. An empty pid is fine: the script
            // finds the process by its socket, the pid is only a hint.
            let tail = self.log_tail(&paths.log);
            let pid = spawned.pid.map(|p| p.to_string()).unwrap_or_default();
            match self.exec.run_script(shell::KILL_SCRIPT, &[&dir, &id, &pid]) {
                Ok(out) => match protocol::parse_kill(&out.stdout) {
                    Ok(outcome) => tracing::debug!(%id, ?outcome, "cleaned up a failed create"),
                    Err(e) => tracing::warn!(%id, error = %e, "cleanup after a failed create"),
                },
                Err(e) => tracing::warn!(%id, error = %e, "cleanup after a failed create"),
            }
            return Err(SessionError::NotReady {
                name: name.to_string(),
                timeout: REACHABLE_TIMEOUT,
                log: paths.log.clone(),
                log_tail: tail,
            }
            .into());
        }

        let mut session = Session::new(id, name.to_string(), spawned.pid.unwrap_or(0), num);
        session.write_atomic(&paths.json)?;
        // The caller attaches to this without re-listing, so give it the same
        // resolved number a listing would have.
        session.state.num = num;

        install_detach_alias(&paths.sock);

        tracing::info!(id = %session.id, name, pid = session.pid, "session ready");
        Ok(session)
    }

    fn kill_session(&self, s: &Session) -> Result<()> {
        let paths = self.paths(&s.id)?;
        let dir = self.dir.to_string_lossy().into_owned();

        // Straight to signals — SIGTERM first, inside the script, so nvim still
        // runs VimLeavePre, writes its ShaDa file and unlinks its own socket.
        // The recorded pid is only a starting guess: the script uses it only if
        // it still owns this session's socket, since pids get reused.
        let pid = if s.pid > 1 {
            s.pid.to_string()
        } else {
            String::new()
        };
        let out = self
            .exec
            .run_script(shell::KILL_SCRIPT, &[&dir, &s.id, &pid])?;
        if !out.ok() {
            tracing::warn!(status = out.status, stderr = %out.stderr, "kill.sh failed");
        }

        // Files are removed only once the session is genuinely gone; see
        // `kill_outcome`.
        kill_outcome(protocol::parse_kill(&out.stdout)?, &s.name)?;
        // A socket the script could not remove would resurrect the session in
        // the next listing.
        if paths.sock.exists() && self.is_reapable(&paths.sock) {
            self.reap(&paths);
        }
        tracing::info!(id = %s.id, name = %s.name, "session killed");
        Ok(())
    }

    fn rename_session(&self, s: &Session, new_name: &str) -> Result<()> {
        crate::session::validate_name(new_name)?;
        ensure_name_free(&self.list_sessions()?, new_name, Some(&s.id))?;

        let paths = self.paths(&s.id)?;

        let bytes = std::fs::read(&paths.json).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                // The session was killed, or reaped, between the picker
                // listing it and the rename being confirmed.
                NvmuxError::Session(SessionError::NotFound(s.name.clone()))
            } else {
                NvmuxError::Io(e)
            }
        })?;
        let mut session = Session::from_json(&bytes, &paths.json)?;
        session.name = new_name.to_string();
        session.write_atomic(&paths.json)?;

        tracing::info!(id = %s.id, from = %s.name, to = new_name, "renamed");
        Ok(())
    }

    fn local_socket_for(&self, s: &Session) -> Result<PathBuf> {
        // Locally the identity function; the SSH implementation is where this
        // seam earns its keep.
        Ok(self.paths(&s.id)?.sock)
    }
}
