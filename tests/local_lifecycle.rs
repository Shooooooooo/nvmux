//! End-to-end tests for the local session lifecycle, against real
//! `nvim --headless` processes — no TUI, no mocks. The failures they exist for
//! (a session that spawns but is never ready, a stale socket that resurrects a
//! dead session, a kill that signals the wrong pid) cannot be caught by unit
//! tests.
//!
//! Each test gets its own runtime directory, so they run in parallel without
//! racing and never touch the user's real `/tmp/nvmux-<uid>`. They skip, rather
//! than fail, when there is no usable `nvim` on `$PATH` — unless
//! `$NVMUX_TEST_REQUIRE` names `nvim`, which is how CI keeps them honest.

#[macro_use]
mod common;

use std::os::unix::fs::DirBuilderExt;
use std::time::{Duration, Instant};

use common::{command_line, Scratch};
use nvmux::session::Liveness;
use nvmux::transport::Transport;

#[test]
fn create_list_and_kill_a_session() {
    require_nvim!();
    let scratch = Scratch::new("lifecycle");
    let t = scratch.transport();

    assert!(
        t.list_sessions().expect("list").is_empty(),
        "should start empty"
    );

    let session = t
        .create_session("dotfiles", &common::launch())
        .expect("create");
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
    let session = t
        .create_session("pidcheck", &common::launch())
        .expect("create");
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

    let session = t
        .create_session("before", &common::launch())
        .expect("create");
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

    // The number is as much a handle as the id: a rename must not move it, or
    // `<prefix> <n>` would start naming a different session.
    assert_eq!(
        listed[0].num, session.num,
        "rename must not change the number"
    );
    assert_eq!(listed[0].state.num, session.num);
}

/// Numbers are assigned once and kept, and a killed session's number is handed
/// out again so the list stays dense and single-digit for as long as possible.
#[test]
fn session_numbers_are_stable_and_a_freed_one_is_reused() {
    require_nvim!();
    let scratch = Scratch::new("numbers");
    let t = scratch.transport();

    let first = t
        .create_session("first", &common::launch())
        .expect("create");
    let second = t
        .create_session("second", &common::launch())
        .expect("create");
    let third = t
        .create_session("third", &common::launch())
        .expect("create");
    assert_eq!(
        (first.num, second.num, third.num),
        (1, 2, 3),
        "numbering starts at 1 and counts up"
    );

    t.kill_session(&second).expect("kill");

    // The survivors keep the numbers they were given: killing 2 must not
    // renumber 3 down to 2.
    let listed = t.list_sessions().expect("list");
    let numbered: Vec<(String, u32)> = listed
        .iter()
        .map(|s| (s.name.clone(), s.state.num))
        .collect();
    assert_eq!(
        numbered,
        vec![("first".to_string(), 1), ("third".to_string(), 3)],
        "the gap is left where the killed session was"
    );

    // And the next create refills the hole rather than climbing to 4.
    let fourth = t
        .create_session("fourth", &common::launch())
        .expect("create");
    assert_eq!(fourth.num, 2, "the freed number is handed out again");

    let listed = t.list_sessions().expect("list");
    let nums: Vec<u32> = listed.iter().map(|s| s.state.num).collect();
    assert_eq!(nums, vec![1, 2, 3], "the list reads in number order");
}

/// The number has to survive the round trip through `<id>.json`, which is the
/// only thing a second nvmux on another machine ever sees.
#[test]
fn a_number_survives_the_metadata_round_trip() {
    require_nvim!();
    let scratch = Scratch::new("num-roundtrip");
    let t = scratch.transport();

    let session = t.create_session("only", &common::launch()).expect("create");
    let v = common::read_meta(&scratch.0.join(format!("{}.json", session.id)));
    assert_eq!(
        v.get("num").and_then(serde_json::Value::as_u64),
        Some(u64::from(session.num)),
        "the number must be on disk, not just in memory: {v}"
    );

    assert_eq!(t.list_sessions().expect("list")[0].num, session.num);
}

/// Metadata written before numbering existed has no `num` key at all. It must
/// still list, and still be reachable by keystroke.
#[test]
fn metadata_without_a_number_still_lists_and_gets_one() {
    require_nvim!();
    let scratch = Scratch::new("num-legacy");
    let t = scratch.transport();

    let session = t
        .create_session("legacy", &common::launch())
        .expect("create");
    let path = scratch.0.join(format!("{}.json", session.id));

    // Rewrite it the way an older nvmux would have.
    let mut v = common::read_meta(&path);
    v.as_object_mut().expect("object").remove("num");
    common::write_meta(&path, &v);

    let listed = t.list_sessions().expect("list");
    assert_eq!(listed.len(), 1, "an unnumbered session must still list");
    assert_eq!(listed[0].num, 0, "still unnumbered on disk");
    assert_eq!(
        listed[0].state.num, 1,
        "but given a number to display and press"
    );
}

#[test]
fn duplicate_names_are_refused_on_create_and_rename() {
    require_nvim!();
    let scratch = Scratch::new("dupes");
    let t = scratch.transport();

    let first = t
        .create_session("taken", &common::launch())
        .expect("create");
    assert!(
        t.create_session("taken", &common::launch()).is_err(),
        "duplicate name must be refused"
    );
    assert!(
        t.create_session("TAKEN", &common::launch()).is_err(),
        "duplicate check should not be case-sensitive"
    );

    let second = t
        .create_session("other", &common::launch())
        .expect("create second");
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

    let session = t
        .create_session("unsaved", &common::launch())
        .expect("create");
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

    let session = t
        .create_session("doomed", &common::launch())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let json = sock.with_extension("json");

    common::sigkill_and_wait(session.pid);
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

    let session = t
        .create_session("building", &common::launch())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");

    // Block the editor's main loop the way `:!make` would. `system()` does not
    // pump the event loop, so deferred RPC calls stop answering entirely.
    common::block_editor(&sock, "call system('sleep 8')", Duration::from_millis(500));

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
            .create_session(name, &common::launch())
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

    let session = t
        .create_session("compiling", &common::launch())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let pid = session.pid;

    // Peg the main loop with real work for longer than any probe budget.
    common::block_editor(
        &sock,
        "call luaeval('(function() local t=os.clock() while os.clock()-t<6 do end return 1 end)()')",
        Duration::from_millis(700),
    );

    let listed = t.list_sessions().expect("list");

    assert!(
        common::alive(pid),
        "precondition: the session should still be running"
    );

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
    let session = t
        .create_session("poisoned", &common::launch())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");

    // Block the editor, then make a call that cannot possibly be answered.
    common::block_editor(&sock, "call system('sleep 4')", Duration::from_millis(400));

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
    let session = t
        .create_session("stubborn", &common::launch())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let json = sock.with_extension("json");

    // A live pid that is emphatically not this session — the shape a recycled
    // pid takes after a reboot or a long uptime.
    let mut decoy = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn decoy");

    // The embedded script, not the file on disk: this is the copy that ships.
    let out = nvmux::proc::run_local(
        nvmux::shell::KILL_SCRIPT,
        &[
            &scratch.0.to_string_lossy(),
            &session.id,
            &decoy.id().to_string(),
        ],
    )
    .expect("run kill.sh");
    let stdout = out.stdout;

    assert!(
        stdout.contains("RESULT killed"),
        "expected the session to be found by socket, got: {stdout}"
    );
    assert!(!sock.exists(), "the socket should have been cleaned up");
    assert!(!json.exists(), "the metadata should have been cleaned up");

    assert!(
        common::alive(decoy.id()),
        "kill.sh signalled a process that was not the session"
    );
    let _ = decoy.kill();
    let _ = decoy.wait();
}

/// With nothing serving the socket, the leftover files are swept up.
#[test]
fn killing_an_already_dead_session_cleans_up_its_files() {
    require_nvim!();
    let scratch = Scratch::new("alreadydead");
    let t = scratch.transport();
    let session = t
        .create_session("ghost", &common::launch())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let json = sock.with_extension("json");

    common::sigkill_and_wait(session.pid);

    t.kill_session(&session)
        .expect("killing a dead session should succeed");
    assert!(!sock.exists(), "the stale socket should be gone");
    assert!(!json.exists(), "the orphaned metadata should be gone");
}

/// A host with `/proc` but no `ps` must still be able to kill a session.
///
/// `list.sh` always had a `/proc` path; `kill.sh` and `spawn.sh` did not, and
/// looked the session up with `ps` alone. On a minimal container — `/proc`
/// mounted, procps not installed — `kill.sh` therefore found nothing, reported
/// `absent`, deleted the socket and left nvim running with no socket left for
/// any listing to find it by. Exactly the orphan its own comments forbid.
///
/// Linux only: the fallback being tested is the `/proc` one.
#[test]
#[cfg(target_os = "linux")]
fn a_session_can_be_killed_on_a_host_with_proc_but_no_ps() {
    require_nvim!();
    if !std::path::Path::new("/proc/self/cmdline").exists() {
        eprintln!("skipping: no /proc");
        return;
    }
    let scratch = Scratch::new("nops");
    let t = scratch.transport();
    let session = t
        .create_session("noprocps", &common::launch())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");

    // A PATH with everything the script needs except `ps`.
    let bin = scratch.0.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    for tool in [
        "sh", "tr", "grep", "awk", "rm", "sleep", "kill", "id", "printf", "cut", "dirname",
    ] {
        if let Ok(real) = which(tool) {
            let _ = std::os::unix::fs::symlink(real, bin.join(tool));
        }
    }

    let out = std::process::Command::new(bin.join("sh"))
        .arg("-s")
        .arg(&scratch.0)
        .arg(&session.id)
        .arg("")
        .env_clear()
        .env("PATH", &bin)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin
                .take()
                .expect("stdin")
                .write_all(nvmux::shell::KILL_SCRIPT.as_bytes())?;
            c.wait_with_output()
        })
        .expect("run kill.sh");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("RESULT killed"),
        "without ps the session must still be found by /proc, got: {stdout} \
         stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        common::wait_until(Duration::from_secs(3), || !common::alive(session.pid)),
        "the session should have been signalled, not just reported gone"
    );
    assert!(!sock.exists(), "the socket should have been removed");
}

/// The first `tool` on `$PATH`, so the sandbox above can link real binaries.
#[cfg(target_os = "linux")]
fn which(tool: &str) -> Result<std::path::PathBuf, ()> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join(tool))
        .find(|c| c.is_file())
        .ok_or(())
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
    let session = t
        .create_session("staleish", &common::launch())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let json = sock.with_extension("json");

    let mut meta = common::read_meta(&json);
    meta["pid"] = serde_json::json!(999_999_u32);
    common::write_meta(&json, &meta);

    let stale = common::find_by_id(&t, &session.id).expect("listed");

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

    let session = t
        .create_session("selfquit", &common::launch())
        .expect("create");
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

/// A session waiting at a hit-enter prompt cannot answer a deferred call, and
/// the picker used to wait the whole probe budget for it — on a cleared
/// screen, once per such session. It is listed at once now, as busy, and kept:
/// it is reachable, so it must never be reaped.
///
/// AstroNvim's `cmdheight = 0` puts a session there on every one-line error;
/// any message that scrolls does it at the default `cmdheight`.
#[test]
fn a_session_at_a_hit_enter_prompt_is_listed_at_once_as_busy() {
    require_nvim!();
    let scratch = Scratch::new("hitenter");
    let t = scratch.transport();
    let session = t
        .create_session("prompted", &common::launch())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let _ui = common::HitEnter::open(&sock);

    let started = Instant::now();
    let listed = t.list_sessions().expect("list");
    let took = started.elapsed();

    assert!(
        took < Duration::from_secs(1),
        "listing took {took:?}: the probe waited on a deferred call again"
    );
    let s = listed
        .iter()
        .find(|s| s.id == session.id)
        .expect("a session at a prompt must still be listed");
    assert_eq!(s.state.liveness, Liveness::Busy);
    assert!(sock.exists(), "a session at a prompt must never be reaped");
}

/// The probe's verdict is "busy" only while the prompt lasts: once the prompt
/// is ended, the same session is alive again. Pins both branches the probe took.
#[test]
fn probe_is_busy_at_a_hit_enter_prompt_and_alive_after_it() {
    require_nvim!();
    let scratch = Scratch::new("hitenterprobe");
    let t = scratch.transport();
    let session = t
        .create_session("prompted", &common::launch())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let _ui = common::HitEnter::open(&sock);

    let started = Instant::now();
    assert_eq!(nvmux::rpc::probe(&sock), Liveness::Busy);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the probe waited out a deferred call"
    );

    common::press_enter(&sock);
    assert!(
        common::wait_until(Duration::from_secs(5), || nvmux::rpc::probe(&sock)
            == Liveness::Alive),
        "the session should be alive again once the prompt is over"
    );
}

/// A fresh attach to a session at a hit-enter prompt used to fail after the
/// probe budget — `nvim_list_uis` is deferred, and the only client that could
/// end the prompt was the one about to be started. It now ends the prompt
/// itself, with the one key the prompt consumes, and attaches.
#[test]
fn a_fresh_attach_to_a_session_at_a_hit_enter_prompt_ends_it_and_attaches() {
    require_nvim!();
    let scratch = Scratch::new("hitenterattach");
    let t = scratch.transport();
    let session = t
        .create_session("prompted", &common::launch())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let _ui = common::HitEnter::open(&sock);

    let started = Instant::now();
    let attachment = nvmux::pty::spawn(&session.id, &sock).expect("attach");
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(2),
        "the attach took {took:?}: it waited on the prompt"
    );

    assert!(
        !common::at_hit_enter(&sock),
        "the prompt should have been ended by the attach"
    );
    // `spawn` returns as soon as the client is forked, and the client takes a
    // few milliseconds to attach its UI; the count is polled for, not asserted
    // at once. Two means the helper's client and the new one, and that the
    // deferred call answering is the proof the prompt is over.
    assert!(
        common::wait_until(Duration::from_secs(5), || {
            nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
                .and_then(|mut c| c.list_uis())
                .is_ok_and(|n| n == 2)
        }),
        "the new client never attached its UI"
    );

    // Retires the new client; the helper's goes with `_ui`.
    attachment.terminate();
}

/// The whole point of asking how nvim is launched: the command chosen is the
/// command that runs, and the session it starts is an ordinary session —
/// listed, reachable, and killable by the same socket-matching as any other.
#[test]
fn a_chosen_command_is_what_runs_and_the_session_behaves_normally() {
    require_nvim!();
    let scratch = Scratch::new("chosen-command");
    let t = scratch.transport();

    let launch = common::launch_with("--clean");
    let session = t.create_session("bespoke", &launch).expect("create");

    let listed = t.list_sessions().expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].state.liveness,
        Liveness::Alive,
        "a session started with a chosen command is a session like any other"
    );

    // The argument is in the running process, so it really was passed through
    // rather than dropped somewhere between the prompt and the spawn.
    let args = command_line(session.pid);
    assert!(
        args.contains("--clean"),
        "the chosen argument never reached nvim: {args:?}"
    );
    assert!(
        args.contains("--headless"),
        "nor did the rest of the line: {args:?}"
    );

    // And it is recorded, so a session's metadata says what produced it.
    let stored = scratch.metadata(&session.id);
    assert_eq!(stored.command, launch.line());

    // Killing still finds it: it is found by its socket, and the socket is
    // where the command said to put it.
    t.kill_session(&session).expect("kill");
    assert!(t.list_sessions().expect("list").is_empty());
}

/// A command that names nothing must fail fast and say what was wrong, rather
/// than spending the readiness budget and blaming the session for not starting.
#[test]
fn a_command_that_names_nothing_fails_at_once_and_says_so() {
    require_nvim!();
    let scratch = Scratch::new("no-such-command");
    let t = scratch.transport();

    let launch = nvmux::launch::Launch::parse("nvmux-no-such-editor --listen {sock}")
        .expect("a well-formed command naming a program that is not there");

    let started = std::time::Instant::now();
    let err = t
        .create_session("hopeless", &launch)
        .expect_err("nothing could have started");
    assert!(
        err.to_string().contains("nvmux-no-such-editor"),
        "the error must name the command: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "it must not wait out the five-second readiness budget first"
    );
    assert!(
        t.list_sessions().expect("list").is_empty(),
        "a failed create must leave nothing behind"
    );
}

/// The sharp edge of letting the whole line be edited: `--listen <sock>` need
/// not come last, and a session with arguments after it must still be found.
///
/// The scripts' fast path reads a `--listen` argument to the end of the line, so
/// this is exactly the shape it cannot match. Listing and killing both have to
/// fall through to asking the process table for the socket instead — which is
/// the difference between a session nvmux can manage and one it has orphaned.
#[test]
fn a_session_whose_socket_is_not_the_last_argument_is_still_found_and_killed() {
    require_nvim!();
    let scratch = Scratch::new("socket-not-last");
    let t = scratch.transport();

    let launch = nvmux::launch::Launch::parse("nvim --headless --listen {sock} --clean")
        .expect("a valid command");
    let session = t.create_session("trailing", &launch).expect("create");

    let args = command_line(session.pid);
    assert!(
        args.trim_end().ends_with("--clean"),
        "the fixture must put something after the socket: {args:?}"
    );

    let listed = t.list_sessions().expect("list");
    assert_eq!(listed.len(), 1, "the session must still be listed");
    assert_eq!(listed[0].state.liveness, Liveness::Alive);

    let sock = t.local_socket_for(&session).expect("socket path");
    t.kill_session(&session).expect("kill");
    assert!(
        t.list_sessions().expect("list").is_empty(),
        "a session nvmux cannot kill is one it has orphaned"
    );
    assert!(!sock.exists(), "kill must unlink the socket");
}
