//! Running a shell script in a child process and collecting what it said.
//!
//! Both halves of nvmux do this: locally through `/bin/sh`, remotely through
//! `ssh`. They differ only in the command they build and how they classify a
//! failure to spawn it, so the plumbing lives here and the error mapping stays
//! with each caller.
//!
//! The script always travels on **stdin** and its variable parts as positional
//! arguments — never interpolated into a command string. See [`crate::shell`].

use std::io::Write;
use std::process::{Command, Stdio};

use crate::error::{NvmuxError, Result};

/// What a script run produced.
#[derive(Debug, Clone)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub status: i32,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status == 0
    }
}

impl From<std::process::Output> for Output {
    fn from(out: std::process::Output) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            status: out.status.code().unwrap_or(-1),
        }
    }
}

/// Spawn `cmd`, feed it `script` on stdin, and wait for it.
///
/// Returns `io::Result` rather than a crate error so each caller keeps its own
/// classification — `ssh` in particular has to tell "no ssh binary" from "ssh
/// ran and failed", which the `io::ErrorKind` is the only signal for.
pub fn run_feeding_stdin(cmd: &mut Command, script: &str) -> std::io::Result<Output> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("child has no stdin"))?
        .write_all(script.as_bytes())?;

    Ok(child.wait_with_output()?.into())
}

/// Run `script` under this machine's `/bin/sh` with `args` as `$1..$n`.
pub fn run_local(script: &str, args: &[&str]) -> Result<Output> {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-s").args(args);
    run_feeding_stdin(&mut cmd, script).map_err(NvmuxError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_arguments_arrive_intact() {
        let out = run_local(
            r#"printf '[%s]' "$1" "$2" "$3""#,
            &["one", "two words", "it's"],
        )
        .expect("run");
        assert!(out.ok(), "stderr: {}", out.stderr);
        assert_eq!(out.stdout, "[one][two words][it's]");
    }

    #[test]
    fn shell_metacharacters_in_arguments_are_not_evaluated() {
        let out = run_local(r#"printf '%s' "$1""#, &["$(echo pwned)"]).expect("run");
        assert_eq!(out.stdout, "$(echo pwned)", "argument was re-evaluated");
    }

    /// A missing binary must reach the caller as `NotFound` and not be flattened
    /// into a generic failure: `ssh.rs` classifies on exactly this to tell "ssh
    /// is not installed" from "ssh ran and could not connect".
    #[test]
    fn a_missing_binary_keeps_its_error_kind() {
        let err = run_feeding_stdin(&mut Command::new("nvmux-no-such-binary"), "echo hi")
            .expect_err("must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err}");
    }

    #[test]
    fn a_failing_script_reports_its_status_not_an_error() {
        let out = run_local("exit 3", &[]).expect("run");
        assert_eq!(out.status, 3);
        assert!(!out.ok());
    }
}
