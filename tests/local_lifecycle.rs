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
use nvmux::session::{Liveness, Session};
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

/// The shell nvmux runs its scripts in — and so `scripts/spawn.sh`, and so the
/// editor `spawn.sh` launches — runs under the mask nvmux was launched with,
/// not the 0077 nvmux clamps itself to.
///
/// Asked of `sh` directly, so this holds the line on machines with no nvim,
/// where the session test below can only skip.
#[test]
fn the_script_shell_runs_under_the_launch_umask() {
    common::establish_launch_umask();

    let out = nvmux::proc::run_local("umask", &[]).expect("run a script");
    assert!(out.ok(), "the script failed: {}", out.stderr);
    assert_eq!(
        out.stdout.trim(),
        common::launch_umask_printed(),
        "the script shell was handed 0077 rather than the launch mask"
    );
}

/// A session writes the user's files, so it gets the user's umask.
///
/// Asked of the session rather than of the spawn: `system('umask')` runs a shell
/// as a child of the editor, which is the inheritance a `:terminal`, a `:!make`
/// and a language server get — and the one a user notices when a new file comes
/// out 0600.
///
/// The whole production wiring is under test here: `restrict_umask` clamping the
/// process and recording what it replaced, `proc::sh_command` handing that back
/// to the shell, and `spawn.sh` adding no mask of its own. The modes asserted
/// afterwards are the other half of the trade — what keeps the socket private
/// now that the mask does not.
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

    // Born 0775 under that mask, and private anyway: `spawn.sh` chmods it, and
    // it was never anywhere but inside a 0700 directory.
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
        reported,
        common::launch_umask_printed(),
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
        .create_session("before", &common::launch(), common::anywhere())
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

    // A rename edits the name and nothing else: neither the rank on disk nor
    // the row it sorts into, or `<prefix> <n>` would start naming a different
    // session every time one was renamed.
    assert_eq!(
        listed[0].num, session.num,
        "rename must not change the rank"
    );
    assert_eq!(listed[0].state.num, session.state.num);
}

/// The column is recalculated on every listing: kill the second of three and
/// the third becomes the second, against real metadata rather than a fixture.
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

    // The whole point: "third" moves up into the hole, and nothing had to be
    // written to its metadata for that to happen.
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

    // A create appends rather than reusing the freed rank, so the new session
    // comes up at the bottom of the list and nothing above it moves.
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

/// A reorder is only worth anything if it sticks. The picker arranges the
/// numbers, `renumber` writes them, and the next listing has to read the same
/// order back — against real sessions and real metadata, which is the part the
/// pure tests cannot cover.
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

    // What the picker sends after dragging "third" to the top: every visible
    // session, each paired with the number of the row it now sits in.
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

    // And it is on disk, not just in the listing this process built.
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

/// A session with no metadata is skipped rather than having some conjured for
/// it. An orphan — a live socket whose `<id>.json` was never written — is shown
/// under a name the picker made up, and a reorder passing over one must not make
/// that placeholder real.
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

    // Killed here, before anything below can fail: `Scratch` finds what to
    // kill through `<id>.json`, which this session no longer has. A signal
    // rather than `kill_session`, which deletes `<id>.json` and so would hide
    // one that `renumber` had wrongly written.
    common::sigkill_and_wait(session.pid);

    renumbered.expect("a missing file is not an error");
    assert!(
        !json.exists(),
        "metadata was conjured for a session that had none"
    );
}

/// The number has to survive the round trip through `<id>.json`, which is the
/// only thing a second nvmux on another machine ever sees.
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

/// Metadata written before numbering existed has no `num` key at all. It must
/// still list, and still be reachable by keystroke.
#[test]
fn metadata_without_a_number_still_lists_and_gets_one() {
    require_nvim!();
    let scratch = Scratch::new("num-legacy");
    let t = scratch.transport();

    let session = t
        .create_session("legacy", &common::launch(), common::anywhere())
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
        .create_session("building", &common::launch(), common::anywhere())
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

/// Every listing after the first runs in the shell the first one started.
/// What a fresh shell and a kept one must agree on is the whole of the
/// listing: names, ids, numbers, liveness — and the `list.sh` sweep in between
/// must find nothing to sweep, or the tenth answer would differ from the first.
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

    // Everything the picker draws and everything a switch resolves against.
    let rows = |sessions: &[Session]| -> Vec<(String, String, u32, u32, Liveness)> {
        sessions
            .iter()
            .map(|s| {
                (
                    s.id.clone(),
                    s.name.clone(),
                    s.pid,
                    s.state.num,
                    s.state.liveness,
                )
            })
            .collect()
    };
    let first = rows(&t.list_sessions().expect("list"));
    assert_eq!(first.len(), 2);
    for i in 1..10 {
        let again = rows(&t.list_sessions().expect("list"));
        assert_eq!(again, first, "listing {i} differs from the first");
    }

    // A script that fails in that shell — a spawn of a program that is not
    // there, which exits on its error path — changes nothing for the listing
    // after it.
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
        .create_session("compiling", &common::launch(), common::anywhere())
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
        .create_session("poisoned", &common::launch(), common::anywhere())
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
        .create_session("stubborn", &common::launch(), common::anywhere())
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
        .create_session("noprocps", &common::launch(), common::anywhere())
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
        .create_session("selfquit", &common::launch(), common::anywhere())
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
        .create_session("prompted", &common::launch(), common::anywhere())
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

/// **The attach that never came back.** A session waiting for a key nvmux may
/// not press must still be attached to — promptly, and without being typed at.
///
/// This is the shape of the reported bug. `nvim_list_uis` is deferred and the
/// attach probe's connection has no budget, so a session in this state held the
/// probe for ever: the spinner turned, nothing happened, and `Esc` was the only
/// way out. It looked like nvmux had lost the session.
///
/// It is also why the fix is not a cleverer key. Mode `n` with `blocking` set
/// is what a crashed `vim.schedule()` callback leaves behind *and* what a
/// half-typed `g` leaves behind, and over RPC those are one state. So the probe
/// says so and gets out of the way: the client it is holding back is how the
/// user presses the key that ends the wait, since `nvim_input` is a fast call
/// and lands whatever the editor is parked in.
#[test]
fn a_session_waiting_for_a_key_is_attached_to_without_being_typed_at() {
    require_nvim!();
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

    // The whole invariant, in one assertion: the session is *still* waiting for
    // its second key. Nothing nvmux did completed the command the user started.
    let after = common::mode(&sock).expect("the mode is a fast call");
    assert!(
        after.blocking && after.mode == "n",
        "nvmux typed into a state it cannot identify: {after:?}"
    );

    attachment.terminate();
}

/// The other half: once a key does arrive — from the user, on the client the
/// attach just handed them — the session serves deferred calls again and the
/// paint queued behind the wait is released.
///
/// Attaching is therefore the whole recovery, not a workaround for it.
#[test]
fn a_key_ends_the_wait_and_releases_what_was_queued_behind_it() {
    require_nvim!();
    let scratch = Scratch::new("blockedkey");
    let t = scratch.transport();
    let session = t
        .create_session("wedged", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");
    common::block_on_a_key(&sock);

    // Deferred calls are dead while it waits: this is what parked the probe.
    assert_eq!(nvmux::rpc::probe(&sock), Liveness::Busy);

    let attachment = nvmux::pty::spawn(&session.id, &sock, "1  wedged").expect("attach");
    // The client's UI attach is deferred too, so it is queued behind the same
    // key and the session is still blocked with it forked.
    assert!(
        common::mode(&sock).is_some_and(|m| m.blocking),
        "the attach should not have ended the wait by itself"
    );

    // The user presses a key. `nvim_input` is what the client sends one with.
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

/// The whole point of asking how nvim is launched: the command chosen is the
/// command that runs, and the session it starts is an ordinary session —
/// listed, reachable, and killable by the same socket-matching as any other.
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

/// The whole point of asking: the editor really is started in the directory it
/// was given, not in whatever directory nvmux happened to be run from.
///
/// Asked of the live Neovim rather than of `/proc`, which does not exist on
/// macOS, and rather than of the pid's environment, which would not notice a
/// `cd` at all.
#[test]
fn a_session_starts_in_the_directory_it_was_given() {
    require_nvim!();
    let scratch = Scratch::new("cwd");
    let t = scratch.transport();

    // A directory that is definitely not this process's, and definitely not the
    // home directory a shell would otherwise have dropped the session into.
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

/// A directory that is not there is refused by the spawn script before it
/// launches anything, so the error names the directory rather than arriving
/// five seconds later as "the session did not become ready".
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

/// `<id>.json` records where a session was started, beside what launched it —
/// so a listing can say, and a future nvmux has the answer without asking the
/// editor.
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
