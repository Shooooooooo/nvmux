//! Sessions on another host, reached over SSH.
//!
//! Spawning, listing and killing are the local transport's bodies, in
//! [`crate::transport`]: the same scripts from [`crate::shell`], the same
//! parsing in [`crate::transport::protocol`], and the same
//! [`crate::proc::Shell`] they run in. What is remote about a remote session is
//! what this file keeps: the shell is started by [`Ssh::start_shell`] over a
//! master connection, a socket over there is reached from here through a
//! forward onto that master — [`Transport::local_socket_for`], the seam that
//! makes everything downstream identical — liveness is what the script said,
//! since a forward answers nothing about it, and metadata is written back
//! through a script rather than edited in place.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, TryLockError};

use crate::error::{NvimError, NvmuxError, Result, SshError};
use crate::ids;
use crate::launch::Launch;
use crate::nvim;
use crate::paths;
use crate::proc::{Output, Shell};
use crate::session::{Liveness, Session};
use crate::shell;
use crate::ssh::{classify, Ssh};
use crate::transport::{self, plan_rename, protocol, Host, Location, Transport};

pub struct SshTransport {
    location: Location,
    ssh: Ssh,
    /// The shell on the host, over the master. Started once by [`Self::new`],
    /// with the login shell that costs, and replaced by [`Self::run_script`]
    /// if it dies — which it does with the master it rides on.
    shell: Mutex<Option<Shell>>,
    /// Short, stable, filename-safe token for this host. Namespaces the local
    /// end of every forward and the ControlPath, so two hosts holding sessions
    /// with the same id cannot collide on one local path.
    host_token: String,
    /// Our own runtime directory, which holds the forwarded sockets.
    local_dir: PathBuf,
    /// The runtime directory on the host that owns the nvim processes.
    remote_dir: String,
    /// That host's home directory, from the same greeting, or empty if it could
    /// not say. A path on this machine means nothing over there, so this is the
    /// only home directory a remote session may be started in by default.
    remote_home: String,
    /// Sessions whose sockets are already forwarded, so re-attaching does not
    /// pay for a redundant round trip.
    forwarded: Mutex<HashSet<String>>,
    /// The listing that came back with the greeting, still unparsed, waiting
    /// for the first [`Transport::list_sessions`] to take it.
    ///
    /// Opening a host and drawing the picker are two questions with one answer,
    /// and asking them separately costs a second script run over the link. The
    /// window this is held across is the few milliseconds between
    /// [`SshTransport::new`] returning and the picker's first listing — nvmux
    /// creates, renames and kills nothing in between — and it is taken exactly
    /// once, so every listing after it is fresh.
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
        let host_token = ids::host_token(&host);
        let local_dir = paths::ensure_runtime_dir()?;
        let control_path = paths::control_path(&local_dir, &host_token)?;

        // Checked before connecting, so the error names the real problem.
        crate::ssh::check_local()?;

        let ssh = Ssh::new(host.clone(), control_path);
        ssh.ensure_master()?;
        let mut shell = ssh.start_shell()?;

        // One round trip for the runtime directory, the remote Neovim version
        // *and* the sessions the picker is about to draw — the shell's first,
        // so it also absorbs whatever the login shell prints on the way up.
        //
        // Which means the listing now happens before the version gate below,
        // rather than after it: a host nvmux is about to refuse has its runtime
        // directory swept first. Only ever of sessions that are already gone —
        // `list.sh` removes a socket nothing serves and metadata with no socket
        // — so the sweep is the one it would have done on the next successful
        // run anyway.
        let out = checked_script(&host, &mut shell, shell::HELLO_SCRIPT, &[])?;
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
            shell: Mutex::new(Some(shell)),
            host_token,
            local_dir,
            remote_dir: probe.runtime_dir,
            remote_home: probe.home,
            forwarded: Mutex::new(HashSet::new()),
            first_listing: Mutex::new(Some(out.stdout)),
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

    /// Run one of the shared scripts on the remote host, in its shell.
    ///
    /// A dead master is reported as such rather than as a mysterious failure,
    /// because it is the one condition the user can do something about. A
    /// shell found dead is replaced *before* a run and never after one — a
    /// script that was half way through must not run twice — and the master
    /// it rode on is checked first, or the replacement would die the same way.
    fn run_script(&self, script: &str, args: &[&str]) -> Result<Output> {
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
            *slot = None;
            self.reconnect_if_needed()?;
            *slot = Some(self.ssh.start_shell()?);
        }
        let shell = slot.as_mut().expect("just started");
        match checked_script(self.host(), shell, script, args) {
            Ok(out) => Ok(out),
            Err(err) => {
                // A shell that died is dropped now; one that merely ran a
                // script that failed is kept.
                if slot.as_mut().is_some_and(|shell| !shell.is_alive()) {
                    *slot = None;
                }
                if matches!(err, SshError::NoMaster(_) | SshError::MasterDied(_)) {
                    // Report it, drop the forwards we believed in, and let the
                    // caller fall back to the picker.
                    self.forwarded.lock().map(|mut f| f.clear()).ok();
                }
                Err(err.into())
            }
        }
    }

    /// Bring the master back if it died while nobody was using it, and forget
    /// the forwards that died with it.
    ///
    /// Checking first turns "the connection died while you were in the picker"
    /// into a sentence rather than a forwarding failure.
    ///
    /// The shell rides on the master, so a shell that is still there is a
    /// master that is — and asking the shell is a zero-timeout poll on a pipe,
    /// where `ssh -O check` is a fork per attach. Neither notices a master
    /// whose connection is wedged rather than gone; that is what the master's
    /// own `ServerAlive` options are for. Only without a shell to ask, or with
    /// one found dead, is ssh asked — and a dead shell is dropped here, so the
    /// next script starts a fresh one over whatever master this leaves.
    fn reconnect_if_needed(&self) -> Result<()> {
        // A slot that is locked has a script running in it, which is as alive
        // as a shell gets.
        let shell_alive = match self.shell.try_lock() {
            Ok(mut slot) => match slot.as_mut().map(|shell| shell.is_alive()) {
                Some(true) => Some(true),
                Some(false) => {
                    *slot = None;
                    Some(false)
                }
                None => None,
            },
            Err(TryLockError::WouldBlock) => Some(true),
            Err(TryLockError::Poisoned(_)) => None,
        };
        let master_alive = match shell_alive {
            Some(alive) => alive,
            None => self.ssh.is_master_alive(),
        };
        if !master_alive {
            self.forwarded.lock().map(|mut f| f.clear()).ok();
            self.ssh.ensure_master()?;
        }
        Ok(())
    }
}

/// Run a script and insist it actually ran.
///
/// A nonzero exit with nothing on stdout is ssh itself failing, not the script
/// reporting something: the script would have printed its terminator. A shell
/// that died mid-run is the same failure told a different way — ssh's own exit
/// status and last words — and is classified from them just the same. Used by
/// `SshTransport::new` too, before there is a `self` to call the method on.
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

    /// The greeting already carried one, once.
    fn take_listing(&self) -> Option<String> {
        self.first_listing.lock().ok().and_then(|mut f| f.take())
    }

    /// Liveness comes from the remote script, so drawing the picker needs no
    /// forward per session. connect() through a forward would say nothing
    /// anyway: ssh accepts first and resets afterwards.
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

    /// Both ends checked before spawning: the remote path against our own
    /// budget (see `remote_sock`), and the local end of the forward it will
    /// need against this machine's.
    fn host_sock(&self, id: &str) -> Result<PathBuf> {
        let remote = self.remote_sock(id)?;
        self.local_sock(id)?;
        Ok(remote)
    }

    fn reach(&self, id: &str, host_sock: &Path) -> Result<PathBuf> {
        let local = self.local_sock(id)?;
        self.ssh.forward(&local, host_sock)?;
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
        protocol::require_terminator(&out.stdout, "metadata write")
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

    /// Onto the master this transport already brought up, not a new connection:
    /// the `ControlPath` is the whole handle, and `ssh` finds the live master
    /// through it.
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

    /// Writes the record the picker holds, with the name changed, rather than
    /// re-reading the remote file first: that would be another round trip, and
    /// the only field a rename may change is the name. A session killed
    /// between the listing and the confirm gets its metadata rewritten, which
    /// the next listing's sweep removes again, since its socket is gone.
    fn rename_session(&self, s: &Session, new_name: &str) -> Result<()> {
        plan_rename(&self.list_sessions()?, new_name, &s.id)?;

        let mut updated = s.clone();
        updated.name = new_name.to_string();
        self.write_meta(&updated)?;
        tracing::info!(host = %self.host(), id = %s.id, to = new_name, "renamed");
        Ok(())
    }

    /// One round trip for the whole arrangement. Writes the records the picker
    /// holds — see [`Transport::renumber`] for why that is complete and why the
    /// script, not this side, is what refuses to invent metadata for an orphan.
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
        protocol::parse_renumber(&out.stdout)?;
        tracing::info!(host = %self.host(), count = sessions.len(), "renumbered");
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

        self.reconnect_if_needed()?;

        self.ssh.forward(&local, &remote)?;
        self.forwarded
            .lock()
            .map(|mut f| f.insert(s.id.clone()))
            .ok();
        Ok(local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc;

    /// A shell here standing in for one over there: what `checked_script` does
    /// with a run does not depend on what started the shell.
    fn sh() -> Shell {
        Shell::start(&mut proc::sh_command(), "/bin/sh").expect("start /bin/sh")
    }

    /// The rule the function exists for: a script that said nothing and
    /// failed is not a script result, it is ssh failing to run one.
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

    /// A script that reported something and then failed is a script result,
    /// however it ended: its caller reads the stdout, as the kill path does.
    #[test]
    fn a_failure_with_output_is_the_scripts_to_report() {
        let mut shell = sh();
        let out = checked_script("h", &mut shell, "printf said; exit 1", &[]).expect("a result");
        assert_eq!(out.stdout, "said");
        assert_eq!(out.status, 1);
    }

    /// The shell going away mid-run reads as ssh's own exit: a signal is `-1`,
    /// as a one-shot `Output` reported it, and the words on stderr decide.
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
