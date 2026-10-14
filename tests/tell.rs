//! What the notifier tells the session in front ([`nvmux::tell`]), against
//! real `nvim --headless` sessions: that the watcher numbers what it keeps,
//! knows whether its terminal was on the screen, and says which buffer that
//! is and where; and that the notifier posts a card, as it happens, for what
//! the other sessions say — and of the session in front, only for what a
//! terminal of its own off the screen says.
//!
//! They skip, rather than fail, when there is no usable `nvim` on `$PATH` —
//! unless `$NVMUX_TEST_REQUIRE` names `nvim`, as it does in CI.

#[macro_use]
mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{wait_until, Scratch};
use nvmux::card::{self, Card, Go, Post, Posted};
use nvmux::notes::{self, Heard, Kind};
use nvmux::rpc::Client;
use nvmux::tell::Notifier;
use nvmux::transport::Transport;
use rmpv::Value;

/// Run Lua in the session, as a test's own stand-in for whatever a plugin or
/// a terminal program would have done there.
fn lua(sock: &Path, code: &str) -> Value {
    let mut client = Client::connect(sock, Duration::from_secs(5)).expect("connect");
    client
        .call(
            "nvim_exec_lua",
            vec![Value::from(code), Value::Array(Vec::new())],
        )
        .expect("nvim_exec_lua")
}

/// A program in a hidden terminal buffer, printing `printf`'s format and
/// staying up a while, as an agent at a prompt does.
fn in_a_terminal(sock: &Path, printf: &str) {
    lua(
        sock,
        &format!(
            r#"local buf = vim.api.nvim_create_buf(true, false)
vim.api.nvim_buf_call(buf, function()
  vim.fn.jobstart({{ 'sh', '-c', [=[printf '{printf}'; sleep 30]=] }}, {{ term = true }})
end)"#
        ),
    );
}

/// A job that runs `script` and exits, in a terminal buffer no window shows —
/// or, `on_screen`, in the current window — waited for until its `TermClose`
/// has been, so that the watcher has seen it go. Says which buffer it was.
fn a_job_that_exits(sock: &Path, script: &str, on_screen: bool) -> i64 {
    let place = if on_screen {
        "vim.cmd('enew')\nlocal buf = vim.api.nvim_get_current_buf()"
    } else {
        "local buf = vim.api.nvim_create_buf(true, false)"
    };
    lua(
        sock,
        &format!(
            r#"local closed = false
{place}
vim.api.nvim_create_autocmd('TermClose', {{
  buffer = buf,
  once = true,
  callback = function()
    closed = true
  end,
}})
vim.api.nvim_buf_call(buf, function()
  vim.fn.jobstart({{ 'sh', '-c', [=[{script}]=] }}, {{ term = true }})
end)
vim.wait(5000, function()
  return closed
end)
return buf"#
        ),
    )
    .as_i64()
    .expect("the buffer")
}

fn a_session(scratch: &Scratch, name: &str) -> (nvmux::session::Session, PathBuf) {
    let t = scratch.transport();
    let s = t
        .create_session(name, &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&s).expect("socket path");
    (s, sock)
}

/// A careful read, of an editor waiting for a key, says nothing and asks
/// nothing; and what the watcher keeps is numbered as it comes, says whether
/// its terminal was on the screen, and which buffer that is and the tab page
/// it is in, while it is.
#[test]
fn the_watcher_numbers_what_it_keeps_and_knows_where_its_terminal_is() {
    require_nvim!();
    let scratch = Scratch::new("tell-heard");
    let (_, sock) = a_session(&scratch, "heard");
    assert_eq!(
        notes::hear(&sock),
        Heard::Said(Vec::new()),
        "given the watcher"
    );

    // First, while the current window still holds a buffer of its own: in a
    // terminal's, the editor's mode is `nt`, not the `n` the wait looks for.
    common::block_on_a_key(&sock);
    assert_eq!(notes::hear(&sock), Heard::Unknown);
    Client::connect(&sock, Duration::from_secs(2))
        .expect("connect")
        .input("<Esc>")
        .expect("press a key");
    assert!(
        wait_until(Duration::from_secs(5), || common::mode(&sock)
            .is_some_and(|m| !m.blocking)),
        "the key never ended the wait"
    );

    let hidden = a_job_that_exits(&sock, "exit 3", false);
    let shown = a_job_that_exits(&sock, "exit 0", true);
    let Heard::Said(news) = notes::hear(&sock) else {
        panic!("the session said nothing");
    };
    assert_eq!(
        news.iter()
            .map(|n| (n.kind, n.shown, n.buf, n.tab))
            .collect::<Vec<_>>(),
        [
            (Kind::Failed, Some(false), Some(hidden), None),
            (Kind::Finished, Some(true), Some(shown), Some(1)),
        ],
        "{news:?}"
    );
    assert!(news[0].seq < news[1].seq, "{news:?}");

    lua(
        &sock,
        &format!("vim.api.nvim_buf_delete({hidden}, {{ force = true }})"),
    );
    let Heard::Said(news) = notes::hear(&sock) else {
        panic!("the session said nothing");
    };
    assert_eq!(news[0].buf, None, "a buffer that is gone is none: {news:?}");
}

/// The card a post shows, and where acting on it goes: none for one that lets
/// go of what is up.
fn shown(post: &Post) -> Option<(&Card, Option<Go>)> {
    match &post.what {
        Posted::Card(card, go) => Some((card, *go)),
        Posted::Forget(_) => None,
    }
}

/// Wait up to `within` for posts on the board that `want` says are the ones,
/// and hand back every post taken meanwhile.
fn posts_until(within: Duration, want: impl Fn(&[Post]) -> bool) -> Vec<Post> {
    let deadline = Instant::now() + within;
    let mut taken = Vec::new();
    while Instant::now() < deadline && !want(&taken) {
        taken.extend(card::board().take());
        std::thread::sleep(Duration::from_millis(50));
    }
    taken.extend(card::board().take());
    taken
}

/// The session in front is posted a card for what another session's
/// terminals say as it happens — a command that finished, a notification —
/// by that session's name, going there; of its own terminals, only what one
/// off the screen says, as `here`, going to it; and nothing while nvmux's own
/// screens are up. A screen that comes up while another session keeps what
/// it said is posted that session's mark alone — and told to let it go once
/// the session keeps nothing.
#[test]
fn the_notifier_posts_cards_for_the_session_in_front() {
    require_nvim!();
    let scratch = Scratch::new("tell-notifier");
    let (front, front_sock) = a_session(&scratch, "front");
    let (other, other_sock) = a_session(&scratch, "other");

    let notifier = Notifier::start();
    notifier.watch(&front.id, "front", front_sock.clone());
    notifier.watch(&other.id, "other", other_sock.clone());
    notifier.front(Some(&front.id));
    // The first round only finds out how far each session has got.
    std::thread::sleep(Duration::from_millis(800));
    card::board().take();

    let within = Duration::from_secs(8);
    let says =
        |p: &Post, what: &str| shown(p).is_some_and(|(c, _)| c.body.as_deref() == Some(what));
    let saying = |what: &'static str| move |posts: &[Post]| posts.iter().any(|p| says(p, what));

    a_job_that_exits(&other_sock, "exit 0", false);
    let posts = posts_until(within, saying("sh: exited with code 0"));
    let post = posts
        .iter()
        .find(|p| says(p, "sh: exited with code 0"))
        .unwrap_or_else(|| panic!("no card: {posts:?}"));
    assert_eq!(post.front, front.id, "for the session in front");
    let (card, go) = shown(post).expect("a card");
    assert_eq!(card.kind, card::Kind::Finished);
    assert_eq!(card.title, "other");
    assert_eq!(card.about.as_deref(), Some(other.id.as_str()));
    assert_eq!(card.footer.as_deref(), Some("Ctrl-Space ⏎ go there"));
    assert_eq!(
        go,
        Some(Go::Session(nvmux::ids::Id::new(&other.id).expect("id")))
    );

    in_a_terminal(&other_sock, r"\033]777;notify;Build;finished\007");
    let posts = posts_until(within, saying("Build · finished"));
    assert!(
        posts
            .iter()
            .filter_map(shown)
            .any(|(c, _)| c.kind == card::Kind::Notified
                && c.title == "other"
                && c.mark == Some(card::Kind::Notified)),
        "{posts:?}"
    );

    let hidden = a_job_that_exits(&front_sock, "exit 3", false);
    let posts = posts_until(within, saying("sh: exited with code 3"));
    let post = posts
        .iter()
        .find(|p| says(p, "sh: exited with code 3"))
        .unwrap_or_else(|| panic!("a terminal of its own off the screen: {posts:?}"));
    let (card, go) = shown(post).expect("a card");
    assert_eq!(card.title, "here");
    assert_eq!(card.footer.as_deref(), Some("Ctrl-Space ⏎ show it"));
    assert_eq!(go, Some(Go::Buffer(hidden)));

    a_job_that_exits(&front_sock, "exit 0", true);
    let posts = posts_until(notes::POLL + Duration::from_millis(800), |_| false);
    assert!(
        !posts.iter().any(|p| says(p, "sh: exited with code 0")),
        "posted for a terminal it was showing: {posts:?}"
    );

    notifier.front(None);
    std::thread::sleep(Duration::from_millis(100));
    a_job_that_exits(&other_sock, "exit 2", false);
    let posts = posts_until(notes::POLL + Duration::from_millis(800), |_| false);
    assert!(posts.is_empty(), "posted with nobody in front: {posts:?}");

    // Back in front, the other session keeps a failure over a command that
    // finished: its mark alone, the failure's.
    notifier.front(Some(&front.id));
    let posts = posts_until(within, |posts| {
        posts.iter().filter_map(shown).any(|(c, _)| c.only_mark)
    });
    let (mark, go) = posts
        .iter()
        .filter_map(shown)
        .find(|(c, _)| c.only_mark)
        .unwrap_or_else(|| panic!("no mark: {posts:?}"));
    assert_eq!(
        (mark.title.as_str(), mark.kind, mark.mark),
        ("other", card::Kind::Failed, Some(card::Kind::Failed))
    );
    assert_eq!(mark.body.as_deref(), Some("sh: exited with code 2"));
    assert_eq!(
        go,
        Some(Go::Session(nvmux::ids::Id::new(&other.id).expect("id")))
    );

    // Seen: the mark is let go of.
    notes::seen(&other_sock);
    let forget = |p: &Post| p.what == Posted::Forget(other.id.clone());
    let posts = posts_until(within, |posts| posts.iter().any(forget));
    assert!(
        posts.iter().any(forget),
        "the mark was never let go of: {posts:?}"
    );
}
