//! Shared by the integration test files.
//!
//! The suites need something the machine may not have — a usable `nvim`, an
//! ssh host that answers — and skip rather than fail without it, so a laptop
//! without one still runs the rest. In CI a skip is a silent no-op, which is
//! why `$NVMUX_TEST_REQUIRE` exists: a comma-separated list of what must be
//! present (`nvim`, `ssh`), turning the skip for each into a failure.

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

/// What a session is launched with unless the test is about the command
/// itself: the built-in default, which is what nvmux ran before the command
/// could be chosen at all.
pub fn launch() -> Launch {
    Launch::parse(nvmux::launch::DEFAULT).expect("the built-in default must parse")
}

/// The working directory for a test that does not care which one it gets.
///
/// `/` rather than the scratch directory: it exists on both hosts a test may
/// run against, needs no setup, and cannot be swept away by a `Scratch` drop
/// while a session is still sitting in it. The tests that *do* care name their
/// own directory, and say why.
pub fn anywhere() -> &'static str {
    "/"
}

/// A launch command with extra arguments spliced in, for the tests that check
/// the chosen command really is what runs.
pub fn launch_with(args: &str) -> Launch {
    Launch::parse(&format!("nvim {args} --headless --listen {{sock}}")).expect("a valid command")
}

/// Whether `$NVMUX_TEST_REQUIRE` names this requirement.
pub fn required(what: &str) -> bool {
    std::env::var("NVMUX_TEST_REQUIRE")
        .map(|v| v.split(',').any(|w| w.trim() == what))
        .unwrap_or(false)
}

/// Skip the test — or fail it, when `$NVMUX_TEST_REQUIRE` names `$what` — if
/// `$available` is false.
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

/// Skip rather than fail where Neovim is missing or too old: the same check
/// `main` makes before it will run at all.
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

/// A scratch runtime directory, removed when the guard drops. Each test gets
/// its own, so tests run in parallel without racing and never touch the
/// user's real `/tmp/nvmux-<uid>`.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(tag: &str) -> Self {
        // A session's socket is made in here, and a socket's path may be 100
        // bytes long at most (see `nvmux::paths`). On macOS the temporary
        // directory takes 49 of them, a process id up to five, the counter
        // below two and the rest of the socket's path 25: a tag longer than
        // the 19 left passes on Linux and fails there.
        assert!(
            tag.len() <= 19,
            "the tag {tag:?} is too long for a socket's path on macOS"
        );
        // A plain counter, not `ThreadId`, whose Debug form is `ThreadId(2)`.
        // Parentheses are regex metacharacters, so a directory named that way
        // cannot be matched literally by anything pattern-based later.
        //
        // In base 36, which keeps it to two characters for 1,296 scratches a
        // binary: tests/client.rs makes more than a hundred, and in decimal a
        // third digit put a socket under a 19-byte tag one byte past the limit.
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

    /// A session's `<id>.json` as it was written, for the tests that care what
    /// reached disk rather than what the transport returned.
    pub fn metadata(&self, id: &str) -> Session {
        let path = self.0.join(format!("{id}.json"));
        let bytes = std::fs::read(&path).expect("read the session metadata");
        Session::from_json(&bytes, &path).expect("parse the session metadata")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Kill anything still listening before removing the directory, or
        // stray nvim processes survive the run and pile up across runs.
        //
        // Signal the recorded pids directly rather than with `pkill`: the pid
        // in `<id>.json` was validated against its socket at spawn time. That
        // leaves a live session with no `<id>.json` out of reach, so a test
        // that makes one has to kill it itself.
        //
        // And only a process that is that session still, its socket on its
        // command line: a session that has gone may have left its pid to
        // another process by now — another test's, run alongside, or anything
        // else of the user's — and a test may write a pid of its own making.
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

/// A running process's command line, the way the session-host scripts read one.
/// Empty when it cannot be found, which the caller's assertion then reports.
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

/// The umask these suites pretend nvmux was launched with.
///
/// Permissive, and the shared-group setting rather than a value no login would
/// produce, so a session that reports it can only have inherited it — nothing
/// in nvmux would arrive at 0002 any other way.
pub const LAUNCH_UMASK: libc::mode_t = 0o002;

/// Establish [`LAUNCH_UMASK`] as this binary's launch mask, the way `main` does
/// it: clamp the process, and record the mask that clamp replaced.
///
/// Once per binary, because `paths::restrict_umask` keeps the first mask it
/// sees. The process's own mask is put straight back, so the tests running
/// alongside are left as they were — and nothing has to hold still afterwards,
/// since what the spawn path reads from here on is the *recorded* mask rather
/// than the live one.
pub fn establish_launch_umask() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let original = nix::sys::stat::umask(Mode::from_bits_truncate(LAUNCH_UMASK));
        nvmux::paths::restrict_umask();
        nix::sys::stat::umask(original);
    });
}

/// The mask `umask` printed, read back as a number: sh prints `0002`, but a
/// session's `system()` runs the user's 'shell', and fish prints `002`.
pub fn printed_umask(printed: &str) -> libc::mode_t {
    libc::mode_t::from_str_radix(printed.trim(), 8)
        .unwrap_or_else(|_| panic!("umask printed {printed:?}, not an octal mask"))
}

/// Names every test session distinctly, so a failed run cannot poison the next.
pub fn unique(tag: &str) -> String {
    format!("it-{tag}-{}", std::process::id())
}

/// Wait up to `within` for `cond` to hold, polling gently. Returns whether it
/// did, so a caller can assert with its own message.
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

/// `kill -9`, then wait for the process to actually go.
///
/// SIGKILL is asynchronous: without the wait, the assertions that follow race
/// the kernel. The socket file is deliberately left behind — that is what makes
/// stale cleanup necessary rather than theoretical.
pub fn sigkill_and_wait(pid: u32) {
    let target = Pid::from_raw(pid as i32);
    nix::sys::signal::kill(target, Signal::SIGKILL).expect("kill -9");
    assert!(
        wait_until(Duration::from_secs(3), || !alive(pid)),
        "pid {pid} survived SIGKILL"
    );
}

/// Is this pid still signallable?
pub fn alive(pid: u32) -> bool {
    nix::sys::signal::kill(Pid::from_raw(pid as i32), None).is_ok()
}

/// The session with this id in a fresh listing, or `None` if it is gone.
pub fn find_by_id<T: Transport>(t: &T, id: &str) -> Option<Session> {
    t.list_sessions()
        .expect("list")
        .into_iter()
        .find(|s| s.id == id)
}

/// Read a session's `<id>.json` as raw JSON.
pub fn read_meta(json: &Path) -> serde_json::Value {
    let bytes = std::fs::read(json).expect("read metadata");
    serde_json::from_slice(&bytes).expect("parse metadata")
}

/// Write a session's `<id>.json` back.
pub fn write_meta(json: &Path, value: &serde_json::Value) {
    std::fs::write(json, serde_json::to_vec(value).expect("encode")).expect("write metadata");
}

/// Occupy a session's main loop with `cmd`, so it is reachable but cannot
/// answer a deferred call.
///
/// The thread is deliberately abandoned: the block is the point, and joining it
/// would wait out the very command that is meant to still be running. `settle`
/// is how long to let the call land before the caller looks, and stays per-call
/// because the commands take different amounts of time to get going.
pub fn block_editor(sock: &Path, cmd: &'static str, settle: Duration) {
    let sock = sock.to_path_buf();
    std::thread::spawn(move || {
        let mut c = Client::connect(&sock, Duration::from_secs(2)).expect("connect");
        let _ = c.command(cmd);
    });
    std::thread::sleep(settle);
}

/// A real `--remote-ui` client on a throwaway pty, holding its session at a
/// hit-enter prompt (`Press ENTER or type command to continue`) until dropped.
///
/// [`block_editor`] cannot produce that prompt: a headless server with no UI
/// returns from `wait_return()` at once (`message.c`: `if (headless_mode &&
/// !ui_active()) return;`), and `getchar()`/`input()` keep serving deferred
/// calls while they wait. The prompt needs a UI, so this attaches one the way
/// nvmux does, then opens the prompt with a fast `nvim_input`: a two-line
/// message scrolls at any `'cmdheight'`, so no option has to be set for it.
///
/// The prompt outlives this client — it is the *server* that is waiting — so
/// a test that wants it ended must [`press_enter`] itself.
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
            // Takes the slave, or the master would never see EOF once the
            // client exits.
            .spawn(pts)
            .expect("spawn remote-ui client");

        // Drain what the client draws: on a full pty it blocks mid-write and
        // stops talking to the server.
        let mut reader = reader(&master);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while matches!(reader.read(&mut buf), Ok(n) if n > 0) {}
        });

        let this = Self {
            child,
            _master: master,
        };

        // The UI has to be attached before the prompt can exist. A deferred
        // call, which is fine: nothing is blocked yet.
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

/// A second handle on a pty's master, for a thread to read the child from. A
/// read once the child has gone ends in EOF on macOS and EIO on Linux.
pub fn reader(master: &Pty) -> std::fs::File {
    let fd = master
        .as_fd()
        .try_clone_to_owned()
        .expect("clone the pty master");
    std::fs::File::from(fd)
}

/// End a child as the tests have always ended one, which is how portable-pty's
/// `kill` did: a hangup, a moment to act on it, and SIGKILL only for a child
/// still there after that. Waited for either way; one already waited for is
/// left alone, since its pid may be someone else's by now.
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

/// How a child exited, as the tests compare it: its exit code, or 1 for one a
/// signal ended.
pub fn exit_code(status: ExitStatus) -> u32 {
    status.code().map_or(1, |code| code as u32)
}

/// Whether the server behind `sock` is at a hit-enter prompt right now. A
/// fast call, answered even then.
pub fn at_hit_enter(sock: &Path) -> bool {
    mode(sock).is_some_and(|m| m.at_hit_enter())
}

/// The mode the server behind `sock` is in right now, or `None` if it could
/// not be asked. A fast call, answered even while it is blocked, which is the
/// whole reason anything asks it.
pub fn mode(sock: &Path) -> Option<nvmux::rpc::Mode> {
    Client::connect(sock, Duration::from_secs(2))
        .and_then(|mut c| c.get_mode())
        .ok()
}

/// Leave the session waiting for a key in a state nvmux must *not* answer for
/// the user: a half-typed multi-key command, which is mode `n` with `blocking`
/// set.
///
/// Unlike [`HitEnter`] this needs no UI — measured on 0.12.5, a headless server
/// with none blocks on this exactly as one with a UI does, because it is
/// `vgetc` waiting for the second key rather than `wait_return` waiting to be
/// dismissed. Which is the point of the state: it is indistinguishable over RPC
/// from a session wedged by a crashed callback, and from a command the user
/// started and walked away from, so nvmux may not type into it.
///
/// Ends on any key. Nothing here presses one, which is what the tests check.
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

// What the tests of the notifier, and of the sessions telling it their news,
// share: a session, what happens in it, and the cards it brings.

/// Run Lua in the session, as a test's own stand-in for whatever a plugin or
/// a terminal program would have done there.
pub fn lua(sock: &Path, code: &str) -> Value {
    lua_within(sock, code, Duration::from_secs(5))
}

/// [`lua`], given `budget` to answer in.
pub fn lua_within(sock: &Path, code: &str, budget: Duration) -> Value {
    let mut client = Client::connect(sock, budget).expect("connect");
    client
        .call(
            "nvim_exec_lua",
            vec![Value::from(code), Value::Array(Vec::new())],
        )
        .expect("nvim_exec_lua")
}

/// A program in a hidden terminal buffer, printing `printf`'s format and
/// staying up a while, as an agent at a prompt does.
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

/// A job that runs `script` and exits, in a terminal buffer no window shows —
/// or, `on_screen`, in the current window — waited for until its `TermClose`
/// has been, so that the watcher has seen it go. Says which buffer it was.
pub fn a_job_that_exits(sock: &Path, script: &str, on_screen: bool) -> i64 {
    let place = if on_screen {
        "vim.cmd('enew')\nlocal buf = vim.api.nvim_get_current_buf()"
    } else {
        "local buf = vim.api.nvim_create_buf(true, false)"
    };
    // Given longer than the wait in it, so that a job slow to close, on a
    // busy machine, is waited out by Neovim and not by the call; and one
    // that never closes fails the call, saying so.
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
    // Past its startup before a test asks it anything on a budget of a
    // second, as the notes' reads do: Neovim takes a call that waits for its
    // main loop only once it has started, which a busy machine makes later.
    lua(&sock, "return 1");
    (s, sock)
}

/// The card a post shows, and where acting on it goes: none for one that lets
/// go of what is up.
pub fn shown(post: &Post) -> Option<(&Card, Option<Go>)> {
    match &post.what {
        Posted::Card(card, go) => Some((card, *go)),
        Posted::Forget(_) => None,
    }
}

/// Wait up to `within` for posts on the board that `want` says are the ones,
/// and hand back every post taken meanwhile.
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
