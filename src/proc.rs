//! Running a shell script in a child process and collecting what it said.
//!
//! Both halves of nvmux do this: locally through `/bin/sh`, remotely through
//! `ssh`. They differ only in the command that starts the shell and in how a
//! failure to start it is classified, so the plumbing lives here and the error
//! mapping stays with each caller.
//!
//! The script always travels on **stdin** and its variable parts as positional
//! arguments — never interpolated into a command line. See [`crate::shell`].
//!
//! # One shell per host, not one per script
//!
//! [`Shell`] is the production path: one `sh` started per host and kept, with
//! every script sent down its stdin. Starting a shell per script cost more than
//! the scripts did — measured, ten listings took 51 ms as fresh shells and 20 ms
//! inside one already running, and on a host where a fork is fifteen
//! milliseconds rather than one the startup was most of what a listing cost.
//! Over ssh a fresh shell per script also meant a *login* shell per script:
//! 100 ms with an empty profile, and unbounded with a real one. [`run_local`]
//! and [`run_feeding_stdin`] remain as the one-shot form, for a test that wants
//! to see what a script says without a shell to keep.
//!
//! # Every run in a subshell
//!
//! The scripts were written for a shell that dies when they end, and they use
//! it: `exit` on every error path, a `cd` into the session's directory,
//! `umask`, `export`, and caches the prelude fills once per run and never
//! again. So each run is wrapped in `( … )` and the parent shell stays exactly
//! as it started. That costs a fork with no exec — 0.4 ms here — and a
//! re-reading of the prelude, 0.3 ms; a fresh shell was 5 ms. What it buys is
//! the scripts unchanged, and a parent that cannot be left in a state by one
//! run that the next would inherit.
//!
//! # Framing
//!
//! After the subshell the parent prints a marker line carrying the run's exit
//! status. The marker carries a secret drawn when the shell starts, which
//! travels only on stdin, so nothing a script prints can be mistaken for it — a
//! directory named after one, say. The needle searched for begins with the
//! newline *before* the marker, which makes a run's stdout exactly the bytes a
//! one-shot shell would have produced: nothing is added, and the parsers in
//! [`crate::transport::protocol`] see no difference.

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

/// Spawn `cmd`, feed it `script` on stdin, and wait for it.
///
/// The one-shot form, kept for tests. Returns `io::Result` rather than a crate
/// error for the same reason [`Shell::start`] does: a spawn that failed keeps
/// its `io::ErrorKind`, so a test can tell "no such binary" from "ran and
/// failed" — the signal [`crate::ssh`] classifies on, through
/// [`Shell::start`].
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

/// Run `script` under this machine's `/bin/sh` with `args` as `$1..$n`, in a
/// shell of its own. The one-shot form, kept for tests.
pub fn run_local(script: &str, args: &[&str]) -> Result<Output> {
    let mut cmd = sh_command();
    cmd.args(args);
    run_feeding_stdin(&mut cmd, script).map_err(NvmuxError::Io)
}

/// `/bin/sh -s`: what a local [`Shell`] is started from ([`Shell::local`]),
/// and what the one-shot form runs.
///
/// It runs under the umask nvmux was *started* with rather than the one nvmux
/// clamped itself to, because this shell is what runs `scripts/spawn.sh`, and
/// `spawn.sh` is what launches the session's editor. See
/// [`crate::paths::restrict_umask`] for what the clamp is still for.
///
/// Only the local shell: a umask does not travel over ssh, so a remote session
/// takes the remote host's own — which is what anything else launched there
/// would get, and is not this machine's business to override.
fn sh_command() -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-s");
    if let Some(mask) = crate::paths::launch_umask() {
        let mask = mask.bits();
        // Between fork and exec, so it lands on this child and on nothing else
        // — the parent's own mask is shared with every thread and cannot be
        // moved around a spawn. One `umask(2)` call: no allocation, no locks,
        // which is all a pre-exec hook may do.
        unsafe {
            cmd.pre_exec(move || {
                libc::umask(mask);
                Ok(())
            });
        }
    }
    cmd
}

/// The marker's fixed prefix. What follows it is the shell's secret and the
/// run's sequence number.
const MARK: &str = "NVMUX_MARK_";

/// How long a shell gets to leave on its own once its stdin is closed, before
/// it is killed. An idle `sh` exits on EOF at once; `ssh` has a channel to
/// close first. Bounded because a shell that will not leave must not hold up
/// the caller's exit, and nothing is lost by killing one whose stdin is gone.
const EXIT_GRACE: Duration = Duration::from_millis(200);

/// How long to keep collecting stderr after a run that failed, past what has
/// already arrived.
///
/// Locally the subshell's last write on stderr lands before its exit does, and
/// the marker only after that, so a zero-timeout look sees everything. Over ssh
/// the two streams are multiplexed, and sshd may forward the marker a moment
/// before the stderr chunk it read in the same pass. The consumers of stderr
/// are the failure paths — `classify`, the tail of a log that did not start —
/// so a failed run is worth a short wait and a successful one is not.
const STDERR_SETTLE: Duration = Duration::from_millis(50);

/// One long-lived `sh` on a session host, running every script nvmux sends it.
///
/// Started from whatever command reaches the host — `/bin/sh` here, `ssh` over
/// there — and used identically from then on. Not `Clone`, and not shared: one
/// run at a time, on the thread that owns it.
pub struct Shell {
    /// For logs and for [`Died`]: `/bin/sh`, or `ssh <host>`.
    name: String,
    child: Child,
    /// `None` once closed, on death and on drop. Closing it is how the shell is
    /// told to leave.
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    /// `None` once it has reached EOF.
    stderr: Option<ChildStderr>,
    /// Bytes read from stdout that no run has claimed yet.
    out: Vec<u8>,
    /// Bytes read from stderr that no run has claimed yet.
    err: Vec<u8>,
    /// Never written to stdout: what makes a marker unforgeable.
    secret: String,
    /// The last run's number; zero was the handshake.
    seq: u64,
    /// The handshake's needle, until the first run has read past it.
    ready: Option<Vec<u8>>,
    /// The exit status once the shell has been reaped. `Some` means dead.
    status: Option<i32>,
}

impl fmt::Debug for Shell {
    /// Name, pid and where the sequence is at — not the buffers, which can be
    /// a whole listing.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shell")
            .field("name", &self.name)
            .field("pid", &self.child.id())
            .field("seq", &self.seq)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// The shell is gone. What a run reports instead of an [`Output`].
///
/// The status is the shell's own, `-1` for a signal, and the stderr is
/// everything it said on the way out — for `ssh`, the words that say why the
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
    /// Start a shell from `cmd`, with all three streams piped.
    ///
    /// Returns as soon as the process exists. The handshake that proves a shell
    /// is on the other end — a marker line the shell is asked to print — is
    /// written now and read by the first [`Shell::run`], which saves a round
    /// trip on every connect and lets a shell be started before it is needed.
    /// Everything the shell says on stdout before that line, a login banner
    /// most likely, is discarded then. What it says on stderr is kept for the
    /// first run: the frame goes out before the handshake comes back, so the
    /// first script's stderr and a profile's cannot be told apart — and a
    /// one-shot run through a login shell carried the profile's stderr too.
    ///
    /// A binary that is not there is `ErrorKind::NotFound`, which is what
    /// [`crate::ssh`] classifies on; a shell that starts and dies at once is
    /// reported by the first run instead, with whatever it said on stderr.
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
        // A failure to write here is a shell that is already gone. Not reported
        // yet: the first run's write fails the same way and reports it with the
        // shell's last words attached, which is the useful form.
        let _ = shell
            .stdin
            .as_mut()
            .expect("just set")
            .write_all(format!("printf '\\n%s %s\\n' {mark} ready\n").as_bytes());
        shell.ready = Some(needle(&mark));
        tracing::debug!(name = %shell.name, pid = shell.child.id(), "shell started");
        Ok(shell)
    }

    /// This machine's `/bin/sh`, started from [`sh_command`] so it runs under
    /// the launch umask. Every local shell is started through here: the
    /// transport's, the directory lister's, and the ones the tests stand in
    /// for a remote host with.
    pub fn local() -> io::Result<Shell> {
        Shell::start(&mut sh_command(), "/bin/sh")
    }

    /// Run `script` with `args` as `$1..$n`, in a subshell, and collect what it
    /// said and how it ended.
    ///
    /// A script that fails is an `Ok` with a nonzero status, exactly as a
    /// one-shot run would report it. `Err` is the shell itself being gone — not
    /// retried here, since a script that was half way through must not run
    /// twice, and the owner replaces the shell before its next call.
    pub fn run(&mut self, script: &str, args: &[&str]) -> std::result::Result<Output, Died> {
        if let Some(status) = self.status {
            return Err(Died {
                name: self.name.clone(),
                status,
                stderr: String::new(),
            });
        }
        // Nothing legitimate reaches stdout between runs, apart from the
        // handshake that has not been read yet.
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
        // Written before anything is read: the shell cannot answer until it has
        // parsed the whole `( … ); printf` list, so there is nothing to read
        // yet, and a frame fits the pipe with room to spare.
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
            // Stderr is left where it is. This run's frame was written before
            // the handshake was read, so the script may already be running,
            // and there is no telling its stderr from a profile's: the two
            // pipes carry no order between them. It all goes to this run,
            // which is what a one-shot run through a login shell got.
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

    /// Whether the shell is still there, asked without sending it anything.
    ///
    /// A zero-timeout poll: a shell that is alive and idle has nothing to say,
    /// and one that has died has closed its stdout. A death found here is
    /// reaped here, so an owner that asks before every run replaces a shell
    /// that died while nobody was looking — a master that timed out, a remote
    /// `sh` that was killed — without a run ever failing for it.
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
                // Kept rather than discarded: the handshake line arrives while
                // the shell is idle if it was started ahead of its first run.
                Ok(len) if len > 0 => self.out.extend_from_slice(&buf[..len]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                _ => {
                    self.die("exited while idle");
                    return false;
                }
            }
        }
    }

    /// The shell's process id, for tests and logs.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Read stdout until `needle` and the rest of its line have arrived,
    /// collecting stderr on the way so a chatty script can never block on it.
    ///
    /// Returns the bytes before the needle and the text after it on its line.
    fn read_until(&mut self, needle: &[u8]) -> std::result::Result<(Vec<u8>, String), Died> {
        let mut buf = [0u8; 16 * 1024];
        // How far `out` has been searched for this needle, so a long listing is
        // not rescanned from the start on every read.
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
                    // EOF. The marker may have come with the last bytes, in
                    // which case the next pass finds it; otherwise the shell is
                    // gone with the run unfinished.
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

    /// One read from stderr into the unclaimed buffer. EOF closes the stream
    /// for good; it is never polled again.
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

    /// Collect the stderr that is already there — and, after a failed run, what
    /// is about to be — so it is attributed to this run rather than the next.
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

    /// The shell is gone, or about to be: close it, reap it, gather its last
    /// words, and say so.
    fn die(&mut self, why: &str) -> Died {
        drop(self.stdin.take());
        let status = self.reap();
        // What it wrote on stderr on the way out is the reason, for `ssh` —
        // bounded, because a grandchild could in theory be holding the pipe.
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

    /// Wait for the shell to exit, killing it if it takes longer than
    /// [`EXIT_GRACE`]. Its stdin must already be closed.
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

/// Closing stdin is the request to leave; an idle `sh` honours it at once and
/// `ssh` closes its channel and follows. Neither pipe is read: a shell that is
/// leaving has nothing to say that anyone is waiting for.
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

/// The marker line's first word: the prefix, the shell's secret, the run.
fn marker(secret: &str, seq: u64) -> String {
    format!("{MARK}{secret}_{seq}")
}

/// What is searched for: the newline the parent prints ahead of the marker,
/// the marker, and the space before its payload. Starting with the newline is
/// what keeps a run's stdout byte-exact — that newline is the parent's, not the
/// script's.
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
/// `set --` inside the subshell gives the script its `$1..$n`, quoted by
/// [`shell::quote`] — the one place a value is embedded in shell text, and it
/// is read by the same POSIX `sh` that `quote` is tested against. `</dev/null`
/// because the shell reads its own input in chunks and the next frame could
/// already be in its buffer: a script that read stdin would get that, or
/// nothing predictable, where it should get EOF. None of them do, and this
/// keeps it that way. The `printf` runs in the parent, so `$?` is the
/// subshell's, whatever the script did on its way out.
pub(crate) fn frame(marker: &str, script: &str, args: &[&str]) -> Vec<u8> {
    let mut f = Vec::with_capacity(script.len() + 256);
    f.extend_from_slice(b"( set -- ");
    f.extend_from_slice(shell::quote_all(args).as_bytes());
    f.push(b'\n');
    f.extend_from_slice(script.as_bytes());
    // Unconditional: a script whose last line has no newline would otherwise
    // swallow the `)`. A blank line is nothing to a shell.
    f.push(b'\n');
    f.extend_from_slice(format!(") </dev/null; printf '\\n%s %s\\n' {marker} \"$?\"\n").as_bytes());
    f
}

/// What a search of the unclaimed output found.
enum Scan {
    NotFound,
    /// The needle is there but its line has not all arrived; `start` is where
    /// it begins, so the next search starts there rather than past it.
    Incomplete {
        start: usize,
    },
    /// `start` is where the needle begins — the run's stdout ends there —
    /// `payload` is the text after it on its line, and `end` is just past that
    /// line, which is where the next run's bytes begin.
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

    /// A scratch directory for the tests that need files on disk.
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

    /// `set --` is the one place a value is embedded in shell text, so this is
    /// the injection test for it: a value that would run as a command if the
    /// quoting were wrong, and one that would split.
    #[test]
    fn arguments_are_quoted_not_evaluated_and_do_not_accumulate() {
        let mut shell = sh();
        let out = run(
            &mut shell,
            r#"printf '[%s]' "$@""#,
            &["$(echo pwned)", "a b", "'", ""],
        );
        assert_eq!(out.stdout, "[$(echo pwned)][a b]['][]");
        // Fewer arguments next time must mean fewer, not the old ones lingering.
        assert_eq!(
            run(&mut shell, r#"printf '%s' "$#""#, &["only"]).stdout,
            "1"
        );
        assert_eq!(run(&mut shell, r#"printf '%s' "$#""#, &[]).stdout, "0");
    }

    /// The needle starts with the parent's own newline, so nothing is added to
    /// or taken from what the script wrote — the parsers see exactly what a
    /// one-shot shell gave them.
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

    /// The real listing, through the shell and through a fresh process, must
    /// agree to the byte — twice, since the second run is the one a stale cache
    /// would corrupt.
    #[test]
    fn a_listing_through_the_shell_is_what_a_fresh_shell_gives() {
        let dir = scratch("listing");
        // A stand-in session: a socket-named file with metadata beside it, so
        // the listing has a row to flatten. Not a socket, so it is reported
        // and never swept.
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

    /// Nothing a script prints can end its own run: not the scripts' own
    /// terminator, and not a line shaped like a marker.
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
        // The secret never appears on stdout, which is the whole guarantee.
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

    /// A script that writes more than a pipe holds, on both streams at once,
    /// must still finish: stderr is drained while stdout is waited on.
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

    /// Everything the shipped scripts do to a shell — and they do all of it —
    /// dies with the subshell it was done in.
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
        // The guards the scripts open with: fatal to a shell, not to this one.
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

    /// A script's stdin is EOF, not the stream the next script arrives on.
    #[test]
    fn a_runs_stdin_is_not_the_command_stream() {
        let mut shell = sh();
        let out = run(&mut shell, r#"read -r x; printf '[%s]' "${x:-EOF}""#, &[]);
        assert_eq!(out.stdout, "[EOF]");
        assert_eq!(run(&mut shell, "printf next", &[]).stdout, "next");
    }

    /// Every shipped script, on its failure path and its success path, in one
    /// shell, back to back — and the shell is still there at the end.
    #[test]
    fn every_shipped_script_runs_in_one_shell() {
        use crate::transport::protocol;
        let dir = scratch("scripts");
        let arg = dir.to_string_lossy().into_owned();
        let mut shell = sh();

        // A listing with no argument at all dies of its own usage guard.
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
        // Every later run is refused at once rather than waited for.
        let again = shell.run("printf x", &[]).expect_err("still dead");
        assert_eq!(again.status, -1);
        // And a new shell is a new shell.
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

    /// What `ssh` does when authentication fails: say why on stderr and exit.
    /// The first run is what reports it, with the words attached.
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

    /// A login banner belongs to nobody. A profile's complaints go to the
    /// first run, which is where a one-shot run through a login shell had them
    /// — and where a script that starts talking before the handshake is read
    /// needs its own stderr to be (see `a_chatty_script_cannot_wedge_the_shell`).
    #[test]
    fn what_a_shell_says_before_its_handshake_is_not_the_first_runs_output() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "echo Welcome; echo warn >&2; exec sh -s"]);
        let mut shell = Shell::start(&mut cmd, "chatty").expect("start");
        let out = run(&mut shell, "printf hi; printf mine >&2", &[]);
        assert_eq!(out.stdout, "hi");
        assert_eq!(out.stderr, "warn\nmine");
        // Only the first run: the second sees nothing of it.
        let out = run(&mut shell, "printf again", &[]);
        assert_eq!(out.stdout, "again");
        assert_eq!(out.stderr, "");
    }

    /// Started ahead of time, the handshake arrives while the shell is idle;
    /// `is_alive` must keep it for the first run rather than throw it away.
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
        // Searching from past the needle misses it, which is why an incomplete
        // find reports where it started.
        assert!(matches!(find_marker(full, &needle, 4), Scan::NotFound));
    }
}
