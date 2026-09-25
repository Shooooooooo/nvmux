//! Driving the `ssh` client.
//!
//! A subprocess rather than a Rust SSH library, so that `~/.ssh/config`,
//! `ProxyJump`, agent forwarding and hardware keys all work unmodified. The
//! `openssh` crate does not cleanly expose `ssh -O forward`, which is the whole
//! mechanism here.
//!
//! One `ControlMaster` per host, brought up by [`master_args`]; each session
//! then adds a unix-socket forward onto that existing master with `-O forward`,
//! removed with `-O cancel`, tested with `-O check`.
//!
//! `ControlPath` is computed by nvmux rather than left to ssh's `%C`/`%h%p%r`
//! tokens, which expand to unpredictable lengths — see [`crate::paths`]. It is
//! also not where a master binds: each binds a socket of its own and is linked
//! to the `ControlPath` once it is up, so that no master can ever remove
//! another's — see [`Ssh::ensure_master_within`].
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::SshError;
use crate::paths;
use crate::proc;
use crate::shell;

/// The minimum `ssh` nvmux supports.
///
/// 6.7 is where unix-domain socket forwarding (`-L <local_sock>:<remote_sock>`)
/// was added, which is the entire remote transport. `MIN_SSH` is the same
/// version as a tuple; a test ties the two together.
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

/// Refuse early if the local `ssh` is missing or too old, so the error names
/// the real problem rather than surfacing later as a failed forward.
pub fn check_local() -> Result<(), SshError> {
    let out = Command::new("ssh")
        .arg("-V")
        .output()
        .map_err(|_| SshError::NotFound)?;
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
        // An unrecognised banner is not worth refusing over — plenty of forks
        // exist, and the forward itself will fail clearly enough.
        None => Ok(()),
    }
}

/// Options shared by every invocation. `BatchMode` is deliberately **not** set:
/// a passphrase prompt, a hardware key touch or a 2FA challenge must still work.
fn common(ctl: &Path) -> Vec<String> {
    vec!["-o".into(), format!("ControlPath={}", ctl.display())]
}

/// How long an unattended command waits to reach the host. Only a *connect*
/// budget: a listing that is merely slow still lands, and nothing here bounds
/// the host's own thinking time.
const UNATTENDED_CONNECT_SECS: u64 = 5;

/// Options for a command nvmux asked for rather than one the user did — so far
/// only the create prompt's directory completion, which runs on a worker thread
/// with nobody watching it.
///
/// This is where `BatchMode=yes` belongs, and it is the exact inverse of the
/// reasoning above rather than a contradiction of it. A prompt the user can
/// answer is right for a command they asked for; for a listing they did not ask
/// for it is a disaster, because there is no terminal to answer it on — the
/// screen belongs to ratatui — and the child would sit on a passphrase prompt
/// forever, holding the worker thread with it. `ssh -O check` cannot be used to
/// pre-empt that either: it asks the local multiplexing socket, so it answers
/// yes for a master whose connection is wedged.
///
/// So: never ask, and give up on an unreachable host rather than hanging. The
/// cost of being wrong is a suggestion the user did not get; the cost of the
/// interactive options here would be a prompt that quietly stops completing and
/// a thread that never ends.
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

/// Arguments for bringing up a master connection, bound at `own`: its own
/// socket, not the shared `ControlPath` — see [`Ssh::ensure_master_within`].
///
/// `StreamLocalBindUnlink=yes` belongs **here** and nowhere else: the master
/// performs the bind, so setting it on an `-O forward` client does nothing.
/// Without it, `-O cancel` leaves the local socket file on disk and the next
/// forward onto that path fails with rc 255 and `mux_client_forward: forwarding
/// request failed` — which breaks detach-then-reattach. Measured both ways.
///
/// `connect_timeout` bounds how long the connection attempt itself may take,
/// in seconds; `None` leaves it to the system's TCP timeout, which is what the
/// first connection wants — the user is watching it, and can stop it. A
/// *re*connection is different: it is one of a series, and a link that is still
/// down drops packets rather than refusing them, so without a bound each
/// attempt would hang for the minutes the kernel allows and the series would
/// never get to its next try. See [`crate::reconnect`].
fn master_args(host: &str, own: &Path, connect_timeout: Option<u64>) -> Vec<String> {
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
        // Notice a dead link rather than hanging on a half-open TCP connection.
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ];
    if let Some(secs) = connect_timeout {
        args.extend(["-o".into(), format!("ConnectTimeout={secs}")]);
    }
    args.extend(common(own));
    args.push(host.to_string());
    args
}

/// Ask the master answering on `own` to exit.
fn exit_args(host: &str, own: &Path) -> Vec<String> {
    let mut args = common(own);
    args.extend(["-O".into(), "exit".into(), host.to_string()]);
    args
}

/// Ask whether the master is alive.
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

/// Add a unix-socket forward to the existing master, without reconnecting.
fn forward_args(host: &str, ctl: &Path, local: &Path, remote: &Path) -> Vec<String> {
    forward_op("forward", host, ctl, local, remote)
}

/// Remove a forward.
fn cancel_args(host: &str, ctl: &Path, local: &Path, remote: &Path) -> Vec<String> {
    forward_op("cancel", host, ctl, local, remote)
}

/// Arguments for starting the host's shell — the one `sh` that
/// [`proc::Shell`] feeds every script nvmux runs there — through a **login**
/// shell:
///
/// ```text
/// ssh -T <host> exec "${SHELL:-/bin/bash}" -l -c 'exec sh -s' nvmux
/// ```
///
/// A login shell because `ssh host cmd` does not read `.zprofile`, so `nvim` is
/// off `$PATH` on most real setups. `$SHELL` expands on the *remote* side —
/// [`shell::login_shell_wrapper`] is the one place that spells this, so the
/// tested form and the shipped form are the same string. It is paid once per
/// host: the scripts, and their arguments, travel down this shell's stdin.
///
/// `-T`, because that stdin is the command stream: a host with `RequestTTY
/// force` in its config would otherwise get a pty, and a pty echoes every
/// script back into the output. `-n` must never be added: it puts `/dev/null`
/// on stdin, and `sh -s` then reads nothing, prints nothing and exits 0 — an
/// *empty* result, silently, rather than an error.
fn shell_args(host: &str, ctl: &Path) -> Vec<String> {
    shell_args_with(common(ctl), host)
}

/// The same command, with the options an unattended shell needs — see
/// [`unattended`]. Identical in every other respect, so what runs on the host is
/// the same program reached the same way.
fn unattended_shell_args(host: &str, ctl: &Path) -> Vec<String> {
    shell_args_with(unattended(ctl), host)
}

fn shell_args_with(mut args: Vec<String>, host: &str) -> Vec<String> {
    args.push("-T".into());
    args.extend([host.to_string(), remote_shell_command()]);
    args
}

/// What the login shell is told to run. `exec`, so the login shell is gone once
/// `sh` is up: nothing of its own — a `.bash_logout`, say — runs when the stream
/// closes, and nothing of it sits between ssh and the shell. `nvmux` is that
/// login shell's `$0`, which is how it shows up in a process listing.
fn remote_shell_command() -> String {
    format!("{} nvmux", shell::login_shell_wrapper("exec sh -s"))
}

/// Turn an ssh failure into something a caller can act on. ssh reports almost
/// everything as exit 255, so stderr is the only signal available.
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

/// Whether ssh's words say there is no master behind the `ControlPath`:
/// nothing there to connect to, or a socket whose master has gone.
fn says_no_master(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("control socket connect")
        || lower.contains("no such file or directory") && lower.contains("control")
}

/// What connecting to the `ControlPath` says — the question `ssh -O check`
/// asks, without the fork, and so without an answer that has had
/// milliseconds to go stale by the time it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// Something accepted: in a directory only we can write to, a master.
    Answered,
    /// Nothing is there.
    Absent,
    /// Something is there and nothing accepted: a socket whose master has
    /// gone, or not a socket at all.
    Refused,
}

fn probe(path: &Path) -> Probe {
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => Probe::Answered,
        Err(e) if e.kind() == ErrorKind::NotFound => Probe::Absent,
        Err(_) => Probe::Refused,
    }
}

/// What is on the `ControlPath` once anything dead there has been cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cleared {
    /// Nothing: a master can be linked there.
    Empty,
    /// A master answered, so there is one to use and nothing to start.
    Taken,
}

/// A driver for one host's ssh connection.
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

    /// The `ControlPath` of this host's master connection. Handed out so that a
    /// second, `Send` driver onto the *same* master can be built without
    /// recomputing it — see [`crate::dirs::DirSource`].
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

    /// Is a master connection up for this host?
    pub fn is_master_alive(&self) -> bool {
        self.run(&check_args(&self.host, &self.control_path))
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Bring up a master connection, or confirm the existing one. Reusing a live
    /// master is what makes per-session forwards cheap.
    pub fn ensure_master(&self) -> Result<(), SshError> {
        self.ensure_master_within(None)
    }

    /// The same, with the connection attempt bounded to `connect_timeout`
    /// seconds — see [`master_args`] for when that is wanted.
    ///
    /// A master binds a socket of its own, [`paths::master_socket`], and is
    /// linked to the `ControlPath` once it is up rather than bound there. The
    /// reason is what ssh does on its way out: it removes its `ControlPath` by
    /// name, whoever's socket the name holds by then. Every nvmux talking to
    /// one host shares that name, and a master that has lost it — to anything
    /// that deletes the file — keeps running for as long as it has clients,
    /// then takes the name from under whichever master was linked there since.
    /// That master's shell is still up, so everything that asks it says the
    /// link is fine, and every `-O forward` fails for want of a socket.
    /// Verified. Bound at a name of its own, a master that leaves removes only
    /// that.
    ///
    /// The link is also what settles two nvmux bringing masters up at once:
    /// the first to link wins, and the other master is told to exit. Bound at
    /// the shared name, the second would have found it taken, which ssh
    /// answers by switching multiplexing off and staying up regardless — an
    /// idle connection nothing could reach, for as long as the network held.
    ///
    /// Which leaves every master's linked name behind when it exits: a socket
    /// nothing listens on, cleared by [`Self::clear_control_path`] before the
    /// next master is linked there.
    pub fn ensure_master_within(&self, connect_timeout: Option<u64>) -> Result<(), SshError> {
        if self.is_master_alive() {
            return Ok(());
        }
        if self.clear_control_path()? == Cleared::Taken {
            return Ok(());
        }
        let nonce = crate::ids::nonce().map_err(|e| SshError::Failed {
            code: -1,
            stderr: format!("naming a master connection to {}: {e}", self.host),
        })?;
        let own = paths::master_socket(&self.control_path, &nonce);
        // `-f` backgrounds ssh once authentication is done, so this blocks for
        // exactly as long as a key touch or passphrase takes.
        let out = self
            .run(&master_args(&self.host, &own, connect_timeout))
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

    /// Make way on the `ControlPath` for a new master — or find that one has
    /// arrived there since `ssh -O check` looked.
    ///
    /// What a dead master leaves there, cleanly or killed outright, is a
    /// socket nothing listens on, and a name taken is a name no new master can
    /// be linked to. So a dead one is removed: only a socket, only ours, and
    /// only once connecting to it has been refused — never on the word of
    /// `-O check`, whose answer is a fork old by the time it is read. Another
    /// nvmux can link its master in that gap, and removing that master's
    /// socket on `-O check`'s say-so is how a master came to be running with
    /// no name to be reached by — 11 times in 260 when a second nvmux began
    /// its restart about as long after the first as a master takes to come
    /// up. A socket that accepts is that master, and is used rather than
    /// joined by a second.
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

    /// Link the master answering on `own` to the `ControlPath`, where every
    /// other ssh command finds it — unless another master got there first, in
    /// which case that one serves and this one is told to go.
    fn publish(&self, own: &Path) -> Result<(), SshError> {
        // Round again only when the name is found dead, or gone, between the
        // failed link and the look at what took it.
        for _ in 0..3 {
            let taken = match std::fs::hard_link(own, &self.control_path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => self.clear_control_path(),
                // The master went between `-f` returning and here, and took
                // its socket with it.
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

    /// Tell the master answering on `own` to exit. Nothing was ever linked to
    /// it, so no client can be on it: this ends the connection it holds and
    /// nothing else. Left alone it would still go, a minute later, by
    /// `ControlPersist`.
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
    /// Cancelling first is what makes that hold. If the master already believes
    /// it is forwarding this pair, a second `-O forward` is a **silent no-op**:
    /// it exits 0 and does not recreate the socket file. A local socket that
    /// went missing while the forward was still registered would otherwise leave
    /// a forward that reports success and cannot be connected to.
    ///
    /// No master to forward on is [`SshError::NoMaster`] rather than a failed
    /// forward, because it is the one failure a caller can do something about:
    /// bring a master back, and ask again.
    pub fn forward(&self, local: &Path, remote: &Path) -> Result<(), SshError> {
        // Cancelling a forward that does not exist fails; that is the common
        // case, not a problem.
        let _ = self.run(&cancel_args(&self.host, &self.control_path, local, remote));

        // `-O cancel` exits 0 without removing the file, and a forward onto an
        // existing path fails hard. The master's `StreamLocalBindUnlink` covers
        // this too, but an older nvmux's master might not have it.
        if let Err(e) = std::fs::remove_file(local) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %local.display(), error = %e, "could not clear local socket");
            }
        }

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

    /// Remove a forward and the local socket it left behind.
    pub fn cancel(&self, local: &Path, remote: &Path) {
        let _ = self.run(&cancel_args(&self.host, &self.control_path, local, remote));
        // `-O cancel` returns 0 without removing the file.
        let _ = std::fs::remove_file(local);
    }

    /// Start this host's shell. Every script goes down its stdin from then on,
    /// never interpolated into a command line — see [`shell_args`].
    ///
    /// Returns once `ssh` exists, not once the host has answered: the shell's
    /// first run reads the handshake, so a host that turns out to be
    /// unreachable is reported by that run, classified from ssh's stderr.
    pub fn start_shell(&self) -> Result<proc::Shell, SshError> {
        self.start(shell_args(&self.host, &self.control_path))
    }

    /// The same, for scripts nvmux runs on its own account: this shell never
    /// prompts and gives up on an unreachable host. See [`unattended`].
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

/// The `ControlPath` is occupied by something no master can be linked over.
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

    /// The advertised minimum and the one the check actually applies must be
    /// the same version; they used to live in different modules.
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

    /// The one difference between the shell the user's own commands run in and
    /// the one that answers questions nvmux asked on its own account. Getting
    /// this backwards either way is bad: `BatchMode` on the interactive path
    /// breaks every key that needs a passphrase or a touch, and its absence on
    /// the completion path leaves a worker thread sitting on a prompt no one
    /// can see, let alone answer.
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

    /// Otherwise the two would drift, and a completion would end up running a
    /// different program, or reaching it a different way, from every other
    /// script — which is exactly the sort of difference that is only ever found
    /// in the field.
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
        let args = master_args("myhost", Path::new(CTL), None);
        let s = joined(&args);
        // This one is load-bearing: without it `-O cancel` leaves the socket
        // file and the next forward fails rc 255, breaking re-attach.
        assert!(s.contains("StreamLocalBindUnlink=yes"), "missing in: {s}");
        assert!(s.contains("ControlMaster=yes"));
        assert!(s.contains("ControlPersist=60"));
        assert!(s.contains("ExitOnForwardFailure=yes"));
        assert!(s.contains("ServerAliveInterval=15"));
        assert!(s.contains("ServerAliveCountMax=3"));
        assert!(s.contains(&format!("ControlPath={CTL}")));
        assert_eq!(args.last().expect("host"), "myhost");
    }

    /// The first connection waits as long as the system does; a reconnection
    /// is bounded, and only then.
    #[test]
    fn a_connect_timeout_is_set_only_when_asked_for() {
        let unbounded = joined(&master_args("myhost", Path::new(CTL), None));
        assert!(!unbounded.contains("ConnectTimeout"), "{unbounded}");
        let bounded = joined(&master_args("myhost", Path::new(CTL), Some(10)));
        assert!(bounded.contains("ConnectTimeout=10"), "{bounded}");
        assert!(
            bounded.ends_with("myhost"),
            "the host is still last: {bounded}"
        );
    }

    /// Setting it on the forward client does nothing, because the master does
    /// the bind — so its absence there is correct, not an oversight.
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

    /// A master nothing was ever linked to can only be reached at the socket
    /// it bound, so that is where it is told to go.
    #[test]
    fn a_master_is_retired_at_its_own_socket() {
        let own = format!("{CTL}.abcdefgh");
        let s = joined(&exit_args("myhost", Path::new(&own)));
        assert_eq!(s, format!("-o ControlPath={own} -O exit myhost"));
    }

    /// Both ways ssh says there is no master behind the `ControlPath` — no
    /// socket, and a socket nothing listens on — and not the refusal a
    /// working master gives a forward it cannot make.
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

    /// The race this guards against, reduced to its end state: `-O check`
    /// has already said nothing is there, and by the time the path is looked
    /// at, another nvmux's master is. Removing it would leave that master
    /// running with no name to be reached by.
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

    /// What a master leaves behind — every one now, since only its own name
    /// goes with it — is cleared, so the next can be linked in its place.
    #[test]
    fn a_control_path_nothing_answers_on_is_cleared() {
        let dir = crate::test_support::scratch_dir("ssh-refused");
        let ctl = dir.join("cm");
        drop(std::os::unix::net::UnixListener::bind(&ctl).expect("bind"));
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

    /// Only a socket is ever removed: whatever else is there is reported
    /// rather than deleted, before a connection is spent on a master that
    /// could not be linked over it.
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
            assert!(master_args(host, Path::new(CTL), None).contains(&host.to_string()));
            assert!(check_args(host, Path::new(CTL)).contains(&host.to_string()));
        }
    }

    #[test]
    fn the_remote_command_runs_through_a_login_shell() {
        let args = shell_args("myhost", Path::new(CTL));
        let remote = args.last().expect("remote command");
        assert!(
            remote.contains("${SHELL:-/bin/bash}"),
            "no login shell: {remote}"
        );
        assert!(remote.contains(" -l -c "), "not a login shell: {remote}");
        assert!(
            remote.contains("exec sh -s"),
            "scripts must arrive on stdin, with the login shell out of the way: {remote}"
        );
        assert!(remote.ends_with(" nvmux"), "the login shell's $0: {remote}");
        // $SHELL must expand remotely, so it must not be single-quoted — but it
        // must be double-quoted, or a value with a space word-splits there.
        assert!(!remote.contains("'${SHELL"), "SHELL was quoted: {remote}");
        assert!(
            remote.contains(r#""${SHELL:-/bin/bash}""#),
            "SHELL must be double-quoted: {remote}"
        );
    }

    /// The command line carries nothing that varies per script: scripts and
    /// their arguments go down the shell's stdin, so there is no second shell
    /// layer for a value to be re-parsed by.
    #[test]
    fn the_command_line_carries_no_script_and_no_arguments() {
        let a = shell_args("myhost", Path::new(CTL));
        assert_eq!(a, shell_args("myhost", Path::new(CTL)));
        assert!(
            !joined(&a).contains("\"$@\""),
            "arguments on the command line: {a:?}"
        );
    }

    /// `-n` plus `sh -s` silently yields an empty result rather than an error,
    /// and a pty would echo every script back into its own output.
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

    /// The whole chain, locally: the command ssh would hand the remote login
    /// shell, run by a shell here, with scripts and arguments fed to it exactly
    /// as the transport feeds them.
    #[test]
    fn script_arguments_survive_shell_metacharacters() {
        let args = shell_args("h", Path::new(CTL));
        let remote = args.last().expect("remote").clone();
        // What sshd does with the command: hand it to a shell. Minus the
        // `exec`s, so the login shell and `sh` are this test's children rather
        // than its replacements.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(remote.replace("exec ", ""));
        let mut shell = proc::Shell::start(&mut cmd, "as sshd would").expect("start");
        let out = shell
            .run(
                r#"printf '[%s]' "$1" "$2" "$3""#,
                &["my project", "it's", "$(id)"],
            )
            .expect("the shell is alive");
        assert_eq!(
            out.stdout, "[my project][it's][$(id)]",
            "arguments were mangled or evaluated (stderr: {})",
            out.stderr
        );
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
        // What a machine that has just woken up says, on each platform and
        // under the bounded connect a reconnection uses.
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
        // Anything unrecognised keeps its text rather than being guessed at.
        match classify("h", 3, "something entirely new") {
            SshError::Failed { code, stderr } => {
                assert_eq!(code, 3);
                assert_eq!(stderr, "something entirely new");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
