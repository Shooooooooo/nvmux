//! Sessions on this machine. No SSH anywhere in this file.

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::{self, SessionPaths};
use crate::error::{NvmuxError, Result, RpcError, SessionError};
use crate::ids;
use crate::rpc;
use crate::session::{Liveness, Session};
use crate::shell;
use crate::transport::exec::{Executor, LocalExecutor};
use crate::transport::{finish_listing, protocol, Location, Transport};

/// How long to wait for a new session's socket to appear and accept a
/// connection.
///
/// This is a *reachability* budget, not a readiness one. It deliberately does
/// not wait for the editor to finish sourcing `init.lua`: a config that clones
/// plugins on first run, or runs a slow `system()` call at startup, can take far
/// longer than any timeout worth having here, and a session that is up but still
/// starting is a perfectly good session. Killing it because it was slow would
/// destroy work the user can see happening.
const REACHABLE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to let a session shut down gracefully after `qa!` before escalating.
///
/// Generous on purpose: this is where `VimLeavePre`, ShaDa writes and session
/// files happen. Cutting it short to feel responsive corrupts exit-time state,
/// which is the opposite of what a session manager is for.
const QUIT_TIMEOUT: Duration = Duration::from_secs(10);

const POLL_INTERVAL: Duration = Duration::from_millis(25);

pub struct LocalTransport {
    location: Location,
    dir: PathBuf,
    exec: LocalExecutor,
}

impl LocalTransport {
    pub fn new() -> Result<Self> {
        Self::with_dir(config::ensure_runtime_dir()?)
    }

    /// Use an explicit runtime directory instead of the default.
    ///
    /// Exists so integration tests can drive a real session lifecycle without
    /// touching (or racing) the user's actual sessions in `/tmp/nvmux-<uid>`.
    /// The same security check applies: a test directory is still a directory
    /// we are about to put a socket in.
    pub fn with_dir(dir: PathBuf) -> Result<Self> {
        config::ensure_dir_secure(&dir)?;
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

    /// Wait until the socket accepts a connection and a real Neovim answers.
    ///
    /// Deliberately *not* waiting for a deferred call. `nvim_get_api_info` is
    /// answered off the main loop, so it proves there is a Neovim behind the
    /// socket without requiring `init.lua` to have finished — which is the
    /// distinction that keeps a slow-starting session from being destroyed for
    /// being slow.
    fn wait_until_reachable(&self, sock: &Path, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(mut client) = rpc::Client::connect(sock, rpc::PROBE_TIMEOUT) {
                if client.api_info().is_ok() {
                    return true;
                }
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        false
    }

    /// The last few lines of a session's log, for error messages.
    ///
    /// This is the first thing anyone wants when a session will not start, so
    /// it is included in the error rather than left for the user to find.
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

    /// Whether a socket file is safe to delete as stale.
    ///
    /// Three conditions, all required:
    ///
    /// * the probe says nothing is listening,
    /// * `lstat` says the path really is a socket, and
    /// * we own it.
    ///
    /// The middle check is not paranoia. On `AF_UNIX`, `connect()` returns
    /// `ECONNREFUSED` for *any* non-listening inode — a regular file gives it,
    /// and so does a directory, where a naive unlink would hit `EISDIR`. Reaping
    /// on the errno alone would delete whatever happened to be sitting at that
    /// path.
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
            // The path does not exist at all, so there is nothing to reap.
            //
            // Note a dangling symlink does NOT arrive here: `symlink_metadata`
            // succeeds on one, and it is the `is_socket()` check above that
            // rejects it.
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

            // The script's `kill -0` result is only a hint. Locally the socket
            // is right here, so replace it with the real answer.
            s.state.liveness = rpc::probe(&paths.sock);

            match s.state.liveness {
                // Never reap a busy session. A session blocked in `:!make` is
                // reachable but cannot answer a deferred call, and deleting it
                // out from under someone mid-build is unforgivable.
                Liveness::Alive | Liveness::Busy => alive.push(s),
                Liveness::Dead => {
                    if self.is_reapable(&paths.sock) {
                        self.reap(&paths);
                    } else {
                        // Something is at that path that we will not delete.
                        // Hide it from the picker but leave it on disk.
                        tracing::debug!(id = %s.id, "dead session left in place");
                    }
                }
            }
        }

        finish_listing(alive)
    }

    fn create_session(&self, name: &str) -> Result<Session> {
        crate::session::validate_name(name)?;

        // Names are display metadata and carry no identity, but two sessions
        // with the same name in one picker is a usability trap, so it is
        // refused at the point of creation.
        if self
            .list_sessions()?
            .iter()
            .any(|s| s.name.eq_ignore_ascii_case(name))
        {
            return Err(SessionError::Exists(name.to_string()).into());
        }

        let id = ids::new_id().map_err(|e| std::io::Error::other(e.to_string()))?;
        let paths = self.paths(&id)?;
        let dir = self.dir.to_string_lossy().into_owned();

        tracing::info!(%id, name, "spawning session");
        let out = self.exec.run_script(shell::SPAWN_SCRIPT, &[&dir, &id])?;
        let spawned = protocol::parse_spawn(&out.stdout)?;

        if !spawned.socket_appeared || !self.wait_until_reachable(&paths.sock, REACHABLE_TIMEOUT) {
            // Clean up rather than leaving a half-created session lying around,
            // then report the log, which is what anyone debugging this needs.
            let tail = self.log_tail(&paths.log);
            if let Some(pid) = spawned.pid {
                let _ = self
                    .exec
                    .run_script(shell::KILL_SCRIPT, &[&dir, &id, &pid.to_string()]);
            }
            self.reap(&paths);
            return Err(SessionError::NotReady {
                name: name.to_string(),
                timeout: REACHABLE_TIMEOUT,
                log: paths.log.clone(),
                log_tail: tail,
            }
            .into());
        }

        let session = Session::new(id, name.to_string(), spawned.pid.unwrap_or(0));
        session.write_atomic(&paths.json)?;

        // Cheap, and it gives the user a way out that is not `:q`. `command!`
        // requires an uppercase name — `command! q` is E183 — so the alias
        // cannot shadow `:q` itself, which is why the README still has to warn
        // about it. Failure here is not worth failing the create over.
        if let Ok(mut client) = rpc::Client::connect(&paths.sock, rpc::CONNECT_TIMEOUT) {
            if let Err(e) = client.command("command! -bar Detach detach") {
                tracing::debug!(error = %e, "could not install the :Detach alias");
            }
        }

        tracing::info!(id = %session.id, name, pid = session.pid, "session ready");
        Ok(session)
    }

    fn kill_session(&self, s: &Session) -> Result<()> {
        let paths = self.paths(&s.id)?;
        let dir = self.dir.to_string_lossy().into_owned();

        // Unconditional. nvmux does not ask the session whether it has unsaved
        // buffers, so there is no state to consult and no way for the answer to
        // be wrong, stale, or unobtainable. To leave a session normally, switch
        // to it and `:q` — in a remote UI that ends the session, because the
        // editor *is* the session.
        //
        // Graceful first all the same: `qa!` lets nvim run VimLeavePre, write
        // its ShaDa file and unlink its own socket. That is about the editor
        // shutting down cleanly, not about consulting it.
        if let Ok(mut client) = rpc::Client::connect(&paths.sock, rpc::PROBE_TIMEOUT) {
            match client.command("qa!") {
                Ok(()) => {}
                // The server usually closes the socket without answering, so a
                // reset or a timeout here is success, not failure.
                Err(e) if e.is_definitely_dead() => {}
                Err(RpcError::Timeout(_)) => {}
                Err(e) => tracing::debug!(error = %e, "qa! did not answer cleanly"),
            }
        }

        // Give it a moment to go away on its own before escalating.
        let deadline = Instant::now() + QUIT_TIMEOUT;
        while Instant::now() < deadline {
            if rpc::probe(&paths.sock) == Liveness::Dead {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }

        // If the graceful path worked, we are done. Judge by whether the session
        // is actually gone rather than by whether a signal was delivered: `qa!`
        // never touches the pid, so a stale or recycled pid in the metadata says
        // nothing about whether the session shut down.
        let gone = rpc::probe(&paths.sock) == Liveness::Dead;
        if gone {
            if paths.sock.exists() && self.is_reapable(&paths.sock) {
                self.reap(&paths);
            } else {
                // nvim unlinks its own socket on a clean exit; sweep the rest.
                for p in [&paths.json, &paths.log] {
                    let _ = std::fs::remove_file(p);
                }
            }
            tracing::info!(id = %s.id, name = %s.name, "session exited");
            return Ok(());
        }

        // Still there, so escalate. The script only signals a pid it can confirm
        // still owns this session's socket, and it removes the files only once
        // the process is genuinely gone.
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

        // Reporting success regardless would be the worst outcome available
        // here: the session would vanish from the picker while its Neovim kept
        // running, with no socket left for nvmux to ever find, show, or kill.
        match protocol::parse_kill(&out.stdout)? {
            protocol::KillOutcome::Killed | protocol::KillOutcome::Absent => {
                if paths.sock.exists() && self.is_reapable(&paths.sock) {
                    self.reap(&paths);
                }
                tracing::info!(id = %s.id, name = %s.name, "session killed");
                Ok(())
            }
            protocol::KillOutcome::Orphaned => Err(SessionError::NotKilled {
                name: s.name.clone(),
                reason: "it did not exit, even after SIGKILL",
            }
            .into()),
            protocol::KillOutcome::Refused => Err(SessionError::NotKilled {
                name: s.name.clone(),
                reason: "it is still running and the recorded process id no longer \
                         belongs to it, so nothing was signalled",
            }
            .into()),
        }
    }

    fn rename_session(&self, s: &Session, new_name: &str) -> Result<()> {
        crate::session::validate_name(new_name)?;
        if self
            .list_sessions()?
            .iter()
            .any(|other| other.id != s.id && other.name.eq_ignore_ascii_case(new_name))
        {
            return Err(SessionError::Exists(new_name.to_string()).into());
        }

        let paths = self.paths(&s.id)?;

        // A metadata edit and nothing more. The socket is never renamed or
        // moved: its path is the session's identity, and the display name is
        // only data. Renaming the socket would mean re-listening on a second
        // address and re-pointing any SSH forward, for no benefit.
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
        // Locally this is the identity function: the session socket already is
        // a path on this machine. All the interesting work happens in the SSH
        // implementation, which is exactly why this is the seam.
        Ok(self.paths(&s.id)?.sock)
    }
}
