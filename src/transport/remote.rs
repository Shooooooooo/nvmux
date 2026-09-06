//! Sessions on another host, reached over SSH.
//!
//! Spawning, listing and killing are shared with the local transport — the same
//! scripts, the same parsing. Only how a script gets run and how a socket
//! becomes reachable differ, which is what
//! [`crate::transport::exec::Executor`] and [`Transport::local_socket_for`] are
//! for.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::error::{NvimError, NvmuxError, Result, SessionError, SshError};
use crate::ids;
use crate::nvim;
use crate::paths;
use crate::session::{Liveness, Session};
use crate::shell;
use crate::ssh::Ssh;
use crate::transport::{
    ensure_name_free, finish_listing, install_detach_alias, kill_outcome, protocol,
    wait_until_reachable, Location, Transport, REACHABLE_TIMEOUT,
};

pub struct SshTransport {
    location: Location,
    ssh: Ssh,
    /// Short, stable, filename-safe token for this host. Namespaces the local
    /// end of every forward and the ControlPath, so two hosts holding sessions
    /// with the same id cannot collide on one local path.
    host_token: String,
    /// Our own runtime directory, which holds the forwarded sockets.
    local_dir: PathBuf,
    /// The runtime directory on the host that owns the nvim processes.
    remote_dir: String,
    /// Sessions whose sockets are already forwarded, so re-attaching does not
    /// pay for a redundant round trip.
    forwarded: Mutex<HashSet<String>>,
}

impl std::fmt::Debug for SshTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshTransport")
            .field("host", &self.ssh.host())
            .field("remote_dir", &self.remote_dir)
            .finish_non_exhaustive()
    }
}

impl SshTransport {
    pub fn new(host: String) -> Result<Self> {
        let host_token = ids::host_token(&host);
        let local_dir = paths::ensure_runtime_dir()?;
        let control_path = paths::control_path(&local_dir, &host_token)?;

        // Checked before connecting, so the error names the real problem.
        check_local_ssh()?;

        let ssh = Ssh::new(host.clone(), control_path);
        ssh.ensure_master()?;

        // One round trip for both the runtime directory and the remote Neovim
        // version.
        let out = ssh.run_script(shell::PROBE_SCRIPT, &[])?;
        if !out.ok() && out.stdout.trim().is_empty() {
            return Err(NvmuxError::Ssh(crate::ssh::classify(
                &host,
                out.status,
                &out.stderr,
            )));
        }
        let probe = protocol::parse_probe(&out.stdout)?;

        if probe.nvim_banner.is_empty() {
            return Err(NvimError::NotFound {
                where_: host.clone(),
                min: nvim::MIN_VERSION,
            }
            .into());
        }
        let version = nvim::parse_version(&probe.nvim_banner).ok_or_else(|| {
            NvimError::UnparsableVersion {
                where_: host.clone(),
                raw: probe.nvim_banner.clone(),
            }
        })?;
        if !version.is_supported() {
            return Err(NvimError::TooOld {
                where_: host.clone(),
                found: version.to_string(),
                min: nvim::MIN_VERSION,
            }
            .into());
        }
        tracing::info!(%host, %version, dir = %probe.runtime_dir, "remote host ready");

        Ok(Self {
            location: Location::Ssh(host),
            ssh,
            host_token,
            local_dir,
            remote_dir: probe.runtime_dir,
            forwarded: Mutex::new(HashSet::new()),
        })
    }

    /// The session's socket path **on the remote host**, validated against our
    /// own budget rather than ssh's: ssh checks `-L` endpoints against the
    /// *local* `sun_path` size, because it has not connected yet, so a local
    /// Linux talking to a remote macOS would accept a path that fails on the
    /// far side.
    fn remote_sock(&self, id: &str) -> Result<PathBuf> {
        if !ids::is_valid_id(id) {
            return Err(crate::error::PathError::MalformedId(id.to_string()).into());
        }
        let p = PathBuf::from(format!("{}/{}.sock", self.remote_dir, id));
        paths::check_sock_path(&p)?;
        Ok(p)
    }

    fn local_sock(&self, id: &str) -> Result<PathBuf> {
        Ok(paths::forwarded_sock(
            &self.local_dir,
            &self.host_token,
            id,
        )?)
    }

    fn host(&self) -> &str {
        self.ssh.host()
    }

    /// Remove local forwarded sockets whose session no longer exists. The name
    /// is `<host_token>-<id>.sock`, so only sessions on *this* host can match.
    ///
    /// Deliberately not called when a listing fails: "the host did not answer"
    /// must never be mistaken for "you have no sessions", or a transient ssh
    /// hiccup would tear down the forwards of live sessions.
    fn sweep_orphaned_forwards(&self, live: &[Session]) {
        let prefix = format!("{}-", self.host_token);
        let Ok(entries) = std::fs::read_dir(&self.local_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(id) = name
                .strip_prefix(&prefix)
                .and_then(|rest| rest.strip_suffix(".sock"))
            else {
                continue;
            };
            if live.iter().any(|s| s.id == id) {
                continue;
            }
            // Cancel before unlinking, or the master keeps the forward
            // registered and a later re-forward becomes a silent no-op that
            // never recreates the file.
            if let Ok(remote) = self.remote_sock(id) {
                self.ssh.cancel(&path, &remote);
            } else {
                let _ = std::fs::remove_file(&path);
            }
            self.forwarded.lock().map(|mut f| f.remove(id)).ok();
            tracing::debug!(%id, "removed an orphaned forward");
        }
    }

    /// Run one of the shared scripts on the remote host.
    ///
    /// A dead master is reported as such rather than as a mysterious failure,
    /// because it is the one condition the user can do something about.
    fn run_script(&self, script: &str, args: &[&str]) -> Result<crate::transport::exec::Output> {
        let out = self.ssh.run_script(script, args)?;
        if !out.ok() && out.stdout.trim().is_empty() {
            let err = crate::ssh::classify(self.host(), out.status, &out.stderr);
            if matches!(err, SshError::NoMaster(_) | SshError::MasterDied(_)) {
                // Report it, drop the forwards we believed in, and let the
                // caller fall back to the picker.
                self.forwarded.lock().map(|mut f| f.clear()).ok();
            }
            return Err(err.into());
        }
        Ok(out)
    }
}

impl Transport for SshTransport {
    fn location(&self) -> &Location {
        &self.location
    }

    fn list_sessions(&self) -> Result<Vec<Session>> {
        let out = self.run_script(shell::LIST_SCRIPT, &[&self.remote_dir])?;
        let mut sessions = protocol::rows_to_sessions(protocol::parse_listing(&out.stdout)?);

        // Liveness comes from the remote script, so drawing the picker needs no
        // forward per session. connect() through a forward would say nothing
        // anyway: ssh accepts first and resets afterwards.
        for s in &mut sessions {
            if s.state.liveness == Liveness::Dead {
                tracing::debug!(id = %s.id, "remote session reported not serving");
            }
        }
        sessions.retain(|s| s.state.liveness != Liveness::Dead);
        self.sweep_orphaned_forwards(&sessions);
        finish_listing(sessions)
    }

    fn create_session(&self, name: &str) -> Result<Session> {
        crate::session::validate_name(name)?;
        // The listing doubles as the source of the new session's number; see the
        // local transport.
        let existing = self.list_sessions()?;
        ensure_name_free(&existing, name, None)?;
        let num = crate::transport::next_free_num(&existing);

        let id = ids::new_id().map_err(|e| std::io::Error::other(e.to_string()))?;
        // Check the length before spawning, not after: an overlong path makes
        // Neovim silently truncate and bind somewhere we could never reach.
        let remote_sock = self.remote_sock(&id)?;
        self.local_sock(&id)?;

        tracing::info!(host = %self.host(), %id, name, "spawning remote session");
        let out = self.run_script(shell::SPAWN_SCRIPT, &[&self.remote_dir, &id])?;
        let spawned = protocol::parse_spawn(&out.stdout)?;

        if !spawned.socket_appeared {
            let _ = self.run_script(
                shell::KILL_SCRIPT,
                &[
                    &self.remote_dir,
                    &id,
                    &spawned.pid.map(|p| p.to_string()).unwrap_or_default(),
                ],
            );
            return Err(SessionError::NotReady {
                name: name.to_string(),
                timeout: REACHABLE_TIMEOUT,
                log: PathBuf::from(format!("{}:{}/{}.log", self.host(), self.remote_dir, id)),
                log_tail: out.stderr.trim().to_string(),
            }
            .into());
        }

        // Confirm through a forward, which also proves the forward works before
        // the user tries to attach.
        let local = self.local_sock(&id)?;
        self.ssh.forward(&local, &remote_sock)?;
        self.forwarded.lock().map(|mut f| f.insert(id.clone())).ok();

        if !wait_until_reachable(&local, REACHABLE_TIMEOUT) {
            // A forward succeeding proves nothing: `ssh -O forward` to a
            // nonexistent remote socket still exits 0 and creates a working
            // local socket.
            self.ssh.cancel(&local, &remote_sock);
            let _ = self.run_script(
                shell::KILL_SCRIPT,
                &[
                    &self.remote_dir,
                    &id,
                    &spawned.pid.map(|p| p.to_string()).unwrap_or_default(),
                ],
            );
            return Err(SessionError::NotReady {
                name: name.to_string(),
                timeout: REACHABLE_TIMEOUT,
                log: PathBuf::from(format!("{}:{}/{}.log", self.host(), self.remote_dir, id)),
                log_tail: "the session never answered through the forward".into(),
            }
            .into());
        }

        let mut session = Session::new(id.clone(), name.to_string(), spawned.pid.unwrap_or(0), num);
        let json = session.to_json()?;
        let out = self.run_script(shell::WRITE_META_SCRIPT, &[&self.remote_dir, &id, &json])?;
        protocol::parse_end(&out.stdout, "metadata write")?;
        // See the local transport: the caller attaches without re-listing.
        session.state.num = num;

        install_detach_alias(&local);

        tracing::info!(host = %self.host(), id = %session.id, name, "remote session ready");
        Ok(session)
    }

    fn kill_session(&self, s: &Session) -> Result<()> {
        let pid = if s.pid > 1 {
            s.pid.to_string()
        } else {
            String::new()
        };
        let out = self.run_script(shell::KILL_SCRIPT, &[&self.remote_dir, &s.id, &pid])?;
        let outcome = protocol::parse_kill(&out.stdout)?;

        // Drop the forward regardless: re-attaching would rebuild it anyway.
        if let (Ok(local), Ok(remote)) = (self.local_sock(&s.id), self.remote_sock(&s.id)) {
            self.ssh.cancel(&local, &remote);
        }
        self.forwarded.lock().map(|mut f| f.remove(&s.id)).ok();

        kill_outcome(outcome, &s.name)?;
        tracing::info!(host = %self.host(), id = %s.id, "remote session killed");
        Ok(())
    }

    /// Writes the record the picker holds, with the name changed, rather than
    /// re-reading the remote file first: that would be another round trip, and
    /// the only field a rename may change is the name. A session killed
    /// between the listing and the confirm gets its metadata rewritten, which
    /// the next listing's sweep removes again, since its socket is gone.
    fn rename_session(&self, s: &Session, new_name: &str) -> Result<()> {
        crate::session::validate_name(new_name)?;
        ensure_name_free(&self.list_sessions()?, new_name, Some(&s.id))?;

        let mut updated = s.clone();
        updated.name = new_name.to_string();
        let json = updated.to_json()?;
        let out = self.run_script(shell::WRITE_META_SCRIPT, &[&self.remote_dir, &s.id, &json])?;
        protocol::parse_end(&out.stdout, "metadata write")?;
        tracing::info!(host = %self.host(), id = %s.id, to = new_name, "renamed");
        Ok(())
    }

    fn local_socket_for(&self, s: &Session) -> Result<PathBuf> {
        let local = self.local_sock(&s.id)?;
        let remote = self.remote_sock(&s.id)?;

        let known = self
            .forwarded
            .lock()
            .map(|f| f.contains(&s.id))
            .unwrap_or(false);
        if known && local.exists() {
            return Ok(local);
        }

        // Checking first turns "the connection died while you were in the
        // picker" into a sentence rather than a forwarding failure.
        if !self.ssh.is_master_alive() {
            self.forwarded.lock().map(|mut f| f.clear()).ok();
            self.ssh.ensure_master()?;
        }

        self.ssh.forward(&local, &remote)?;
        self.forwarded
            .lock()
            .map(|mut f| f.insert(s.id.clone()))
            .ok();
        Ok(local)
    }
}

/// Refuse early if the local ssh cannot forward unix sockets at all.
fn check_local_ssh() -> Result<()> {
    let out = std::process::Command::new("ssh")
        .arg("-V")
        .output()
        .map_err(|_| SshError::NotFound)?;
    // `ssh -V` writes to stderr.
    let banner = if out.stderr.is_empty() {
        String::from_utf8_lossy(&out.stdout).into_owned()
    } else {
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    match nvim::parse_ssh_version(&banner) {
        Some(v) if v >= (6, 7) => Ok(()),
        Some((major, minor)) => Err(SshError::TooOld {
            found: format!("{major}.{minor}"),
            min: crate::ssh::MIN_SSH_VERSION,
        }
        .into()),
        // An unrecognised banner is not worth refusing over — plenty of forks
        // exist, and the forward itself will fail clearly enough.
        None => Ok(()),
    }
}
