//! Quick listing against real `nvim --headless`: it waits at most a quarter
//! second on a session that cannot answer (`nvmux::rpc::QUICK_PROBE_TIMEOUT`).

#[macro_use]
mod common;

use std::time::{Duration, Instant};

use common::Scratch;
use nvmux::session::Liveness;
use nvmux::transport::Transport;

/// A blocked session (as by `:!make`) is listed busy and kept, an idle one
/// answers, and a dead one is found dead and reaped, all within that budget.
#[test]
fn a_quick_listing_does_not_wait_on_a_busy_session() {
    require_nvim!();
    let scratch = Scratch::new("quick");
    let t = scratch.transport();
    let mut made = ["building", "editing", "doomed"]
        .into_iter()
        .map(|name| {
            let session = t
                .create_session(name, &common::launch(), common::anywhere())
                .expect("create");
            let sock = t.local_socket_for(&session).expect("socket");
            (session, sock)
        })
        .collect::<Vec<_>>();
    let (doomed, doomed_sock) = made.pop().expect("three");
    let (idle, _) = made.pop().expect("two");
    let (busy, busy_sock) = made.pop().expect("one");
    common::sigkill_and_wait(doomed.pid);
    assert!(
        doomed_sock.exists(),
        "precondition: SIGKILL should leave a stale socket"
    );
    // Longer than a full probe timeout (3 s).
    common::block_editor(
        &busy_sock,
        "call system('sleep 8')",
        Duration::from_millis(500),
    );

    let started = Instant::now();
    let listed = t.list_sessions().expect("list");
    let took = started.elapsed();

    let liveness = |id: &str| listed.iter().find(|s| s.id == id).map(|s| s.state.liveness);
    assert_eq!(liveness(&busy.id), Some(Liveness::Busy), "{listed:?}");
    assert_eq!(liveness(&idle.id), Some(Liveness::Alive), "{listed:?}");
    assert_eq!(listed.len(), 2, "the dead session listed: {listed:?}");
    assert!(busy_sock.exists(), "a busy session must never be reaped");
    assert!(
        !doomed_sock.exists(),
        "the stale socket should have been reaped"
    );
    // Must finish well short of a full probe timeout, even on a busy machine.
    assert!(
        took < nvmux::rpc::PROBE_TIMEOUT - Duration::from_millis(500),
        "the listing waited {took:?} on a busy session"
    );
}
