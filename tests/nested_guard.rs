//! The nested-launch guard, end to end.
//!
//! Two halves that have to meet: `scripts/spawn.sh` marks a session's editor,
//! and nvmux refuses to start when it finds that mark. The first test proves
//! the mark reaches a real Neovim — which is exactly the environment a
//! `:terminal` opened inside the session hands to its shell — and the rest
//! drive the real binary, since a guard that never reached `main` would still
//! pass every unit test in `nested`.

#[macro_use]
mod common;

use std::process::{Command, Output, Stdio};
use std::time::Duration;

use common::Scratch;
use nvmux::transport::Transport;

/// Run the real binary with `$NVMUX` set to `marker`, and with the two things
/// `run` looks at next deliberately sabotaged: no `nvim` on `$PATH`, and a
/// `$NVMUX_CONFIG` naming a file that is not there.
///
/// Both are load-bearing. They keep the run bounded — one that gets *past* the
/// guard stops at once instead of sitting at a picker no stdin will ever
/// answer — and they turn the ordering into something a test can see: if
/// either of their messages comes back, the guard did not go first.
fn nvmux_with_marker(marker: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nvmux"))
        .env(nvmux::nested::MARKER, marker)
        .env("NVMUX_CONFIG", "/nonexistent/nvmux/config.toml")
        .env("PATH", "")
        .stdin(Stdio::null())
        .output()
        .expect("run nvmux")
}

/// The marker has to be on the *editor*, not on nvmux: the editor is what
/// starts a `:terminal` shell, and over ssh it is the only end of the
/// connection our own environment never reaches.
#[test]
fn a_sessions_editor_carries_the_marker() {
    require_nvim!();
    let scratch = Scratch::new("marker");
    let t = scratch.transport();

    let session = t
        .create_session("marked", &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket");

    let mut client = nvmux::rpc::Client::connect(&sock, Duration::from_secs(2)).expect("connect");
    let value = client
        .call("nvim_eval", vec![rmpv::Value::String("$NVMUX".into())])
        .expect("eval $NVMUX");

    assert_eq!(
        value.as_str(),
        sock.to_str(),
        "the editor should carry its own socket as ${}",
        nvmux::nested::MARKER
    );
}

/// Needs no nvim, and that is the point: the guard runs before the version
/// check, before the config, and so before any first-run prompt.
#[test]
fn a_nested_launch_is_refused_before_anything_else_is_looked_at() {
    let out = nvmux_with_marker("/tmp/nvmux-0/abcdefgh.sock");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("already inside an nvmux session"),
        "{stderr}"
    );
    assert!(stderr.contains("/tmp/nvmux-0/abcdefgh.sock"), "{stderr}");
    // The two ways on: the picker from inside, or unsetting the marker.
    assert!(stderr.contains("<prefix> Space"), "{stderr}");
    assert!(stderr.contains("$NVMUX"), "{stderr}");
    // Neither sabotage was reached, so nothing ran ahead of the guard.
    assert!(!stderr.contains("not found on $PATH"), "{stderr}");
    assert!(!stderr.contains("config file not found"), "{stderr}");
}

/// The documented override, `NVMUX= nvmux`. It only has to get *past* the
/// guard; what it runs into next is the ordinary preflight, so the assertion
/// is deliberately negative rather than pinned to whatever that then says.
#[test]
fn an_empty_marker_is_not_a_session() {
    let stderr = String::from_utf8_lossy(&nvmux_with_marker("").stderr).into_owned();
    assert!(!stderr.contains("already inside"), "{stderr}");
}
