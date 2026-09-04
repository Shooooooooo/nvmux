//! End-to-end tests for the SSH transport, against a real `sshd`.
//!
//! These need a host nvmux can actually reach. The host comes from
//! `$NVMUX_TEST_SSH_HOST`, defaulting to `selftest`; if that host does not
//! answer, every test here **skips** rather than fails, because a developer
//! machine has no reason to have one configured.
//!
//! Set one up like this to run them:
//!
//! ```text
//! Host selftest
//!   HostName 127.0.0.1
//!   User <you>
//!   IdentityFile ~/.ssh/id_ed25519
//! ```
//!
//! Pointing the alias at localhost is not a cheat. It exercises the real ssh
//! client, a real ControlMaster, real unix-socket forwarding and a real remote
//! login shell — everything except network latency.

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
        if !reachable() {
            eprintln!("skipping: {} is not reachable over ssh", host());
            return;
        }
    };
}

/// Names every test session distinctly, so a failed run cannot poison the next.
fn unique(tag: &str) -> String {
    format!("it-{tag}-{}", std::process::id())
}

/// Removes every session this test file created, whatever happened.
struct Cleanup(SshTransport, Vec<String>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Ok(sessions) = self.0.list_sessions() {
            for s in sessions {
                if self.1.iter().any(|n| s.name.starts_with(n.as_str())) {
                    let _ = self.0.kill_session(&s);
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
    let guard = Cleanup(
        SshTransport::new(host()).expect("connect"),
        vec![name.clone()],
    );

    let session = t.create_session(&name).expect("create");
    assert!(
        session.pid > 1,
        "spawn should report a validated remote pid"
    );

    // The seam: a path on THIS machine that speaks to a Neovim on the far side.
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

    drop(guard);
}

#[test]
fn a_remote_session_outlives_the_transport_that_made_it() {
    require_ssh!();
    let name = unique("outlive");
    let guard = Cleanup(
        SshTransport::new(host()).expect("connect"),
        vec![name.clone()],
    );

    let id = {
        let t = SshTransport::new(host()).expect("connect");
        let s = t.create_session(&name).expect("create");
        s.id
    }; // transport dropped, ssh commands finished

    // A completely fresh transport must find it, which is the whole point of
    // keeping the metadata on the session host.
    let t2 = SshTransport::new(host()).expect("reconnect");
    let found = t2
        .list_sessions()
        .expect("list")
        .into_iter()
        .find(|s| s.id == id)
        .expect("the session should have survived");
    assert_eq!(found.name, name);

    drop(guard);
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
    let guard = Cleanup(
        SshTransport::new(host()).expect("connect"),
        vec![name.clone()],
    );

    let session = t.create_session(&name).expect("create");
    let first = t.local_socket_for(&session).expect("forward");
    assert!(first.exists());

    // Tear the local end down the way a detach does.
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

    drop(guard);
}

#[test]
fn renaming_a_remote_session_moves_no_socket() {
    require_ssh!();
    let t = SshTransport::new(host()).expect("connect");
    let name = unique("rename");
    let guard = Cleanup(
        SshTransport::new(host()).expect("connect"),
        vec![name.clone(), format!("{name}-after")],
    );

    let session = t.create_session(&name).expect("create");
    let before = t.local_socket_for(&session).expect("forward");

    let after_name = format!("{name}-after");
    t.rename_session(&session, &after_name).expect("rename");

    let listed = t
        .list_sessions()
        .expect("list")
        .into_iter()
        .find(|s| s.id == session.id)
        .expect("still listed");
    assert_eq!(listed.name, after_name);
    assert_eq!(
        t.local_socket_for(&listed).expect("forward"),
        before,
        "a rename must not move the socket, or every forward would have to be rebuilt"
    );

    drop(guard);
}

#[test]
fn killing_a_remote_session_removes_it_and_its_forward() {
    require_ssh!();
    let t = SshTransport::new(host()).expect("connect");
    let name = unique("kill");

    let session = t.create_session(&name).expect("create");
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
    let guard = Cleanup(
        SshTransport::new(host()).expect("connect"),
        vec![unique("quote")],
    );

    let session = t.create_session(&name).expect("create");
    let found = t
        .list_sessions()
        .expect("list")
        .into_iter()
        .find(|s| s.id == session.id)
        .expect("listed");
    assert_eq!(
        found.name, name,
        "the name was mangled or evaluated in transit"
    );

    drop(guard);
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
