//! Sessions on this machine. No SSH anywhere in this file.

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use crate::error::{NvmuxError, Result, SessionError};
use crate::launch::Launch;
use crate::paths::{self, SessionPaths};
use crate::proc;
use crate::rpc;
use crate::session::{Liveness, Session};
use crate::shell;
use crate::transport::{
    finish_listing, install_detach_alias, kill_outcome, opt_pid_arg, pid_arg, plan_create,
    plan_rename, protocol, wait_until_reachable, Location, Transport, REACHABLE_TIMEOUT,
};

pub struct LocalTransport {
    location: Location,
    dir: PathBuf,
    /// `dir` as the scripts take it. Built once; every script run needs it.
    dir_arg: String,
    /// This user's home directory, or empty if `$HOME` is unset or relative.
    /// The local answer to the question `scripts/hello.sh` asks a remote host.
    home: String,
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
            dir_arg: dir.to_string_lossy().into_owned(),
            location: Location::Local,
            dir,
            home: home_dir(),
        })
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

    /// Parse `<id>.json`, or `None` when there is not one.
    ///
    /// A missing file is not an error here: it is how both a session reaped
    /// since the listing and an orphan — a live socket whose metadata was never
    /// written — present themselves, and the two callers want to say different
    /// things about that.
    fn read_meta(&self, json: &Path) -> Result<Option<Session>> {
        let bytes = match std::fs::read(json) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(NvmuxError::Io(e)),
        };
        Ok(Some(Session::from_json(&bytes, json)?))
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
        let out = proc::run_local(shell::LIST_SCRIPT, &[&self.dir_arg])?;
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

        Ok(finish_listing(alive))
    }

    fn home(&self) -> &str {
        &self.home
    }

    fn dir_source(&self) -> crate::dirs::DirSource {
        crate::dirs::DirSource::Local
    }

    fn create_session(&self, name: &str, launch: &Launch, directory: &str) -> Result<Session> {
        // The listing is also what the new session's number is allocated
        // from, so numbering costs no extra work here.
        let (id, num) = plan_create(&self.list_sessions()?, name)?;
        let paths = self.paths(&id)?;

        // The socket is substituted here rather than in the script: this side
        // already computed the path, and `SessionPaths::new` has bounded its
        // length, which the script has no way to do.
        let argv = launch.argv_for(&paths.sock.to_string_lossy());
        let mut args: Vec<&str> = vec![&self.dir_arg, &id, directory];
        args.extend(argv.iter().map(String::as_str));

        tracing::info!(%id, name, command = launch.line(), directory, "spawning session");
        let out = proc::run_local(shell::SPAWN_SCRIPT, &args)?;
        let spawned = protocol::parse_spawn(&out.stdout)?;

        if !spawned.socket_appeared || !wait_until_reachable(&paths.sock, REACHABLE_TIMEOUT) {
            // Clean up rather than leave a half-created session behind — but
            // through the kill script, which removes the files only once it
            // has seen the process go. Unlinking the socket here on the
            // strength of "it did not answer in time" would orphan a Neovim
            // that was merely slow to start. An empty pid is fine: the script
            // finds the process by its socket, the pid is only a hint.
            let tail = self.log_tail(&paths.log);
            let pid = opt_pid_arg(spawned.pid);
            match proc::run_local(shell::KILL_SCRIPT, &[&self.dir_arg, &id, &pid]) {
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

        let mut session = Session::new(id, name.to_string(), spawned.pid.unwrap_or(0), num)
            .launched_with(launch.line())
            .started_in(directory);
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

        // Straight to signals — SIGTERM first, inside the script, so nvim still
        // runs VimLeavePre, writes its ShaDa file and unlinks its own socket.
        // The recorded pid is only a starting guess: the script uses it only if
        // it still owns this session's socket, since pids get reused.
        let pid = pid_arg(s.pid);
        let out = proc::run_local(shell::KILL_SCRIPT, &[&self.dir_arg, &s.id, &pid])?;
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
        plan_rename(&self.list_sessions()?, new_name, &s.id)?;

        let paths = self.paths(&s.id)?;
        let Some(mut session) = self.read_meta(&paths.json)? else {
            // The session was killed, or reaped, between the picker listing it
            // and the rename being confirmed.
            return Err(NvmuxError::Session(SessionError::NotFound(s.name.clone())));
        };
        session.name = new_name.to_string();
        session.write_atomic(&paths.json)?;

        tracing::info!(id = %s.id, from = %s.name, to = new_name, "renamed");
        Ok(())
    }

    /// Read, edit, write — rather than writing the record the picker is holding.
    /// The file is right here, so a rename another nvmux landed between the
    /// listing and this edit is kept rather than clobbered.
    fn renumber(&self, sessions: &[Session]) -> Result<()> {
        for s in sessions {
            let paths = self.paths(&s.id)?;
            let Some(mut on_disk) = self.read_meta(&paths.json)? else {
                // No metadata to edit: an orphan, whose name the picker only
                // synthesised, or a session reaped since the listing. Writing
                // one would make a placeholder name real, and a vanished row
                // must not abort the rest of the arrangement.
                tracing::debug!(id = %s.id, "no metadata to renumber");
                continue;
            };
            on_disk.num = s.num;
            on_disk.write_atomic(&paths.json)?;
        }
        Ok(())
    }

    fn local_socket_for(&self, s: &Session) -> Result<PathBuf> {
        // Locally the identity function; the SSH implementation is where this
        // seam earns its keep.
        Ok(self.paths(&s.id)?.sock)
    }
}

/// This machine's home directory, as a session host would report it.
///
/// `$HOME` rather than the password database, matching what `scripts/hello.sh`
/// reads on a remote host and for the same reason: it is the answer the user's
/// own shell would give. Anything unset or relative is no answer at all, and an
/// empty string says so rather than standing in for one.
fn home_dir() -> String {
    match std::env::var_os("HOME") {
        Some(h) if Path::new(&h).is_absolute() => h.to_string_lossy().into_owned(),
        _ => String::new(),
    }
}
