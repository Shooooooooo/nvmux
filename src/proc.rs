//! Run shell scripts in a child shell and collect their output: locally via
//! `/bin/sh`, remotely via `ssh`. Callers classify start failures themselves.
//!
//! Scripts travel on **stdin** with their variable parts as positional
//! arguments, never interpolated into a command line (see [`crate::shell`]).
//!
//! # One shell per host
//!
//! [`Shell`] keeps one `sh` per host and sends every script down its stdin. A
//! shell per script cost more than the scripts (ten listings: 51 ms vs 20 ms),
//! and over ssh meant a login shell per script (100 ms with an empty profile,
//! unbounded with a real one). [`run_local`] and [`run_feeding_stdin`] are
//! one-shot forms kept for tests.
//!
//! # Each run in a subshell
//!
//! The scripts assume a shell that exits with them (`exit` on errors, `cd`,
//! `umask`, `export`, prelude caches), so each run is wrapped in `( … )` and
//! the parent never changes state. Cost: a fork without exec (0.4 ms) plus
//! re-reading the prelude (0.3 ms), vs 5 ms for a fresh shell.
//!
//! # Framing
//!
//! After the subshell the parent prints a marker line with the exit status.
//! The marker holds a secret generated at start and sent only on stdin, so
//! script output cannot forge it. The needle starts with the newline *before*
//! the marker, so a run's stdout is byte-identical to a one-shot shell's for
//! the parsers in [`crate::transport::protocol`].

use std::fmt;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{NvmuxError, Result};
use crate::pty::{pollfd, ready};
use crate::shell;

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

/// Spawn `cmd`, feed it `script` on stdin, and wait. One-shot form, kept for
/// tests. Returns `io::Result`, as [`Shell::start`] does, so a failed spawn
/// keeps its `io::ErrorKind` ("no such binary" vs "ran and failed").
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

/// Run `script` in a fresh local `/bin/sh` with `args` as `$1..$n`. One-shot
/// form, kept for tests.
pub fn run_local(script: &str, args: &[&str]) -> Result<Output> {
    let mut cmd = sh_command();
    cmd.args(args);
    run_feeding_stdin(&mut cmd, script).map_err(NvmuxError::Io)
}

/// `/bin/sh -s`, for [`Shell::local`] and the one-shot form.
///
/// Runs under the umask nvmux was *started* with, not the clamped one
/// ([`crate::paths::restrict_umask`]), because this shell runs
/// `scripts/spawn.sh`, which launches the session's editor. Local only: a
/// umask does not travel over ssh, so remote sessions get the remote host's.
fn sh_command() -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-s");
    if let Some(mask) = crate::paths::launch_umask() {
        let mask = mask.bits();
        // SAFETY: `umask(2)` neither allocates nor locks, as a pre-exec hook
        // requires. Set between fork and exec so only the child gets it: the
        // parent's mask is process-wide and cannot be swapped around a spawn.
        unsafe {
            cmd.pre_exec(move || {
                libc::umask(mask);
                Ok(())
            });
        }
    }
    cmd
}

/// Followed by the shell's secret and the run's sequence number.
const MARK: &str = "NVMUX_MARK_";

/// How long a shell may take to exit after stdin closes before it is killed.
/// An idle `sh` exits on EOF at once; `ssh` must close its channel first.
/// Bounded so a stuck shell cannot delay the caller's exit.
const EXIT_GRACE: Duration = Duration::from_millis(200);

/// Extra time to collect stderr after a failed run.
///
/// Locally all stderr arrives before the marker. Over ssh the streams are
/// multiplexed and sshd may forward the marker just before stderr it read in
/// the same pass. Only failure paths read stderr (`classify`, log tails), so
/// only failed runs wait.
const STDERR_SETTLE: Duration = Duration::from_millis(50);

/// One long-lived `sh` on a session host, running every script nvmux sends.
/// Started via `/bin/sh` locally or `ssh` remotely, then used identically. Not
/// shared: one run at a time, on the owning thread.
pub struct Shell {
    /// For logs and [`Died`]: `/bin/sh` or `ssh <host>`.
    name: String,
    child: Child,
    /// `None` once closed, which tells the shell to exit.
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    /// `None` after EOF.
    stderr: Option<ChildStderr>,
    /// Unclaimed stdout bytes.
    out: Vec<u8>,
    /// Unclaimed stderr bytes.
    err: Vec<u8>,
    /// Never written to stdout, so markers cannot be forged.
    secret: String,
    /// The last run's number; 0 is the handshake.
    seq: u64,
    /// The handshake needle, until the first run reads past it.
    ready: Option<Vec<u8>>,
    /// Exit status once reaped; `Some` means dead.
    status: Option<i32>,
}

impl fmt::Debug for Shell {
    /// Omits the buffers, which can hold a whole listing.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shell")
            .field("name", &self.name)
            .field("pid", &self.child.id())
            .field("seq", &self.seq)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// The shell is gone; returned by a run instead of an [`Output`]. `status` is
/// `-1` for a signal. `stderr` is what it said on exit: for `ssh`, why the
/// connection ended, which [`crate::ssh::classify`] reads.
#[derive(Debug, Clone)]
pub struct Died {
    pub name: String,
    pub status: i32,
    pub stderr: String,
}

impl fmt::Display for Died {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} died with status {} while running a script",
            self.name, self.status
        )?;
        let said = self.stderr.trim();
        if !said.is_empty() {
            write!(f, ": {said}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Died {}

impl Shell {
    /// Start a shell from `cmd`. Returns once the process exists: the
    /// handshake (a marker line) is written now and read by the first
    /// [`Shell::run`], saving a round trip. Stdout before it (e.g. a login
    /// banner) is discarded; stderr before it goes to the first run, since the
    /// first script's stderr cannot be told from a profile's.
    ///
    /// A missing binary is `ErrorKind::NotFound`, which [`crate::ssh`]
    /// classifies on; a shell that dies at once is reported by the first run.
    pub fn start(cmd: &mut Command, name: impl Into<String>) -> io::Result<Shell> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("child has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("child has no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("child has no stderr"))?;
        let secret = crate::ids::nonce()?;

        let mut shell = Shell {
            name: name.into(),
            child,
            stdin: Some(stdin),
            stdout,
            stderr: Some(stderr),
            out: Vec::new(),
            err: Vec::new(),
            secret,
            seq: 0,
            ready: None,
            status: None,
        };
        let mark = marker(&shell.secret, 0);
        // A failed write means the shell is gone; the first run reports it,
        // with its stderr.
        let _ = shell
            .stdin
            .as_mut()
            .expect("just set")
            .write_all(format!("printf '\\n%s %s\\n' {mark} ready\n").as_bytes());
        shell.ready = Some(needle(&mark));
        tracing::debug!(name = %shell.name, pid = shell.child.id(), "shell started");
        Ok(shell)
    }

    /// This machine's `/bin/sh`, under the launch umask (see [`sh_command`]).
    /// Every local shell starts here.
    pub fn local() -> io::Result<Shell> {
        Shell::start(&mut sh_command(), "/bin/sh")
    }

    /// Run `script` in a subshell with `args` as `$1..$n`.
    ///
    /// A failing script is `Ok` with a nonzero status. `Err` means the shell is
    /// gone. Not retried, since a half-run script must not run twice; the owner
    /// replaces the shell before its next call.
    pub fn run(&mut self, script: &str, args: &[&str]) -> std::result::Result<Output, Died> {
        if let Some(status) = self.status {
            return Err(Died {
                name: self.name.clone(),
                status,
                stderr: String::new(),
            });
        }
        // Only an unread handshake may legitimately arrive between runs.
        if self.ready.is_none() && !self.out.is_empty() {
            tracing::debug!(
                name = %self.name,
                bytes = self.out.len(),
                "discarding output the shell produced between runs"
            );
            self.out.clear();
        }

        let started = Instant::now();
        self.seq += 1;
        let mark = marker(&self.secret, self.seq);
        let frame = frame(&mark, script, args);
        // Write the whole frame before reading: the shell cannot answer until
        // it has parsed the whole `( … ); printf` list, and a frame fits in the
        // pipe.
        let write = match self.stdin.as_mut() {
            Some(stdin) => stdin.write_all(&frame),
            None => Err(io::Error::other("stdin already closed")),
        };
        if let Err(e) = write {
            return Err(self.die(&format!("writing a script: {e}")));
        }

        if let Some(needle) = self.ready.take() {
            let (banner, payload) = self.read_until(&needle)?;
            if !banner.is_empty() {
                tracing::debug!(
                    name = %self.name,
                    banner = %String::from_utf8_lossy(&banner),
                    "discarding what the shell said before its handshake"
                );
            }
            // Stderr is left for this run (see `Shell::start`).
            if payload != "ready" {
                return Err(self.die("the shell did not answer its handshake"));
            }
        }

        let (stdout, payload) = self.read_until(&needle(&mark))?;
        let status = match payload.parse::<i32>() {
            Ok(status) => status,
            Err(_) => {
                tracing::warn!(name = %self.name, payload, "a marker carried no status");
                -1
            }
        };
        self.settle_stderr(status != 0);
        let stderr = String::from_utf8_lossy(&std::mem::take(&mut self.err)).into_owned();
        tracing::debug!(
            name = %self.name,
            seq = self.seq,
            status,
            ms = started.elapsed().as_secs_f64() * 1000.0,
            "script ran"
        );
        Ok(Output {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr,
            status,
        })
    }

    /// Whether the shell is alive, via a zero-timeout poll of stdout (closed
    /// once dead). A death found here is reaped, so owners checking before
    /// each run replace a shell that died while idle without a run failing.
    pub fn is_alive(&mut self) -> bool {
        if self.status.is_some() {
            return false;
        }
        let mut buf = [0u8; 4096];
        loop {
            let mut fds = [pollfd(self.stdout.as_raw_fd())];
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) };
            if n <= 0 || !ready(&fds[0]) {
                return true;
            }
            match self.stdout.read(&mut buf) {
                // Kept: a shell started early delivers its handshake while
                // idle.
                Ok(len) if len > 0 => self.out.extend_from_slice(&buf[..len]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                _ => {
                    self.die("exited while idle");
                    return false;
                }
            }
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Read stdout until `needle` and the rest of its line arrive, draining
    /// stderr meanwhile so a chatty script cannot block on it. Returns the
    /// bytes before the needle and the rest of its line.
    fn read_until(&mut self, needle: &[u8]) -> std::result::Result<(Vec<u8>, String), Died> {
        let mut buf = [0u8; 16 * 1024];
        // Resume point, so a long listing is not rescanned on every read.
        let mut scanned = 0;
        loop {
            match find_marker(&self.out, needle, scanned) {
                Scan::Found {
                    start,
                    payload,
                    end,
                } => {
                    let stdout = self.out[..start].to_vec();
                    self.out.drain(..end);
                    return Ok((stdout, payload));
                }
                Scan::Incomplete { start } => scanned = start,
                Scan::NotFound => scanned = self.out.len().saturating_sub(needle.len() - 1),
            }

            let stderr_fd = self.stderr.as_ref().map_or(-1, |e| e.as_raw_fd());
            let mut fds = [pollfd(self.stdout.as_raw_fd()), pollfd(stderr_fd)];
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(self.die(&format!("poll: {e}")));
            }
            // stderr first: it is the one that could fill and stall the shell.
            if ready(&fds[1]) {
                self.read_stderr(&mut buf);
            }
            if ready(&fds[0]) {
                match self.stdout.read(&mut buf) {
                    Ok(len) if len > 0 => self.out.extend_from_slice(&buf[..len]),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    // EOF. If the marker came with the last bytes the next
                    // pass finds it; otherwise the run was cut short.
                    _ => {
                        if matches!(find_marker(&self.out, needle, scanned), Scan::Found { .. }) {
                            continue;
                        }
                        return Err(self.die("its stdout closed mid-run"));
                    }
                }
            }
        }
    }

    /// One read from stderr. After EOF it is never polled again.
    fn read_stderr(&mut self, buf: &mut [u8]) {
        let Some(stderr) = self.stderr.as_mut() else {
            return;
        };
        match stderr.read(buf) {
            Ok(len) if len > 0 => self.err.extend_from_slice(&buf[..len]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            _ => self.stderr = None,
        }
    }

    /// Collect pending stderr (after a failed run, also what arrives within
    /// [`STDERR_SETTLE`]) so it goes to this run, not the next.
    fn settle_stderr(&mut self, failed: bool) {
        let mut buf = [0u8; 4096];
        let deadline = failed.then(|| Instant::now() + STDERR_SETTLE);
        loop {
            let Some(stderr) = self.stderr.as_ref() else {
                return;
            };
            let timeout = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()).as_millis() as libc::c_int,
                None => 0,
            };
            let mut fds = [pollfd(stderr.as_raw_fd())];
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout) };
            if n <= 0 || !ready(&fds[0]) {
                return;
            }
            self.read_stderr(&mut buf);
        }
    }

    /// Close and reap the shell, collect its final stderr, and report it dead.
    fn die(&mut self, why: &str) -> Died {
        drop(self.stdin.take());
        let status = self.reap();
        // For `ssh`, stderr on exit is the reason. Bounded, since a grandchild
        // could hold the pipe open.
        let deadline = Instant::now() + EXIT_GRACE;
        let mut buf = [0u8; 4096];
        while let Some(stderr) = self.stderr.as_ref() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let mut fds = [pollfd(stderr.as_raw_fd())];
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, left.as_millis() as libc::c_int) };
            if n <= 0 || !ready(&fds[0]) {
                break;
            }
            self.read_stderr(&mut buf);
        }
        self.status = Some(status);
        tracing::debug!(
            name = %self.name,
            status,
            why,
            unclaimed_stdout = self.out.len(),
            "shell died"
        );
        self.out.clear();
        Died {
            name: self.name.clone(),
            status,
            stderr: String::from_utf8_lossy(&std::mem::take(&mut self.err)).into_owned(),
        }
    }

    /// Wait for the shell to exit, killing it after [`EXIT_GRACE`]. Stdin must
    /// already be closed.
    fn reap(&mut self) -> i32 {
        let deadline = Instant::now() + EXIT_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return status.code().unwrap_or(-1),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                _ => {
                    let _ = self.child.kill();
                    return self
                        .child
                        .wait()
                        .map(|s| s.code().unwrap_or(-1))
                        .unwrap_or(-1);
                }
            }
        }
    }
}

/// Closing stdin asks the shell to exit. Neither pipe is read.
impl Drop for Shell {
    fn drop(&mut self) {
        if self.status.is_some() {
            return;
        }
        drop(self.stdin.take());
        let status = self.reap();
        tracing::debug!(name = %self.name, status, "shell stopped");
    }
}

fn marker(secret: &str, seq: u64) -> String {
    format!("{MARK}{secret}_{seq}")
}

/// The parent's newline before the marker, the marker, and the space before
/// its payload. Leading with the parent's newline keeps stdout byte-exact.
fn needle(marker: &str) -> Vec<u8> {
    let mut n = Vec::with_capacity(marker.len() + 2);
    n.push(b'\n');
    n.extend_from_slice(marker.as_bytes());
    n.push(b' ');
    n
}

/// Everything written to the shell for one run.
///
/// ```text
/// ( set -- 'a' 'b c'
/// <script>
/// ) </dev/null; printf '\n%s %s\n' NVMUX_MARK_… "$?"
/// ```
///
/// `set --` gives the script its `$1..$n`, quoted by [`shell::quote`]: the
/// only place a value is embedded in shell text. `</dev/null` because the
/// shell reads input in chunks and the next frame may already be buffered; a
/// script reading stdin must get EOF. The `printf` runs in the parent, so `$?`
/// is the subshell's status.
pub(crate) fn frame(marker: &str, script: &str, args: &[&str]) -> Vec<u8> {
    let mut f = Vec::with_capacity(script.len() + 256);
    f.extend_from_slice(b"( set -- ");
    f.extend_from_slice(shell::quote_all(args).as_bytes());
    f.push(b'\n');
    f.extend_from_slice(script.as_bytes());
    // Always: a script without a trailing newline would swallow the `)`.
    f.push(b'\n');
    f.extend_from_slice(format!(") </dev/null; printf '\\n%s %s\\n' {marker} \"$?\"\n").as_bytes());
    f
}

enum Scan {
    NotFound,
    /// The needle's line is incomplete; the next search resumes at `start`.
    Incomplete {
        start: usize,
    },
    /// The run's stdout ends at `start`; `payload` is the rest of the needle's
    /// line; the next run's bytes begin at `end`.
    Found {
        start: usize,
        payload: String,
        end: usize,
    },
}

/// Look for `needle` in `buf[from..]`.
fn find_marker(buf: &[u8], needle: &[u8], from: usize) -> Scan {
    let from = from.min(buf.len());
    let Some(at) = buf[from..].windows(needle.len()).position(|w| w == needle) else {
        return Scan::NotFound;
    };
    let start = from + at;
    let after = start + needle.len();
    let Some(nl) = buf[after..].iter().position(|&b| b == b'\n') else {
        return Scan::Incomplete { start };
    };
    let line = &buf[after..after + nl];
    let payload = String::from_utf8_lossy(line)
        .trim_end_matches('\r')
        .to_string();
    Scan::Found {
        start,
        payload,
        end: after + nl + 1,
    }
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

    #[test]
    fn a_failing_script_reports_its_status_not_an_error() {
        let out = run_local("exit 3", &[]).expect("run");
        assert_eq!(out.status, 3);
        assert!(!out.ok());
    }

    // ---- the persistent shell ------------------------------------------------

    fn sh() -> Shell {
        Shell::local().expect("start /bin/sh")
    }

    fn run(shell: &mut Shell, script: &str, args: &[&str]) -> Output {
        shell.run(script, args).expect("the shell is alive")
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        crate::test_support::scratch_dir(&format!("proc-{tag}"))
    }

    #[test]
    fn a_shell_runs_scripts_one_after_another() {
        let mut shell = sh();
        assert_eq!(run(&mut shell, "printf one", &[]).stdout, "one");
        assert_eq!(run(&mut shell, "printf two", &[]).stdout, "two");
        let out = run(
            &mut shell,
            r#"printf '[%s]' "$1" "$2" "$3""#,
            &["one", "two words", "it's"],
        );
        assert!(out.ok(), "stderr: {}", out.stderr);
        assert_eq!(out.stdout, "[one][two words][it's]");
    }

    /// Injection test for `set --`, the only place a value is embedded in
    /// shell text.
    #[test]
    fn arguments_are_quoted_not_evaluated_and_do_not_accumulate() {
        let mut shell = sh();
        let out = run(
            &mut shell,
            r#"printf '[%s]' "$@""#,
            &["$(echo pwned)", "a b", "'", ""],
        );
        assert_eq!(out.stdout, "[$(echo pwned)][a b]['][]");
        // Arguments from an earlier run must not linger.
        assert_eq!(
            run(&mut shell, r#"printf '%s' "$#""#, &["only"]).stdout,
            "1"
        );
        assert_eq!(run(&mut shell, r#"printf '%s' "$#""#, &[]).stdout, "0");
    }

    #[test]
    fn output_is_byte_exact() {
        let mut shell = sh();
        assert_eq!(
            run(&mut shell, "printf 'no newline'", &[]).stdout,
            "no newline"
        );
        assert_eq!(run(&mut shell, "printf 'a\\n\\n'", &[]).stdout, "a\n\n");
        assert_eq!(run(&mut shell, ":", &[]).stdout, "");
        assert_eq!(run(&mut shell, "printf '\\n'", &[]).stdout, "\n");
    }

    /// Twice: the second run is the one a stale cache would corrupt.
    #[test]
    fn a_listing_through_the_shell_is_what_a_fresh_shell_gives() {
        let dir = scratch("listing");
        // A stand-in session. Not a real socket, so it is listed but never
        // swept.
        std::fs::write(dir.join("aaaaaaaa.sock"), b"").expect("sock");
        std::fs::write(
            dir.join("aaaaaaaa.json"),
            "{\n  \"id\": \"aaaaaaaa\",\n\t\"name\": \"x\",\r\n  \"created\": 1, \"pid\": 1, \"num\": 1\n}\n",
        )
        .expect("json");
        let arg = dir.to_string_lossy().into_owned();

        let fresh = run_local(shell::LIST_SCRIPT, &[&arg]).expect("fresh");
        let mut shell = sh();
        for _ in 0..2 {
            let kept = run(&mut shell, shell::LIST_SCRIPT, &[&arg]);
            assert_eq!(kept.stdout, fresh.stdout);
            assert_eq!(kept.status, fresh.status);
            assert!(kept.stdout.contains("NVMUX_END"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn terminator_lookalikes_in_the_output_do_not_end_a_run() {
        let mut shell = sh();
        let script = "printf 'NVMUX_END\\n'; printf 'NVMUX_MARK_aaaaaaaa_1 0\\n'; printf 'NVMUX_MARK_ 0\\n'; printf 'tail'";
        let out = run(&mut shell, script, &[]);
        assert_eq!(
            out.stdout,
            "NVMUX_END\nNVMUX_MARK_aaaaaaaa_1 0\nNVMUX_MARK_ 0\ntail"
        );
        assert!(out.ok());
        // The secret never reaches stdout.
        assert!(!out.stdout.contains(&shell.secret));
        assert_eq!(run(&mut shell, "printf still", &[]).stdout, "still");
    }

    #[test]
    fn a_failing_script_is_a_status_and_the_shell_goes_on() {
        let mut shell = sh();
        let out = run(&mut shell, "exit 3", &[]);
        assert_eq!(out.status, 3);
        assert!(!out.ok());
        assert_eq!(run(&mut shell, "printf fine", &[]).stdout, "fine");
    }

    #[test]
    fn stderr_is_attributed_to_the_run_that_wrote_it() {
        let mut shell = sh();
        let a = run(&mut shell, "printf a >&2; exit 1", &[]);
        assert_eq!(a.stderr, "a");
        let quiet = run(&mut shell, ":", &[]);
        assert_eq!(quiet.stderr, "", "the previous run's stderr leaked");
        let b = run(&mut shell, "printf b >&2", &[]);
        assert_eq!(b.stderr, "b");
    }

    /// More than a pipe holds on both streams: stderr must drain while stdout
    /// is awaited.
    #[test]
    fn a_chatty_script_cannot_wedge_the_shell() {
        let mut shell = sh();
        let script = "i=0; while [ $i -lt 2000 ]; do printf '%0500d\\n' 0; printf 'e%0100d\\n' 0 >&2; i=$((i+1)); done";
        let out = run(&mut shell, script, &[]);
        assert!(out.ok());
        assert_eq!(out.stdout.lines().count(), 2000);
        assert_eq!(out.stderr.lines().count(), 2000);
        assert_eq!(run(&mut shell, "printf next", &[]).stdout, "next");
    }

    #[test]
    fn a_run_cannot_change_the_shell_the_next_run_gets() {
        let mut shell = sh();
        let probe = r#"printf '%s|%s|%s|%s|%s' "$PWD" "$(umask)" "${FOO:-unset}" "$(command -v f || printf nof)" "${NVMUX_LISTENING_TAKEN:-unset}""#;
        let before = run(&mut shell, probe, &[]).stdout;
        let out = run(
            &mut shell,
            "cd /; umask 077; export FOO=bar; f() { :; }; NVMUX_LISTENING_TAKEN=1; set -u; exit 5",
            &[],
        );
        assert_eq!(out.status, 5);
        assert_eq!(run(&mut shell, probe, &[]).stdout, before);
        // The scripts' `${1:?}` guards kill only the subshell.
        let guarded = run(&mut shell, r#": "${1:?usage}"; printf reached"#, &[]);
        assert_ne!(guarded.status, 0);
        assert_eq!(guarded.stdout, "");
        assert!(
            guarded.stderr.contains("usage"),
            "stderr: {}",
            guarded.stderr
        );
        assert_eq!(run(&mut shell, "printf alive", &[]).stdout, "alive");
    }

    #[test]
    fn a_runs_stdin_is_not_the_command_stream() {
        let mut shell = sh();
        let out = run(&mut shell, r#"read -r x; printf '[%s]' "${x:-EOF}""#, &[]);
        assert_eq!(out.stdout, "[EOF]");
        assert_eq!(run(&mut shell, "printf next", &[]).stdout, "next");
    }

    #[test]
    fn every_shipped_script_runs_in_one_shell() {
        use crate::transport::protocol;
        let dir = scratch("scripts");
        let arg = dir.to_string_lossy().into_owned();
        let mut shell = sh();

        // No argument: the usage guard fails.
        assert_ne!(run(&mut shell, shell::LIST_SCRIPT, &[]).status, 0);
        assert!(shell.is_alive());
        // An empty directory is an empty listing.
        let out = run(&mut shell, shell::LIST_SCRIPT, &[&arg]);
        assert!(protocol::parse_listing(&out.stdout)
            .expect("listing")
            .is_empty());
        // A launch command that does not exist is refused, with the reason.
        let out = run(
            &mut shell,
            shell::SPAWN_SCRIPT,
            &[
                &arg,
                "bbbbbbbb",
                &arg,
                "nvmux-no-such-editor",
                "--listen",
                "x",
            ],
        );
        assert!(protocol::parse_spawn(&out.stdout).is_err());
        assert!(out.stdout.contains("ERROR"), "{}", out.stdout);
        // Killing what was never there.
        let out = run(&mut shell, shell::KILL_SCRIPT, &[&arg, "bbbbbbbb", ""]);
        assert_eq!(
            protocol::parse_kill(&out.stdout).expect("kill"),
            protocol::KillOutcome::Absent
        );
        // Metadata written and renumbered.
        let out = run(
            &mut shell,
            shell::WRITE_META_SCRIPT,
            &[
                &arg,
                "cccccccc",
                r#"{"id":"cccccccc","name":"c","created":1,"pid":1,"num":1}"#,
            ],
        );
        protocol::require_terminator(&out.stdout, "metadata write").expect("written");
        let out = run(
            &mut shell,
            shell::RENUMBER_SCRIPT,
            &[
                &arg,
                "cccccccc",
                r#"{"id":"cccccccc","name":"c","created":1,"pid":1,"num":2}"#,
            ],
        );
        protocol::parse_write(&out.stdout, "renumber").expect("renumbered");
        // A directory that is not there is an empty answer.
        let out = run(&mut shell, shell::DIRS_SCRIPT, &[&format!("{arg}/nope")]);
        assert!(protocol::parse_dirs(&out.stdout)
            .expect("dirs")
            .names
            .is_empty());
        // The greeting, which sets its own `$1` for the listing inside it.
        let out = run(&mut shell, shell::HELLO_SCRIPT, &[]);
        protocol::parse_probe(&out.stdout).expect("probe");

        assert!(shell.is_alive());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dead_shell_is_reported_once_and_a_fresh_start_works() {
        let mut shell = sh();
        // `$$` in a subshell is still the parent's pid.
        let died = shell
            .run("kill -KILL $$", &[])
            .expect_err("the shell was killed");
        assert_eq!(died.status, -1, "a signal has no exit code: {died:?}");
        assert!(!shell.is_alive());
        // Later runs fail at once.
        let again = shell.run("printf x", &[]).expect_err("still dead");
        assert_eq!(again.status, -1);
        let mut fresh = sh();
        assert_eq!(run(&mut fresh, "printf ok", &[]).stdout, "ok");
    }

    #[test]
    fn a_shell_that_dies_while_idle_is_found_out_without_a_run() {
        let mut shell = sh();
        assert_eq!(run(&mut shell, "printf x", &[]).stdout, "x");
        assert!(shell.is_alive());
        unsafe {
            libc::kill(shell.pid() as libc::pid_t, libc::SIGKILL);
        }
        assert!(
            crate::test_support::wait_until(Duration::from_secs(2), || !shell.is_alive()),
            "a killed shell still reads as alive"
        );
    }

    #[test]
    fn a_shell_that_cannot_be_started_keeps_its_error_kind() {
        let err =
            Shell::start(&mut Command::new("nvmux-no-such-binary"), "x").expect_err("must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err}");
    }

    /// Like `ssh` failing auth: reason on stderr, then exit. The first run
    /// reports it.
    #[test]
    fn a_shell_that_dies_before_its_handshake_reports_what_it_said() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "echo nope >&2; exit 7"]);
        let mut shell = Shell::start(&mut cmd, "doomed").expect("start");
        let died = shell.run("printf x", &[]).expect_err("died");
        assert_eq!(died.status, 7);
        assert_eq!(died.stderr, "nope\n");
        assert!(died.to_string().contains("nope"), "{died}");
    }

    /// A login banner is dropped; stderr before the handshake goes to the
    /// first run (see `Shell::start`).
    #[test]
    fn what_a_shell_says_before_its_handshake_is_not_the_first_runs_output() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "echo Welcome; echo warn >&2; exec sh -s"]);
        let mut shell = Shell::start(&mut cmd, "chatty").expect("start");
        let out = run(&mut shell, "printf hi; printf mine >&2", &[]);
        assert_eq!(out.stdout, "hi");
        assert_eq!(out.stderr, "warn\nmine");
        // Only the first run gets it.
        let out = run(&mut shell, "printf again", &[]);
        assert_eq!(out.stdout, "again");
        assert_eq!(out.stderr, "");
    }

    /// The handshake arrives while idle; `is_alive` must keep it for the first
    /// run.
    #[test]
    fn a_shell_started_ahead_of_its_first_run_still_answers() {
        let mut shell = sh();
        std::thread::sleep(Duration::from_millis(100));
        assert!(shell.is_alive());
        assert_eq!(run(&mut shell, "printf later", &[]).stdout, "later");
    }

    #[test]
    fn dropping_a_shell_ends_it() {
        let shell = sh();
        let pid = shell.pid() as libc::pid_t;
        drop(shell);
        // Reaped, so the pid is no longer ours to signal.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[test]
    fn a_marker_is_found_only_when_its_line_is_complete() {
        let needle = needle("NVMUX_MARK_secret_3");
        let full = b"out\nNVMUX_MARK_secret_3 0\nnext";
        match find_marker(full, &needle, 0) {
            Scan::Found {
                start,
                payload,
                end,
            } => {
                assert_eq!(start, 3);
                assert_eq!(payload, "0");
                assert_eq!(end, 26);
                assert_eq!(&full[end..], b"next");
            }
            _ => panic!("not found"),
        }
        // No output at all: the needle is the first thing.
        assert!(matches!(
            find_marker(b"\nNVMUX_MARK_secret_3 7\n", &needle, 0),
            Scan::Found { start: 0, .. }
        ));
        // The line is still arriving.
        assert!(matches!(
            find_marker(b"out\nNVMUX_MARK_secret_3 1", &needle, 0),
            Scan::Incomplete { start: 3 }
        ));
        // Marker text inside a line is not the marker.
        assert!(matches!(
            find_marker(b"foo NVMUX_MARK_secret_3 0\n", &needle, 0),
            Scan::NotFound
        ));
        // A carriage return before the newline is not part of the payload.
        match find_marker(b"\nNVMUX_MARK_secret_3 2\r\n", &needle, 0) {
            Scan::Found { payload, .. } => assert_eq!(payload, "2"),
            _ => panic!("not found"),
        }
        // Searching past the needle misses it, hence `Incomplete { start }`.
        assert!(matches!(find_marker(full, &needle, 4), Scan::NotFound));
    }
}
