//! Shared by the integration test files.
//!
//! Suites needing a usable `nvim` or an ssh host skip without it.
//! `$NVMUX_TEST_REQUIRE` (comma-separated: `nvim`, `ssh`) turns those skips
//! into failures, as CI needs.

#![allow(dead_code)]

use std::io::Read;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use nix::sys::signal::Signal;
use nix::sys::stat::Mode;
use nix::unistd::Pid;
use nvmux::card::{Card, Go, Post, Posted};
use nvmux::launch::Launch;
use nvmux::rpc::Client;
use nvmux::session::Session;
use nvmux::transport::local::LocalTransport;
use nvmux::transport::Transport;
use pty_process::blocking::{Command, Pty};
use rmpv::Value;

/// The built-in default launch command.
pub fn launch() -> Launch {
    Launch::parse(nvmux::launch::DEFAULT).expect("the built-in default must parse")
}

/// A working directory for tests that do not care: `/` exists on every host
/// and cannot be removed by a `Scratch` drop while a session is in it.
pub fn anywhere() -> &'static str {
    "/"
}

/// A launch command with extra arguments spliced in.
pub fn launch_with(args: &str) -> Launch {
    Launch::parse(&format!("nvim {args} --headless --listen {{sock}}")).expect("a valid command")
}

pub fn required(what: &str) -> bool {
    std::env::var("NVMUX_TEST_REQUIRE")
        .map(|v| v.split(',').any(|w| w.trim() == what))
        .unwrap_or(false)
}

/// If `$available` is false, skip the test, or fail it when
/// `$NVMUX_TEST_REQUIRE` names `$what`.
#[macro_export]
macro_rules! require {
    ($what:literal, $available:expr, $why:expr) => {
        if !$available {
            if $crate::common::required($what) {
                panic!("NVMUX_TEST_REQUIRE names {} but {}", $what, $why);
            }
            eprintln!("skipping: {}", $why);
            return;
        }
    };
}

/// The same Neovim presence and version check `main` makes.
pub fn nvim_available() -> bool {
    nvmux::nvim::check_local().is_ok()
}

#[macro_export]
macro_rules! require_nvim {
    () => {
        $crate::require!(
            "nvim",
            $crate::common::nvim_available(),
            "no usable nvim on $PATH"
        );
    };
}

/// A per-test runtime directory, removed on drop, so tests run in parallel
/// and never touch the user's real `/tmp/nvmux-<uid>`.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(tag: &str) -> Self {
        // Socket paths are capped at 100 bytes (see `nvmux::paths`). On macOS
        // the temp dir takes 49, the pid up to 5, the counter 2 and the rest
        // of the socket path 25, leaving 19 for the tag.
        assert!(
            tag.len() <= 19,
            "the tag {tag:?} is too long for a socket's path on macOS"
        );
        // A counter, not `ThreadId`, whose Debug form has regex-special
        // parentheses. Base 36 keeps it to two characters for up to 1,296
        // scratches per binary; tests/client.rs makes over a hundred, and a
        // third decimal digit overflowed the socket path limit.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert!(n < 36 * 36, "more scratches than two characters count");
        let digit = |d: u32| char::from_digit(d, 36).expect("a base-36 digit");
        let n: String = if n < 36 {
            digit(n).into()
        } else {
            [digit(n / 36), digit(n % 36)].into_iter().collect()
        };
        let dir = std::env::temp_dir().join(format!("nvmux-it-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }

    pub fn transport(&self) -> LocalTransport {
        LocalTransport::with_dir(self.0.clone()).expect("build transport")
    }

    /// A session's `<id>.json` as written to disk.
    pub fn metadata(&self, id: &str) -> Session {
        let path = self.0.join(format!("{id}.json"));
        let bytes = std::fs::read(&path).expect("read the session metadata");
        Session::from_json(&bytes, &path).expect("parse the session metadata")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Kill the sessions' nvim processes, or they pile up across runs.
        // Uses the pids in `<id>.json` (a session without one must be killed
        // by its test), and only while the process still has the socket on
        // its command line: a dead session's pid may have been reused, and
        // tests may write pids of their own.
        if let Ok(entries) = std::fs::read_dir(&self.0) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().is_none_or(|x| x != "json") {
                    continue;
                }
                let Some(meta) = std::fs::read_to_string(&p)
                    .ok()
                    .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
                else {
                    continue;
                };
                let sock = meta
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(|id| format!("{id}.sock"));
                let pid = meta
                    .get("pid")
                    .and_then(serde_json::Value::as_i64)
                    .filter(|pid| *pid > 1);
                if let (Some(sock), Some(pid)) = (sock, pid) {
                    if command_line(pid as u32).contains(&sock) {
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(pid as i32),
                            nix::sys::signal::Signal::SIGKILL,
                        );
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A process's command line, read as the session-host scripts read it; empty
/// if not found.
pub fn command_line(pid: u32) -> String {
    if let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) {
        return String::from_utf8_lossy(&raw).replace('\0', " ");
    }
    std::process::Command::new("ps")
        .args(["-ww", "-o", "args=", "-p", &pid.to_string()])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

/// The umask these suites pretend nvmux was launched with. Nothing in nvmux
/// produces 0002 itself, so a session reporting it must have inherited it.
pub const LAUNCH_UMASK: libc::mode_t = 0o002;

/// Record [`LAUNCH_UMASK`] as the launch mask, as `main` does, then restore
/// the live mask for other tests; the spawn path reads the recorded one. Once
/// per binary, since `paths::restrict_umask` keeps the first mask it sees.
pub fn establish_launch_umask() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let original = nix::sys::stat::umask(Mode::from_bits_truncate(LAUNCH_UMASK));
        nvmux::paths::restrict_umask();
        nix::sys::stat::umask(original);
    });
}

/// Parse `umask` output: sh prints `0002`, fish (a user's 'shell') `002`.
pub fn printed_umask(printed: &str) -> libc::mode_t {
    libc::mode_t::from_str_radix(printed.trim(), 8)
        .unwrap_or_else(|_| panic!("umask printed {printed:?}, not an octal mask"))
}

/// Names every test session distinctly, so a failed run cannot poison the next.
pub fn unique(tag: &str) -> String {
    format!("it-{tag}-{}", std::process::id())
}

/// Poll `cond` for up to `within`; returns whether it held.
pub fn wait_until(within: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cond()
}

/// `kill -9` and wait for the process to exit (SIGKILL is asynchronous). The
/// socket file is left behind on purpose, to exercise stale cleanup.
pub fn sigkill_and_wait(pid: u32) {
    let target = Pid::from_raw(pid as i32);
    nix::sys::signal::kill(target, Signal::SIGKILL).expect("kill -9");
    assert!(
        wait_until(Duration::from_secs(3), || !alive(pid)),
        "pid {pid} survived SIGKILL"
    );
}

pub fn alive(pid: u32) -> bool {
    nix::sys::signal::kill(Pid::from_raw(pid as i32), None).is_ok()
}

pub fn find_by_id<T: Transport>(t: &T, id: &str) -> Option<Session> {
    t.list_sessions()
        .expect("list")
        .into_iter()
        .find(|s| s.id == id)
}

pub fn read_meta(json: &Path) -> serde_json::Value {
    let bytes = std::fs::read(json).expect("read metadata");
    serde_json::from_slice(&bytes).expect("parse metadata")
}

pub fn write_meta(json: &Path, value: &serde_json::Value) {
    std::fs::write(json, serde_json::to_vec(value).expect("encode")).expect("write metadata");
}

/// Occupy a session's main loop with `cmd`, so it is reachable but cannot
/// answer a deferred call. The thread is never joined, since that would wait
/// for `cmd`. `settle` is how long to let the call land.
pub fn block_editor(sock: &Path, cmd: &'static str, settle: Duration) {
    let sock = sock.to_path_buf();
    std::thread::spawn(move || {
        let mut c = Client::connect(&sock, Duration::from_secs(2)).expect("connect");
        let _ = c.command(cmd);
    });
    std::thread::sleep(settle);
}

/// A real `--remote-ui` client on a throwaway pty that puts its session at a
/// hit-enter prompt.
///
/// [`block_editor`] cannot: a headless server with no UI returns from
/// `wait_return()` at once (`message.c`), and `getchar()`/`input()` keep
/// serving deferred calls. A two-line `:echo` scrolls at any `'cmdheight'`.
///
/// The prompt is the server's, so it outlives this client; end it with
/// [`press_enter`].
pub struct HitEnter {
    child: Child,
    // Held so the pty outlives the client; dropped last.
    _master: Pty,
}

impl HitEnter {
    pub fn open(sock: &Path) -> Self {
        let (master, pts) = pty_process::blocking::open().expect("open a pty");
        master
            .resize(pty_process::Size::new(24, 80))
            .expect("size the pty");
        let child = Command::new("nvim")
            .arg("--server")
            .arg(sock)
            .arg("--remote-ui")
            .env("TERM", "xterm-256color")
            // Takes the slave, or the master never sees EOF on client exit.
            .spawn(pts)
            .expect("spawn remote-ui client");

        // Drain output: on a full pty the client blocks and stops talking to
        // the server.
        let mut reader = reader(&master);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while matches!(reader.read(&mut buf), Ok(n) if n > 0) {}
        });

        let this = Self {
            child,
            _master: master,
        };

        // The prompt needs the UI attached. `list_uis` is deferred, which is
        // fine before anything blocks.
        assert!(
            wait_until(Duration::from_secs(15), || {
                Client::connect(sock, Duration::from_secs(2))
                    .and_then(|mut c| c.list_uis())
                    .is_ok_and(|n| n >= 1)
            }),
            "the remote-ui client never attached to {}",
            sock.display()
        );

        Client::connect(sock, Duration::from_secs(2))
            .expect("connect")
            .input(":echo \"a\\nb\"<CR>")
            .expect("open the prompt");
        assert!(
            wait_until(Duration::from_secs(10), || at_hit_enter(sock)),
            "the server never reached the hit-enter prompt"
        );
        this
    }
}

impl Drop for HitEnter {
    fn drop(&mut self) {
        hang_up_and_reap(&mut self.child);
    }
}

/// A second handle on a pty's master for a reader thread. Once the child
/// exits, reads end in EOF on macOS and EIO on Linux.
pub fn reader(master: &Pty) -> std::fs::File {
    let fd = master
        .as_fd()
        .try_clone_to_owned()
        .expect("clone the pty master");
    std::fs::File::from(fd)
}

/// SIGHUP, up to ~200 ms to exit, then SIGKILL (as portable-pty's `kill`
/// did), and reap. A child already reaped is left alone: its pid may be
/// reused.
pub fn hang_up_and_reap(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let pid = Pid::from_raw(child.id() as i32);
    if nix::sys::signal::kill(pid, Signal::SIGHUP).is_ok() {
        for attempt in 0..5 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(50));
            }
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The exit code, or 1 if a signal ended the child.
pub fn exit_code(status: ExitStatus) -> u32 {
    status.code().map_or(1, |code| code as u32)
}

pub fn at_hit_enter(sock: &Path) -> bool {
    mode(sock).is_some_and(|m| m.at_hit_enter())
}

/// The server's mode, or `None` if unreachable. A fast call, answered even
/// while the server is blocked.
pub fn mode(sock: &Path) -> Option<nvmux::rpc::Mode> {
    Client::connect(sock, Duration::from_secs(2))
        .and_then(|mut c| c.get_mode())
        .ok()
}

/// Leave the session in a half-typed multi-key command (mode `n`,
/// `blocking`), which nvmux must not answer for the user: over RPC it looks
/// the same as a wedged session or an abandoned command.
///
/// Unlike [`HitEnter`] this needs no UI (checked on 0.12.5): it is `vgetc`
/// waiting, not `wait_return`. Any key ends it.
pub fn block_on_a_key(sock: &Path) {
    Client::connect(sock, Duration::from_secs(2))
        .expect("connect")
        .input("g")
        .expect("start a multi-key command");
    assert!(
        wait_until(Duration::from_secs(10), || {
            mode(sock).is_some_and(|m| m.blocking && m.mode == "n")
        }),
        "the server never started waiting for the second key"
    );
}

/// End a hit-enter prompt the way a user would.
pub fn press_enter(sock: &Path) {
    Client::connect(sock, Duration::from_secs(2))
        .expect("connect")
        .input("<CR>")
        .expect("press enter");
}

// Helpers for the notifier and session-notes tests.

pub fn lua(sock: &Path, code: &str) -> Value {
    lua_within(sock, code, Duration::from_secs(5))
}

pub fn lua_within(sock: &Path, code: &str, budget: Duration) -> Value {
    let mut client = Client::connect(sock, budget).expect("connect");
    client
        .call(
            "nvim_exec_lua",
            vec![Value::from(code), Value::Array(Vec::new())],
        )
        .expect("nvim_exec_lua")
}

/// Run `printf` in a hidden terminal buffer and keep the program alive, like
/// an agent waiting at a prompt.
pub fn in_a_terminal(sock: &Path, printf: &str) {
    lua(
        sock,
        &format!(
            r#"local buf = vim.api.nvim_create_buf(true, false)
vim.api.nvim_buf_call(buf, function()
  vim.fn.jobstart({{ 'sh', '-c', [=[printf '{printf}'; sleep 30]=] }}, {{ term = true }})
end)"#
        ),
    );
}

/// Run `script` in a terminal buffer (hidden, or in the current window if
/// `on_screen`) and wait for its `TermClose`. Returns the buffer number.
pub fn a_job_that_exits(sock: &Path, script: &str, on_screen: bool) -> i64 {
    let place = if on_screen {
        "vim.cmd('enew')\nlocal buf = vim.api.nvim_get_current_buf()"
    } else {
        "local buf = vim.api.nvim_create_buf(true, false)"
    };
    // The RPC budget exceeds the Lua wait, so a slow close is waited out in
    // Neovim and one that never closes raises a clear error.
    lua_within(
        sock,
        &format!(
            r#"local closed = false
{place}
vim.api.nvim_create_autocmd('TermClose', {{
  buffer = buf,
  once = true,
  callback = function()
    closed = true
  end,
}})
vim.api.nvim_buf_call(buf, function()
  vim.fn.jobstart({{ 'sh', '-c', [=[{script}]=] }}, {{ term = true }})
end)
if not vim.wait(10000, function()
  return closed
end) then
  error('the job never closed')
end
return buf"#
        ),
        Duration::from_secs(15),
    )
    .as_i64()
    .expect("the buffer")
}

pub fn a_session(scratch: &Scratch, name: &str) -> (nvmux::session::Session, PathBuf) {
    let t = scratch.transport();
    let s = t
        .create_session(name, &launch(), anywhere())
        .expect("create");
    let sock = t.local_socket_for(&s).expect("socket path");
    // Wait out startup: the notes' reads have a 1 s budget, and Neovim only
    // answers main-loop calls once started, which is slow on a busy machine.
    lua(&sock, "return 1");
    (s, sock)
}

/// A post's card and its target, or `None` for a `Forget`.
pub fn shown(post: &Post) -> Option<(&Card, Option<Go>)> {
    match &post.what {
        Posted::Card(card, go) => Some((card, *go)),
        Posted::Forget(_) => None,
    }
}

/// Take posts from the board until `want` holds or `within` passes; returns
/// all posts taken.
pub fn posts_until(within: Duration, want: impl Fn(&[Post]) -> bool) -> Vec<Post> {
    let deadline = Instant::now() + within;
    let mut taken = Vec::new();
    while Instant::now() < deadline && !want(&taken) {
        taken.extend(nvmux::card::board().take());
        std::thread::sleep(Duration::from_millis(50));
    }
    taken.extend(nvmux::card::board().take());
    taken
}
