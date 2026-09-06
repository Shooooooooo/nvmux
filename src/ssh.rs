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
//! tokens, which expand to unpredictable lengths — see [`crate::paths`].
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::SshError;
use crate::proc;
use crate::shell;

/// The minimum `ssh` nvmux supports.
///
/// 6.7 is where unix-domain socket forwarding (`-L <local_sock>:<remote_sock>`)
/// was added, which is the entire remote transport. `MIN_SSH` is the same
/// version as a tuple; a test ties the two together.
pub const MIN_SSH_VERSION: &str = "6.7";
const MIN_SSH: (u64, u64) = (6, 7);

/// Parse the version out of `ssh -V` output, e.g. `OpenSSH_9.6p1 Ubuntu-3...`.
pub fn parse_ssh_version(banner: &str) -> Option<(u64, u64)> {
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

/// Arguments for bringing up the shared master connection.
///
/// `StreamLocalBindUnlink=yes` belongs **here** and nowhere else: the master
/// performs the bind, so setting it on an `-O forward` client does nothing.
/// Without it, `-O cancel` leaves the local socket file on disk and the next
/// forward onto that path fails with rc 255 and `mux_client_forward: forwarding
/// request failed` — which breaks detach-then-reattach. Measured both ways.
pub fn master_args(host: &str, ctl: &Path) -> Vec<String> {
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
    args.extend(common(ctl));
    args.push(host.to_string());
    args
}

/// Ask whether the master is alive.
pub fn check_args(host: &str, ctl: &Path) -> Vec<String> {
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
pub fn forward_args(host: &str, ctl: &Path, local: &Path, remote: &Path) -> Vec<String> {
    forward_op("forward", host, ctl, local, remote)
}

/// Remove a forward.
pub fn cancel_args(host: &str, ctl: &Path, local: &Path, remote: &Path) -> Vec<String> {
    forward_op("cancel", host, ctl, local, remote)
}

/// Arguments for running a script on the remote host, through a **login** shell:
///
/// ```text
/// exec "${SHELL:-/bin/bash}" -l -c 'sh -s "$@"' nvmux <args...>
/// ```
///
/// A login shell because `ssh host cmd` does not read `.zprofile`, so `nvim` is
/// off `$PATH` on most real setups. `$SHELL` expands on the *remote* side —
/// [`shell::login_shell_wrapper`] is the one place that spells this, so the
/// tested form and the shipped form are the same string.
///
/// `-n` must never be added: with `sh -s` it silently yields an *empty* result,
/// because stdin comes from /dev/null and `sh` exits 0.
pub fn exec_args(host: &str, ctl: &Path, script_args: &[&str]) -> Vec<String> {
    let mut args = common(ctl);
    let remote = format!(
        "{} nvmux {}",
        shell::login_shell_wrapper(r#"sh -s "$@""#),
        shell::quote_all(script_args)
    );
    args.extend([host.to_string(), remote]);
    args
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
        || lower.contains("connection timed out")
        || lower.contains("connection refused")
        || lower.contains("network is unreachable")
    {
        return SshError::Unreachable(host.to_string());
    }
    if lower.contains("control socket connect")
        || lower.contains("no such file or directory") && lower.contains("control")
    {
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
        if self.is_master_alive() {
            return Ok(());
        }
        self.clear_stale_control_socket();
        // `-f` backgrounds ssh once authentication is done, so this blocks for
        // exactly as long as a key touch or passphrase takes.
        let out = self
            .run(&master_args(&self.host, &self.control_path))
            .map_err(|e| spawn_error(e, &self.host))?;
        if !out.status.success() {
            return Err(classify(
                &self.host,
                out.status.code().unwrap_or(-1),
                &String::from_utf8_lossy(&out.stderr),
            ));
        }
        Ok(())
    }

    /// Remove a ControlPath left behind by a master that died uncleanly.
    ///
    /// A SIGKILLed master does not unlink its socket, and starting a new one
    /// over the corpse does **not** fail: ssh prints `ControlSocket ... already
    /// exists, disabling multiplexing` and exits **0**. Multiplexing is then
    /// silently off and every later `-O forward` fails with no master. Verified.
    ///
    /// Only called once `-O check` has said nothing is listening, and only for
    /// a socket we own.
    fn clear_stale_control_socket(&self) {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let Ok(meta) = std::fs::symlink_metadata(&self.control_path) else {
            return;
        };
        if !meta.file_type().is_socket() {
            tracing::warn!(
                path = %self.control_path.display(),
                "refusing to remove a stale ControlPath that is not a socket"
            );
            return;
        }
        if meta.uid() != nix::unistd::geteuid().as_raw() {
            tracing::warn!(path = %self.control_path.display(), "stale ControlPath is not ours");
            return;
        }
        tracing::info!(path = %self.control_path.display(), "removing a stale ControlPath");
        let _ = std::fs::remove_file(&self.control_path);
    }

    /// Point a local socket at a remote one, idempotently.
    ///
    /// Cancelling first is what makes that hold. If the master already believes
    /// it is forwarding this pair, a second `-O forward` is a **silent no-op**:
    /// it exits 0 and does not recreate the socket file. A local socket that
    /// went missing while the forward was still registered would otherwise leave
    /// a forward that reports success and cannot be connected to.
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
            return Err(SshError::ForwardFailed {
                local: local.to_path_buf(),
                remote: remote.to_path_buf(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
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

    /// The script goes over stdin, never interpolated into the command line.
    pub fn run_script(&self, script: &str, args: &[&str]) -> Result<proc::Output, SshError> {
        let argv = exec_args(&self.host, &self.control_path, args);
        tracing::debug!(args = ?argv, "ssh exec");
        let mut cmd = Command::new("ssh");
        cmd.args(&argv);
        proc::run_feeding_stdin(&mut cmd, script).map_err(|e| spawn_error(e, &self.host))
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

    #[test]
    fn the_master_sets_stream_local_bind_unlink() {
        let args = master_args("myhost", Path::new(CTL));
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
            assert!(master_args(host, Path::new(CTL)).contains(&host.to_string()));
            assert!(check_args(host, Path::new(CTL)).contains(&host.to_string()));
        }
    }

    #[test]
    fn the_remote_command_runs_through_a_login_shell() {
        let args = exec_args("myhost", Path::new(CTL), &["/tmp/nvmux-0", "abcdefgh"]);
        let remote = args.last().expect("remote command");
        assert!(
            remote.contains("${SHELL:-/bin/bash}"),
            "no login shell: {remote}"
        );
        assert!(remote.contains(" -l -c "), "not a login shell: {remote}");
        assert!(
            remote.contains("sh -s"),
            "script must arrive on stdin: {remote}"
        );
        // $SHELL must expand remotely, so it must not be single-quoted — but it
        // must be double-quoted, or a value with a space word-splits there.
        assert!(!remote.contains("'${SHELL"), "SHELL was quoted: {remote}");
        assert!(
            remote.contains(r#""${SHELL:-/bin/bash}""#),
            "SHELL must be double-quoted: {remote}"
        );
    }

    /// `-n` plus `sh -s` silently yields an empty result rather than an error.
    #[test]
    fn the_exec_form_never_passes_dash_n() {
        let args = exec_args("myhost", Path::new(CTL), &["a"]);
        assert!(
            !args.iter().any(|a| a == "-n"),
            "-n would empty the script: {args:?}"
        );
    }

    #[test]
    fn script_arguments_survive_shell_metacharacters() {
        use std::process::Command;
        // Emulate what the remote shell does with the command ssh hands it.
        let args = exec_args("h", Path::new(CTL), &["my project", "it's", "$(id)"]);
        let remote = args.last().expect("remote").clone();
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(remote.replace("exec ", ""))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin
                    .take()
                    .expect("stdin")
                    .write_all(br#"printf '[%s]' "$1" "$2" "$3""#)?;
                c.wait_with_output()
            })
            .expect("run");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "[my project][it's][$(id)]",
            "arguments were mangled or evaluated"
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
