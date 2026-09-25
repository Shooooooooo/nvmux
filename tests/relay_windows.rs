//! The relay transport from Windows, end to end: what `nvmux <host>` rests on
//! there — Windows' own `ssh`, which cannot multiplex, a session's endpoint
//! served as a named pipe, and the relay at the far end — against a real host.
//!
//! `$NVMUX_TEST_SSH_HOST` names the host: a Linux or macOS machine this one
//! reaches with `ssh` and no prompt, with Neovim on it. Unlike the Unix tests
//! there is no default, since a Windows machine has no `selftest` of its own
//! to fall back on; without a host these skip, and `$NVMUX_TEST_REQUIRE=ssh`
//! turns the skip into a failure, as it does there.
#![cfg(windows)]

use nvmux::launch::Launch;
use nvmux::transport::relay::RelayTransport;
use nvmux::transport::Transport;

fn host() -> Option<String> {
    std::env::var("NVMUX_TEST_SSH_HOST")
        .ok()
        .filter(|h| !h.is_empty())
}

fn required() -> bool {
    std::env::var("NVMUX_TEST_REQUIRE").is_ok_and(|v| v.split(',').any(|w| w.trim() == "ssh"))
}

/// The host, when there is one this machine's `ssh` can reach unprompted.
fn reachable() -> Option<String> {
    let host = host()?;
    let ok = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            &host,
            "true",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success());
    ok.then_some(host)
}

macro_rules! require_host {
    () => {
        match reachable() {
            Some(host) => host,
            None => {
                let why = "no host in $NVMUX_TEST_SSH_HOST that ssh reaches unprompted";
                assert!(
                    !required(),
                    "NVMUX_TEST_REQUIRE names ssh but there is {why}"
                );
                eprintln!("skipping: {why}");
                return;
            }
        }
    };
}

fn unique(tag: &str) -> String {
    format!("win-{tag}-{}", std::process::id())
}

fn launch() -> Launch {
    Launch::parse(nvmux::launch::DEFAULT).expect("the built-in default must parse")
}

/// Removes every session this test made, whatever happened.
struct Cleanup {
    host: String,
    names: Vec<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let Ok(t) = RelayTransport::new(self.host.clone()) else {
            return;
        };
        if let Ok(sessions) = t.list_sessions() {
            for s in sessions {
                if self.names.iter().any(|n| s.name.starts_with(n.as_str())) {
                    let _ = t.kill_session(&s);
                }
            }
        }
    }
}

/// A session made from Windows, and reached through the pipe the relay serves
/// for it: a real request answered by the editor over there, a deferred one
/// that proves it is serving, and a second channel beside the first — what an
/// attach opens. Then renamed, and killed, which stops its pipe.
#[test]
fn a_session_made_from_windows_is_reachable_through_its_pipe() {
    let host = require_host!();
    let t = RelayTransport::new(host.clone()).expect("connect");
    let name = unique("reach");
    let _guard = Cleanup {
        host,
        names: vec![name.clone()],
    };

    let session = t.create_session(&name, &launch(), "/").expect("create");
    let pipe = t.local_socket_for(&session).expect("an endpoint");
    assert!(
        pipe.to_string_lossy().starts_with(r"\\.\pipe\nvmux-"),
        "a session's endpoint here is a named pipe: {}",
        pipe.display()
    );

    let mut client =
        nvmux::rpc::Client::connect(&pipe, nvmux::rpc::PROBE_TIMEOUT).expect("connect");
    let info = client.api_info().expect("api_info through the pipe");
    assert!(info.is_supported(), "remote nvim {info} is too old");
    client
        .list_bufs()
        .expect("a deferred call through the pipe");

    let mut second =
        nvmux::rpc::Client::connect(&pipe, nvmux::rpc::PROBE_TIMEOUT).expect("connect again");
    second.api_info().expect("a second channel");
    client.api_info().expect("the first is unaffected");
    drop((client, second));

    let after = format!("{name}-after");
    t.rename_session(&session, &after).expect("rename");
    let listed = t
        .list_sessions()
        .expect("list")
        .into_iter()
        .find(|s| s.id == session.id)
        .expect("still listed");
    assert_eq!(listed.name, after);

    t.kill_session(&listed).expect("kill");
    assert!(
        !t.list_sessions()
            .expect("list")
            .iter()
            .any(|s| s.id == session.id),
        "the session should be gone from the listing"
    );
    assert!(
        nvmux::rpc::Client::connect(&pipe, nvmux::rpc::CONNECT_TIMEOUT).is_err(),
        "its pipe should have stopped"
    );
}

/// Sessions are the host's, not the relay's: a second transport — a second
/// nvmux, or this one after its link dropped — finds and reaches them.
#[test]
fn a_session_outlives_the_relay_that_made_it() {
    let host = require_host!();
    let name = unique("outlive");
    let _guard = Cleanup {
        host: host.clone(),
        names: vec![name.clone()],
    };
    let id = {
        let t = RelayTransport::new(host.clone()).expect("connect");
        t.create_session(&name, &launch(), "/").expect("create").id
    };

    let t = RelayTransport::new(host).expect("connect again");
    let found = t
        .list_sessions()
        .expect("list")
        .into_iter()
        .find(|s| s.id == id)
        .expect("the session should have survived");
    let pipe = t.local_socket_for(&found).expect("an endpoint");
    let mut client =
        nvmux::rpc::Client::connect(&pipe, nvmux::rpc::PROBE_TIMEOUT).expect("connect");
    client.api_info().expect("reachable through the new relay");
}

/// The create prompt's completion, which asks the host on a shell of its own:
/// a channel on the connection already up.
#[test]
fn directories_are_listed_from_windows() {
    let host = require_host!();
    let t = RelayTransport::new(host).expect("connect");
    let mut lister = nvmux::dirs::Lister::new(t.dir_source());
    let listing = lister.children("/").expect("a listing");
    assert!(
        listing.names.iter().any(|n| n == "tmp"),
        "/ should hold /tmp: {:?}",
        listing.names
    );
}
