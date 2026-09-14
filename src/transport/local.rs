//! Sessions on this machine. No SSH anywhere in this file.

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, TryLockError};

use crate::error::{NvmuxError, Result, SessionError};
use crate::launch::Launch;
use crate::paths::{self, SessionPaths};
use crate::proc::{self, Shell};
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
    /// The `sh` every script runs in. Started by the first script that needs
    /// it, so building a transport does no I/O, and replaced if it dies.
    shell: Mutex<Option<Shell>>,
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
            shell: Mutex::new(None),
        })
    }

    /// Run one of the shared scripts in this machine's shell.
    ///
    /// A script that fails is an `Ok` with a status, which each caller reads
    /// its own way. `Err` is the shell itself: gone mid-run, or not startable.
    /// A shell found dead is replaced *before* a run and never after one — a
    /// script that was half way through must not run twice — so the caller
    /// that hit the death sees it, and the next one gets a fresh shell.
    fn run(&self, script: &str, args: &[&str]) -> Result<proc::Output> {
        let mut slot = match self.shell.try_lock() {
            Ok(slot) => slot,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            // No transport method calls another while running a script, so
            // this is a bug being reported rather than a wait being refused.
            Err(TryLockError::WouldBlock) => {
                return Err(NvmuxError::Io(std::io::Error::other(
                    "the shell is already running a script",
                )));
            }
        };
        if slot.as_mut().is_none_or(|shell| !shell.is_alive()) {
            *slot = Some(Shell::start(&mut proc::sh_command(), "/bin/sh")?);
        }
        let shell = slot.as_mut().expect("just started");
        shell.run(script, args).map_err(|died| {
            *slot = None;
            NvmuxError::Io(std::io::Error::other(died))
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
        // Split, because the two halves have different fixes: the script is
        // a run in the shell, with a process-table read on a host without
        // /proc/net/unix, while the probes are round trips to editors that may
        // be busy. See the `timing:` records in `main` and `pty`.
        let t_script = std::time::Instant::now();
        let out = self.run(shell::LIST_SCRIPT, &[&self.dir_arg])?;
        tracing::debug!(
            ms = t_script.elapsed().as_secs_f64() * 1000.0,
            "timing: listing script"
        );
        if !out.ok() {
            tracing::warn!(status = out.status, stderr = %out.stderr, "list.sh failed");
        }
        let listed = protocol::rows_to_sessions(protocol::parse_listing(&out.stdout)?);

        let mut alive = Vec::with_capacity(listed.len());
        let t_probe = std::time::Instant::now();
        let probed = listed.len();
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

        tracing::debug!(
            ms = t_probe.elapsed().as_secs_f64() * 1000.0,
            sessions = probed,
            "timing: listing probes"
        );

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
        let out = self.run(shell::SPAWN_SCRIPT, &args)?;
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
            match self.run(shell::KILL_SCRIPT, &[&self.dir_arg, &id, &pid]) {
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
        let out = self.run(shell::KILL_SCRIPT, &[&self.dir_arg, &s.id, &pid])?;
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::DirBuilderExt;

    use super::*;

    fn transport(tag: &str) -> (LocalTransport, PathBuf) {
        let dir = std::env::temp_dir().join(format!("nvmux-local-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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

    /// One `sh` runs every script the transport sends — that is the whole
    /// point — and a script that fails does not cost it. A shell that died
    /// while idle is replaced by the next script, not reported forever.
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

        // A script that fails: spawn.sh with a program that is not there exits
        // on its error path, and the shell it ran in is still the shell.
        let hopeless = Launch::parse("nvmux-no-such-editor --listen {sock}").expect("parses");
        assert!(t.create_session("hopeless", &hopeless, "/").is_err());
        assert_eq!(shell_pid(&t), pid, "a failed script cost the shell");
        assert!(t.list_sessions().expect("list").is_empty());

        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while t
            .shell
            .lock()
            .expect("lock")
            .as_mut()
            .expect("held")
            .is_alive()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the shell did not die"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(t.list_sessions().expect("list").is_empty());
        assert_ne!(shell_pid(&t), pid, "the dead shell was not replaced");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
