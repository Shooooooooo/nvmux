//! A quick listing, end to end, against real `nvim --headless` processes: a
//! listing waits no more than a quarter of a second on a session that cannot
//! answer (see `nvmux::rpc::QUICK_PROBE_TIMEOUT`).

#[macro_use]
mod common;

use std::time::{Duration, Instant};

use common::Scratch;
use nvmux::session::Liveness;
use nvmux::transport::Transport;

/// A session blocked as `:!make` blocks it holds a listing a quarter of a
/// second, and is listed as it would be at the end of a longer wait: busy,
/// and kept. One idle beside it answers, as ever; and one that has gone is
/// found gone in that quarter of a second, and reaped.
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
    // Longer than the three seconds a listing once waited.
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
    // Waiting on the busy session is waiting out a probe's budget, which a
    // listing that does not leaves well short of, on a busy machine too.
    assert!(
        took < nvmux::rpc::PROBE_TIMEOUT - Duration::from_millis(500),
        "the listing waited {took:?} on a busy session"
    );
}
