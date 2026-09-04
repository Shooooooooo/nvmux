//! Driving the `ssh` client. Milestone 5.
//!
//! # Why a subprocess and not a Rust SSH library
//!
//! nvmux shells out to the system `ssh` so that `~/.ssh/config`, `ProxyJump`,
//! agent forwarding and hardware keys all work unmodified. Reimplementing that
//! surface is a project in itself. The `openssh` crate is also unsuitable: it
//! does not cleanly expose `ssh -O forward`, which is the whole mechanism here.
//!
//! # Shape
//!
//! One `ControlMaster` per host, reused for everything:
//!
//! ```text
//! ssh -M -N -f -o ControlMaster=yes -o ControlPath=<short> \
//!     -o ControlPersist=60 -o ExitOnForwardFailure=yes \
//!     -o ServerAliveInterval=15 -o ServerAliveCountMax=3 \
//!     -o StreamLocalBindUnlink=yes <host>
//! ```
//!
//! then per session, onto the *existing* master with no reconnection:
//!
//! ```text
//! ssh -o ControlPath=<ctl> -O forward -L <local_sock>:<remote_sock> <host>
//! ```
//!
//! with `-O cancel` to remove one and `-O check` to test the master.
//!
//! # Two things verified the hard way
//!
//! * `StreamLocalBindUnlink=yes` must be on the **master** invocation. Setting
//!   it on the `-O forward` client does nothing, because the master performs the
//!   bind. Without it: `-O cancel` exits 0 but leaves the local socket file on
//!   disk, and the next `-O forward` onto that path fails with rc 255 and
//!   `mux_client_forward: forwarding request failed`. That breaks
//!   detach-then-reattach, which is the flow this tool exists for. Measured both
//!   ways. nvmux also unlinks the local path itself before every forward, since
//!   belt and braces costs one syscall.
//! * `ssh -O forward` to a *nonexistent* remote socket still exits 0 and creates
//!   a working local socket. A successful forward is therefore no evidence that
//!   the remote Neovim is alive; only an RPC round trip through it is.
//!
//! `ControlPath` is computed by nvmux rather than left to ssh's `%C`/`%h%p%r`
//! tokens, which expand to unpredictable lengths — see [`crate::config`].

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::SshError;
use crate::shell;

/// The minimum ssh nvmux supports: 6.7 added unix-socket forwarding.
pub use crate::nvim::MIN_SSH_VERSION;

/// Options shared by every invocation, master or otherwise.
///
/// `BatchMode` is deliberately **not** set: nvmux drives the user's own ssh so
/// that a passphrase prompt, a hardware key touch, or a 2FA challenge still
/// works. Those all happen on the master connection, once.
fn common(ctl: &Path) -> Vec<String> {
    vec!["-o".into(), format!("ControlPath={}", ctl.display())]
}

/// Arguments for bringing up the shared master connection.
///
/// `StreamLocalBindUnlink=yes` belongs **here** and nowhere else. The master
/// performs the bind, so setting it on an `-O forward` client does nothing —
/// and without it, `-O cancel` leaves the local socket file on disk and the
/// next forward onto that path fails with rc 255 and
/// `mux_client_forward: forwarding request failed`. That breaks
/// detach-then-reattach, which is the flow this tool exists for. Measured both
/// ways.
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

/// Add a unix-socket forward to the existing master, without reconnecting.
pub fn forward_args(host: &str, ctl: &Path, local: &Path, remote: &Path) -> Vec<String> {
    let mut args = common(ctl);
    args.extend([
        "-O".into(),
        "forward".into(),
        "-L".into(),
        format!("{}:{}", local.display(), remote.display()),
        host.to_string(),
    ]);
    args
}

/// Remove a forward.
pub fn cancel_args(host: &str, ctl: &Path, local: &Path, remote: &Path) -> Vec<String> {
    let mut args = common(ctl);
    args.extend([
        "-O".into(),
        "cancel".into(),
        "-L".into(),
        format!("{}:{}", local.display(), remote.display()),
        host.to_string(),
    ]);
    args
}

/// Shut the master down.
pub fn exit_args(host: &str, ctl: &Path) -> Vec<String> {
    let mut args = common(ctl);
    args.extend(["-O".into(), "exit".into(), host.to_string()]);
    args
}

/// Arguments for running a script on the remote host, through a **login** shell.
///
/// `ssh host cmd` gives a non-login, non-interactive shell. On most real setups
/// that means `nvim` and every language server are not on `$PATH`, because they
/// were put there by `.zprofile` or `.bash_profile`, which only a login shell
/// reads. Sessions would fail to spawn with a confusing "command not found".
///
/// The composed remote command is:
///
/// ```text
/// exec ${SHELL:-/bin/bash} -l -c 'sh -s "$@"' nvmux <args...>
/// ```
///
/// `$SHELL` expands on the *remote* side, so it is the remote user's shell, with
/// `bash -l` as the fallback where it is unset. The login shell then runs
/// `sh -s "$@"`, which reads the script from stdin and receives the arguments as
/// ordinary positional parameters it never re-parses — so a session name
/// containing quotes, spaces or `$(...)` is data, not code.
///
/// Note `-n` must never be added: combined with `sh -s` it silently produces an
/// *empty* result, because stdin comes from /dev/null and `sh` reads an empty
/// script and exits 0.
pub fn exec_args(host: &str, ctl: &Path, script_args: &[&str]) -> Vec<String> {
    let mut args = common(ctl);
    let remote = format!(
        "exec ${{SHELL:-/bin/bash}} -l -c {} nvmux {}",
        shell::quote(r#"sh -s "$@""#),
        shell::quote_all(script_args)
    );
    args.extend([host.to_string(), remote]);
    args
}

/// Turn an ssh failure into something a caller can act on.
///
/// ssh reports almost everything as exit 255, so the stderr text is the only
/// signal available.
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

    /// Bring up a master connection, or confirm the existing one.
    ///
    /// Reusing a live master is what makes per-session forwards cheap: adding
    /// one costs a round trip on an established connection rather than a fresh
    /// handshake.
    pub fn ensure_master(&self) -> Result<(), SshError> {
        if self.is_master_alive() {
            return Ok(());
        }
        // `-f` backgrounds ssh once authentication is done, so this call blocks
        // for exactly as long as the user needs to touch a key or type a
        // passphrase, and returns when the connection is usable.
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

    /// Point a local socket at a remote one.
    ///
    /// Idempotent, which takes more than it looks like. Three things have to be
    /// true afterwards: the master holds this forward, the local socket file
    /// exists, and it works.
    ///
    /// Cancelling first is what makes that hold. If the master already believes
    /// it is forwarding this pair, a second `-O forward` is a **silent no-op**:
    /// it exits 0 and does not recreate the socket file. So a local socket that
    /// went missing while the master still had the forward registered — a
    /// detach that removed the file, a `/tmp` reaper, a crash — would leave a
    /// forward that reports success and cannot be connected to, and no amount
    /// of retrying would fix it.
    pub fn forward(&self, local: &Path, remote: &Path) -> Result<(), SshError> {
        // Ignoring the result: cancelling a forward that does not exist fails,
        // which is exactly the common case and not a problem.
        let _ = self.run(&cancel_args(&self.host, &self.control_path, local, remote));

        // Then unlink. `-O cancel` exits 0 without removing the file, and a
        // forward onto an existing path fails hard. `StreamLocalBindUnlink` on
        // the master covers this too; doing it here as well costs one syscall
        // and means a master started by an older nvmux cannot wedge us.
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

    pub fn exit_master(&self) {
        let _ = self.run(&exit_args(&self.host, &self.control_path));
    }

    /// Run a script on the remote host and collect its output.
    pub fn run_script(
        &self,
        script: &str,
        args: &[&str],
    ) -> Result<crate::transport::exec::Output, SshError> {
        use std::io::Write;

        let argv = exec_args(&self.host, &self.control_path, args);
        tracing::debug!(args = ?argv, "ssh exec");
        let mut child = Command::new("ssh")
            .args(&argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| spawn_error(e, &self.host))?;

        // The script goes over stdin, never interpolated into the command line.
        child
            .stdin
            .take()
            .ok_or_else(|| SshError::Failed {
                code: -1,
                stderr: "no stdin on ssh".into(),
            })?
            .write_all(script.as_bytes())
            .map_err(|e| spawn_error(e, &self.host))?;

        let out = child
            .wait_with_output()
            .map_err(|e| spawn_error(e, &self.host))?;
        Ok(crate::transport::exec::Output {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            status: out.status.code().unwrap_or(-1),
        })
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
        // $SHELL must expand remotely, so it must not be single-quoted.
        assert!(!remote.contains("'${SHELL"), "SHELL was quoted: {remote}");
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
