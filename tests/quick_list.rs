//! `[session] quick_list`, end to end, against real `nvim --headless`
//! processes: a listing that waits no more than a quarter of a second on a
//! session that cannot answer, and with the setting off, three seconds. A
//! binary of its own, because the setting is the whole process's (see
//! `nvmux::config::init`), and this turns it off part way.

#[macro_use]
mod common;

use std::time::{Duration, Instant};

use common::Scratch;
use nvmux::session::Liveness;
use nvmux::transport::Transport;

/// Run on `[session] quick_list = true` — the default, said here so that the
/// test says what it is about — the rest the defaults.
fn quick() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let mut settings = nvmux::config::Settings::default();
        settings.session.quick_list = true;
        nvmux::config::init(settings);
    });
    assert!(
        nvmux::config::get().session.quick_list,
        "the settings were read before they were set"
    );
}

/// With `[session] quick_list` on, the default, a session blocked as `:!make`
/// blocks it holds a listing a quarter of a second, and is listed as it is at
/// the end of the longer wait: busy, and kept. One idle beside it answers, as
/// ever; and one that has gone is found gone in that quarter of a second, and
/// reaped. With it off, the busy session holds the listing for the whole of
/// the probe's budget, three seconds, and is listed the same.
#[test]
fn a_quick_listing_does_not_wait_on_a_busy_session() {
    require_nvim!();
    quick();
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
    // Longer than the three seconds a listing would wait without the setting.
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
    assert!(
        took < Duration::from_secs(2),
        "the listing waited {took:?} on a busy session"
    );

    let mut slow = (*nvmux::config::get()).clone();
    slow.session.quick_list = false;
    nvmux::config::apply(slow).expect("valid settings");
    let started = Instant::now();
    let listed = t.list_sessions().expect("list");
    let took = started.elapsed();
    let liveness = |id: &str| listed.iter().find(|s| s.id == id).map(|s| s.state.liveness);
    assert_eq!(liveness(&busy.id), Some(Liveness::Busy), "{listed:?}");
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert!(
        took >= nvmux::rpc::PROBE_TIMEOUT - Duration::from_millis(500),
        "with the setting off, the listing gave up on a busy session after {took:?}"
    );
}
