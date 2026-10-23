//! Sessions on another host, reached over SSH.
//!
//! Spawning, listing and killing share the local transport's code in
//! [`crate::transport`]. What differs: the shell runs over a master connection
//! ([`Ssh::start_shell`]), remote sockets are reached through forwards on it
//! ([`Transport::local_socket_for`]), and liveness comes from the script,
//! since a forward cannot tell.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, TryLockError};

use crate::error::{NvimError, Result, SshError};
use crate::ids;
use crate::launch::Launch;
use crate::nvim;
use crate::paths;
use crate::proc::{Output, Shell};
use crate::session::{Liveness, Session};
use crate::shell;
use crate::ssh::{classify, Ssh};
use crate::transport::{self, plan_rename, protocol, Host, Location, Reconnect, Transport};

pub struct SshTransport {
    location: Location,
    ssh: Ssh,
    /// The host's shell, over the master. Replaced if it dies (as it does with
    /// its master) or right after a master is restored.
    shell: Mutex<Option<Shell>>,
    /// Namespaces the forwards' local ends and the ControlPath, so two hosts
    /// with the same session id cannot collide.
    host_token: String,
    local_dir: PathBuf,
    remote_dir: String,
    /// From the greeting, or empty.
    remote_home: String,
    forwarded: Mutex<HashSet<String>>,
    /// The greeting's listing, taken once by the first listing to save a
    /// script run. Nothing changes sessions in the milliseconds before that.
    first_listing: Mutex<Option<String>>,
}

/// Needed by `Result::expect_err` in the SSH tests; never logged.
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
        Self::with_dir(host, paths::ensure_runtime_dir()?)
    }

    /// Keeps the `ControlPath` and forwards in `local_dir`, so a test can kill
    /// one transport's master without disturbing others.
    pub fn with_dir(host: String, local_dir: PathBuf) -> Result<Self> {
        paths::ensure_dir_secure(&local_dir)?;
        let host_token = ids::host_token(&host);
        let control_path = paths::control_path(&local_dir, &host_token)?;

        // Judged first: an `ssh` too old for nvmux also fails on the master's
        // options, which would hide the real cause.
        let version = crate::ssh::check_local();

        let ssh = Ssh::new(host.clone(), control_path);
        // One round trip for the runtime directory, the Neovim version and the
        // sessions; as the shell's first script it also absorbs whatever the
        // login shell prints. It sweeps dead sessions even on a host the
        // version gate then refuses, which is harmless.
        let greeted = (|| {
            ssh.ensure_master()?;
            let mut shell = ssh.start_shell()?;
            let out = checked_script(&host, &mut shell, shell::HELLO_SCRIPT, &[])?;
            Ok::<_, SshError>((shell, out))
        })();
        version.verdict()?;
        let (shell, out) = greeted?;
        let probe = protocol::parse_probe(&out.stdout)?;

        // An empty banner means nvim is not on the host's PATH.
        if probe.nvim_banner.is_empty() {
            return Err(NvimError::NotFound {
                where_: host.clone(),
                min: nvim::MIN_VERSION,
            }
            .into());
        }
        let version = nvim::check_banner(&host, &probe.nvim_banner)?;
        tracing::info!(%host, %version, dir = %probe.runtime_dir, "remote host ready");

        Ok(Self {
            location: Location::Ssh(host),
            ssh,
            shell: Mutex::new(Some(shell)),
            host_token,
            local_dir,
            remote_dir: probe.runtime_dir,
            remote_home: probe.home,
            forwarded: Mutex::new(HashSet::new()),
            first_listing: Mutex::new(Some(out.stdout)),
        })
    }

    /// Checked against our own limit: ssh checks `-L` endpoints against the
    /// *local* `sun_path` size, which may exceed the remote one.
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

    /// Remove this host's forwarded sockets whose session is gone. Never call
    /// after a failed listing: a transient ssh error would tear down live
    /// forwards.
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
            // Cancel before unlinking, or a later re-forward is a silent no-op
            // that never recreates the file.
            if let Ok(remote) = self.remote_sock(id) {
                self.ssh.cancel(&path, &remote);
            } else {
                let _ = std::fs::remove_file(&path);
            }
            self.forwarded.lock().map(|mut f| f.remove(id)).ok();
            tracing::debug!(%id, "removed an orphaned forward");
        }
    }

    /// A dead shell is replaced before a run, never after (a half-run script
    /// must not run twice), and its master is restored first: `ssh` given a
    /// dead `ControlPath` silently opens a direct connection, and every later
    /// `-O forward` then fails.
    fn run_script(&self, script: &str, args: &[&str]) -> Result<Output> {
        let mut slot = transport::claim_shell(&self.shell)?;
        if slot.as_mut().is_none_or(|shell| !shell.is_alive()) {
            *slot = None;
            // Not `reconnect_if_needed`: its `try_lock` would find this lock
            // held and read it as a busy script over a healthy master.
            self.restore_master(None)?;
            *slot = Some(self.ssh.start_shell()?);
        }
        let shell = slot.as_mut().expect("just started");
        match checked_script(self.host(), shell, script, args) {
            Ok(out) => Ok(out),
            Err(err) => {
                // Drop a dead shell; keep one whose script merely failed.
                if slot.as_mut().is_some_and(|shell| !shell.is_alive()) {
                    *slot = None;
                }
                if matches!(err, SshError::NoMaster(_) | SshError::MasterDied(_)) {
                    self.forwarded.lock().map(|mut f| f.clear()).ok();
                }
                Err(err.into())
            }
        }
    }

    /// Restore the master if it died while idle, forgetting its forwards.
    ///
    /// A live shell implies a live master process, since shells are only
    /// started over a master just seen alive; asking the shell is a
    /// zero-timeout poll where `ssh -O check` is a fork, so this is cheap per
    /// attach. A reachable `ControlPath` is not implied; see [`Self::forward`].
    fn reconnect_if_needed(&self, connect_timeout: Option<u64>) -> Result<Reconnect> {
        // A locked slot has a script running, so the shell is alive.
        let vouched_for_by_a_shell = match self.shell.try_lock() {
            Ok(mut slot) => match slot.as_mut().map(|shell| shell.is_alive()) {
                Some(true) => true,
                Some(false) => {
                    *slot = None;
                    false
                }
                None => false,
            },
            Err(TryLockError::WouldBlock) => true,
            Err(TryLockError::Poisoned(_)) => false,
        };
        if vouched_for_by_a_shell {
            return Ok(Reconnect::Unneeded);
        }
        self.restore_master(connect_timeout)
    }

    /// Ask ssh whether the master is alive (a fork) and restore it if not.
    /// Asks even with a dead shell: a shell can die without its master, and
    /// reporting a reconnection that never happened would send the session
    /// loop back to a session that has ended.
    fn restore_master(&self, connect_timeout: Option<u64>) -> Result<Reconnect> {
        if self.ssh.is_master_alive() {
            return Ok(Reconnect::Unneeded);
        }
        self.forwarded.lock().map(|mut f| f.clear()).ok();
        self.ssh.ensure_master_within(connect_timeout)?;
        tracing::info!(host = %self.host(), "reconnected");
        self.open_shell();
        Ok(Reconnect::Restored)
    }

    /// Start a shell right after a master is restored. `sshd` before 10.1
    /// disables Nagle only when the first session on a connection marks it
    /// interactive, and a forward is not a session: without a shell, frames
    /// written in several parts cost an extra round trip.
    ///
    /// A held slot means a script run will start one itself.
    fn open_shell(&self) {
        if let Ok(mut slot) = self.shell.try_lock() {
            if slot.as_mut().is_none_or(|shell| !shell.is_alive()) {
                *slot = self.ssh.start_shell().ok();
            }
        }
    }

    /// If ssh reports no master, restore one and retry once. The master's
    /// socket can vanish while the process (and so the shell) lives on; the
    /// shell is then dropped, since it would keep vouching for it.
    fn forward(&self, local: &Path, remote: &Path) -> Result<()> {
        match self.ssh.forward(local, remote) {
            Err(SshError::NoMaster(_)) => {
                tracing::info!(host = %self.host(), "no master behind the ControlPath; bringing one back");
                self.let_go_of_shell();
                self.forwarded.lock().map(|mut f| f.clear()).ok();
                self.restore_master(None)?;
                Ok(self.ssh.forward(local, remote)?)
            }
            forwarded => Ok(forwarded?),
        }
    }

    fn let_go_of_shell(&self) {
        if let Ok(mut slot) = transport::claim_shell(&self.shell) {
            *slot = None;
        }
    }
}

/// A nonzero exit with empty stdout is ssh failing, not the script (which
/// always prints its terminator), as is a shell that died mid-run.
fn checked_script(
    host: &str,
    shell: &mut Shell,
    script: &str,
    args: &[&str],
) -> std::result::Result<Output, SshError> {
    let out = match shell.run(script, args) {
        Ok(out) => out,
        Err(died) => return Err(classify(host, died.status, &died.stderr)),
    };
    if !out.ok() && out.stdout.trim().is_empty() {
        return Err(classify(host, out.status, &out.stderr));
    }
    Ok(out)
}

impl Host for SshTransport {
    fn location(&self) -> &Location {
        &self.location
    }

    fn dir(&self) -> &str {
        &self.remote_dir
    }

    fn run(&self, script: &str, args: &[&str]) -> Result<Output> {
        self.run_script(script, args)
    }

    fn take_listing(&self) -> Option<String> {
        self.first_listing.lock().ok().and_then(|mut f| f.take())
    }

    /// Liveness comes from the script: connecting through a forward proves
    /// nothing, as ssh accepts first and resets afterwards.
    fn settle(&self, mut listed: Vec<Session>) -> Result<Vec<Session>> {
        for s in &listed {
            if s.state.liveness == Liveness::Dead {
                tracing::debug!(id = %s.id, "remote session reported not serving");
            }
        }
        listed.retain(|s| s.state.liveness != Liveness::Dead);
        self.sweep_orphaned_forwards(&listed);
        Ok(listed)
    }

    /// Checks both ends' path lengths before spawning.
    fn host_sock(&self, id: &str) -> Result<PathBuf> {
        let remote = self.remote_sock(id)?;
        self.local_sock(id)?;
        Ok(remote)
    }

    fn reach(&self, id: &str, host_sock: &Path) -> Result<PathBuf> {
        let local = self.local_sock(id)?;
        self.forward(&local, host_sock)?;
        self.forwarded
            .lock()
            .map(|mut f| f.insert(id.to_string()))
            .ok();
        Ok(local)
    }

    fn unreach(&self, id: &str) {
        if let (Ok(local), Ok(remote)) = (self.local_sock(id), self.remote_sock(id)) {
            self.ssh.cancel(&local, &remote);
        }
        self.forwarded.lock().map(|mut f| f.remove(id)).ok();
    }

    fn write_meta(&self, session: &Session) -> Result<()> {
        let json = session.to_json()?;
        let out = self.run_script(
            shell::WRITE_META_SCRIPT,
            &[&self.remote_dir, &session.id, &json],
        )?;
        protocol::parse_write(&out.stdout, "metadata write")
    }

    fn log_path(&self, id: &str) -> PathBuf {
        PathBuf::from(format!("{}:{}/{}.log", self.host(), self.remote_dir, id))
    }
}

impl Transport for SshTransport {
    fn location(&self) -> &Location {
        &self.location
    }

    fn list_sessions(&self) -> Result<Vec<Session>> {
        transport::list(self)
    }

    fn home(&self) -> &str {
        &self.remote_home
    }

    fn dir_source(&self) -> crate::dirs::DirSource {
        crate::dirs::DirSource::Ssh {
            host: self.ssh.host().to_string(),
            control_path: self.ssh.control_path().to_path_buf(),
        }
    }

    fn create_session(&self, name: &str, launch: &Launch, directory: &str) -> Result<Session> {
        transport::create(self, name, launch, directory)
    }

    fn kill_session(&self, s: &Session) -> Result<()> {
        transport::kill(self, s)
    }

    /// Writes the picker's record rather than re-reading the remote file. A
    /// session killed meanwhile gets stray metadata, which the next sweep
    /// removes.
    fn rename_session(&self, s: &Session, new_name: &str) -> Result<()> {
        plan_rename(&self.list_sessions()?, new_name, &s.id)?;

        let mut updated = s.clone();
        updated.name = new_name.to_string();
        self.write_meta(&updated)?;
        tracing::info!(host = %self.host(), id = %s.id, to = new_name, "renamed");
        Ok(())
    }

    fn renumber(&self, sessions: &[Session]) -> Result<()> {
        if sessions.is_empty() {
            return Ok(());
        }
        let bodies = sessions
            .iter()
            .map(|s| s.to_json())
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut args: Vec<&str> = Vec::with_capacity(1 + sessions.len() * 2);
        args.push(&self.remote_dir);
        for (s, body) in sessions.iter().zip(&bodies) {
            args.push(&s.id);
            args.push(body);
        }

        let out = self.run_script(shell::RENUMBER_SCRIPT, &args)?;
        protocol::parse_write(&out.stdout, "renumber")?;
        tracing::info!(host = %self.host(), count = sessions.len(), "renumbered");
        Ok(())
    }

    fn local_socket_for(&self, s: &Session) -> Result<PathBuf> {
        let local = self.local_sock(&s.id)?;
        let remote = self.remote_sock(&s.id)?;

        // Before the fast path: a dead master must clear `forwarded`, or
        // every retry uses a dead socket.
        self.reconnect_if_needed(None)?;

        let known = self
            .forwarded
            .lock()
            .map(|f| f.contains(&s.id))
            .unwrap_or(false);
        if known && local.exists() {
            return Ok(local);
        }

        self.reach(&s.id, &remote)
    }

    /// One `ssh -O forward` for all sessions not yet forwarded, instead of a
    /// fork each. A forward is trusted only if all succeeded and its local
    /// socket exists (a refusal does not say which pair); the rest are left to
    /// `local_socket_for`.
    fn reach_all(&self, sessions: &[Session]) {
        if sessions.len() < 2 || self.reconnect_if_needed(None).is_err() {
            return;
        }
        let known = self.forwarded.lock().map(|f| f.clone()).unwrap_or_default();
        let (ids, pairs): (Vec<&str>, Vec<(PathBuf, PathBuf)>) = sessions
            .iter()
            .filter(|s| !known.contains(&s.id))
            .filter_map(|s| {
                let pair = (self.local_sock(&s.id).ok()?, self.remote_sock(&s.id).ok()?);
                Some((s.id.as_str(), pair))
            })
            .unzip();
        if pairs.len() < 2 || !self.ssh.forward_all(&pairs) {
            return;
        }
        if let Ok(mut forwarded) = self.forwarded.lock() {
            for (id, (local, _)) in ids.iter().zip(&pairs) {
                if local.exists() {
                    forwarded.insert((*id).to_string());
                }
            }
        }
    }

    /// Bounded because it is retried. A passphrase or key touch is not.
    fn reconnect(&self) -> Result<Reconnect> {
        self.reconnect_if_needed(Some(RECONNECT_TIMEOUT_SECS))
    }
}

/// A handshake takes well under a second; the TCP timeout takes minutes.
const RECONNECT_TIMEOUT_SECS: u64 = 10;

#[cfg(test)]
mod tests {
    use super::*;

    /// A local shell stands in for a remote one.
    fn sh() -> Shell {
        Shell::local().expect("start /bin/sh")
    }

    #[test]
    fn a_silent_failure_is_classified_rather_than_returned() {
        let mut shell = sh();
        match checked_script("h", &mut shell, "exit 1", &[]) {
            Err(SshError::Failed { code, stderr }) => {
                assert_eq!(code, 1);
                assert_eq!(stderr, "");
            }
            other => panic!("expected a classified failure, got {other:?}"),
        }
        // The shell is fine; only the script failed.
        let out = checked_script("h", &mut shell, "printf still", &[]).expect("runs");
        assert_eq!(out.stdout, "still");
    }

    #[test]
    fn a_failure_with_output_is_the_scripts_to_report() {
        let mut shell = sh();
        let out = checked_script("h", &mut shell, "printf said; exit 1", &[]).expect("a result");
        assert_eq!(out.stdout, "said");
        assert_eq!(out.status, 1);
    }

    /// A signal reads as exit `-1`; stderr decides the error.
    #[test]
    fn a_shell_that_dies_mid_run_is_classified_from_its_last_words() {
        let mut shell = sh();
        // `$$` is the shell's own pid in a subshell too.
        match checked_script("h", &mut shell, "kill -KILL $$", &[]) {
            Err(SshError::Failed { code, .. }) => assert_eq!(code, -1),
            other => panic!("expected a classified death, got {other:?}"),
        }
        assert!(!shell.is_alive());

        let mut shell = sh();
        let err = checked_script(
            "h",
            &mut shell,
            "printf 'Permission denied (publickey).\\n' >&2; kill -KILL $$",
            &[],
        )
        .expect_err("died");
        assert!(matches!(err, SshError::AuthFailed(_)), "{err:?}");
    }
}
