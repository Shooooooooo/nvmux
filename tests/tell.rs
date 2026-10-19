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

use std::time::{Duration, Instant};

use common::{
    a_job_that_exits, a_session, in_a_terminal, lua, posts_until, shown, wait_until, Scratch,
};
use nvmux::card::{self, Go, Post, Posted};
use nvmux::notes::{self, Heard, Kind};
use nvmux::rpc::Client;
use nvmux::tell::Notifier;

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
        notes::hear(&sock, None),
        Heard::Said {
            news: Vec::new(),
            listening: false
        },
        "given the watcher"
    );

    // First, while the current window still holds a buffer of its own: in a
    // terminal's, the editor's mode is `nt`, not the `n` the wait looks for.
    common::block_on_a_key(&sock);
    assert_eq!(notes::hear(&sock, None), Heard::Unknown);
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
    let Heard::Said { news, .. } = notes::hear(&sock, None) else {
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
    let Heard::Said { news, .. } = notes::hear(&sock, None) else {
        panic!("the session said nothing");
    };
    assert_eq!(news[0].buf, None, "a buffer that is gone is none: {news:?}");
}

/// A listener on a session hears from its watcher once a read has named it,
/// and not before: each time news comes, and when what was kept is seen —
/// but not a `seen` with nothing to clear. A read says whether the listener
/// was being told already. Stopped, it goes, and the watcher lets go of its
/// channel the next time it has news.
#[test]
fn a_listener_hears_the_news_once_a_read_names_it() {
    require_nvim!();
    let scratch = Scratch::new("tell-listen");
    let (_, sock) = a_session(&scratch, "listened");
    let (heard, hearing) = std::sync::mpsc::channel();
    let listening = std::thread::spawn({
        let sock = sock.clone();
        move || notes::listen(&sock, |pushed| heard.send(pushed).is_ok())
    });
    let within = Duration::from_secs(5);
    let (chan, stop) = match hearing.recv_timeout(within) {
        Ok(notes::Pushed::Listening { chan, stop }) => (chan, stop),
        other => panic!("not listening: {other:?}"),
    };
    let listening_by = |read: Heard| match read {
        Heard::Said { listening, .. } => listening,
        other => panic!("the session said nothing: {other:?}"),
    };
    let nothing_more = |what: &str| {
        let more = hearing.recv_timeout(Duration::from_millis(300));
        assert!(more.is_err(), "{what}: {more:?}");
    };

    // A read that names nobody gives the session the watcher, which tells
    // nobody; nor does news before a read has named the listener.
    assert!(!listening_by(notes::hear(&sock, None)));
    a_job_that_exits(&sock, "exit 1", false);
    nothing_more("told before it was named");
    assert!(
        !listening_by(notes::hear(&sock, Some(chan))),
        "named by this read, not before it"
    );
    assert!(listening_by(notes::hear(&sock, Some(chan))));
    assert!(
        !listening_by(notes::hear(&sock, None)),
        "a read naming nobody"
    );

    let started = Instant::now();
    a_job_that_exits(&sock, "exit 3", false);
    assert!(
        matches!(hearing.recv_timeout(within), Ok(notes::Pushed::News)),
        "news not told"
    );
    let took = started.elapsed();
    notes::seen(&sock);
    assert!(
        matches!(hearing.recv_timeout(within), Ok(notes::Pushed::News)),
        "what was kept was seen, and not told"
    );
    notes::seen(&sock);
    nothing_more("told of a seen with nothing to clear");
    eprintln!("told {took:?} after the job was started");

    // A watcher put in afresh by a read that names the listener tells it,
    // though the read does not say it was being told.
    lua(&sock, "package.loaded['nvmux.notes'] = nil");
    assert!(!listening_by(notes::hear(&sock, Some(chan))));
    a_job_that_exits(&sock, "exit 5", false);
    assert!(
        matches!(hearing.recv_timeout(within), Ok(notes::Pushed::News)),
        "a watcher put in afresh did not tell the listener it was given"
    );

    stop.fire();
    let ended = listening.join().expect("the listener panicked");
    assert!(ended.is_err(), "stopped, it says why: {ended:?}");
    // Told no more: the watcher found the channel gone and let it go.
    a_job_that_exits(&sock, "exit 4", false);
    assert!(
        !listening_by(notes::hear(&sock, Some(chan))),
        "the watcher kept a channel that had gone"
    );
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
