//! Running a shell script on the host that owns the sessions.
//!
//! Local and remote differ in exactly one primitive: how to run a POSIX `sh`
//! script and collect its stdout. Isolating it here keeps
//! [`crate::transport::protocol`] pure, shared and unit-testable, and lets the
//! same script file run verbatim on both paths.

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

/// Runs scripts somewhere.
pub trait Executor: Send + Sync {
    /// Run `script` under `/bin/sh` with `args` as `$1..$n`, delivered on
    /// **stdin** and never interpolated into a command string — see
    /// [`crate::shell`].
    fn run_script(&self, script: &str, args: &[&str]) -> Result<Output>;

    /// For error messages only.
    fn describe(&self) -> &str;
}

/// Runs scripts on this machine.
pub struct LocalExecutor;

impl Executor for LocalExecutor {
    fn run_script(&self, script: &str, args: &[&str]) -> Result<Output> {
        let mut child = Command::new("/bin/sh")
            .arg("-s")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(NvmuxError::Io)?;

        child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("no stdin on /bin/sh"))?
            .write_all(script.as_bytes())
            .map_err(NvmuxError::Io)?;

        let out = child.wait_with_output().map_err(NvmuxError::Io)?;
        Ok(Output {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            status: out.status.code().unwrap_or(-1),
        })
    }

    fn describe(&self) -> &str {
        "local"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_arguments_arrive_intact() {
        let out = LocalExecutor
            .run_script(
                r#"printf '[%s]' "$1" "$2" "$3""#,
                &["one", "two words", "it's"],
            )
            .expect("run");
        assert!(out.ok(), "stderr: {}", out.stderr);
        assert_eq!(out.stdout, "[one][two words][it's]");
    }

    #[test]
    fn shell_metacharacters_in_arguments_are_not_evaluated() {
        let out = LocalExecutor
            .run_script(r#"printf '%s' "$1""#, &["$(echo pwned)"])
            .expect("run");
        assert_eq!(out.stdout, "$(echo pwned)", "argument was re-evaluated");
    }

    #[test]
    fn a_failing_script_reports_its_status_not_an_error() {
        let out = LocalExecutor.run_script("exit 3", &[]).expect("run");
        assert_eq!(out.status, 3);
        assert!(!out.ok());
    }
}
