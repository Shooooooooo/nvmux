//! End-to-end tests for the relay transport, against a real `sshd`.
//!
//! The same host as `ssh_lifecycle.rs` — `$NVMUX_TEST_SSH_HOST`, defaulting to
//! `selftest` — and the same rule: where no such host answers these skip, and
//! `$NVMUX_TEST_REQUIRE=ssh` turns the skip into a failure.
//!
//! What these add over that file is the other way of reaching the host: one
//! plain ssh connection with the relay at the far end (see `nvmux::mux`),
//! which is the only way there is from Windows. The host needs nothing it did
//! not already have — the relay runs under its own Neovim.
#![cfg(unix)]

#[macro_use]
mod common;

use std::time::Duration;

use common::unique;
use nvmux::transport::relay::RelayTransport;
use nvmux::transport::{Reconnect, Transport};

fn host() -> String {
    std::env::var("NVMUX_TEST_SSH_HOST").unwrap_or_else(|_| "selftest".to_string())
}

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

/// Removes every session this test created, whatever happened.
struct Cleanup(Vec<String>);

impl Cleanup {
    fn of<S: AsRef<str>>(names: impl IntoIterator<Item = S>) -> Self {
        Self(names.into_iter().map(|n| n.as_ref().to_string()).collect())
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let Ok(t) = RelayTransport::new(host()) else {
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

/// The whole of what the relay is for: a session made over it, and reached
/// through the endpoint it serves — a real request, answered by the real
/// editor over there, and a deferred one too, which proves the session is
/// serving rather than that something accepted a connection.
#[test]
fn a_session_created_over_the_relay_is_reachable_through_its_endpoint() {
    require_ssh!();
    let t = RelayTransport::new(host()).expect("connect");
    let name = unique("relay-reach");
    let _guard = Cleanup::of([&name]);

    let session = t
        .create_session(&name, &common::launch(), common::anywhere())
        .expect("create");
    assert!(
        session.pid > 1,
        "spawn should report a validated remote pid"
    );

    let sock = t.local_socket_for(&session).expect("an endpoint");
    assert!(sock.exists(), "no endpoint at {}", sock.display());

    let mut client =
        nvmux::rpc::Client::connect(&sock, nvmux::rpc::PROBE_TIMEOUT).expect("connect");
    let info = client.api_info().expect("api_info through the relay");
    assert!(info.is_supported(), "remote nvim {info} is too old");
    client
        .list_bufs()
        .expect("a deferred call through the relay");

    // A second connection at the same time is a second channel, and both
    // work: the probe and the client connect together on every attach.
    let mut second =
        nvmux::rpc::Client::connect(&sock, nvmux::rpc::PROBE_TIMEOUT).expect("connect");
    second.api_info().expect("a second channel");
    client.api_info().expect("the first is unaffected");
}

/// Sessions are the far side's processes, not the relay's: a transport — and
/// its connection — going leaves them running, for the next one to find.
#[test]
fn a_session_outlives_the_relay_that_made_it() {
    require_ssh!();
    let name = unique("relay-outlive");
    let _guard = Cleanup::of([&name]);

    let id = {
        let t = RelayTransport::new(host()).expect("connect");
        t.create_session(&name, &common::launch(), common::anywhere())
            .expect("create")
            .id
    };

    let t2 = RelayTransport::new(host()).expect("reconnect");
    let found = common::find_by_id(&t2, &id).expect("the session should have survived");
    assert_eq!(found.name, name);
    let sock = t2.local_socket_for(&found).expect("an endpoint");
    let mut client =
        nvmux::rpc::Client::connect(&sock, nvmux::rpc::PROBE_TIMEOUT).expect("connect");
    client.api_info().expect("usable through the new relay");
}

#[test]
fn renaming_and_killing_over_the_relay() {
    require_ssh!();
    let t = RelayTransport::new(host()).expect("connect");
    let name = unique("relay-rename");
    let _guard = Cleanup::of([name.clone(), format!("{name}-after")]);

    let session = t
        .create_session(&name, &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("endpoint");

    let after = format!("{name}-after");
    t.rename_session(&session, &after).expect("rename");
    let listed = common::find_by_id(&t, &session.id).expect("still listed");
    assert_eq!(listed.name, after);
    assert_eq!(t.local_socket_for(&listed).expect("endpoint"), sock);

    t.kill_session(&listed).expect("kill");
    assert!(
        !t.list_sessions()
            .expect("list")
            .iter()
            .any(|s| s.id == session.id),
        "the session should be gone from the listing"
    );
    assert!(!sock.exists(), "its endpoint should have stopped");
}

#[test]
fn names_with_shell_metacharacters_survive_the_relay() {
    require_ssh!();
    let t = RelayTransport::new(host()).expect("connect");
    let name = format!("{} $(id) 'q' \"d\"", unique("relay-quote"));
    let _guard = Cleanup::of([unique("relay-quote")]);

    let session = t
        .create_session(&name, &common::launch(), common::anywhere())
        .expect("create");
    let found = common::find_by_id(&t, &session.id).expect("listed");
    assert_eq!(
        found.name, name,
        "the name was mangled or evaluated in transit"
    );
}

/// The create prompt's completion asks the host about its directories on a
/// shell of its own — over the relay, a channel on the connection already up.
#[test]
fn directories_are_listed_over_the_relay() {
    require_ssh!();
    let t = RelayTransport::new(host()).expect("connect");
    let mut lister = nvmux::dirs::Lister::new(t.dir_source());
    let listing = lister.children("/").expect("a listing");
    assert!(
        listing.names.iter().any(|n| n == "tmp"),
        "/ should hold /tmp: {:?}",
        listing.names
    );
}

/// What a sleeping laptop does to the link: its ssh goes. A session's client
/// then sees its connection close, as it would if the session had ended, and
/// the transport is what tells the two apart — and brings the link back.
#[test]
fn a_link_that_drops_is_brought_back_and_the_session_is_still_there() {
    require_ssh!();
    let t = RelayTransport::new(host()).expect("connect");
    let name = unique("relay-drop");
    let _guard = Cleanup::of([&name]);
    let session = t
        .create_session(&name, &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("endpoint");
    let mut client =
        nvmux::rpc::Client::connect(&sock, nvmux::rpc::PROBE_TIMEOUT).expect("connect");
    client.api_info().expect("up");

    let pid = t.link_pid().expect("a link");
    common::sigkill_and_wait(pid);

    // The client's channel went with the link.
    assert!(
        common::wait_until(Duration::from_secs(5), || client.api_info().is_err()),
        "a connection over a dead link still answers"
    );

    assert_eq!(t.reconnect().expect("reconnect"), Reconnect::Restored);
    assert_eq!(t.reconnect().expect("asked again"), Reconnect::Unneeded);
    assert_ne!(t.link_pid(), Some(pid), "a new link, not the old one");

    let again = t.local_socket_for(&session).expect("endpoint");
    let mut client =
        nvmux::rpc::Client::connect(&again, nvmux::rpc::PROBE_TIMEOUT).expect("connect");
    client
        .list_bufs()
        .expect("the session is reached over the new link");
    assert!(t
        .list_sessions()
        .expect("scripts run over the new link too")
        .iter()
        .any(|s| s.id == session.id));
}

#[test]
fn an_unreachable_host_fails_with_a_useful_message() {
    let err = RelayTransport::new("nvmux-no-such-host.invalid".into())
        .expect_err("must fail")
        .to_string();
    assert!(
        err.contains("unreachable") || err.contains("ssh"),
        "unhelpful error: {err}"
    );
}
