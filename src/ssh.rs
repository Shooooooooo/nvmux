//! Drives the `ssh` client.
//!
//! A subprocess rather than a Rust SSH library, so `~/.ssh/config`,
//! `ProxyJump`, agent forwarding and hardware keys work unmodified; the
//! `openssh` crate does not cleanly expose `ssh -O forward`.
//!
//! One `ControlMaster` per host ([`master_args`]). Each session adds a
//! unix-socket forward on it with `-O forward` (batched when several are
//! wanted at once), removed with `-O cancel`, tested with `-O check`.
//!
//! nvmux computes `ControlPath` itself, since ssh's `%C`/`%h%p%r` tokens
//! expand to unpredictable lengths (see [`crate::paths`]). Each master binds
//! its own socket and is linked to the `ControlPath` once up, so no master can
//! remove another's (see [`Ssh::ensure_master_within`]).
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::SshError;
use crate::paths;
use crate::proc;
use crate::shell;

/// 6.7 added unix-socket forwarding (`-L <local_sock>:<remote_sock>`), which
/// the remote transport relies on. `MIN_SSH` must match (tested).
const MIN_SSH_VERSION: &str = "6.7";
const MIN_SSH: (u64, u64) = (6, 7);

/// Parse the version out of `ssh -V` output, e.g. `OpenSSH_9.6p1 Ubuntu-3...`.
fn parse_ssh_version(banner: &str) -> Option<(u64, u64)> {
    let token = banner.split_whitespace().next()?;
    let rest = token.strip_prefix("OpenSSH_")?;
    let numeric: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = numeric.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

/// Start `ssh -V` so a missing or too-old `ssh` is refused early with a clear
/// error; judged later by [`LocalCheck::verdict`].
pub fn check_local() -> LocalCheck {
    LocalCheck(
        Command::new("ssh")
            .arg("-V")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn(),
    )
}

/// An `ssh -V` in progress; see [`check_local`].
pub struct LocalCheck(std::io::Result<std::process::Child>);

impl LocalCheck {
    pub fn verdict(self) -> Result<(), SshError> {
        let out = self
            .0
            .and_then(std::process::Child::wait_with_output)
            .map_err(|_| SshError::NotFound)?;
        judge_version(&out)
    }
}

fn judge_version(out: &std::process::Output) -> Result<(), SshError> {
    // `ssh -V` writes to stderr.
    let banner = if out.stderr.is_empty() {
        String::from_utf8_lossy(&out.stdout).into_owned()
    } else {
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    match parse_ssh_version(&banner) {
        Some(v) if v >= MIN_SSH => Ok(()),
        Some((major, minor)) => Err(SshError::TooOld {
            found: format!("{major}.{minor}"),
            min: MIN_SSH_VERSION,
        }),
        // Unknown banners (forks) are allowed; a forward would fail clearly.
        None => Ok(()),
    }
}

/// Options for every invocation. `BatchMode` is deliberately **not** set:
/// passphrase prompts, hardware key touches and 2FA must still work.
fn common(ctl: &Path) -> Vec<String> {
    vec!["-o".into(), format!("ControlPath={}", ctl.display())]
}

/// Connect timeout for unattended commands. Bounds only the *connect*, not a
/// slow command.
const UNATTENDED_CONNECT_SECS: u64 = 5;

/// Options for commands nvmux runs on its own (so far only the create
/// prompt's directory completion, on a worker thread). `BatchMode=yes`: no
/// terminal can answer a prompt (ratatui owns it), so ssh would block forever.
/// `ssh -O check` cannot pre-empt that; it says yes for a wedged master.
fn unattended(ctl: &Path) -> Vec<String> {
    let mut args = common(ctl);
    args.extend([
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        format!("ConnectTimeout={UNATTENDED_CONNECT_SECS}"),
    ]);
    args
}

/// Arguments to start a master bound at `own`, its own socket rather than the
/// shared `ControlPath` (see [`Ssh::ensure_master_within`]).
///
/// `StreamLocalBindUnlink=yes` belongs **here** only: the master does the
/// bind, so it does nothing on an `-O forward` client. Without it `-O cancel`
/// leaves the socket file and the next forward onto that path fails (rc 255,
/// `mux_client_forward: forwarding request failed`), breaking reattach.
///
/// `connect_timeout` (seconds): `None` for the first connect, which the user
/// watches. Reconnects need it: a down link drops packets rather than
/// refusing, so each attempt would hang for minutes (see [`crate::reconnect`]).
/// `ipqos`: see [`master_ipqos`].
fn master_args(
    host: &str,
    own: &Path,
    connect_timeout: Option<u64>,
    ipqos: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "-M".into(),
        "-N".into(),
        "-f".into(),
        "-o".into(),
        "ControlMaster=yes".into(),
        "-o".into(),
        "ControlPersist=60".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "StreamLocalBindUnlink=yes".into(),
        // Detect a dead link instead of hanging on a half-open TCP connection.
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ];
    if let Some(secs) = connect_timeout {
        args.extend(["-o".into(), format!("ConnectTimeout={secs}")]);
    }
    if let Some(ipqos) = ipqos {
        args.extend(["-o".into(), format!("IPQoS={ipqos}")]);
    }
    args.extend(common(own));
    args.push(host.to_string());
    args
}

/// Upstream OpenSSH's default from 7.8 until 10.1: `af21` for interactive
/// sessions, `cs1` otherwise.
const UPSTREAM_IPQOS: &str = "af21 cs1";

/// Used instead of [`UPSTREAM_IPQOS`].
const MASTER_IPQOS: &str = "af21 cs0";

/// The `IPQoS` to give a master instead of what `ssh -G` (`config`) reports,
/// if it needs changing. A `-N` master is non-interactive, so ssh uses the
/// second value, by default `cs1` up to 10.0: Wi-Fi queues that behind all
/// other traffic, and on the master it is every keystroke and frame. `cs0` is
/// the unmarked class.
///
/// Only ssh's own default is changed (Debian/Ubuntu ship another pair, 10.1+
/// uses `cs0`, `~/.ssh/config` is the user's, and before 8.7 it wins over
/// `-o` anyway). An `ssh` without `-G` (before 6.8) is left alone.
fn master_ipqos(config: &str) -> Option<&'static str> {
    config
        .lines()
        .find_map(|line| line.strip_prefix("ipqos "))
        .filter(|ipqos| ipqos.trim() == UPSTREAM_IPQOS)
        .map(|_| MASTER_IPQOS)
}

/// Ask the master answering on `own` to exit.
fn exit_args(host: &str, own: &Path) -> Vec<String> {
    let mut args = common(own);
    args.extend(["-O".into(), "exit".into(), host.to_string()]);
    args
}

fn check_args(host: &str, ctl: &Path) -> Vec<String> {
    let mut args = common(ctl);
    args.extend(["-O".into(), "check".into(), host.to_string()]);
    args
}

/// `ssh -O <op> -L <local>:<remote> <host>` against the running master.
fn forward_op(op: &str, host: &str, ctl: &Path, local: &Path, remote: &Path) -> Vec<String> {
    let mut args = common(ctl);
    args.extend([
        "-O".into(),
        op.to_string(),
        "-L".into(),
        format!("{}:{}", local.display(), remote.display()),
        host.to_string(),
    ]);
    args
}

/// Add a forward to the running master, without reconnecting.
fn forward_args(host: &str, ctl: &Path, local: &Path, remote: &Path) -> Vec<String> {
    forward_op("forward", host, ctl, local, remote)
}

fn cancel_args(host: &str, ctl: &Path, local: &Path, remote: &Path) -> Vec<String> {
    forward_op("cancel", host, ctl, local, remote)
}

/// Add all these forwards to the running master in one command.
fn forward_all_args(host: &str, ctl: &Path, pairs: &[(PathBuf, PathBuf)]) -> Vec<String> {
    let mut args = common(ctl);
    args.extend(["-O".into(), "forward".into()]);
    for (local, remote) in pairs {
        args.extend([
            "-L".into(),
            format!("{}:{}", local.display(), remote.display()),
        ]);
    }
    args.push(host.to_string());
    args
}

/// Arguments to start the host's shell (the one `sh` [`proc::Shell`] feeds
/// every script) through a **login** shell:
///
/// ```text
/// ssh -T <host> exec sh -c '... exec "${SHELL:-/bin/bash}" -l -c "exec sh -s" ...' nvmux
/// ```
///
/// A login shell because `ssh host cmd` does not read `.zprofile`, so `nvim`
/// is off `$PATH` on most setups. `$SHELL` expands on the *remote* side in a
/// POSIX `sh`, not in sshd's login shell, which may be fish or tcsh;
/// [`shell::login_shell_wrapper`] is the single, tested spelling of this. Paid
/// once per host: scripts and arguments go down the shell's stdin.
///
/// `-T` because stdin is the command stream: with `RequestTTY force` a pty
/// would echo every script into the output. Never add `-n`: it puts
/// `/dev/null` on stdin, so `sh -s` silently exits 0 with *empty* output.
fn shell_args(host: &str, ctl: &Path) -> Vec<String> {
    shell_args_with(common(ctl), host)
}

/// [`shell_args`] with the [`unattended`] options; otherwise identical, so
/// the same program runs on the host, reached the same way.
fn unattended_shell_args(host: &str, ctl: &Path) -> Vec<String> {
    shell_args_with(unattended(ctl), host)
}

fn shell_args_with(mut args: Vec<String>, host: &str) -> Vec<String> {
    args.push("-T".into());
    args.extend([host.to_string(), remote_shell_command()]);
    args
}

/// The login shell's command. `exec` at every step, so nothing of the login
/// shell (e.g. `.bash_logout`) runs when the stream closes or sits between ssh
/// and the shell. `nvmux` is the `$0` of the `sh` that reads `$SHELL`, as
/// shown in process listings.
fn remote_shell_command() -> String {
    format!("{} nvmux", shell::login_shell_wrapper("exec sh -s"))
}

/// Map an ssh failure to an actionable error. ssh exits 255 for almost
/// everything, so stderr is the only signal.
pub fn classify(host: &str, code: i32, stderr: &str) -> SshError {
    let lower = stderr.to_lowercase();

    if lower.contains("permission denied")
        || lower.contains("too many authentication failures")
        || lower.contains("no supported authentication methods")
    {
        return SshError::AuthFailed(host.to_string());
    }
    if lower.contains("could not resolve hostname")
        || lower.contains("name or service not known")
        || lower.contains("no route to host")
        // "Connection timed out" on Linux, "Operation timed out" on macOS,
        // and "timed out during banner exchange" under `ConnectTimeout`.
        || lower.contains("timed out")
        || lower.contains("connection refused")
        || lower.contains("network is unreachable")
        // macOS, with the Wi-Fi off.
        || lower.contains("host is down")
    {
        return SshError::Unreachable(host.to_string());
    }
    if says_no_master(stderr) {
        return SshError::NoMaster(host.to_string());
    }
    if lower.contains("broken pipe")
        || lower.contains("connection closed")
        || lower.contains("connection reset")
    {
        return SshError::MasterDied(host.to_string());
    }
    SshError::Failed {
        code,
        stderr: stderr.trim().to_string(),
    }
}

/// Whether ssh's stderr says no master is behind the `ControlPath`: no
/// socket, or one whose master has gone.
fn says_no_master(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("control socket connect")
        || lower.contains("no such file or directory") && lower.contains("control")
}

/// Result of connecting to the `ControlPath`: what `ssh -O check` asks, but
/// without a fork, so the answer is not stale by the time it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// Something accepted; in a directory only we can write, a master.
    Answered,
    Absent,
    /// Something is there but refused: a dead master's socket, or not a
    /// socket.
    Refused,
}

fn probe(path: &Path) -> Probe {
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => Probe::Answered,
        Err(e) if e.kind() == ErrorKind::NotFound => Probe::Absent,
        Err(_) => Probe::Refused,
    }
}

/// The `ControlPath` after clearing anything dead there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cleared {
    /// A master can be linked there.
    Empty,
    /// A live master answered; use it.
    Taken,
}

pub struct Ssh {
    host: String,
    control_path: PathBuf,
}

impl Ssh {
    pub fn new(host: String, control_path: PathBuf) -> Self {
        Self { host, control_path }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// Exposed so a second, `Send` driver on the *same* master can be built
    /// without recomputing it (see [`crate::dirs::DirSource`]).
    pub fn control_path(&self) -> &Path {
        &self.control_path
    }

    fn run(&self, args: &[String]) -> std::io::Result<std::process::Output> {
        tracing::debug!(args = ?args, "ssh");
        Command::new("ssh")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
    }

    /// The `IPQoS` override for a master to this host, if any (see
    /// [`master_ipqos`]). `ssh -G` only reads config; it connects to nothing.
    fn ipqos(&self) -> Option<&'static str> {
        let out = self.run(&["-G".into(), self.host.clone()]).ok()?;
        out.status
            .success()
            .then(|| master_ipqos(&String::from_utf8_lossy(&out.stdout)))
            .flatten()
    }

    pub fn is_master_alive(&self) -> bool {
        self.run(&check_args(&self.host, &self.control_path))
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Bring up a master connection, or reuse a live one.
    pub fn ensure_master(&self) -> Result<(), SshError> {
        self.ensure_master_within(None)
    }

    /// [`Self::ensure_master`] with the connect bounded (see [`master_args`]).
    ///
    /// A master binds its own socket ([`paths::master_socket`]) and is
    /// hard-linked to the `ControlPath` once up, because on exit ssh removes
    /// its `ControlPath` by name, whoever's socket it then holds. A master
    /// whose file was deleted would later remove a newer master's name, whose
    /// shell stays up while every `-O forward` fails. Verified.
    ///
    /// Linking also settles two nvmux starting masters at once: the first link
    /// wins and the other exits. (Binding a taken shared name, ssh disables
    /// multiplexing and stays up, unreachable.) Dead linked names are removed
    /// by [`Self::clear_control_path`], which connects first, so a live master
    /// is found without forking `ssh -O check`.
    pub fn ensure_master_within(&self, connect_timeout: Option<u64>) -> Result<(), SshError> {
        if self.clear_control_path()? == Cleared::Taken {
            return Ok(());
        }
        let nonce = crate::ids::nonce().map_err(|e| SshError::Failed {
            code: -1,
            stderr: format!("naming a master connection to {}: {e}", self.host),
        })?;
        let own = paths::master_socket(&self.control_path, &nonce);
        let ipqos = self.ipqos();
        // `-f` backgrounds ssh once authenticated, so this blocks only for a
        // key touch or passphrase.
        let out = self
            .run(&master_args(&self.host, &own, connect_timeout, ipqos))
            .map_err(|e| spawn_error(e, &self.host))?;
        if !out.status.success() {
            return Err(classify(
                &self.host,
                out.status.code().unwrap_or(-1),
                &String::from_utf8_lossy(&out.stderr),
            ));
        }
        self.publish(&own)
    }

    /// Make room on the `ControlPath` for a new master, or find a live one
    /// there to use instead.
    ///
    /// A dead master's socket blocks linking, so it is removed: only a socket,
    /// only ours, only after a refused connect. Never on `-O check`'s stale
    /// word: another nvmux may link its master in the gap, which left a
    /// master unreachable 11 times in 260 restarts staggered by about one
    /// master start-up.
    fn clear_control_path(&self) -> Result<Cleared, SshError> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        match probe(&self.control_path) {
            Probe::Answered => return Ok(Cleared::Taken),
            Probe::Absent => return Ok(Cleared::Empty),
            Probe::Refused => {}
        }
        let meta = match std::fs::symlink_metadata(&self.control_path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Cleared::Empty),
            Err(e) => return Err(in_the_way(&self.control_path, &e.to_string())),
        };
        if !meta.file_type().is_socket() || meta.uid() != nix::unistd::geteuid().as_raw() {
            return Err(in_the_way(
                &self.control_path,
                "not a socket of ours, so nvmux will not remove it",
            ));
        }
        tracing::info!(path = %self.control_path.display(), "removing a dead ControlPath");
        match std::fs::remove_file(&self.control_path) {
            Err(e) if e.kind() != ErrorKind::NotFound => {
                Err(in_the_way(&self.control_path, &e.to_string()))
            }
            _ => Ok(Cleared::Empty),
        }
    }

    /// Link the master on `own` to the `ControlPath`, where other ssh commands
    /// find it, unless another master got there first; then that one serves
    /// and ours is retired.
    fn publish(&self, own: &Path) -> Result<(), SshError> {
        // Retry only if the name was dead or vanished between the failed link
        // and the check.
        for _ in 0..3 {
            let taken = match std::fs::hard_link(own, &self.control_path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => self.clear_control_path(),
                // The master exited after `-f` returned, taking its socket.
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    return Err(SshError::MasterDied(self.host.clone()));
                }
                Err(e) => Err(in_the_way(&self.control_path, &e.to_string())),
            };
            match taken {
                Ok(Cleared::Empty) => {}
                Ok(Cleared::Taken) => {
                    tracing::info!(host = %self.host, "another master was linked first; retiring ours");
                    self.retire(own);
                    return Ok(());
                }
                Err(e) => {
                    self.retire(own);
                    return Err(e);
                }
            }
        }
        self.retire(own);
        Err(in_the_way(
            &self.control_path,
            "it kept changing while a master was being linked to it",
        ))
    }

    /// Tell the master on `own` to exit. Nothing was linked to it, so it has
    /// no clients; this only ends its connection (otherwise `ControlPersist`
    /// would, a minute later).
    fn retire(&self, own: &Path) {
        match self.run(&exit_args(&self.host, own)) {
            Ok(out) if out.status.success() => {}
            Ok(out) => tracing::debug!(
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "could not retire a master"
            ),
            Err(e) => tracing::debug!(error = %e, "could not retire a master"),
        }
    }

    /// Point a local socket at a remote one, idempotently.
    ///
    /// If the master already forwards this pair (masters outlive their nvmux
    /// by `ControlPersist`), `-O forward` is a **silent no-op** that does not
    /// recreate the socket file. So success counts only if the local socket
    /// exists; otherwise the forward is cancelled and redone. No master is
    /// [`SshError::NoMaster`], which the caller can recover from.
    pub fn forward(&self, local: &Path, remote: &Path) -> Result<(), SshError> {
        match self.forward_once(local, remote) {
            Ok(()) if local.exists() => return Ok(()),
            Err(SshError::NoMaster(host)) => return Err(SshError::NoMaster(host)),
            _ => {}
        }
        // Usually fails (no such forward), which is fine.
        let _ = self.run(&cancel_args(&self.host, &self.control_path, local, remote));

        // `-O cancel` exits 0 without removing the file, and a forward onto an
        // existing path fails. `StreamLocalBindUnlink` covers this, but an
        // older nvmux's master may lack it.
        if let Err(e) = std::fs::remove_file(local) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %local.display(), error = %e, "could not clear local socket");
            }
        }
        self.forward_once(local, remote)
    }

    /// One `-O forward`, its failure classified.
    fn forward_once(&self, local: &Path, remote: &Path) -> Result<(), SshError> {
        let out = self
            .run(&forward_args(&self.host, &self.control_path, local, remote))
            .map_err(|e| spawn_error(e, &self.host))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if says_no_master(&stderr) {
                tracing::debug!(host = %self.host, %stderr, "no master to forward on");
                return Err(SshError::NoMaster(self.host.clone()));
            }
            return Err(SshError::ForwardFailed {
                local: local.to_path_buf(),
                remote: remote.to_path_buf(),
                stderr,
            });
        }
        Ok(())
    }

    /// Forward every pair with one `ssh -O forward`; returns whether ssh
    /// reported success. Callers must still check each local socket: an
    /// already-forwarded pair is the same silent no-op as in
    /// [`Self::forward`], and a refused pair fails the command without
    /// stopping the others or saying which. Verified.
    pub fn forward_all(&self, pairs: &[(PathBuf, PathBuf)]) -> bool {
        if pairs.is_empty() {
            return true;
        }
        match self.run(&forward_all_args(&self.host, &self.control_path, pairs)) {
            Ok(out) if out.status.success() => true,
            Ok(out) => {
                tracing::debug!(
                    host = %self.host,
                    stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                    "could not forward the sessions together"
                );
                false
            }
            Err(e) => {
                tracing::debug!(host = %self.host, error = %e, "could not forward the sessions together");
                false
            }
        }
    }

    /// Remove a forward and its local socket.
    pub fn cancel(&self, local: &Path, remote: &Path) {
        let _ = self.run(&cancel_args(&self.host, &self.control_path, local, remote));
        // `-O cancel` returns 0 without removing the file.
        let _ = std::fs::remove_file(local);
    }

    /// Start this host's shell (see [`shell_args`]). Returns once `ssh` is
    /// spawned; the first run reads the handshake, so an unreachable host is
    /// reported by that run, classified from ssh's stderr.
    pub fn start_shell(&self) -> Result<proc::Shell, SshError> {
        self.start(shell_args(&self.host, &self.control_path))
    }

    /// Like [`Self::start_shell`], for scripts nvmux runs on its own: never
    /// prompts and gives up on an unreachable host (see [`unattended`]).
    pub fn start_unattended_shell(&self) -> Result<proc::Shell, SshError> {
        self.start(unattended_shell_args(&self.host, &self.control_path))
    }

    fn start(&self, argv: Vec<String>) -> Result<proc::Shell, SshError> {
        tracing::debug!(args = ?argv, "ssh shell");
        let mut cmd = Command::new("ssh");
        cmd.args(&argv);
        proc::Shell::start(&mut cmd, format!("ssh {}", self.host))
            .map_err(|e| spawn_error(e, &self.host))
    }
}

/// Something on the `ControlPath` that no master can be linked over.
fn in_the_way(path: &Path, why: &str) -> SshError {
    SshError::Failed {
        code: -1,
        stderr: format!("{}: {why}", path.display()),
    }
}

fn spawn_error(e: std::io::Error, host: &str) -> SshError {
    if e.kind() == std::io::ErrorKind::NotFound {
        SshError::NotFound
    } else {
        SshError::Failed {
            code: -1,
            stderr: format!("running ssh for {host}: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_minimum_ssh_version_is_spelled_once() {
        assert_eq!(
            parse_ssh_version(&format!("OpenSSH_{MIN_SSH_VERSION}p1")),
            Some(MIN_SSH)
        );
    }
    #[test]
    fn parses_ssh_banners() {
        assert_eq!(
            parse_ssh_version("OpenSSH_9.6p1 Ubuntu-3ubuntu13.19, OpenSSL 3.0.13"),
            Some((9, 6))
        );
        assert_eq!(
            parse_ssh_version("OpenSSH_9.0p1, LibreSSL 3.3.6"),
            Some((9, 0))
        );
        assert_eq!(parse_ssh_version("OpenSSH_6.7p1"), Some((6, 7)));
        assert_eq!(parse_ssh_version("something else"), None);
    }

    const CTL: &str = "/tmp/nvmux-501/cm-abcdefgh";

    fn joined(args: &[String]) -> String {
        args.join(" ")
    }

    /// `BatchMode` on the interactive path breaks keys needing a passphrase or
    /// touch; without it the completion worker blocks on an invisible prompt.
    #[test]
    fn only_an_unattended_shell_refuses_to_prompt() {
        let interactive = shell_args("myhost", Path::new(CTL));
        assert!(
            !interactive.iter().any(|a| a.contains("BatchMode")),
            "the shell the user's commands run in must still be able to prompt: {interactive:?}"
        );
        assert!(!interactive.iter().any(|a| a.contains("ConnectTimeout")));

        let unattended = unattended_shell_args("myhost", Path::new(CTL));
        assert!(
            unattended.iter().any(|a| a == "BatchMode=yes"),
            "a shell nobody is watching must never prompt: {unattended:?}"
        );
        assert!(
            unattended
                .iter()
                .any(|a| a == &format!("ConnectTimeout={UNATTENDED_CONNECT_SECS}")),
            "and must give up on a host it cannot reach: {unattended:?}"
        );
    }

    #[test]
    fn the_two_option_sets_differ_in_nothing_but_the_options() {
        let interactive = shell_args("myhost", Path::new(CTL));
        let unattended = unattended_shell_args("myhost", Path::new(CTL));
        assert_eq!(
            interactive.last(),
            unattended.last(),
            "the remote command must be the same"
        );
        assert_eq!(
            interactive[interactive.len() - 2],
            unattended[unattended.len() - 2],
            "and so must the host"
        );
    }

    #[test]
    fn the_master_sets_stream_local_bind_unlink() {
        let args = master_args("myhost", Path::new(CTL), None, None);
        let s = joined(&args);
        assert!(s.contains("StreamLocalBindUnlink=yes"), "missing in: {s}");
        assert!(s.contains("ControlMaster=yes"));
        assert!(s.contains("ControlPersist=60"));
        assert!(s.contains("ExitOnForwardFailure=yes"));
        assert!(s.contains("ServerAliveInterval=15"));
        assert!(s.contains("ServerAliveCountMax=3"));
        assert!(s.contains(&format!("ControlPath={CTL}")));
        assert_eq!(args.last().expect("host"), "myhost");
    }

    #[test]
    fn a_connect_timeout_is_set_only_when_asked_for() {
        let unbounded = joined(&master_args("myhost", Path::new(CTL), None, None));
        assert!(!unbounded.contains("ConnectTimeout"), "{unbounded}");
        let bounded = joined(&master_args("myhost", Path::new(CTL), Some(10), None));
        assert!(bounded.contains("ConnectTimeout=10"), "{bounded}");
        assert!(
            bounded.ends_with("myhost"),
            "the host is still last: {bounded}"
        );
    }

    #[test]
    fn only_ssh_s_own_ipqos_is_moved_off_the_lowest_class() {
        let config = |ipqos: &str| format!("user me\nipqos {ipqos}\nserveraliveinterval 0\n");
        assert_eq!(
            master_ipqos(&config("af21 cs1")),
            Some("af21 cs0"),
            "upstream's own pair, 7.8 to 10.0"
        );
        for kept in [
            "lowdelay throughput",
            "ef cs0",
            "none none",
            "af41 cs1",
            "cs1 cs1",
        ] {
            assert_eq!(master_ipqos(&config(kept)), None, "{kept} is left alone");
        }
        assert_eq!(master_ipqos(""), None, "an ssh that cannot say");

        let moved = joined(&master_args(
            "myhost",
            Path::new(CTL),
            None,
            Some("af21 cs0"),
        ));
        assert!(moved.contains("-o IPQoS=af21 cs0"), "{moved}");
        assert!(moved.ends_with("myhost"), "the host is still last: {moved}");
        let left = joined(&master_args("myhost", Path::new(CTL), None, None));
        assert!(!left.contains("IPQoS"), "{left}");
    }

    #[test]
    fn the_forward_client_does_not_repeat_master_only_options() {
        let s = joined(&forward_args(
            "myhost",
            Path::new(CTL),
            Path::new("/tmp/l.sock"),
            Path::new("/tmp/r.sock"),
        ));
        assert!(
            !s.contains("StreamLocalBindUnlink"),
            "pointless on the client: {s}"
        );
        assert!(!s.contains("ControlMaster"));
        assert!(s.contains("-O forward"));
        assert!(s.contains("-L /tmp/l.sock:/tmp/r.sock"));
    }

    /// Nothing was linked to it, so only its own socket reaches it.
    #[test]
    fn a_master_is_retired_at_its_own_socket() {
        let own = format!("{CTL}.abcdefgh");
        let s = joined(&exit_args("myhost", Path::new(&own)));
        assert_eq!(s, format!("-o ControlPath={own} -O exit myhost"));
    }

    #[test]
    fn a_forward_with_no_master_is_told_from_one_the_master_refused() {
        assert!(says_no_master(
            "Control socket connect(/tmp/nvmux-1000/cm-e2gisxlk): No such file or directory"
        ));
        assert!(says_no_master(
            "Control socket connect(/tmp/nvmux-1000/cm-e2gisxlk): Connection refused"
        ));
        assert!(!says_no_master(
            "mux_client_forward_request: forwarding request failed: Port forwarding failed"
        ));
    }

    /// End state of the race: `-O check` said nothing was there, but another
    /// nvmux's master now is. Removing it would leave that master unreachable.
    #[test]
    fn a_control_path_something_answers_on_is_used_rather_than_cleared() {
        let dir = crate::test_support::scratch_dir("ssh-answered");
        let ctl = dir.join("cm");
        let listener = std::os::unix::net::UnixListener::bind(&ctl).expect("bind");
        let ssh = Ssh::new("h".into(), ctl.clone());

        assert_eq!(probe(&ctl), Probe::Answered);
        assert_eq!(ssh.clear_control_path().expect("cleared"), Cleared::Taken);
        assert!(ctl.exists(), "a live master's socket was removed");

        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_control_path_nothing_answers_on_is_cleared() {
        let dir = crate::test_support::scratch_dir("ssh-refused");
        let ctl = dir.join("cm");
        crate::test_support::dead_socket(&ctl);
        let ssh = Ssh::new("h".into(), ctl.clone());

        assert_eq!(probe(&ctl), Probe::Refused, "a socket nothing listens on");
        assert_eq!(ssh.clear_control_path().expect("cleared"), Cleared::Empty);
        assert!(!ctl.exists(), "the dead socket was left in the way");

        assert_eq!(probe(&ctl), Probe::Absent);
        assert_eq!(
            ssh.clear_control_path().expect("nothing to clear"),
            Cleared::Empty
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_control_path_that_is_not_a_socket_is_left_alone_and_reported() {
        let dir = crate::test_support::scratch_dir("ssh-not-a-socket");
        let ctl = dir.join("cm");
        std::fs::write(&ctl, b"not a socket").expect("write");
        let ssh = Ssh::new("h".into(), ctl.clone());

        let err = ssh.clear_control_path().expect_err("refused");
        assert!(err.to_string().contains("not a socket"), "{err}");
        assert_eq!(std::fs::read(&ctl).expect("still there"), b"not a socket");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forwards_wanted_together_are_one_command_of_single_forwards() {
        let pairs = [
            (PathBuf::from("/tmp/a.sock"), PathBuf::from("/r/a.sock")),
            (PathBuf::from("/tmp/b.sock"), PathBuf::from("/r/b.sock")),
        ];
        assert_eq!(
            joined(&forward_all_args("h", Path::new(CTL), &pairs)),
            format!(
                "-o ControlPath={CTL} -O forward \
                 -L /tmp/a.sock:/r/a.sock -L /tmp/b.sock:/r/b.sock h"
            )
        );
        assert_eq!(
            forward_all_args("h", Path::new(CTL), &pairs[..1]),
            forward_args("h", Path::new(CTL), &pairs[0].0, &pairs[0].1)
        );
    }

    /// The banner may be on stdout or stderr; only a recognised one can be
    /// refused.
    #[test]
    fn the_local_ssh_is_judged_by_what_it_says_its_version_is() {
        use std::os::unix::process::ExitStatusExt;
        let said = |stdout: &str, stderr: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: stdout.into(),
            stderr: stderr.into(),
        };
        let current = said("", "OpenSSH_9.6p1 Ubuntu-3ubuntu13.19, OpenSSL 3.0.13\n");
        assert!(judge_version(&current).is_ok());
        assert!(matches!(
            judge_version(&said("", "OpenSSH_6.6p1, OpenSSL 1.0.1f 6 Jan 2014\n")),
            Err(SshError::TooOld { .. })
        ));
        assert!(matches!(
            judge_version(&said("OpenSSH_5.3p1\n", "")),
            Err(SshError::TooOld { .. })
        ));
        assert!(judge_version(&said("", "Dropbear v2022.83\n")).is_ok());

        let missing = LocalCheck(Err(std::io::Error::from(ErrorKind::NotFound)));
        assert!(matches!(missing.verdict(), Err(SshError::NotFound)));
    }

    #[test]
    fn forward_and_cancel_are_symmetric() {
        let (l, r) = (Path::new("/tmp/l.sock"), Path::new("/tmp/r.sock"));
        let f = joined(&forward_args("h", Path::new(CTL), l, r));
        let c = joined(&cancel_args("h", Path::new(CTL), l, r));
        assert_eq!(f.replace("-O forward", "-O cancel"), c);
    }

    #[test]
    fn the_host_is_passed_through_verbatim() {
        // Anything ssh accepts must survive: nvmux does not parse these.
        for host in [
            "myhost",
            "user@myhost",
            "my-config-alias",
            "1.2.3.4",
            "h-1_2.example",
        ] {
            assert!(master_args(host, Path::new(CTL), None, None).contains(&host.to_string()));
            assert!(check_args(host, Path::new(CTL)).contains(&host.to_string()));
        }
    }

    #[test]
    fn the_remote_command_runs_through_a_login_shell() {
        let args = shell_args("myhost", Path::new(CTL));
        let remote = args.last().expect("remote command");
        assert!(
            remote.contains(r#" -l -c "exec sh -s""#),
            "scripts must arrive on stdin of a login shell, with that shell out of the way: {remote}"
        );
        assert!(remote.ends_with(" nvmux"), "the sh's $0: {remote}");
        // $SHELL must expand in the remote sh: the login shell passes the
        // single-quoted word through untouched. Double-quoted, so a value with
        // a space does not split.
        assert!(
            remote.starts_with("exec sh -c '"),
            "the login shell would expand $SHELL itself: {remote}"
        );
        assert!(
            remote.contains(r#""${SHELL:-/bin/bash}""#),
            "SHELL must be double-quoted: {remote}"
        );
    }

    /// Scripts and arguments go down stdin, so no second shell layer re-parses
    /// a value.
    #[test]
    fn the_command_line_carries_no_script_and_no_arguments() {
        let a = shell_args("myhost", Path::new(CTL));
        assert_eq!(a, shell_args("myhost", Path::new(CTL)));
        assert!(
            !joined(&a).contains("\"$@\""),
            "arguments on the command line: {a:?}"
        );
    }

    #[test]
    fn the_shell_is_started_without_dash_n_and_without_a_tty() {
        let args = shell_args("myhost", Path::new(CTL));
        assert!(
            !args.iter().any(|a| a == "-n"),
            "-n would empty every script: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "-T"),
            "a forced tty would echo the scripts: {args:?}"
        );
        let args = unattended_shell_args("myhost", Path::new(CTL));
        assert!(!args.iter().any(|a| a == "-n"));
        assert!(args.iter().any(|a| a == "-T"));
    }

    fn which(name: &str) -> Option<PathBuf> {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .map(|d| d.join(name))
            .find(|c| c.is_file())
    }

    /// The whole chain under every login shell available: the remote command
    /// run as sshd runs it (`<login shell> -c <command>`, `$SHELL` set to it)
    /// and fed scripts as the transport does.
    ///
    /// fish and tcsh reject POSIX syntax in the outer layer, so `/bin/sh` alone
    /// proves nothing. Each shell's login profile sets a variable the script
    /// reads back; for the csh family, which cannot be a login shell and run a
    /// command, that is `.cshrc`. Missing shells are skipped.
    #[test]
    fn scripts_run_under_every_login_shell() {
        let home = crate::test_support::scratch_dir("ssh-login-shells");
        std::fs::create_dir_all(home.join(".config/fish")).expect("fish config dir");
        for (file, text) in [
            (".profile", "NVMUX_PROFILE=profile; export NVMUX_PROFILE\n"),
            (".zprofile", "export NVMUX_PROFILE=zprofile\n"),
            (
                ".config/fish/config.fish",
                "status is-login; and set -gx NVMUX_PROFILE fish-login\n",
            ),
            (".cshrc", "setenv NVMUX_PROFILE cshrc\n"),
        ] {
            std::fs::write(home.join(file), text).expect("write a profile");
        }
        let remote = remote_shell_command();
        let path = std::env::var_os("PATH").unwrap_or_default();

        for (name, profile) in [
            ("sh", "profile"),
            ("dash", "profile"),
            ("bash", "profile"),
            ("ksh", "profile"),
            ("mksh", "profile"),
            ("zsh", "zprofile"),
            ("fish", "fish-login"),
            ("tcsh", "cshrc"),
            ("csh", "cshrc"),
        ] {
            let Some(login_shell) = which(name) else {
                eprintln!("skipping {name}: not on $PATH");
                continue;
            };
            let mut cmd = Command::new(&login_shell);
            cmd.arg("-c")
                .arg(&remote)
                .env_clear()
                .env("PATH", &path)
                .env("HOME", &home)
                .env("SHELL", &login_shell)
                .env("XDG_CONFIG_HOME", home.join(".config"))
                .env("XDG_DATA_HOME", home.join(".local/share"));
            let mut shell = proc::Shell::start(&mut cmd, name).expect("start");
            let out = shell
                .run(
                    r#"printf '[%s]' "$NVMUX_PROFILE" "$1" "$2" "$3""#,
                    &["my project", "it's", "$(id)"],
                )
                .unwrap_or_else(|died| panic!("{died}: {}\nremote: {remote}", died.stderr));
            assert_eq!(
                out.stdout,
                format!("[{profile}][my project][it's][$(id)]"),
                "under {name}: no profile, or arguments mangled (stderr: {})",
                out.stderr
            );
        }
    }

    #[test]
    fn failures_are_classified_by_their_stderr() {
        assert!(matches!(
            classify("h", 255, "Permission denied (publickey)."),
            SshError::AuthFailed(_)
        ));
        assert!(matches!(
            classify(
                "h",
                255,
                "ssh: Could not resolve hostname h: Name or service not known"
            ),
            SshError::Unreachable(_)
        ));
        assert!(matches!(
            classify(
                "h",
                255,
                "ssh: connect to host h port 22: Connection refused"
            ),
            SshError::Unreachable(_)
        ));
        // A machine just woken up, per platform and under a reconnect's
        // bounded connect.
        for words in [
            "ssh: connect to host h port 22: Operation timed out",
            "ssh: connect to host h port 22: Host is down",
            "Connection timed out during banner exchange",
        ] {
            assert!(
                matches!(classify("h", 255, words), SshError::Unreachable(_)),
                "{words}"
            );
        }
        assert!(matches!(
            classify(
                "h",
                255,
                "Control socket connect(/tmp/cm): No such file or directory"
            ),
            SshError::NoMaster(_)
        ));
        assert!(matches!(
            classify("h", 255, "client_loop: send disconnect: Broken pipe"),
            SshError::MasterDied(_)
        ));
        // Unrecognised errors keep their text.
        match classify("h", 3, "something entirely new") {
            SshError::Failed { code, stderr } => {
                assert_eq!(code, 3);
                assert_eq!(stderr, "something entirely new");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
