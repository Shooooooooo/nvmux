//! End-to-end tests of sessions pushing news, against real `nvim --headless`:
//! the notifier listens to each session, a card arrives as its news happens
//! rather than at the next 2 s read, and those reads stop once the session is
//! listened to. A separate binary because the card board is process-wide; the
//! tests take turns.
//!
//! They skip, rather than fail, when there is no usable `nvim` on `$PATH`,
//! unless `$NVMUX_TEST_REQUIRE` names `nvim` (as in CI).

#[macro_use]
mod common;

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use common::{a_job_that_exits, a_session, lua, posts_until, shown, wait_until, Scratch};
use nvmux::card::{self, Post, Posted};
use nvmux::notes::{self, Heard};
use nvmux::rpc::Client;
use nvmux::tell::Notifier;

/// Serialises the tests: the board is process-wide.
fn one_at_a_time() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

/// Give the session the watcher and have it record each read from now on, so
/// the test can see how often the notifier asks.
fn count_reads(sock: &Path) {
    assert!(
        matches!(notes::hear(sock, None), Heard::Said { .. }),
        "the session said nothing"
    );
    lua(
        sock,
        r"local N = package.loaded['nvmux.notes']
local snapshot = N.snapshot
_G.nvmux_reads = {}
N.snapshot = function()
  table.insert(_G.nvmux_reads, vim.uv.hrtime())
  return snapshot()
end",
    );
}

/// How many times the session has been read since [`count_reads`], and how
/// long ago the last of them was: `None` before the first.
fn reads(sock: &Path) -> (u64, Option<Duration>) {
    let said = lua(
        sock,
        r"local reads = _G.nvmux_reads
local last = reads[#reads]
return { #reads, last and math.floor((vim.uv.hrtime() - last) / 1e6) or -1 }",
    );
    let said = said.as_array().expect("an array");
    let count = said[0].as_u64().expect("a count");
    let ago = said[1].as_u64().map(Duration::from_millis);
    (count, ago)
}

fn says(post: &Post, what: &str) -> bool {
    shown(post).is_some_and(|(c, _)| c.body.as_deref() == Some(what))
}

/// Wait for a card saying `what`; returns the time since `since`.
fn card_saying(what: &str, since: Instant) -> Duration {
    let posts = posts_until(Duration::from_secs(8), |posts| {
        posts.iter().any(|p| says(p, what))
    });
    let took = since.elapsed();
    assert!(
        posts.iter().any(|p| says(p, what)),
        "no card saying {what:?}: {posts:?}"
    );
    took
}

/// Wait for session `id`'s mark to be dropped; returns the time since
/// `since`.
fn forgotten(id: &str, since: Instant) -> Duration {
    let forget = |p: &Post| p.what == Posted::Forget(id.to_string());
    let posts = posts_until(Duration::from_secs(8), |posts| posts.iter().any(forget));
    let took = since.elapsed();
    assert!(posts.iter().any(forget), "the mark stayed: {posts:?}");
    took
}

/// A session is read until its watcher is found telling the notifier's
/// listener, then left alone; its news arrives as a card at once, with one
/// read. A seen mark is dropped at once, as is the mark of a session that
/// exits.
#[test]
fn a_card_comes_as_its_news_happens_and_the_reads_stop() {
    require_nvim!();
    let _turn = one_at_a_time();
    let scratch = Scratch::new("push-told");
    let (front, front_sock) = a_session(&scratch, "front");
    let (other, other_sock) = a_session(&scratch, "other");
    let (doomed, doomed_sock) = a_session(&scratch, "doomed");
    count_reads(&other_sock);
    card::board().take();

    let notifier = Notifier::start();
    notifier.watch(&front.id, "front", front_sock.clone());
    notifier.watch(&other.id, "other", other_sock.clone());
    notifier.watch(&doomed.id, "doomed", doomed_sock.clone());
    notifier.front(Some(&front.id));

    // Asked every two seconds, it would never go three without a read.
    let quiet = Duration::from_secs(3);
    assert!(
        wait_until(Duration::from_secs(20), || {
            reads(&other_sock).1.is_some_and(|ago| ago >= quiet)
        }),
        "still read every two seconds: {:?}",
        reads(&other_sock)
    );
    let (before, _) = reads(&other_sock);
    assert!(
        (2..=3).contains(&before),
        "read {before} times before it was left alone"
    );

    // Timed from the job's end, the news itself: startup (pty, shell) varies
    // with load.
    a_job_that_exits(&other_sock, "exit 0", false);
    let started = Instant::now();
    let took = card_saying("sh: exited with code 0", started);
    eprintln!("card {took:?} after the job ended");
    assert!(took < Duration::from_secs(1), "the card took {took:?}");
    std::thread::sleep(notes::POLL + Duration::from_millis(500));
    assert_eq!(
        reads(&other_sock).0,
        before + 1,
        "one read for the news, and none since"
    );

    let started = Instant::now();
    notes::seen(&other_sock);
    let took = forgotten(&other.id, started);
    assert!(took < Duration::from_secs(1), "let go of after {took:?}");

    // Forty notifications in a second: read about four times a second, not
    // forty, and the last one shown.
    let (before, _) = reads(&other_sock);
    lua(
        &other_sock,
        r#"local buf = vim.api.nvim_create_buf(true, false)
vim.api.nvim_buf_call(buf, function()
  vim.fn.jobstart({ 'sh', '-c', [=[
for i in $(seq 1 40); do printf '\033]777;notify;Build;step %s\007' "$i"; sleep 0.025; done
sleep 30]=] }, { term = true })
end)"#,
    );
    let started = Instant::now();
    card_saying("Build · step 40", started);
    std::thread::sleep(Duration::from_millis(500));
    let (after, _) = reads(&other_sock);
    let spent = started.elapsed().as_secs_f64();
    eprintln!(
        "{} reads for forty notifications in {spent:.1} s",
        after - before
    );
    assert!(
        after - before <= 2 + (spent * 4.0) as u64,
        "read {} times in {spent:.1} s",
        after - before
    );

    // One event reported twice (a command's end by its shell, then by its
    // terminal closing) is read once and shown once, as the second.
    let (before, _) = reads(&other_sock);
    let started = Instant::now();
    a_job_that_exits(
        &other_sock,
        r"printf '\033]133;C\007\033]133;D;9\007'; exit 9",
        false,
    );
    let posts = posts_until(Duration::from_secs(8), |posts| {
        posts.iter().any(|p| says(p, "sh: exited with code 9"))
    });
    assert!(
        posts.iter().any(|p| says(p, "sh: exited with code 9")),
        "no card for the terminal: {posts:?}"
    );
    std::thread::sleep(Duration::from_millis(600));
    let posts: Vec<Post> = posts.into_iter().chain(card::board().take()).collect();
    assert!(
        !posts
            .iter()
            .any(|p| says(p, "sh: a command exited with code 9")),
        "the shell's word on it told apart from the terminal's: {posts:?}"
    );
    assert_eq!(
        reads(&other_sock).0,
        before + 1,
        "read more than once for one event, in {:?}",
        started.elapsed()
    );

    // Busy for 2 s after its news: the read it triggers goes unanswered, and
    // the session is read again 2 s later rather than at its next poll.
    lua(
        &other_sock,
        r"vim.api.nvim_create_autocmd('TermClose', {
  once = true,
  callback = function()
    vim.fn.system('sleep 2')
  end,
})",
    );
    let started = Instant::now();
    a_job_that_exits(&other_sock, "exit 6", false);
    let took = card_saying("sh: exited with code 6", started);
    eprintln!("card {took:?} after a job its session was busy behind");
    assert!(
        took < Duration::from_secs(2) + notes::POLL + Duration::from_secs(2),
        "the card took {took:?}"
    );

    let started = Instant::now();
    a_job_that_exits(&doomed_sock, "exit 3", false);
    card_saying("sh: exited with code 3", started);
    // Timed from the signal: connections close at once, though reaping the
    // pid may take a second longer.
    let started = Instant::now();
    let doomed_pid = nix::unistd::Pid::from_raw(i32::try_from(doomed.pid).expect("a pid"));
    nix::sys::signal::kill(doomed_pid, nix::sys::signal::Signal::SIGKILL).expect("kill -9");
    let took = forgotten(&doomed.id, started);
    eprintln!("the mark of a session that went let go of after {took:?}");
    assert!(took < Duration::from_secs(1), "let go of after {took:?}");
}

/// A session that cannot be listened to (waiting for a key when its listener
/// starts) is still read every 2 s, and its news still arrives as a card.
#[test]
fn a_session_not_listened_to_is_read_as_ever() {
    require_nvim!();
    let _turn = one_at_a_time();
    let scratch = Scratch::new("push-unheard");
    let (front, front_sock) = a_session(&scratch, "front");
    let (waiting, waiting_sock) = a_session(&scratch, "waiting");
    count_reads(&waiting_sock);
    common::block_on_a_key(&waiting_sock);
    card::board().take();

    let notifier = Notifier::start();
    notifier.watch(&front.id, "front", front_sock.clone());
    notifier.watch(&waiting.id, "waiting", waiting_sock.clone());
    notifier.front(Some(&front.id));
    std::thread::sleep(Duration::from_millis(500));
    Client::connect(&waiting_sock, Duration::from_secs(2))
        .expect("connect")
        .input("<Esc>")
        .expect("press a key");

    // Read once it can answer; no listener is registered on it yet.
    assert!(
        wait_until(notes::POLL + Duration::from_secs(1), || {
            reads(&waiting_sock).0 > 0
        }),
        "never read once it could answer"
    );
    assert_eq!(
        lua(
            &waiting_sock,
            "return vim.tbl_count(package.loaded['nvmux.notes'].listeners)"
        )
        .as_u64(),
        Some(0),
        "listened to, though it was waiting for a key when its listener started"
    );
    let started = Instant::now();
    a_job_that_exits(&waiting_sock, "exit 0", false);
    let took = card_saying("sh: exited with code 0", started);
    assert!(
        took < notes::POLL + Duration::from_secs(1),
        "the card took {took:?}"
    );
}

/// `:restart` (Neovim 0.12) leaves a new server at the session's socket, so
/// the listener stops and the watcher there, installed by the next read,
/// numbers its news from 1 again. That news is still news.
#[test]
fn a_restarted_session_still_tells_its_news() {
    require_nvim!();
    let _turn = one_at_a_time();
    let scratch = Scratch::new("push-restart");
    let (front, front_sock) = a_session(&scratch, "front");
    let (other, other_sock) = a_session(&scratch, "other");
    // 0.12.0 restarts only for the UI that asks.
    if lua(&other_sock, "return vim.fn.has('nvim-0.12.1') == 1") != rmpv::Value::Boolean(true) {
        eprintln!("skipping: no :restart without a UI before Neovim 0.12.1");
        return;
    }
    card::board().take();

    let notifier = Notifier::start();
    notifier.watch(&front.id, "front", front_sock.clone());
    notifier.watch(&other.id, "other", other_sock.clone());
    notifier.front(Some(&front.id));
    // The first round only takes each session's news as heard.
    let watched = |sock: &Path| {
        lua(
            sock,
            "return pcall(vim.api.nvim_get_autocmds, { group = 'nvmux_notes' })",
        ) == rmpv::Value::Boolean(true)
    };
    assert!(
        wait_until(Duration::from_secs(10), || watched(&other_sock)),
        "the first round never watched the session"
    );
    for code in [1, 2] {
        let started = Instant::now();
        a_job_that_exits(&other_sock, &format!("exit {code}"), false);
        card_saying(&format!("sh: exited with code {code}"), started);
    }

    let pid = |sock: &Path| {
        Client::connect(sock, Duration::from_secs(2))
            .and_then(|mut c| c.eval("getpid()"))
            .ok()
            .and_then(|v| v.as_u64())
    };
    let old = pid(&other_sock).expect("the server's pid");
    // Unanswered: the server quits. From 0.12.5 the bang saves no session,
    // whose file only a UI's arrival would remove.
    let _ = Client::connect(&other_sock, Duration::from_secs(2))
        .and_then(|mut c| c.command("execute has('nvim-0.12.5') ? 'restart!' : 'restart'"));
    let mut new = None;
    assert!(
        wait_until(Duration::from_secs(10), || {
            new = pid(&other_sock).filter(|p| *p != old);
            new.is_some()
        }),
        "no new server took the socket"
    );
    // Killed with the scratch, as the old one would have been.
    let json = other_sock.with_extension("json");
    let mut meta = common::read_meta(&json);
    meta["pid"] = serde_json::json!(new);
    common::write_meta(&json, &meta);
    // The watcher keeps nothing from before it is installed.
    assert!(
        wait_until(Duration::from_secs(10), || watched(&other_sock)),
        "the new server was never watched"
    );

    let started = Instant::now();
    a_job_that_exits(&other_sock, "exit 3", false);
    card_saying("sh: exited with code 3", started);
}
