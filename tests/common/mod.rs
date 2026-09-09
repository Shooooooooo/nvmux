//! Shared by the integration test files.
//!
//! The suites need something the machine may not have — a usable `nvim`, an
//! ssh host that answers — and skip rather than fail without it, so a laptop
//! without one still runs the rest. In CI a skip is a silent no-op, which is
//! why `$NVMUX_TEST_REQUIRE` exists: a comma-separated list of what must be
//! present (`nvim`, `ssh`), turning the skip for each into a failure.

#![allow(dead_code)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::sys::signal::Signal;
use nix::unistd::Pid;
use nvmux::rpc::Client;
use nvmux::session::Session;
use nvmux::transport::local::LocalTransport;
use nvmux::transport::Transport;
use portable_pty::{CommandBuilder, PtySize};

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

/// Skip rather than fail where Neovim is missing or too old.
pub fn nvim_available() -> bool {
    match std::process::Command::new("nvim").arg("--version").output() {
        Ok(out) => nvmux::nvim::parse_version(&String::from_utf8_lossy(&out.stdout))
            .is_some_and(|v| v.is_supported()),
        Err(_) => false,
    }
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
        // A plain counter, not `ThreadId`, whose Debug form is `ThreadId(2)`.
        // Parentheses are regex metacharacters, so a directory named that way
        // cannot be matched literally by anything pattern-based later.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nvmux-it-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }

    pub fn transport(&self) -> LocalTransport {
        LocalTransport::with_dir(self.0.clone()).expect("build transport")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Kill anything still listening before removing the directory, or
        // stray nvim processes survive the run and pile up across runs.
        //
        // Signal the recorded pids directly rather than with `pkill`: the pid
        // in `<id>.json` was validated against its socket at spawn time.
        if let Ok(entries) = std::fs::read_dir(&self.0) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().is_none_or(|x| x != "json") {
                    continue;
                }
                let pid = std::fs::read_to_string(&p)
                    .ok()
                    .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
                    .and_then(|v| v.get("pid").and_then(serde_json::Value::as_i64))
                    .filter(|pid| *pid > 1);
                if let Some(pid) = pid {
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(pid as i32),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Names every test session distinctly, so a failed run cannot poison the next.
pub fn unique(tag: &str) -> String {
    format!("it-{tag}-{}", std::process::id())
}

/// Wait up to `within` for `cond` to hold, polling gently. Returns whether it
/// did, so a caller can assert with its own message.
pub fn wait_until(within: Duration, cond: impl Fn() -> bool) -> bool {
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
    child: Box<dyn portable_pty::Child + Send + Sync>,
    // Held so the pty outlives the client; dropped last.
    _master: Box<dyn portable_pty::MasterPty>,
}

impl HitEnter {
    pub fn open(sock: &Path) -> Self {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new("nvim");
        cmd.arg("--server");
        cmd.arg(sock);
        cmd.arg("--remote-ui");
        cmd.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(cmd)
            .expect("spawn remote-ui client");
        // Or the master would never see EOF once the client exits.
        drop(pair.slave);

        // Drain what the client draws: on a full pty it blocks mid-write and
        // stops talking to the server.
        let mut reader = pair.master.try_clone_reader().expect("reader");
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while matches!(reader.read(&mut buf), Ok(n) if n > 0) {}
        });

        let this = Self {
            child,
            _master: pair.master,
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
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether the server behind `sock` is at a hit-enter prompt right now. A
/// fast call, answered even then.
pub fn at_hit_enter(sock: &Path) -> bool {
    Client::connect(sock, Duration::from_secs(2))
        .and_then(|mut c| c.get_mode())
        .is_ok_and(|m| m.at_hit_enter())
}

/// End a hit-enter prompt the way a user would.
pub fn press_enter(sock: &Path) {
    Client::connect(sock, Duration::from_secs(2))
        .expect("connect")
        .input("<CR>")
        .expect("press enter");
}
