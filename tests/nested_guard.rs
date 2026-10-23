//! The nested-launch guard, end to end: `scripts/spawn.sh` marks a session's
//! editor, and nvmux refuses to start when it finds that mark. These drive the
//! real binary, since a guard not wired into `main` would pass the unit tests.

#[macro_use]
mod common;

use std::process::{Command, Output, Stdio};
use std::time::Duration;

use common::Scratch;
use nvmux::transport::Transport;

/// Run the binary with `$NVMUX` set to `marker`, no `nvim` on `$PATH` and a
/// missing `$NVMUX_CONFIG`. These make a run past the guard exit at once
/// instead of waiting at the picker, and if either error shows up, the guard
/// did not run first.
fn nvmux_with_marker(marker: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nvmux"))
        .env(nvmux::nested::MARKER, marker)
        .env("NVMUX_CONFIG", "/nonexistent/nvmux/config.toml")
        .env("PATH", "")
        .stdin(Stdio::null())
        .output()
        .expect("run nvmux")
}

/// The marker must be on the editor, not nvmux: the editor starts `:terminal`
/// shells, and over ssh nvmux's own environment never reaches it.
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

/// Needs no nvim: the guard runs before the version check, the config and any
/// first-run prompt.
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
    // Both ways out: the picker from inside, or unsetting the marker.
    assert!(stderr.contains("<prefix> Space"), "{stderr}");
    assert!(stderr.contains("$NVMUX"), "{stderr}");
    // Neither sabotage was reached, so nothing ran ahead of the guard.
    assert!(!stderr.contains("not found on $PATH"), "{stderr}");
    assert!(!stderr.contains("config file not found"), "{stderr}");
}

/// The documented override, `NVMUX= nvmux`. The assertion is negative: what
/// comes after the guard is the ordinary preflight.
#[test]
fn an_empty_marker_is_not_a_session() {
    let stderr = String::from_utf8_lossy(&nvmux_with_marker("").stderr).into_owned();
    assert!(!stderr.contains("already inside"), "{stderr}");
}
