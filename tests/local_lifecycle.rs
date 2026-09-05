//! End-to-end tests for the local session lifecycle, against real
//! `nvim --headless` processes — no TUI, no mocks. The failures they exist for
//! (a session that spawns but is never ready, a stale socket that resurrects a
//! dead session, a kill that signals the wrong pid) cannot be caught by unit
//! tests.
//!
//! Each test gets its own runtime directory, so they run in parallel without
//! racing and never touch the user's real `/tmp/nvmux-<uid>`. They skip, rather
//! than fail, when there is no usable `nvim` on `$PATH`.

use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nvmux::session::Liveness;
use nvmux::transport::local::LocalTransport;
use nvmux::transport::Transport;

/// A scratch runtime directory, removed when the guard drops.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        // A plain counter, not `ThreadId`, whose Debug form is `ThreadId(2)`.
        // Parentheses are regex metacharacters, so a directory named that way
        // cannot be matched literally by anything pattern-based later.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nvmux-it-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }

    fn transport(&self) -> LocalTransport {
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

/// Skip rather than fail where Neovim is missing or too old.
fn nvim_available() -> bool {
    match std::process::Command::new("nvim").arg("--version").output() {
        Ok(out) => nvmux::nvim::parse_version(&String::from_utf8_lossy(&out.stdout))
            .is_some_and(|v| v.is_supported()),
        Err(_) => false,
    }
}

macro_rules! require_nvim {
    () => {
        if !nvim_available() {
            eprintln!("skipping: no usable nvim on $PATH");
            return;
        }
    };
}

#[test]
fn create_list_and_kill_a_session() {
    require_nvim!();
    let scratch = Scratch::new("lifecycle");
    let t = scratch.transport();

    assert!(
        t.list_sessions().expect("list").is_empty(),
        "should start empty"
    );

    let session = t.create_session("dotfiles").expect("create");
    assert_eq!(session.name, "dotfiles");
    assert_eq!(session.id.len(), 8, "id should be 8 base32 chars");
    assert!(session.pid > 1, "spawn must report a validated pid");

    let listed = t.list_sessions().expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "dotfiles");
    assert_eq!(
        listed[0].state.liveness,
        Liveness::Alive,
        "a freshly created session must answer a deferred RPC call"
    );

    let sock = t.local_socket_for(&session).expect("socket path");
    assert!(sock.exists(), "socket missing at {}", sock.display());
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&sock).expect("stat").permissions().mode() & 0o777;
    assert_eq!(
        mode & 0o077,
        0,
        "socket is mode {mode:04o}, reachable by others"
    );

    t.kill_session(&session).expect("kill");
    assert!(
        t.list_sessions().expect("list").is_empty(),
        "kill must remove it"
    );
    assert!(!sock.exists(), "kill must unlink the socket");
}

#[test]
fn the_pid_recorded_is_the_process_serving_the_socket() {
    require_nvim!();
    let scratch = Scratch::new("pid");
    let t = scratch.transport();
    let session = t.create_session("pidcheck").expect("create");
    let sock = t.local_socket_for(&session).expect("socket");

    // Require that the pid nvmux would signal is the one actually holding this
    // socket. Getting it wrong means SIGKILLing an unrelated process.
    let out = std::process::Command::new("ps")
        .args(["-ww", "-eo", "pid=,args="])
        .output()
        .expect("ps");
    let listing = String::from_utf8_lossy(&out.stdout);
    let real: Vec<u32> = listing
        .lines()
        .filter(|l| l.contains(&format!("--listen {}", sock.display())))
        .filter_map(|l| l.split_whitespace().next()?.parse().ok())
        .collect();

    assert_eq!(real.len(), 1, "expected exactly one nvim on this socket");
    assert_eq!(
        session.pid, real[0],
        "recorded pid {} is not the process serving the socket ({})",
        session.pid, real[0]
    );
}

#[test]
fn rename_edits_metadata_and_leaves_the_socket_alone() {
    require_nvim!();
    let scratch = Scratch::new("rename");
    let t = scratch.transport();

    let session = t.create_session("before").expect("create");
    let sock_before = t.local_socket_for(&session).expect("socket");

    t.rename_session(&session, "after").expect("rename");

    let listed = t.list_sessions().expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "after");
    assert_eq!(listed[0].id, session.id, "rename must not change identity");

    // The point of rename-as-metadata-edit: the socket path is stable.
    let sock_after = t.local_socket_for(&listed[0]).expect("socket");
    assert_eq!(sock_before, sock_after, "the socket path must not move");
    assert!(sock_after.exists(), "the socket must still be live");
    assert_eq!(listed[0].state.liveness, Liveness::Alive);
}

#[test]
fn duplicate_names_are_refused_on_create_and_rename() {
    require_nvim!();
    let scratch = Scratch::new("dupes");
    let t = scratch.transport();

    let first = t.create_session("taken").expect("create");
    assert!(
        t.create_session("taken").is_err(),
        "duplicate name must be refused"
    );
    assert!(
        t.create_session("TAKEN").is_err(),
        "duplicate check should not be case-sensitive"
    );

    let second = t.create_session("other").expect("create second");
    assert!(
        t.rename_session(&second, "taken").is_err(),
        "renaming onto an existing name must be refused"
    );
    // ...but renaming a session to its own name is not a conflict with itself.
    t.rename_session(&first, "taken")
        .expect("self-rename should work");
}

/// Killing is unconditional: unsaved buffers neither block it nor are consulted.
/// The route out without losing work is `:q` in the session itself.
#[test]
fn unsaved_buffers_do_not_block_a_kill() {
    require_nvim!();
    let scratch = Scratch::new("unsaved");
    let t = scratch.transport();

    let session = t.create_session("unsaved").expect("create");
    let sock = t.local_socket_for(&session).expect("socket");

    let mut client = nvmux::rpc::Client::connect(&sock, Duration::from_secs(2)).expect("connect");
    client.command("enew").expect("new buffer");
    client
        .command("call setline(1, 'unsaved work')")
        .expect("edit");
    drop(client);

    t.kill_session(&session)
        .expect("kill must not be blocked by unsaved work");
    assert!(t.list_sessions().expect("list").is_empty());
    assert!(!sock.exists(), "kill must unlink the socket");
}

#[test]
fn a_dead_session_is_reaped_from_the_listing() {
    require_nvim!();
    let scratch = Scratch::new("reap");
    let t = scratch.transport();

    let session = t.create_session("doomed").expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let json = sock.with_extension("json");

    // SIGKILL leaves the socket file behind — that is what makes stale cleanup
    // necessary rather than theoretical.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(session.pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("kill -9");

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !sock.exists() {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        sock.exists(),
        "precondition: SIGKILL should leave a stale socket"
    );

    assert!(
        t.list_sessions().expect("list").is_empty(),
        "dead session must be hidden"
    );
    assert!(!sock.exists(), "the stale socket should have been reaped");
    assert!(
        !json.exists(),
        "the orphaned metadata should have been reaped too"
    );
}

/// `connect()` returns ECONNREFUSED for any non-listening inode, including a
/// regular file. Reaping on the errno alone would delete arbitrary files.
#[test]
fn a_regular_file_masquerading_as_a_socket_is_never_deleted() {
    let scratch = Scratch::new("notasocket");
    // 0700, not create_dir_all's default: the runtime-dir security check
    // rightly refuses a group- or world-accessible directory.
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&scratch.0)
        .expect("mkdir 0700");
    let t = scratch.transport();

    let decoy = scratch.0.join("abcdefgh.sock");
    std::fs::write(&decoy, b"precious data, not a socket").expect("write");
    std::fs::write(
        scratch.0.join("abcdefgh.json"),
        br#"{"id":"abcdefgh","name":"decoy","created":1,"pid":999999}"#,
    )
    .expect("write");

    let listed = t.list_sessions().expect("list");
    assert!(
        listed.is_empty(),
        "a non-socket must not appear as a session"
    );
    assert!(
        decoy.exists(),
        "nvmux deleted a regular file it mistook for a stale socket"
    );
    assert_eq!(
        std::fs::read(&decoy).expect("read"),
        b"precious data, not a socket",
        "the file was modified"
    );
}

#[test]
fn a_busy_session_is_never_reaped() {
    require_nvim!();
    let scratch = Scratch::new("busy");
    let t = scratch.transport();

    let session = t.create_session("building").expect("create");
    let sock = t.local_socket_for(&session).expect("socket");

    // Block the editor's main loop the way `:!make` would. `system()` does not
    // pump the event loop, so deferred RPC calls stop answering entirely.
    let mut client = nvmux::rpc::Client::connect(&sock, Duration::from_secs(2)).expect("connect");
    std::thread::spawn(move || {
        let _ = client.command("call system('sleep 8')");
    });
    std::thread::sleep(Duration::from_millis(500));

    let listed = t.list_sessions().expect("list");
    assert_eq!(listed.len(), 1, "a busy session must still be listed");
    assert_eq!(
        listed[0].state.liveness,
        Liveness::Busy,
        "blocked-but-reachable must read as Busy, not Dead"
    );
    assert!(sock.exists(), "a busy session must never be reaped");
}

#[test]
fn names_with_shell_metacharacters_survive_a_round_trip() {
    require_nvim!();
    let scratch = Scratch::new("quoting");
    let t = scratch.transport();

    // Wrong quoting anywhere in the spawn path would fail or mangle these.
    for name in [
        "my project",
        "it's mine",
        r#"say "hi""#,
        "$(whoami)",
        "a;b|c&d",
    ] {
        let session = t
            .create_session(name)
            .unwrap_or_else(|e| panic!("create {name:?}: {e}"));
        let listed = t.list_sessions().expect("list");
        let found = listed
            .iter()
            .find(|s| s.id == session.id)
            .unwrap_or_else(|| panic!("{name:?} vanished from the listing"));
        assert_eq!(found.name, name, "name was mangled in transit");
        t.kill_session(&session).expect("kill");
    }
}

/// Regression: a session busy in CPU-bound Lua must not be reaped.
///
/// The original bug ran `nvim_get_api_info` under a 250ms budget and mapped the
/// resulting timeout to `Dead`, which made `list_sessions` unlink a live
/// session's socket. `api_info` is answered off the main loop during `system()`
/// calls — which is why the `a_busy_session_is_never_reaped` test above passed —
/// but it is *not* answered during CPU-bound Lua, where it was measured at 3.7s.
///
/// So this test uses a busy loop specifically, not `system('sleep')`.
#[test]
fn a_session_busy_in_cpu_bound_lua_is_never_reaped() {
    require_nvim!();
    let scratch = Scratch::new("cpubusy");
    let t = scratch.transport();

    let session = t.create_session("compiling").expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let pid = session.pid;

    // Peg the main loop with real work for longer than any probe budget.
    let mut client = nvmux::rpc::Client::connect(&sock, Duration::from_secs(2)).expect("connect");
    std::thread::spawn(move || {
        let _ = client.command(
            "call luaeval('(function() local t=os.clock() while os.clock()-t<6 do end return 1 end)()')",
        );
    });
    std::thread::sleep(Duration::from_millis(700));

    let listed = t.list_sessions().expect("list");

    let alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok();
    assert!(alive, "precondition: the session should still be running");

    assert!(
        sock.exists(),
        "nvmux deleted the socket of a live session that was merely busy"
    );
    assert_eq!(listed.len(), 1, "a busy session must still be listed");
    assert_ne!(
        listed[0].state.liveness,
        Liveness::Dead,
        "a live session must never be classified Dead"
    );
}

/// Regression: a timed-out call must condemn the connection.
///
/// rmpv consumes the stream incrementally, so abandoning a read part-way leaves
/// the tail of that frame in the socket. Without poisoning, the next call
/// decodes something well-formed out of it and returns it as the answer to a
/// different question — which for `dirty_buffer_count` would mean a wrong
/// unsaved-buffer count in a kill prompt, with no error anywhere.
#[test]
fn a_timed_out_call_poisons_the_connection() {
    require_nvim!();
    let scratch = Scratch::new("poison");
    let t = scratch.transport();
    let session = t.create_session("poisoned").expect("create");
    let sock = t.local_socket_for(&session).expect("socket");

    // Block the editor, then make a call that cannot possibly be answered.
    let mut blocker = nvmux::rpc::Client::connect(&sock, Duration::from_secs(2)).expect("connect");
    std::thread::spawn(move || {
        let _ = blocker.command("call system('sleep 4')");
    });
    std::thread::sleep(Duration::from_millis(400));

    let mut client =
        nvmux::rpc::Client::connect(&sock, Duration::from_millis(300)).expect("connect");
    let first = client.list_bufs();
    assert!(first.is_err(), "the call should have timed out");

    let second = client.list_bufs();
    assert!(
        second.is_err(),
        "a poisoned connection returned a value: {second:?}"
    );
    let msg = second.expect_err("must fail").to_string();
    assert!(
        msg.contains("poisoned"),
        "expected a poisoned-connection error, got: {msg}"
    );
}

/// A wrong recorded pid must not misdirect the kill.
///
/// The pid in `<id>.json` was validated when the session was spawned, but that
/// could have been days ago and pids are reused. So it is treated as a guess:
/// used only while it still owns this session's socket, and otherwise ignored
/// in favour of searching by socket, because the socket is the identity.
///
/// The two failures this guards are opposite and both bad — signalling an
/// innocent process that inherited the number, and failing to kill the session.
#[test]
fn a_recycled_pid_neither_misfires_nor_blocks_the_kill() {
    require_nvim!();
    let scratch = Scratch::new("recycled");
    let t = scratch.transport();
    let session = t.create_session("stubborn").expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let json = sock.with_extension("json");

    // A live pid that is emphatically not this session — the shape a recycled
    // pid takes after a reboot or a long uptime.
    let mut decoy = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn decoy");

    let out = std::process::Command::new("/bin/sh")
        .arg("scripts/kill.sh")
        .arg(&scratch.0)
        .arg(&session.id)
        .arg(decoy.id().to_string())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run kill.sh");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("RESULT killed"),
        "expected the session to be found by socket, got: {stdout}"
    );
    assert!(!sock.exists(), "the socket should have been cleaned up");
    assert!(!json.exists(), "the metadata should have been cleaned up");

    assert!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(decoy.id() as i32), None).is_ok(),
        "kill.sh signalled a process that was not the session"
    );
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(decoy.id() as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    let _ = decoy.wait();
}

/// With nothing serving the socket, the leftover files are swept up.
#[test]
fn killing_an_already_dead_session_cleans_up_its_files() {
    require_nvim!();
    let scratch = Scratch::new("alreadydead");
    let t = scratch.transport();
    let session = t.create_session("ghost").expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let json = sock.with_extension("json");

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(session.pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("kill -9");
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline
        && nix::sys::signal::kill(nix::unistd::Pid::from_raw(session.pid as i32), None).is_ok()
    {
        std::thread::sleep(Duration::from_millis(20));
    }

    t.kill_session(&session)
        .expect("killing a dead session should succeed");
    assert!(!sock.exists(), "the stale socket should be gone");
    assert!(!json.exists(), "the orphaned metadata should be gone");
}

/// A stale pid in the metadata must not stop a normal kill from working.
///
/// The graceful `qa!` goes over the socket and never touches the pid, so the
/// session shuts down cleanly; reporting "could not kill" because a signal was
/// not delivered would be a false alarm about a session that is now gone.
#[test]
fn a_stale_pid_does_not_break_an_otherwise_normal_kill() {
    require_nvim!();
    let scratch = Scratch::new("stalepid");
    let t = scratch.transport();
    let session = t.create_session("staleish").expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let json = sock.with_extension("json");

    let mut meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&json).expect("read")).expect("parse");
    meta["pid"] = serde_json::json!(999_999_u32);
    std::fs::write(&json, meta.to_string()).expect("write");

    let stale = t
        .list_sessions()
        .expect("list")
        .into_iter()
        .find(|s| s.id == session.id)
        .expect("listed");

    t.kill_session(&stale)
        .expect("a graceful kill should still succeed");
    assert!(t.list_sessions().expect("list").is_empty());
    assert!(!sock.exists());
    assert!(!json.exists());
}

/// A session that exits on its own leaves metadata and an unbounded log behind.
///
/// Sessions are discovered through `*.sock`, so once nvim unlinks its own
/// socket those files are invisible to every later listing and nothing would
/// ever clean them up.
#[test]
fn metadata_left_by_a_self_terminating_session_is_swept_up() {
    require_nvim!();
    let scratch = Scratch::new("sweep");
    let t = scratch.transport();

    let session = t.create_session("selfquit").expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let json = sock.with_extension("json");
    let log = sock.with_extension("log");

    let mut client = nvmux::rpc::Client::connect(&sock, Duration::from_secs(2)).expect("connect");
    let _ = client.command("qa!");

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && sock.exists() {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !sock.exists(),
        "precondition: a clean exit unlinks the socket"
    );
    assert!(json.exists(), "precondition: it leaves the metadata behind");

    assert!(t.list_sessions().expect("list").is_empty());
    assert!(!json.exists(), "orphaned metadata was never cleaned up");
    assert!(!log.exists(), "the orphaned log was never cleaned up");
}
