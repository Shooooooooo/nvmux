//! Sessions on this machine. No SSH anywhere in this file.
//!
//! What is local about a local session: its socket is reachable as it is, its
//! liveness can be asked over that socket rather than taken from a script's
//! process-table lookup, and its metadata is a file right here — re-read and
//! edited in place, so a rename another nvmux landed in between is kept. The
//! rest is the shared bodies in [`crate::transport`].

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

    fn paths(&self, id: &str) -> Result<SessionPaths> {
        Ok(SessionPaths::new(&self.dir, id)?)
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

impl Host for LocalTransport {
    fn location(&self) -> &Location {
        &self.location
    }

    fn dir(&self) -> &str {
        &self.dir_arg
    }

    /// Run one of the shared scripts in this machine's shell.
    ///
    /// A script that fails is an `Ok` with a status, which each caller reads
    /// its own way. `Err` is the shell itself: gone mid-run, or not startable.
    /// A shell found dead is replaced *before* a run and never after one — a
    /// script that was half way through must not run twice — so the caller
    /// that hit the death sees it, and the next one gets a fresh shell.
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

    /// The script's liveness hint is only a hint; the socket is right here,
    /// so it is replaced with the real answer, and a session found dead is
    /// reaped on the spot.
    fn settle(&self, listed: Vec<Session>) -> Result<Vec<Session>> {
        let t_probe = std::time::Instant::now();
        let probed = listed.len();
        let mut alive = Vec::with_capacity(listed.len());
        for mut s in listed {
            let paths = self.paths(&s.id)?;
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
        Ok(alive)
    }

    /// `SessionPaths::new` is what bounds the length.
    fn host_sock(&self, id: &str) -> Result<PathBuf> {
        Ok(self.paths(id)?.sock)
    }

    /// The identity function: the socket is already here.
    fn reach(&self, _id: &str, host_sock: &Path) -> Result<PathBuf> {
        Ok(host_sock.to_path_buf())
    }

    /// A socket the script could not remove would resurrect the session in
    /// the next listing.
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

    /// The last few lines of a session's log, included in the error because it
    /// is the first thing anyone wants when a session will not start.
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
}
