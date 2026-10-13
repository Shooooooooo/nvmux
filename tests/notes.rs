//! The watcher a session keeps its notes in ([`nvmux::notes`]), against real
//! `nvim --headless` sessions: what it records of progress messages, of what
//! language servers say they are doing and of what programs in its terminal
//! buffers send, what a read makes of that, and that clearing it and a newer
//! version of it each do what they say — and the picker's reader, which does
//! all of it on a thread.
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

/// A job in a hidden terminal buffer that runs `script` and exits, waited for
/// until its `TermClose` has been, so that the watcher has seen it go.
fn a_job_that_exits(sock: &Path, script: &str) {
    lua(
        sock,
        &format!(
            r#"local closed = false
local buf = vim.api.nvim_create_buf(true, false)
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

/// A language server at work is busy, by its own name, from its `begin` to
/// its `end` — or until it goes, since one that dies mid-work never says it
/// has finished. The server is a stand-in in the session itself, as Neovim's
/// own tests have one, saying what clangd says while it indexes.
#[test]
fn a_language_server_is_busy_until_its_work_ends_or_it_goes() {
    require_nvim!();
    let scratch = Scratch::new("notes-lsp");
    let (_, sock) = a_session(&scratch, "indexing");
    assert_eq!(notes::read(&sock), Reading::Said(None));

    lua(
        &sock,
        r#"local server = {}
_G.server = server
vim.lsp.start({
  name = 'clangd',
  cmd = function(dispatchers)
    server.dispatchers = dispatchers
    local closing = false
    return {
      request = function(method, _, callback)
        if method == 'initialize' then
          callback(nil, { capabilities = {} })
        elseif method == 'shutdown' then
          callback(nil, vim.NIL)
        end
        return true, 1
      end,
      notify = function(method)
        if method == 'exit' then
          dispatchers.on_exit(0, 15)
        end
      end,
      is_closing = function()
        return closing
      end,
      terminate = function()
        closing = true
      end,
    }
  end,
})
function server.progress(value)
  server.dispatchers.notification('$/progress', { token = 'backgroundIndexProgress', value = value })
end"#,
    );
    let progress = |value: &str| lua(&sock, &format!("_G.server.progress({value})"));

    progress("{ kind = 'begin', title = 'indexing', percentage = 0 }");
    let begun = read_until(&sock, |n| n.is_some()).expect("busy");
    assert_eq!(begun.kind, Kind::Busy(None), "0% is no percentage yet");
    assert_eq!(
        (begun.title.as_str(), begun.text.as_str()),
        ("clangd", "indexing")
    );

    progress("{ kind = 'report', message = '3/120', percentage = 2 }");
    let report = read_until(&sock, |n| n.is_some_and(|n| n.kind == Kind::Busy(Some(2))))
        .expect("the report");
    assert_eq!(report.text, "indexing 3/120");

    progress("{ kind = 'report', percentage = 50 }");
    let halfway =
        read_until(&sock, |n| n.is_some_and(|n| n.kind == Kind::Busy(Some(50)))).expect("halfway");
    assert_eq!(
        halfway.text, "indexing 3/120",
        "what a report leaves out stands"
    );

    progress("{ kind = 'end', message = 'done' }");
    assert_eq!(
        read_until(&sock, |n| n.is_none()),
        None,
        "done when it ends"
    );

    progress("{ kind = 'begin', title = 'indexing' }");
    read_until(&sock, |n| n.is_some()).expect("busy again");
    lua(&sock, "vim.lsp.get_clients({ name = 'clangd' })[1]:stop()");
    assert_eq!(
        read_until(&sock, |n| n.is_none()),
        None,
        "not busy once the server has gone"
    );
}

/// Neovim's own progress messages are nothing running: a write's begins and
/// ends within `:write`, and one that fails never says it has ended, which
/// would leave the session busy for good. What an older watcher kept of one
/// goes on the way in.
#[test]
fn neovims_own_progress_messages_are_never_busy() {
    require_nvim!();
    let scratch = Scratch::new("notes-own");
    let (_, sock) = a_session(&scratch, "writing");
    assert_eq!(notes::read(&sock), Reading::Said(None));
    if !has_progress(&sock) {
        return;
    }

    let wrote = lua(
        &sock,
        r"vim.cmd('noswapfile edit /nonexistent-nvmux-dir/f.txt')
vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'x' })
local ok = pcall(vim.cmd.write)
return ok",
    );
    assert_eq!(wrote, Value::Boolean(false), "the write failed");
    assert_eq!(notes::read(&sock), Reading::Said(None), "and is not busy");

    lua(
        &sock,
        r"local N = require('nvmux.notes')
N.progress['nvim\0bufwrite'] = { title = '', text = 'f.txt', at = os.time() }
N.version = -1",
    );
    assert_eq!(
        notes::read(&sock),
        Reading::Said(None),
        "an older watcher's, dropped"
    );
}

/// A command in a terminal buffer that fails is kept like a failed progress
/// message: one its shell says has (OSC 133, the way fish says it, naming the
/// command line as it starts), and a job that exits with a code other than 0.
/// One that succeeds is nothing.
#[test]
fn a_command_that_fails_in_a_terminal_is_kept() {
    require_nvim!();
    let scratch = Scratch::new("notes-term");
    let (_, sock) = a_session(&scratch, "building");
    assert_eq!(notes::read(&sock), Reading::Said(None));

    in_a_terminal(
        &sock,
        r"\033]133;C;cmdline_url=cargo%%20build\033\134\033]133;D;101\033\134",
    );
    let failed = read_until(&sock, |n| n.is_some()).expect("the failure");
    assert_eq!(failed.kind, Kind::Failed);
    assert_eq!(
        (failed.title.as_str(), failed.text.as_str()),
        ("cargo build", "exited with code 101")
    );

    // A shell that does not name the command line: the terminal's program.
    notes::seen(&sock);
    in_a_terminal(&sock, r"\033]133;D;2\033\134");
    let unnamed = read_until(&sock, |n| n.is_some()).expect("the unnamed failure");
    assert_eq!(
        (unnamed.title.as_str(), unnamed.text.as_str()),
        ("sh", "a command exited with code 2")
    );

    // A command that succeeds, after a notification it would otherwise
    // have taken the place of.
    notes::seen(&sock);
    in_a_terminal(
        &sock,
        r"\033]777;notify;Build;started\007\033]133;C;cmdline_url=ls\033\134\033]133;D;0\033\134",
    );
    read_until(&sock, |n| n.is_some()).expect("the notification");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        read_until(&sock, |n| n.is_some()).map(|n| n.kind),
        Some(Kind::Notified),
        "a command that succeeds is no failure"
    );

    notes::seen(&sock);
    a_job_that_exits(&sock, "exit 3");
    let exited = read_until(&sock, |n| n.is_some()).expect("the job's failure");
    assert_eq!(exited.kind, Kind::Failed);
    assert_eq!(
        (exited.title.as_str(), exited.text.as_str()),
        ("sh", "exited with code 3")
    );
    notes::seen(&sock);
    a_job_that_exits(&sock, "exit 0");
    assert_eq!(
        notes::read(&sock),
        Reading::Said(None),
        "a job that exits with 0 is no failure"
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
/// what it recorded.
#[test]
fn a_watcher_is_replaced_in_place() {
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
        Some(if has_progress(&sock) { 4 } else { 3 }),
        "the autocommands replaced, not added to"
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
/// the one it was opened from cleared first.
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

    let poller = Poller::start(Some(left.id.clone()));
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
}
