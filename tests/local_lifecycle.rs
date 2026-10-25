//! End-to-end tests of the local session lifecycle against real
//! `nvim --headless` processes (no TUI, no mocks).
//!
//! Each test has its own runtime directory, so they run in parallel and never
//! touch the real `/tmp/nvmux-<uid>`. They skip without `nvim` on `$PATH`
//! unless `$NVMUX_TEST_REQUIRE` names `nvim` (as in CI).

#[macro_use]
mod common;

use std::os::unix::fs::DirBuilderExt;
use std::time::{Duration, Instant};

use common::{command_line, Scratch};
use nvmux::session::{Liveness, Session};
use nvmux::transport::Transport;

/// Make [`nvmux::pty::spawn`] start `nvim --remote-ui`. The default client is
/// the running executable with `--client`, which here is this test binary.
/// `tests/client.rs` covers nvmux's own client.
fn neovims_client() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let mut settings = nvmux::config::Settings::default();
        settings.client.ui = nvmux::config::Ui::Nvim;
        nvmux::config::init(settings);
    });
    assert_eq!(
        nvmux::config::get().client.ui,
        nvmux::config::Ui::Nvim,
        "the settings were read before they were set"
    );
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

    let session = t
        .create_session("dotfiles", &common::launch(), common::anywhere())
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

/// When create returns, the session already answers and has `:Detach` and its
/// metadata.
#[test]
fn a_created_session_answers_and_has_its_alias_and_its_metadata() {
    require_nvim!();
    let scratch = Scratch::new("ready");
    let t = scratch.transport();

    let session = t
        .create_session("ready", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let mut client = nvmux::rpc::Client::connect(&sock, Duration::from_secs(2)).expect("connect");
    assert_eq!(
        client.eval("exists(':Detach')").expect("eval"),
        rmpv::Value::from(2),
        "no :Detach command"
    );
    let defined = client
        .eval("execute('command Detach')")
        .expect("eval")
        .as_str()
        .map(str::to_string)
        .unwrap_or_default();
    assert!(defined.contains("detach"), "{defined:?}");
    drop(client);

    let listed = t.list_sessions().expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].name, "ready",
        "no metadata: listed {:?}",
        listed[0]
    );
    t.kill_session(&session).expect("kill");
}

/// The script shell (and so `spawn.sh` and the editor) gets the launch umask,
/// not nvmux's 0077. Checked on `sh` directly so it runs without nvim.
#[test]
fn the_script_shell_runs_under_the_launch_umask() {
    common::establish_launch_umask();

    let out = nvmux::proc::run_local("umask", &[]).expect("run a script");
    assert!(out.ok(), "the script failed: {}", out.stderr);
    assert_eq!(
        common::printed_umask(&out.stdout),
        common::LAUNCH_UMASK,
        "the script shell was handed 0077 rather than the launch mask"
    );
}

/// A session writes the user's files, so it gets the launch umask.
///
/// Asked via `system('umask')`, i.e. a child of the editor, as `:terminal`,
/// `:!make` and language servers are. Covers `restrict_umask`,
/// `proc::sh_command` and `spawn.sh`; the mode checks verify the socket is
/// still private without the restrictive mask.
#[test]
fn a_session_runs_under_the_umask_nvmux_was_launched_with() {
    require_nvim!();
    common::establish_launch_umask();
    let scratch = Scratch::new("umask");
    let t = scratch.transport();

    let session = t
        .create_session("umask", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");

    let reported = {
        let mut client =
            nvmux::rpc::Client::connect(&sock, Duration::from_secs(2)).expect("connect");
        let value = client.eval("system('umask')").expect("ask for the umask");
        value
            .as_str()
            .expect("umask prints a string")
            .trim()
            .to_string()
    };

    // Created 0775 under that mask, but `spawn.sh` chmods it and it lives in
    // a 0700 directory.
    use std::os::unix::fs::PermissionsExt;
    let sock_mode = std::fs::metadata(&sock).expect("stat").permissions().mode() & 0o777;
    let dir_mode = std::fs::metadata(&scratch.0)
        .expect("stat")
        .permissions()
        .mode()
        & 0o777;
    let log_mode = std::fs::metadata(scratch.0.join(format!("{}.log", session.id)))
        .expect("stat")
        .permissions()
        .mode()
        & 0o777;

    t.kill_session(&session).expect("kill");

    assert_eq!(
        common::printed_umask(&reported),
        common::LAUNCH_UMASK,
        "the session was clamped rather than handed the launch mask"
    );
    assert_eq!(
        sock_mode & 0o077,
        0,
        "socket is mode {sock_mode:04o} under a permissive umask"
    );
    assert_eq!(
        dir_mode, 0o700,
        "runtime directory is mode {dir_mode:04o} under a permissive umask"
    );
    assert_eq!(
        log_mode, 0o600,
        "log is mode {log_mode:04o} under a permissive umask"
    );
}

#[test]
fn the_pid_recorded_is_the_process_serving_the_socket() {
    require_nvim!();
    let scratch = Scratch::new("pid");
    let t = scratch.transport();
    let session = t
        .create_session("pidcheck", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");

    // A wrong pid would mean SIGKILLing an unrelated process.
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
        .create_session("before", &common::launch(), common::anywhere())
        .expect("create");
    let sock_before = t.local_socket_for(&session).expect("socket");

    t.rename_session(&session, "after").expect("rename");

    let listed = t.list_sessions().expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "after");
    assert_eq!(listed[0].id, session.id, "rename must not change identity");

    let sock_after = t.local_socket_for(&listed[0]).expect("socket");
    assert_eq!(sock_before, sock_after, "the socket path must not move");
    assert!(sock_after.exists(), "the socket must still be live");
    assert_eq!(listed[0].state.liveness, Liveness::Alive);

    // Neither rank nor row may change, or `<prefix> <n>` would shift.
    assert_eq!(
        listed[0].num, session.num,
        "rename must not change the rank"
    );
    assert_eq!(listed[0].state.num, session.state.num);
}

/// Row numbers are recomputed on every listing, against real metadata.
#[test]
fn killing_a_session_renumbers_the_ones_below_it() {
    require_nvim!();
    let scratch = Scratch::new("numbers");
    let t = scratch.transport();

    let first = t
        .create_session("first", &common::launch(), common::anywhere())
        .expect("create");
    let second = t
        .create_session("second", &common::launch(), common::anywhere())
        .expect("create");
    let third = t
        .create_session("third", &common::launch(), common::anywhere())
        .expect("create");
    assert_eq!(
        (first.state.num, second.state.num, third.state.num),
        (1, 2, 3),
        "numbering starts at 1 and counts up"
    );

    t.kill_session(&second).expect("kill");

    // "third" moves up without its metadata being rewritten.
    let listed = t.list_sessions().expect("list");
    let numbered: Vec<(String, u32)> = listed
        .iter()
        .map(|s| (s.name.clone(), s.state.num))
        .collect();
    assert_eq!(
        numbered,
        vec![("first".to_string(), 1), ("third".to_string(), 2)],
        "the session below the killed one is renumbered"
    );
    assert_eq!(
        listed.iter().map(|s| s.num).collect::<Vec<_>>(),
        vec![first.num, third.num],
        "the ranks on disk are untouched, hole and all"
    );

    // A create appends rather than reusing the freed rank.
    let fourth = t
        .create_session("fourth", &common::launch(), common::anywhere())
        .expect("create");
    assert_eq!(fourth.state.num, 3, "it is the third row");
    assert!(
        fourth.num > third.num,
        "and ranks past everything on the host"
    );

    let listed = t.list_sessions().expect("list");
    let numbered: Vec<(String, u32)> = listed
        .iter()
        .map(|s| (s.name.clone(), s.state.num))
        .collect();
    assert_eq!(
        numbered,
        vec![
            ("first".to_string(), 1),
            ("third".to_string(), 2),
            ("fourth".to_string(), 3),
        ],
        "the list is dense and in creation order"
    );
}

/// A `renumber` must survive the next listing, with real metadata.
#[test]
fn a_reorder_is_still_there_after_a_relisting() {
    require_nvim!();
    let scratch = Scratch::new("reorder");
    let t = scratch.transport();

    for name in ["first", "second", "third"] {
        t.create_session(name, &common::launch(), common::anywhere())
            .expect("create");
    }
    let listed = t.list_sessions().expect("list");
    let names: Vec<&str> = listed.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["first", "second", "third"]);

    // What the picker sends after moving "third" to the top: every visible
    // session with its new row number.
    let numbers: Vec<u32> = listed.iter().map(|s| s.state.num).collect();
    let arranged = ["third", "first", "second"];
    let batch: Vec<Session> = arranged
        .iter()
        .zip(&numbers)
        .map(|(name, num)| {
            let mut s = listed
                .iter()
                .find(|s| s.name == *name)
                .expect("a session by that name")
                .clone();
            s.num = *num;
            s
        })
        .collect();
    t.renumber(&batch).expect("renumber");

    let listed = t.list_sessions().expect("list");
    let names: Vec<&str> = listed.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, arranged, "the new order did not survive the listing");
    assert_eq!(
        listed.iter().map(|s| s.state.num).collect::<Vec<_>>(),
        numbers,
        "the numbers themselves should not have changed, only their owners"
    );

    // And on disk.
    for s in &listed {
        let v = common::read_meta(&scratch.0.join(format!("{}.json", s.id)));
        assert_eq!(
            v.get("num").and_then(serde_json::Value::as_u64),
            Some(u64::from(s.state.num)),
            "{} was not written: {v}",
            s.name
        );
    }
}

/// An orphan (live socket, no `<id>.json`) is listed under a placeholder name;
/// a reorder must not write metadata for it.
#[test]
fn a_reorder_never_writes_metadata_for_a_session_that_has_none() {
    require_nvim!();
    let scratch = Scratch::new("reorder-orphan");
    let t = scratch.transport();

    let session = t
        .create_session("only", &common::launch(), common::anywhere())
        .expect("create");
    let json = scratch.0.join(format!("{}.json", session.id));
    std::fs::remove_file(&json).expect("make it an orphan");

    let mut orphaned = session.clone();
    orphaned.num = 7;
    let renumbered = t.renumber(&[orphaned]);

    // Killed before any assert can fail, since `Scratch` cleans up via
    // `<id>.json`. A signal, not `kill_session`, which would delete a wrongly
    // written `<id>.json`.
    common::sigkill_and_wait(session.pid);

    renumbered.expect("a missing file is not an error");
    assert!(
        !json.exists(),
        "metadata was conjured for a session that had none"
    );
}

/// `<id>.json` is all a second nvmux on another machine sees.
#[test]
fn a_number_survives_the_metadata_round_trip() {
    require_nvim!();
    let scratch = Scratch::new("num-roundtrip");
    let t = scratch.transport();

    let session = t
        .create_session("only", &common::launch(), common::anywhere())
        .expect("create");
    let v = common::read_meta(&scratch.0.join(format!("{}.json", session.id)));
    assert_eq!(
        v.get("num").and_then(serde_json::Value::as_u64),
        Some(u64::from(session.num)),
        "the number must be on disk, not just in memory: {v}"
    );

    assert_eq!(t.list_sessions().expect("list")[0].num, session.num);
}

/// Metadata from before numbering has no `num` key.
#[test]
fn metadata_without_a_number_still_lists_and_gets_one() {
    require_nvim!();
    let scratch = Scratch::new("num-legacy");
    let t = scratch.transport();

    let session = t
        .create_session("legacy", &common::launch(), common::anywhere())
        .expect("create");
    let path = scratch.0.join(format!("{}.json", session.id));

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
        .create_session("taken", &common::launch(), common::anywhere())
        .expect("create");
    assert!(
        t.create_session("taken", &common::launch(), common::anywhere())
            .is_err(),
        "duplicate name must be refused"
    );
    assert!(
        t.create_session("TAKEN", &common::launch(), common::anywhere())
            .is_err(),
        "duplicate check should not be case-sensitive"
    );

    let second = t
        .create_session("other", &common::launch(), common::anywhere())
        .expect("create second");
    assert!(
        t.rename_session(&second, "taken").is_err(),
        "renaming onto an existing name must be refused"
    );
    // Renaming to its own name is not a conflict.
    t.rename_session(&first, "taken")
        .expect("self-rename should work");
}

/// Kill is unconditional; `:q` in the session is the safe way out.
#[test]
fn unsaved_buffers_do_not_block_a_kill() {
    require_nvim!();
    let scratch = Scratch::new("unsaved");
    let t = scratch.transport();

    let session = t
        .create_session("unsaved", &common::launch(), common::anywhere())
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
        .create_session("doomed", &common::launch(), common::anywhere())
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
    // 0700, or the runtime-dir security check refuses it.
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
        .create_session("building", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");

    // Block the main loop like `:!make`; deferred RPC calls stop answering.
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

/// Busy sessions use the whole probe budget, so they must be probed in
/// parallel: eight take about one budget, not eight.
#[test]
fn busy_sessions_are_probed_together() {
    require_nvim!();
    const BUSY: u32 = 8;
    let budget = nvmux::rpc::QUICK_PROBE_TIMEOUT;
    // One budget for the probes together, plus room for the rest of the
    // listing on a loaded runner.
    let bound = budget + Duration::from_secs(1);
    assert!(
        bound < budget * BUSY,
        "{bound:?} is not under {BUSY} budgets: serial probing could pass"
    );
    let scratch = Scratch::new("busymany");
    let t = scratch.transport();

    let socks: Vec<_> = (0..BUSY)
        .map(|i| {
            let session = t
                .create_session(&format!("busy{i}"), &common::launch(), common::anywhere())
                .expect("create");
            t.local_socket_for(&session).expect("socket")
        })
        .collect();
    // Outlasts even a serial listing, so every probe uses its whole budget.
    for sock in &socks {
        common::block_editor(sock, "call system('sleep 10')", Duration::ZERO);
    }
    std::thread::sleep(Duration::from_millis(500));

    let started = Instant::now();
    let listed = t.list_sessions().expect("list");
    let took = started.elapsed();

    assert_eq!(
        listed.len(),
        socks.len(),
        "every busy session must still be listed"
    );
    assert!(
        listed.iter().all(|s| s.state.liveness == Liveness::Busy),
        "blocked-but-reachable must read as Busy: {listed:?}"
    );
    assert!(
        socks.iter().all(|s| s.exists()),
        "a busy session must never be reaped"
    );
    assert!(
        took < bound,
        "{BUSY} busy sessions held the listing {took:?}: a budget each, one after another"
    );
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
            .create_session(name, &common::launch(), common::anywhere())
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

/// Listings after the first reuse the first one's shell; a fresh and a kept
/// shell must give the same listing.
#[test]
fn ten_listings_on_one_transport_agree() {
    require_nvim!();
    let scratch = Scratch::new("tenfold");
    let t = scratch.transport();
    let a = t
        .create_session("alpha", &common::launch(), common::anywhere())
        .expect("create alpha");
    let b = t
        .create_session("beta", &common::launch(), common::anywhere())
        .expect("create beta");

    // Busy vs idle is excluded: a 250 ms probe on a loaded machine can say
    // either. Only "not dead" is checked.
    let rows = |sessions: &[Session]| -> Vec<(String, String, u32, u32)> {
        assert!(
            sessions.iter().all(|s| s.state.liveness != Liveness::Dead),
            "a live session listed as dead: {sessions:?}"
        );
        sessions
            .iter()
            .map(|s| (s.id.clone(), s.name.clone(), s.pid, s.state.num))
            .collect()
    };
    let first = rows(&t.list_sessions().expect("list"));
    assert_eq!(first.len(), 2);
    for i in 1..10 {
        let again = rows(&t.list_sessions().expect("list"));
        assert_eq!(again, first, "listing {i} differs from the first");
    }

    // A failing script in that shell must not affect the next listing.
    let hopeless = nvmux::launch::Launch::parse("nvmux-no-such-editor --listen {sock}")
        .expect("a well-formed command naming a program that is not there");
    assert!(t
        .create_session("hopeless", &hopeless, common::anywhere())
        .is_err());
    assert_eq!(rows(&t.list_sessions().expect("list")), first);

    t.kill_session(&a).expect("kill alpha");
    t.kill_session(&b).expect("kill beta");
    assert!(t.list_sessions().expect("list").is_empty());
}

/// Regression: a 250 ms `nvim_get_api_info` timeout was mapped to `Dead` and
/// the live socket unlinked. `api_info` is answered during `system()` but not
/// during CPU-bound Lua (measured 3.7 s), hence a busy loop here.
#[test]
fn a_session_busy_in_cpu_bound_lua_is_never_reaped() {
    require_nvim!();
    let scratch = Scratch::new("cpubusy");
    let t = scratch.transport();

    let session = t
        .create_session("compiling", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let pid = session.pid;

    // Longer than any probe budget.
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

/// Regression: on Linux a blocking connect to a full accept queue waits until
/// the server accepts, with no timeout. A stalled session's queue fills with
/// connections nvmux dropped unaccepted, and a listing then hung in its
/// probe's connect until the session ran again. macOS refuses that connect
/// instead.
#[cfg(target_os = "linux")]
#[test]
fn a_stalled_session_with_a_full_accept_queue_is_listed_at_once_as_busy() {
    require_nvim!();
    // A script run and a probe whose connect fails at once take about 15 ms.
    const DEADLINE: Duration = Duration::from_secs(5);
    let scratch = Scratch::new("stalled");
    let t = scratch.transport();
    let session = t
        .create_session("stalled", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let files = [
        sock.clone(),
        sock.with_extension("json"),
        sock.with_extension("log"),
    ];

    let stopped = Stopped::new(session.pid);
    let (queued, waiting) = fill_accept_queue(&sock);

    let (tx, rx) = std::sync::mpsc::channel();
    let dir = scratch.0.clone();
    let lister = std::thread::spawn(move || {
        let listed = nvmux::transport::local::LocalTransport::with_dir(dir)
            .and_then(|t| t.list_sessions())
            .map_err(|e| e.to_string());
        let _ = tx.send(listed);
    });
    let outcome = rx.recv_timeout(DEADLINE);
    let held = is_stopped(session.pid);
    let reaped: Vec<_> = files.iter().filter(|p| !p.exists()).collect();

    // Resumed, Neovim accepts what waits, which ends any connect still waiting.
    drop(stopped);
    let ended = common::wait_until(Duration::from_secs(10), || {
        lister.is_finished() && waiting.is_finished()
    });
    drop(queued);
    assert!(ended, "a connect still waited after the session resumed");
    lister.join().expect("the listing thread panicked");
    waiting.join().expect("the connecting thread panicked");
    t.kill_session(&session).expect("kill");

    assert!(
        held,
        "precondition: the session ran before the listing ended"
    );
    let listed = outcome
        .unwrap_or_else(|_| {
            panic!("the listing still waited after {DEADLINE:?}, on a connect to the full queue")
        })
        .expect("list");
    let s = listed
        .iter()
        .find(|s| s.id == session.id)
        .unwrap_or_else(|| panic!("the stalled session was not listed: {listed:?}"));
    assert_eq!(s.state.liveness, Liveness::Busy);
    assert!(
        reaped.is_empty(),
        "a stalled session's files were reaped: {reaped:?}"
    );
}

/// Neovim's listen backlog (`MAX_CONNECTIONS`). Linux queues one more before
/// a connect waits.
#[cfg(target_os = "linux")]
const NEOVIM_BACKLOG: usize = 32;

/// A process held by SIGSTOP until dropped.
#[cfg(target_os = "linux")]
struct Stopped(u32);

#[cfg(target_os = "linux")]
impl Stopped {
    fn new(pid: u32) -> Self {
        use nix::sys::signal::{kill, Signal};
        kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGSTOP).expect("SIGSTOP");
        let stopped = Self(pid);
        // Until it stops, Neovim still accepts.
        assert!(
            common::wait_until(Duration::from_secs(3), || is_stopped(pid)),
            "pid {pid} never stopped"
        );
        stopped
    }
}

#[cfg(target_os = "linux")]
impl Drop for Stopped {
    fn drop(&mut self) {
        use nix::sys::signal::{kill, Signal};
        let _ = kill(nix::unistd::Pid::from_raw(self.0 as i32), Signal::SIGCONT);
    }
}

#[cfg(target_os = "linux")]
fn is_stopped(pid: u32) -> bool {
    // The state follows the command name, which may itself hold a `)`.
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('T'))
    })
}

/// Connect to a stopped server until a connect waits on its full queue.
///
/// Returns the queued connections and the thread still connecting, which
/// ends once the server accepts or goes.
#[cfg(target_os = "linux")]
fn fill_accept_queue(
    sock: &std::path::Path,
) -> (
    Vec<std::os::unix::net::UnixStream>,
    std::thread::JoinHandle<()>,
) {
    let mut queued = Vec::new();
    // Far past the backlog, so a server that still accepts fails the test.
    while queued.len() < 4 * NEOVIM_BACKLOG {
        let (tx, rx) = std::sync::mpsc::channel();
        let to = sock.to_path_buf();
        let connecting = std::thread::spawn(move || {
            let _ = tx.send(std::os::unix::net::UnixStream::connect(&to));
        });
        // A connect the queue has room for returns at once.
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(stream) => queued.push(stream.expect("connect to the stopped session")),
            Err(_) => {
                assert!(
                    queued.len() > NEOVIM_BACKLOG,
                    "a connect waited after only {} were queued",
                    queued.len()
                );
                return (queued, connecting);
            }
        }
    }
    panic!("the stopped session queued {} connects", queued.len());
}

/// Regression: a timed-out read leaves the rest of its frame in the socket
/// (rmpv decodes incrementally), and the next call would silently return
/// that leftover as its answer.
#[test]
fn a_timed_out_call_poisons_the_connection() {
    require_nvim!();
    let scratch = Scratch::new("poison");
    let t = scratch.transport();
    let session = t
        .create_session("poisoned", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");

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

/// The recorded pid may have been reused, so it is trusted only while it owns
/// the session's socket; otherwise the session is found by socket. Guards
/// against both signalling an innocent process and failing to kill.
#[test]
fn a_recycled_pid_neither_misfires_nor_blocks_the_kill() {
    require_nvim!();
    let scratch = Scratch::new("recycled");
    let t = scratch.transport();
    let session = t
        .create_session("stubborn", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let json = sock.with_extension("json");

    // A live pid that is not this session, as a recycled one would be.
    let mut decoy = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn decoy");

    // The embedded script is the copy that ships.
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

#[test]
fn killing_an_already_dead_session_cleans_up_its_files() {
    require_nvim!();
    let scratch = Scratch::new("alreadydead");
    let t = scratch.transport();
    let session = t
        .create_session("ghost", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let json = sock.with_extension("json");

    common::sigkill_and_wait(session.pid);

    t.kill_session(&session)
        .expect("killing a dead session should succeed");
    assert!(!sock.exists(), "the stale socket should be gone");
    assert!(!json.exists(), "the orphaned metadata should be gone");
}

/// Regression: without `ps` (minimal containers), `kill.sh` reported `absent`,
/// deleted the socket and left nvim running unreachable. Tests the `/proc`
/// fallback, so Linux only.
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
        .create_session("noprocps", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");

    // A PATH with everything the script needs except `ps`.
    let bin = scratch.0.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    for tool in [
        "sh", "tr", "grep", "awk", "rm", "sleep", "kill", "id", "printf", "cut", "dirname", "ls",
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

#[cfg(target_os = "linux")]
fn which(tool: &str) -> Result<std::path::PathBuf, ()> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join(tool))
        .find(|c| c.is_file())
        .ok_or(())
}

/// The graceful `qa!` goes over the socket, so a stale pid must not turn a
/// clean shutdown into a "could not kill" error.
#[test]
fn a_stale_pid_does_not_break_an_otherwise_normal_kill() {
    require_nvim!();
    let scratch = Scratch::new("stalepid");
    let t = scratch.transport();
    let session = t
        .create_session("staleish", &common::launch(), common::anywhere())
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

/// A session that exits on its own unlinks its socket but leaves metadata and
/// log, which a listing driven by `*.sock` would never find.
#[test]
fn metadata_left_by_a_self_terminating_session_is_swept_up() {
    require_nvim!();
    let scratch = Scratch::new("sweep");
    let t = scratch.transport();

    let session = t
        .create_session("selfquit", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let json = sock.with_extension("json");
    let log = sock.with_extension("log");

    let mut client = nvmux::rpc::Client::connect(&sock, Duration::from_secs(2)).expect("connect");
    let _ = client.command("qa!");

    assert!(
        common::wait_until(Duration::from_secs(5), || !sock.exists()),
        "precondition: a clean exit unlinks the socket"
    );
    assert!(json.exists(), "precondition: it leaves the metadata behind");

    assert!(t.list_sessions().expect("list").is_empty());
    assert!(!json.exists(), "orphaned metadata was never cleaned up");
    assert!(!log.exists(), "the orphaned log was never cleaned up");
}

/// Regression: a session at a hit-enter prompt cannot answer a deferred call,
/// and the picker waited the full probe budget for each one. Common with
/// `cmdheight = 0` (AstroNvim) or any scrolling message.
#[test]
fn a_session_at_a_hit_enter_prompt_is_listed_at_once_as_busy() {
    require_nvim!();
    let scratch = Scratch::new("hitenter");
    let t = scratch.transport();
    let session = t
        .create_session("prompted", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let _ui = common::HitEnter::open(&sock);

    let started = Instant::now();
    let listed = t.list_sessions().expect("list");
    let took = started.elapsed();

    // Only catches a full-budget wait; a 250 ms quick-budget wait is checked
    // by probe_is_busy_at_a_hit_enter_prompt_and_alive_after_it.
    assert!(
        took < nvmux::rpc::PROBE_TIMEOUT - Duration::from_millis(500),
        "listing took {took:?}: it waited out a probe's whole budget on the session at its prompt"
    );
    let s = listed
        .iter()
        .find(|s| s.id == session.id)
        .expect("a session at a prompt must still be listed");
    assert_eq!(s.state.liveness, Liveness::Busy);
    assert!(sock.exists(), "a session at a prompt must never be reaped");
}

#[test]
fn probe_is_busy_at_a_hit_enter_prompt_and_alive_after_it() {
    require_nvim!();
    let scratch = Scratch::new("hitenterprobe");
    let t = scratch.transport();
    let session = t
        .create_session("prompted", &common::launch(), common::anywhere())
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

/// Regression: `nvim_list_uis` is deferred, so attaching to a session at a
/// hit-enter prompt timed out. The attach now sends the key that ends it.
#[test]
fn a_fresh_attach_to_a_session_at_a_hit_enter_prompt_ends_it_and_attaches() {
    require_nvim!();
    neovims_client();
    let scratch = Scratch::new("hitenterattach");
    let t = scratch.transport();
    let session = t
        .create_session("prompted", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    let _ui = common::HitEnter::open(&sock);

    let started = Instant::now();
    let attachment = nvmux::pty::spawn(&session.id, &sock, "1  prompted").expect("attach");
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(2),
        "the attach took {took:?}: it waited on the prompt"
    );

    assert!(
        !common::at_hit_enter(&sock),
        "the prompt should have been ended by the attach"
    );
    // Polled: `spawn` returns at fork, before the UI attaches. Two UIs are the
    // helper's and the new one; a deferred call answering proves the prompt
    // is over.
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

/// Regression: a session waiting for a key held the attach probe forever
/// (`nvim_list_uis` is deferred and the probe had no budget).
///
/// nvmux must not press a key itself: mode `n` with `blocking` is the same
/// over RPC after a crashed `vim.schedule()` callback and a half-typed `g`.
/// Instead it attaches, and the user's key (`nvim_input` is a fast call) ends
/// the wait.
#[test]
fn a_session_waiting_for_a_key_is_attached_to_without_being_typed_at() {
    require_nvim!();
    neovims_client();
    let scratch = Scratch::new("blockedattach");
    let t = scratch.transport();
    let session = t
        .create_session("wedged", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    common::block_on_a_key(&sock);

    let started = Instant::now();
    let attachment = nvmux::pty::spawn(&session.id, &sock, "1  wedged").expect("attach");
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(2),
        "the attach took {took:?}: it waited on a session that cannot answer"
    );

    // Still waiting for its second key: nvmux typed nothing.
    let after = common::mode(&sock).expect("the mode is a fast call");
    assert!(
        after.blocking && after.mode == "n",
        "nvmux typed into a state it cannot identify: {after:?}"
    );

    attachment.terminate();
}

/// Once the user's key arrives through the attached client, deferred calls
/// (including the queued UI attach) proceed.
#[test]
fn a_key_ends_the_wait_and_releases_what_was_queued_behind_it() {
    require_nvim!();
    neovims_client();
    let scratch = Scratch::new("blockedkey");
    let t = scratch.transport();
    let session = t
        .create_session("wedged", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    common::block_on_a_key(&sock);

    assert_eq!(nvmux::rpc::probe(&sock), Liveness::Busy);

    let attachment = nvmux::pty::spawn(&session.id, &sock, "1  wedged").expect("attach");
    // The client's UI attach is deferred too, queued behind the same key.
    assert!(
        common::mode(&sock).is_some_and(|m| m.blocking),
        "the attach should not have ended the wait by itself"
    );

    // The user presses a key; the client sends it with `nvim_input`.
    nvmux::rpc::Client::connect(&sock, Duration::from_secs(2))
        .expect("connect")
        .input("<Esc>")
        .expect("press a key");

    assert!(
        common::wait_until(Duration::from_secs(10), || nvmux::rpc::probe(&sock)
            == Liveness::Alive),
        "the session should serve deferred calls again once a key has arrived"
    );
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
                .and_then(|mut c| c.list_uis())
                .is_ok_and(|n| n == 1)
        }),
        "the client's queued UI attach never landed"
    );

    attachment.terminate();
}

#[test]
fn a_chosen_command_is_what_runs_and_the_session_behaves_normally() {
    require_nvim!();
    let scratch = Scratch::new("chosen-command");
    let t = scratch.transport();

    let launch = common::launch_with("--clean");
    let session = t
        .create_session("bespoke", &launch, common::anywhere())
        .expect("create");

    let listed = t.list_sessions().expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].state.liveness,
        Liveness::Alive,
        "a session started with a chosen command is a session like any other"
    );

    let args = command_line(session.pid);
    assert!(
        args.contains("--clean"),
        "the chosen argument never reached nvim: {args:?}"
    );
    assert!(
        args.contains("--headless"),
        "nor did the rest of the line: {args:?}"
    );

    let stored = scratch.metadata(&session.id);
    assert_eq!(stored.command, launch.line());

    t.kill_session(&session).expect("kill");
    assert!(t.list_sessions().expect("list").is_empty());
}

/// Must fail fast with the command named, not after the readiness budget.
#[test]
fn a_command_that_names_nothing_fails_at_once_and_says_so() {
    require_nvim!();
    let scratch = Scratch::new("no-such-command");
    let t = scratch.transport();

    let launch = nvmux::launch::Launch::parse("nvmux-no-such-editor --listen {sock}")
        .expect("a well-formed command naming a program that is not there");

    let started = std::time::Instant::now();
    let err = t
        .create_session("hopeless", &launch, common::anywhere())
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

/// The scripts' fast path reads `--listen` to the end of the line, so with
/// arguments after the socket, listing and killing must fall back to
/// searching the process table.
#[test]
fn a_session_whose_socket_is_not_the_last_argument_is_still_found_and_killed() {
    require_nvim!();
    let scratch = Scratch::new("socket-not-last");
    let t = scratch.transport();

    let launch = nvmux::launch::Launch::parse("nvim --headless --listen {sock} --clean")
        .expect("a valid command");
    let session = t
        .create_session("trailing", &launch, common::anywhere())
        .expect("create");

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

/// Asked of Neovim itself: `/proc` does not exist on macOS, and the process
/// environment would not reflect a `cd`.
#[test]
fn a_session_starts_in_the_directory_it_was_given() {
    require_nvim!();
    let scratch = Scratch::new("cwd");
    let t = scratch.transport();

    // Neither this process's cwd nor the home directory.
    let where_ = scratch.0.join("workspace");
    std::fs::create_dir_all(&where_).expect("make the working directory");
    let where_ = where_.canonicalize().expect("canonicalise");

    let session = t
        .create_session("elsewhere", &common::launch(), &where_.to_string_lossy())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");

    let answer = scratch.0.join("cwd.txt");
    let mut client = nvmux::rpc::Client::connect(&sock, Duration::from_secs(2)).expect("connect");
    client
        .command(&format!(
            "call writefile([getcwd()], '{}')",
            answer.display()
        ))
        .expect("ask the editor where it is");
    drop(client);

    let got = std::fs::read_to_string(&answer).expect("read the answer");
    assert_eq!(
        std::path::Path::new(got.trim())
            .canonicalize()
            .expect("canonicalise the answer"),
        where_,
        "the session is not where it was told to start"
    );

    t.kill_session(&session).expect("kill");
}

/// `spawn.sh` refuses it up front, naming the directory, instead of a
/// readiness timeout five seconds later.
#[test]
fn a_directory_that_is_not_there_is_refused_before_anything_is_spawned() {
    require_nvim!();
    let scratch = Scratch::new("cwd-missing");
    let t = scratch.transport();

    let missing = scratch.0.join("no-such-directory");
    let started = Instant::now();
    let err = t
        .create_session("hopeless", &common::launch(), &missing.to_string_lossy())
        .expect_err("nothing could have started there");

    assert!(
        err.to_string().contains("no-such-directory")
            && err.to_string().contains("not a directory"),
        "the refusal must name the directory: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "it must not wait out the readiness timeout for a session it never started"
    );
    assert!(
        t.list_sessions().expect("list").is_empty(),
        "a refused create must leave nothing behind"
    );
}

#[test]
fn the_directory_is_recorded_in_the_session_metadata() {
    require_nvim!();
    let scratch = Scratch::new("cwd-meta");
    let t = scratch.transport();

    let where_ = scratch.0.join("recorded");
    std::fs::create_dir_all(&where_).expect("make the working directory");

    let session = t
        .create_session("noted", &common::launch(), &where_.to_string_lossy())
        .expect("create");
    assert_eq!(session.directory, where_.to_string_lossy());
    assert_eq!(
        scratch.metadata(&session.id).directory,
        where_.to_string_lossy(),
        "what the transport returned must be what reached disk"
    );

    t.kill_session(&session).expect("kill");
}
