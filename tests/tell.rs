//! What nvmux tells the editor in front ([`nvmux::tell`]), against real
//! `nvim --headless` sessions: that a message is said the way the editor says
//! one — into `:messages`, an error included, cut to the room the row has, and
//! held for a UI where there is none yet — that an editor waiting for a key is
//! asked nothing; that the watcher numbers what it keeps and knows whether its
//! terminal was on the screen; and that the notifier tells the session in front
//! what the others say as it happens, and of its own only what a terminal off
//! the screen says.
//!
//! They skip, rather than fail, when there is no usable `nvim` on `$PATH` —
//! unless `$NVMUX_TEST_REQUIRE` names `nvim`, as it does in CI.

#[macro_use]
mod common;

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{wait_until, Scratch};
use nvmux::notes::{self, Heard, Kind};
use nvmux::rpc::Client;
use nvmux::tell::{self, Level, Notifier};
use nvmux::transport::Transport;
use rmpv::Value;
use unicode_width::UnicodeWidthStr;

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

/// The session's `:messages`.
fn messages(sock: &Path) -> String {
    lua(
        sock,
        "return vim.api.nvim_exec2('messages', { output = true }).output",
    )
    .as_str()
    .unwrap_or_default()
    .to_string()
}

/// A UI on the session, for as long as the client is held: what a client is
/// to the editor, without the drawing. It never reads what it is sent, which
/// for a test's few redraws is nobody's concern.
fn a_ui(sock: &Path) -> Client<UnixStream> {
    let mut client = Client::connect(sock, Duration::from_secs(5)).expect("connect");
    client
        .call(
            "nvim_ui_attach",
            vec![Value::from(80), Value::from(24), Value::Map(Vec::new())],
        )
        .expect("nvim_ui_attach");
    client
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
/// has been, so that the watcher has seen it go.
fn a_job_that_exits(sock: &Path, script: &str, on_screen: bool) {
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
end)"#
        ),
    );
}

fn a_session(scratch: &Scratch, name: &str) -> (nvmux::session::Session, PathBuf) {
    let t = scratch.transport();
    let s = t
        .create_session(name, &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&s).expect("socket");
    (s, sock)
}

/// Said in the editor and kept in its history, an error as well as the rest;
/// and, too long for the row, cut to the room there is rather than left to
/// stop the editor at a hit-enter prompt.
#[test]
fn a_message_is_said_the_way_the_editor_says_one() {
    require_nvim!();
    let scratch = Scratch::new("tell-said");
    let (_, sock) = a_session(&scratch, "said");
    let _ui = a_ui(&sock);

    let finished = "✓ api-server · cargo test: exited with code 0";
    assert!(tell::say(&sock, finished, Level::Info).expect("said"));
    assert!(
        wait_until(Duration::from_secs(5), || messages(&sock)
            .contains(finished)),
        "{}",
        messages(&sock)
    );

    let failed = "! api-server · cargo build: exited with code 101";
    assert!(tell::say(&sock, failed, Level::Error).expect("said"));
    assert!(
        wait_until(Duration::from_secs(5), || messages(&sock).contains(failed)),
        "an error is said, not raised: {}",
        messages(&sock)
    );

    let long = format!(
        "! api-server · {}",
        "cargo build --release --all-targets: exited with code 101 ".repeat(4)
    );
    assert!(tell::say(&sock, &long, Level::Error).expect("said"));
    assert!(
        wait_until(Duration::from_secs(5), || messages(&sock).ends_with('…')),
        "{}",
        messages(&sock)
    );
    let said = messages(&sock);
    let last = said.lines().last().unwrap_or_default();
    let room = lua(&sock, "return vim.v.echospace").as_u64().expect("room");
    assert!(
        last.width() as u64 <= room,
        "{last:?} is wider than {room} columns"
    );
    assert_eq!(
        common::mode(&sock).map(|m| m.blocking),
        Some(false),
        "a message too long for its row stopped the editor"
    );
}

/// A session with no UI yet — a client just started — holds what it is told
/// until one attaches: given before that, it would not be on the screen.
#[test]
fn a_message_waits_for_a_ui_to_say_it_to() {
    require_nvim!();
    let scratch = Scratch::new("tell-ui");
    let (_, sock) = a_session(&scratch, "waiting");

    assert!(tell::say(&sock, "api-server", Level::Info).expect("asked"));
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !messages(&sock).contains("api-server"),
        "said with nobody to see it"
    );
    let _ui = a_ui(&sock);
    assert!(
        wait_until(Duration::from_secs(5), || messages(&sock)
            .contains("api-server")),
        "{}",
        messages(&sock)
    );
}

/// An editor waiting for a key is asked nothing: a message left with it would
/// be said once the key came, long after its moment.
#[test]
fn an_editor_waiting_for_a_key_is_asked_nothing() {
    require_nvim!();
    let scratch = Scratch::new("tell-wedged");
    let (_, sock) = a_session(&scratch, "wedged");
    let _ui = a_ui(&sock);
    common::block_on_a_key(&sock);

    assert!(!tell::say(&sock, "too soon", Level::Info).expect("asked"));

    Client::connect(&sock, Duration::from_secs(2))
        .expect("connect")
        .input("<Esc>")
        .expect("press a key");
    assert!(
        wait_until(Duration::from_secs(5), || common::mode(&sock)
            .is_some_and(|m| !m.blocking)),
        "the key never ended the wait"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(!messages(&sock).contains("too soon"), "{}", messages(&sock));
}

/// A careful read, of an editor waiting for a key, says nothing and asks
/// nothing; and what the watcher keeps is numbered as it comes, and says
/// whether its terminal was on the screen.
#[test]
fn the_watcher_numbers_what_it_keeps_and_knows_what_was_on_the_screen() {
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

    a_job_that_exits(&sock, "exit 3", false);
    a_job_that_exits(&sock, "exit 0", true);
    let Heard::Said(news) = notes::hear(&sock) else {
        panic!("the session said nothing");
    };
    assert_eq!(
        news.iter().map(|n| (n.kind, n.shown)).collect::<Vec<_>>(),
        [(Kind::Failed, Some(false)), (Kind::Finished, Some(true))],
        "{news:?}"
    );
    assert!(news[0].seq < news[1].seq, "{news:?}");
}

/// The session in front is told what another session's terminals say as it
/// happens — a command that finished, a notification — by that session's
/// name; of its own terminals, only what one off the screen says, without
/// the name; and nothing while nvmux's own screens are up.
#[test]
fn the_notifier_tells_the_session_in_front_what_the_others_say() {
    require_nvim!();
    let scratch = Scratch::new("tell-notifier");
    let (front, front_sock) = a_session(&scratch, "front");
    let (other, other_sock) = a_session(&scratch, "other");
    let _ui = a_ui(&front_sock);

    let notifier = Notifier::start();
    notifier.watch(&front.id, "front", front_sock.clone());
    notifier.watch(&other.id, "other", other_sock.clone());
    notifier.front(Some(&front.id));
    // The first round only finds out how far each session has got.
    std::thread::sleep(Duration::from_millis(800));

    let told = |what: &str| {
        let what = what.to_string();
        let sock = front_sock.clone();
        move || messages(&sock).contains(&what)
    };
    let within = Duration::from_secs(8);

    a_job_that_exits(&other_sock, "exit 0", false);
    assert!(
        wait_until(within, told("✓ other · sh: exited with code 0")),
        "{}",
        messages(&front_sock)
    );
    in_a_terminal(&other_sock, r"\033]777;notify;Build;finished\007");
    assert!(
        wait_until(within, told("∗ other · Build · finished")),
        "{}",
        messages(&front_sock)
    );

    a_job_that_exits(&front_sock, "exit 3", false);
    assert!(
        wait_until(within, told("! sh: exited with code 3")),
        "a terminal of its own off the screen: {}",
        messages(&front_sock)
    );
    a_job_that_exits(&front_sock, "exit 0", true);
    std::thread::sleep(notes::POLL + Duration::from_millis(800));
    assert!(
        !told("✓ sh: exited with code 0")(),
        "told of a terminal it was showing: {}",
        messages(&front_sock)
    );

    notifier.front(None);
    std::thread::sleep(Duration::from_millis(100));
    a_job_that_exits(&other_sock, "exit 2", false);
    std::thread::sleep(notes::POLL + Duration::from_millis(800));
    assert!(
        !told("other · sh: exited with code 2")(),
        "told with nobody in front: {}",
        messages(&front_sock)
    );
}
