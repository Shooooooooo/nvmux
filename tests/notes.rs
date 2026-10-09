//! The watcher a session keeps its notes in ([`nvmux::notes`]), against real
//! `nvim --headless` sessions: what it records of progress messages and of
//! what programs in its terminal buffers send, what a read makes of that, and
//! that clearing it, a newer version of it and taking it out each do what they
//! say — and the picker's reader, which does all of it on a thread.
//!
//! They skip, rather than fail, when there is no usable `nvim` on `$PATH` —
//! unless `$NVMUX_TEST_REQUIRE` names `nvim`, as it does in CI.

#[macro_use]
mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::Scratch;
use nvmux::notes::{self, Kind, Note, Poller, Reading};
use nvmux::transport::Transport;
use rmpv::Value;

/// Run Lua in the session, as a test's own stand-in for whatever a plugin or
/// a terminal program would have done there.
fn lua(sock: &Path, code: &str) -> Value {
    let mut client = nvmux::rpc::Client::connect(sock, Duration::from_secs(5)).expect("connect");
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

/// Read until the note is what `want` says, or a few seconds have gone.
fn read_until(sock: &Path, want: impl Fn(Option<&Note>) -> bool) -> Option<Note> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Reading::Said(note) = notes::read(sock) {
            if want(note.as_ref()) || Instant::now() > deadline {
                return note;
            }
        } else if Instant::now() > deadline {
            panic!("the session never answered");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn has_watcher(sock: &Path) -> bool {
    lua(
        sock,
        "return pcall(vim.api.nvim_get_autocmds, { group = 'nvmux_notes' })",
    ) == Value::Boolean(true)
}

/// Whether the session's Neovim has progress messages at all (0.12 on).
fn has_progress(sock: &Path) -> bool {
    lua(sock, "return vim.fn.has('nvim-0.12') == 1") == Value::Boolean(true)
}

fn a_session(scratch: &Scratch, name: &str) -> (nvmux::session::Session, std::path::PathBuf) {
    let t = scratch.transport();
    let s = t
        .create_session(name, &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&s).expect("socket");
    (s, sock)
}

#[test]
fn a_session_says_what_runs_in_it_and_what_its_terminals_ask_for() {
    require_nvim!();
    let scratch = Scratch::new("notes-said");
    let (_, sock) = a_session(&scratch, "noted");

    // The first read is what gives the session its watcher, and there is
    // nothing to say yet.
    assert!(!has_watcher(&sock));
    assert_eq!(notes::read(&sock), Reading::Said(None));
    assert!(has_watcher(&sock));

    if has_progress(&sock) {
        lua(
            &sock,
            "_G.p = vim.api.nvim_echo({ { 'working' } }, false, { kind = 'progress', \
             source = 'test', title = 'claude', status = 'running' })",
        );
        let busy = read_until(&sock, |n| n.is_some()).expect("busy");
        assert_eq!(busy.kind, Kind::Busy(None));
        assert_eq!(
            (busy.title.as_str(), busy.text.as_str()),
            ("claude", "working")
        );

        lua(
            &sock,
            "vim.api.nvim_echo({ { 'indexing' } }, false, { kind = 'progress', \
             source = 'lsp', title = 'lua_ls', status = 'running', percent = 42 })",
        );
        // The progress message that started last is the one shown — unless
        // both started in the same second, when either may be.
        let latest = read_until(&sock, |n| n.is_some_and(|n| n.kind == Kind::Busy(Some(42))));
        assert!(latest.is_some());

        lua(
            &sock,
            "vim.api.nvim_echo({ { 'exited with code 1' } }, false, { kind = 'progress', \
             source = 'test', title = 'claude', status = 'failed', id = _G.p })",
        );
        let failed =
            read_until(&sock, |n| n.is_some_and(|n| n.kind == Kind::Failed)).expect("the failure");
        assert_eq!(failed.text, "exited with code 1");

        notes::seen(&sock);
        let after = read_until(&sock, |n| n.is_some_and(|n| n.kind != Kind::Failed));
        assert_eq!(
            after.map(|n| n.kind),
            Some(Kind::Busy(Some(42))),
            "seen clears the failure; what still runs stays"
        );
    }

    // What an agent sends in Ghostty, from a terminal no window shows.
    in_a_terminal(
        &sock,
        r"\033]777;notify;Claude Code;Claude needs your permission\007",
    );
    let notified = read_until(&sock, |n| n.is_some_and(|n| n.kind == Kind::Notified))
        .expect("the notification");
    assert_eq!(notified.title, "Claude Code");
    assert_eq!(notified.text, "Claude needs your permission");

    // iTerm2's, and kitty's — in Claude Code's three parts, ST-terminated.
    notes::seen(&sock);
    in_a_terminal(&sock, r"\033]9;Task done\007");
    let iterm =
        read_until(&sock, |n| n.is_some_and(|n| n.kind == Kind::Notified)).expect("iTerm2's");
    assert_eq!(iterm.text, "Task done");
    notes::seen(&sock);
    in_a_terminal(
        &sock,
        r"\033]99;i=1:d=0:p=title;Claude Code\033\134\033]99;i=1:p=body;Claude is waiting for your input\033\134\033]99;i=1:d=1:a=focus;\033\134",
    );
    let kitty =
        read_until(&sock, |n| n.is_some_and(|n| n.kind == Kind::Notified)).expect("kitty's");
    assert_eq!(
        (kitty.title.as_str(), kitty.text.as_str()),
        ("Claude Code", "Claude is waiting for your input")
    );
}

/// A program's own progress bar, and ConEmu's other commands under OSC 9, are
/// not notifications.
#[test]
fn a_programs_own_progress_bar_is_not_a_note() {
    require_nvim!();
    let scratch = Scratch::new("notes-osc94");
    let (_, sock) = a_session(&scratch, "bar");
    assert_eq!(notes::read(&sock), Reading::Said(None));
    in_a_terminal(
        &sock,
        r"\033]9;4;3;\007\033]9;9;/tmp\007\033]777;notify;x;then this\007",
    );
    let n = read_until(&sock, |n| n.is_some()).expect("the notification after them");
    assert_eq!(n.text, "then this", "nothing before it was taken for one");
}

/// A session holding another nvmux's watcher is given this one's, and keeps
/// what it recorded; taken out, it is gone, record and all.
#[test]
fn a_watcher_is_replaced_in_place_and_taken_out_whole() {
    require_nvim!();
    let scratch = Scratch::new("notes-version");
    let (_, sock) = a_session(&scratch, "versioned");
    assert_eq!(notes::read(&sock), Reading::Said(None));
    in_a_terminal(&sock, r"\033]777;notify;Build;finished\007");
    read_until(&sock, |n| n.is_some()).expect("recorded");

    lua(&sock, "require('nvmux.notes').version = -1");
    let kept = read_until(&sock, |n| n.is_some()).expect("kept across the new version");
    assert_eq!(kept.text, "finished");
    assert_eq!(
        lua(&sock, "return require('nvmux.notes').version"),
        Value::from(notes::VERSION)
    );
    assert_eq!(
        lua(
            &sock,
            "return #vim.api.nvim_get_autocmds({ group = 'nvmux_notes' })"
        )
        .as_u64(),
        Some(if has_progress(&sock) { 2 } else { 1 }),
        "the autocommands replaced, not added to"
    );

    notes::remove(&sock);
    assert!(!has_watcher(&sock));
    assert_eq!(
        lua(&sock, "return package.loaded['nvmux.notes'] == nil"),
        Value::Boolean(true)
    );
}

/// A session coming to the front is given the watcher if it has none, and
/// what it kept is cleared, being looked at.
#[test]
fn a_session_brought_to_the_front_is_watched_from_then_on() {
    require_nvim!();
    let scratch = Scratch::new("notes-touch");
    let (_, sock) = a_session(&scratch, "touched");
    notes::touch(&sock);
    assert!(has_watcher(&sock), "given the watcher");
    in_a_terminal(&sock, r"\033]777;notify;Build;finished\007");
    read_until(&sock, |n| n.is_some()).expect("recorded");
    notes::touch(&sock);
    assert_eq!(
        notes::read(&sock),
        Reading::Said(None),
        "cleared on arrival"
    );
}

/// What the picker reads with: every session handed over is read at once,
/// the one it was opened from cleared first — and with the notes off, the
/// watcher is taken out instead and nothing comes back.
#[test]
fn the_pickers_reader_reads_each_session_and_clears_the_one_just_left() {
    require_nvim!();
    let scratch = Scratch::new("notes-poller");
    let (left, left_sock) = a_session(&scratch, "left");
    let (other, other_sock) = a_session(&scratch, "other");
    for sock in [&left_sock, &other_sock] {
        assert_eq!(notes::read(sock), Reading::Said(None));
        in_a_terminal(
            sock,
            r"\033]777;notify;Claude Code;Claude needs your permission\007",
        );
        read_until(sock, |n| n.is_some()).expect("recorded");
    }

    let poller = Poller::start(nvmux::config::Notes::Signs, Some(left.id.clone()));
    poller.watch(&left.id, left_sock.clone());
    poller.watch(&other.id, other_sock.clone());
    let deadline = Instant::now() + Duration::from_secs(5);
    let notes = loop {
        if let Some(n) = poller.take() {
            break n;
        }
        assert!(Instant::now() < deadline, "the reader said nothing");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        !notes.contains_key(&left.id),
        "the session just left was cleared"
    );
    assert_eq!(
        notes.get(&other.id).map(|n| n.kind),
        Some(Kind::Notified),
        "{notes:?}"
    );
    drop(poller);

    let off = Poller::start(nvmux::config::Notes::Off, None);
    off.watch(&other.id, other_sock.clone());
    let deadline = Instant::now() + Duration::from_secs(5);
    while has_watcher(&other_sock) {
        assert!(Instant::now() < deadline, "the watcher was never taken out");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(off.take(), None, "nothing read with the notes off");
}
