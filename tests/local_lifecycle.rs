//! End-to-end tests for the local session lifecycle.
//!
//! These drive the real [`Transport`] API against real `nvim --headless`
//! processes — no TUI, no mocks. That is the point: the parts of milestone 2
//! most likely to break silently (a session that spawns but is never ready, a
//! stale socket that resurrects a dead session, a kill that signals the wrong
//! pid) cannot be caught by unit tests.
//!
//! Each test gets its own runtime directory so they can run in parallel without
//! racing, and none of them touch the user's real `/tmp/nvmux-<uid>`.
//!
//! They are skipped, not failed, when there is no usable `nvim` on `$PATH`.

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
        // Signal the recorded pids directly rather than pattern-matching with
        // `pkill`: the pid in `<id>.json` was validated against its socket at
        // spawn time, which makes this both precise and immune to any quoting
        // or metacharacter problem in the path.
        if let Ok(entries) = std::fs::read_dir(&self.0) {
            for e in entries.flatten() {
                let p = e.path();
                if !p.extension().is_some_and(|x| x == "json") {
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

    // The three files exist, and the socket is not group- or world-accessible.
    let sock = t.local_socket_for(&session).expect("socket path");
    assert!(sock.exists(), "socket missing at {}", sock.display());
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&sock).expect("stat").permissions().mode() & 0o777;
    assert_eq!(
        mode & 0o077,
        0,
        "socket is mode {mode:04o}, reachable by others"
    );

    t.kill_session(&session, false).expect("kill");
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

    // Ask the OS which process actually holds this socket, and require that it
    // is the pid nvmux would signal. Getting this wrong means SIGKILLing an
    // unrelated process.
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

    // The whole point of rename-as-metadata-edit: the socket path is stable, so
    // nothing has to re-listen and no SSH forward has to be rebuilt.
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

#[test]
fn unsaved_buffers_block_a_kill_until_forced() {
    require_nvim!();
    let scratch = Scratch::new("dirty");
    let t = scratch.transport();

    let session = t.create_session("unsaved").expect("create");
    let sock = t.local_socket_for(&session).expect("socket");

    // Make a buffer genuinely modified.
    let mut client = nvmux::rpc::Client::connect(&sock, Duration::from_secs(2)).expect("connect");
    client.command("enew").expect("new buffer");
    client
        .command("call setline(1, 'unsaved work')")
        .expect("edit");

    let dirty = client.dirty_buffer_count().expect("count");
    assert_eq!(dirty, 1, "the buffer should register as modified");
    drop(client);

    // A plain kill must refuse and say why.
    let err = t.kill_session(&session, false).expect_err("must refuse");
    let msg = err.to_string();
    assert!(
        msg.contains("unsaved") && msg.contains('1'),
        "the error should name the unsaved count: {msg}"
    );
    assert!(
        sock.exists(),
        "a refused kill must not have killed anything"
    );

    // Forcing gets through.
    t.kill_session(&session, true).expect("forced kill");
    assert!(t.list_sessions().expect("list").is_empty());
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

    // If quoting were wrong anywhere in the spawn path, these would either fail
    // to create or come back mangled.
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
        t.kill_session(&session, true).expect("kill");
    }
}
