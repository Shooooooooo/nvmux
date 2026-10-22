//! Sessions saying their news, end to end, against real `nvim --headless`
//! processes: the notifier listens to each session, a card comes as what it
//! tells happens rather than at the next of the reads every two seconds, and
//! those reads stop once the session is told. A binary of its own, because the
//! board the cards go on is the whole process's: its tests take turns.
//!
//! They skip, rather than fail, when there is no usable `nvim` on `$PATH` —
//! unless `$NVMUX_TEST_REQUIRE` names `nvim`, as it does in CI.

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

/// The tests' turns: the board is the process's.
fn one_at_a_time() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

/// Give the session the watcher, and have it count its reads from now on: the
/// test's own way of seeing how often the notifier asks.
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

/// Whether `post` is a card that says `what`.
fn says(post: &Post, what: &str) -> bool {
    shown(post).is_some_and(|(c, _)| c.body.as_deref() == Some(what))
}

/// Wait for a card saying `what`, and say how long it took from `since`.
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

/// Wait for the mark of session `id` to be let go of, and say how long it
/// took from `since`.
fn forgotten(id: &str, since: Instant) -> Duration {
    let forget = |p: &Post| p.what == Posted::Forget(id.to_string());
    let posts = posts_until(Duration::from_secs(8), |posts| posts.iter().any(forget));
    let took = since.elapsed();
    assert!(posts.iter().any(forget), "the mark stayed: {posts:?}");
    took
}

/// A session is read until its watcher is found telling the notifier's
/// listener, and then left alone; and what it tells comes as a card as it
/// happens, with one read for it. What it kept, seen, has its mark let go of
/// at once; and so does a session that goes, which its listener hears as it
/// goes.
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

    // Timed from the job's end, which is the news: its start is a pty, a
    // shell and its exit before that, which a busy machine makes longer.
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

    // A program that sends notification after notification, forty in a
    // second: read four times a second or so, not forty, and the last of
    // them told.
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

    // One event said twice — a command's end, by its shell and then by its
    // terminal closing — is read once, and told once, as the second.
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

    // Busy for two seconds straight after its news: the read the news brings
    // on is not answered, and the session, which has said it once, is read
    // again two seconds on rather than at the next read it would have had.
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
    // Timed from the signal: the process is gone at once, its connections
    // with it, though the pid may wait a second longer to be reaped.
    let started = Instant::now();
    let doomed_pid = nix::unistd::Pid::from_raw(i32::try_from(doomed.pid).expect("a pid"));
    nix::sys::signal::kill(doomed_pid, nix::sys::signal::Signal::SIGKILL).expect("kill -9");
    let took = forgotten(&doomed.id, started);
    eprintln!("the mark of a session that went let go of after {took:?}");
    assert!(took < Duration::from_secs(1), "let go of after {took:?}");
}

/// A session that cannot be listened to — waiting for a key when its listener
/// starts — is read every two seconds as it always was, and what it tells
/// comes as a card all the same.
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

    // Read once it can answer, which says how far it has got — and names no
    // listener, there being none on it yet.
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
