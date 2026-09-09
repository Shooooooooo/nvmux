//! End-to-end tests for the SSH transport, against a real `sshd`.
//!
//! The host comes from `$NVMUX_TEST_SSH_HOST`, defaulting to `selftest`; where
//! no such host answers these **skip** rather than fail. Set one up with:
//!
//! ```text
//! Host selftest
//!   HostName 127.0.0.1
//!   User <you>
//!   IdentityFile ~/.ssh/id_ed25519
//! ```
//!
//! Pointing that alias at localhost is not a cheat: it exercises the real ssh
//! client, a real ControlMaster, real unix-socket forwarding and a real remote
//! login shell — everything except latency. `$NVMUX_TEST_REQUIRE=ssh` turns
//! the skip into a failure.

#[macro_use]
mod common;

use common::unique;
use nvmux::transport::remote::SshTransport;
use nvmux::transport::Transport;

fn host() -> String {
    std::env::var("NVMUX_TEST_SSH_HOST").unwrap_or_else(|_| "selftest".to_string())
}

/// Is the test host usable? Skips are silent on purpose.
fn reachable() -> bool {
    std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            &host(),
            "true",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

macro_rules! require_ssh {
    () => {
        require!(
            "ssh",
            reachable(),
            format!("{} is not reachable over ssh", host())
        );
    };
}

/// Removes every session this test file created, whatever happened.
///
/// The transport is built in `drop`, not held: one of these guards exists per
/// test, and connecting up front cost a second full ssh connect and probe per
/// test for a cleanup that usually has nothing to do.
struct Cleanup(Vec<String>);

impl Cleanup {
    fn of<S: AsRef<str>>(names: impl IntoIterator<Item = S>) -> Self {
        Self(names.into_iter().map(|n| n.as_ref().to_string()).collect())
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let Ok(t) = SshTransport::new(host()) else {
            return;
        };
        if let Ok(sessions) = t.list_sessions() {
            for s in sessions {
                if self.0.iter().any(|n| s.name.starts_with(n.as_str())) {
                    let _ = t.kill_session(&s);
                }
            }
        }
    }
}

#[test]
fn a_session_created_over_ssh_is_reachable_through_the_forward() {
    require_ssh!();
    let t = SshTransport::new(host()).expect("connect");
    let name = unique("reach");
    let _guard = Cleanup::of([&name]);

    let session = t.create_session(&name, &common::launch()).expect("create");
    assert!(
        session.pid > 1,
        "spawn should report a validated remote pid"
    );

    let sock = t.local_socket_for(&session).expect("forward");
    assert!(sock.exists(), "no local socket at {}", sock.display());
    assert!(
        sock.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains('-')),
        "the local end must be namespaced by host token: {}",
        sock.display()
    );

    let mut client =
        nvmux::rpc::Client::connect(&sock, nvmux::rpc::PROBE_TIMEOUT).expect("connect via forward");
    let info = client.api_info().expect("api_info through the forward");
    assert!(info.is_supported(), "remote nvim {info} is too old");

    // A deferred call too, which proves the session is actually serving rather
    // than merely that ssh accepted the connection — `ssh -O forward` to a
    // nonexistent remote socket also exits 0 and yields a working local socket.
    client
        .list_bufs()
        .expect("deferred call through the forward");
}

#[test]
fn a_remote_session_outlives_the_transport_that_made_it() {
    require_ssh!();
    let name = unique("outlive");
    let _guard = Cleanup::of([&name]);

    let id = {
        let t = SshTransport::new(host()).expect("connect");
        let s = t.create_session(&name, &common::launch()).expect("create");
        s.id
    }; // transport dropped, ssh commands finished

    // A completely fresh transport must find it, which is the whole point of
    // keeping the metadata on the session host.
    let t2 = SshTransport::new(host()).expect("reconnect");
    let found = common::find_by_id(&t2, &id).expect("the session should have survived");
    assert_eq!(found.name, name);
}

/// Detach and re-attach is the flow this tool exists for, and the one that
/// breaks without `StreamLocalBindUnlink=yes` on the master: `-O cancel` exits 0
/// but leaves the local socket file, and the next `-O forward` onto that path
/// fails with rc 255.
#[test]
fn a_forward_can_be_torn_down_and_rebuilt() {
    require_ssh!();
    let t = SshTransport::new(host()).expect("connect");
    let name = unique("reforward");
    let _guard = Cleanup::of([&name]);

    let session = t.create_session(&name, &common::launch()).expect("create");
    let first = t.local_socket_for(&session).expect("forward");
    assert!(first.exists());

    std::fs::remove_file(&first).ok();

    // A fresh transport has no memory of the forward, so this exercises the
    // rebuild rather than the cached path.
    let t2 = SshTransport::new(host()).expect("reconnect");
    let again = t2.local_socket_for(&session).expect("re-forward");
    assert_eq!(first, again, "the local path should be stable");
    assert!(again.exists(), "the forward was not rebuilt");

    let mut client =
        nvmux::rpc::Client::connect(&again, nvmux::rpc::PROBE_TIMEOUT).expect("connect");
    client.api_info().expect("usable after re-forward");
}

#[test]
fn renaming_a_remote_session_moves_no_socket() {
    require_ssh!();
    let t = SshTransport::new(host()).expect("connect");
    let name = unique("rename");
    let _guard = Cleanup::of([name.clone(), format!("{name}-after")]);

    let session = t.create_session(&name, &common::launch()).expect("create");
    let before = t.local_socket_for(&session).expect("forward");

    let after_name = format!("{name}-after");
    t.rename_session(&session, &after_name).expect("rename");

    let listed = common::find_by_id(&t, &session.id).expect("still listed");
    assert_eq!(listed.name, after_name);
    assert_eq!(
        t.local_socket_for(&listed).expect("forward"),
        before,
        "a rename must not move the socket, or every forward would have to be rebuilt"
    );
}

#[test]
fn killing_a_remote_session_removes_it_and_its_forward() {
    require_ssh!();
    let t = SshTransport::new(host()).expect("connect");
    let name = unique("kill");
    // Even the kill test needs a guard: if the kill under test fails, the
    // session would otherwise outlive the run on the remote host.
    let _guard = Cleanup::of([&name]);

    let session = t.create_session(&name, &common::launch()).expect("create");
    let sock = t.local_socket_for(&session).expect("forward");
    assert!(sock.exists());

    t.kill_session(&session).expect("kill");

    assert!(
        !t.list_sessions()
            .expect("list")
            .iter()
            .any(|s| s.id == session.id),
        "the session should be gone from the listing"
    );
    assert!(
        !sock.exists(),
        "the local end of the forward should have been cleaned up"
    );
}

#[test]
fn remote_names_with_shell_metacharacters_survive() {
    require_ssh!();
    let t = SshTransport::new(host()).expect("connect");
    // Two layers of shell stand between here and the remote file: ssh joins its
    // arguments and the remote login shell parses them again.
    let name = format!("{} $(id) 'q' \"d\"", unique("quote"));
    let _guard = Cleanup::of([unique("quote")]);

    let session = t.create_session(&name, &common::launch()).expect("create");
    let found = common::find_by_id(&t, &session.id).expect("listed");
    assert_eq!(
        found.name, name,
        "the name was mangled or evaluated in transit"
    );
}

#[test]
fn an_unreachable_host_fails_with_a_useful_message() {
    let err = SshTransport::new("nvmux-no-such-host.invalid".into())
        .expect_err("must fail")
        .to_string();
    assert!(
        err.contains("unreachable") || err.contains("ssh"),
        "unhelpful error: {err}"
    );
}

/// A live forward must survive a listing, and a dead one must not.
///
/// The sweep that removes orphaned forwards runs against the same runtime
/// directory the remote listing scans — and when the "remote" host is this
/// machine, which `nvmux localhost` makes an ordinary case, those are literally
/// the same directory. A sweep that only asked "is anything serving this
/// socket?" would find the local end of a live forward, see that no `--listen`
/// process owns it, and delete the forward out from under an attached session.
#[test]
fn a_listing_keeps_live_forwards_and_removes_orphaned_ones() {
    require_ssh!();
    let t = SshTransport::new(host()).expect("connect");
    let name = unique("sweep");
    let _guard = Cleanup::of([&name]);

    let session = t.create_session(&name, &common::launch()).expect("create");
    let live = t.local_socket_for(&session).expect("forward");
    assert!(live.exists());

    let orphan = live.with_file_name(format!("{}-zzzzzzzz.sock", nvmux::ids::host_token(&host())));
    std::fs::write(&orphan, b"").expect("plant orphan");

    t.list_sessions().expect("list");

    assert!(
        live.exists(),
        "a listing deleted the live forward at {}",
        live.display()
    );
    assert!(!orphan.exists(), "the orphaned forward was not cleaned up");

    let mut client =
        nvmux::rpc::Client::connect(&live, nvmux::rpc::PROBE_TIMEOUT).expect("still connectable");
    client.api_info().expect("still usable after a listing");
}

/// The chosen command runs on the host that owns the session, not on this one,
/// and every word of it survives ssh's argument join and the remote login
/// shell's second parse.
#[test]
fn a_chosen_command_runs_on_the_remote_host_with_its_arguments_intact() {
    require_ssh!();
    let t = SshTransport::new(host()).expect("connect");
    let name = unique("remote-command");
    let _guard = Cleanup::of([&name]);

    let launch = common::launch_with("--clean");
    let session = t.create_session(&name, &launch).expect("create");

    // Asked of the remote host, since that is where the process is.
    let remote = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            &host(),
            "ps",
            "-ww",
            "-o",
            "args=",
            "-p",
        ])
        .arg(session.pid.to_string())
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default();
    assert!(
        remote.contains("--clean"),
        "the chosen argument never reached the remote nvim: {remote:?}"
    );

    // Reachable through the forward like any other session, and recorded.
    let sock = t.local_socket_for(&session).expect("forward");
    let mut client =
        nvmux::rpc::Client::connect(&sock, nvmux::rpc::PROBE_TIMEOUT).expect("connect via forward");
    client.api_info().expect("api_info through the forward");

    let listed = t.list_sessions().expect("list");
    let stored = listed
        .iter()
        .find(|s| s.id == session.id)
        .expect("the session should be listed");
    assert_eq!(
        stored.command,
        launch.line(),
        "the remote metadata must record what launched it"
    );

    t.kill_session(&session).expect("kill");
}
