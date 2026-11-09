//! The session notes watcher ([`nvmux::notes`]) against real `nvim
//! --headless` sessions: progress messages, LSP progress and terminal
//! notifications, clearing, version upgrades, and the picker's reader thread.
//!
//! Skipped without a usable `nvim` unless `$NVMUX_TEST_REQUIRE` names `nvim`.

#[macro_use]
mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::Scratch;
use nvmux::notes::{self, Heard, Kind, Note, Poller, Reading};
use nvmux::transport::Transport;
use rmpv::Value;

fn lua(sock: &Path, code: &str) -> Value {
    lua_within(sock, code, Duration::from_secs(5))
}

fn lua_within(sock: &Path, code: &str, budget: Duration) -> Value {
    let mut client = nvmux::rpc::Client::connect(sock, budget).expect("connect");
    client
        .call(
            "nvim_exec_lua",
            vec![Value::from(code), Value::Array(Vec::new())],
        )
        .expect("nvim_exec_lua")
}

/// Run `printf` in a hidden terminal buffer and keep the program alive, like
/// an agent waiting at a prompt.
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

/// Run `script` in a hidden terminal buffer and wait for its `TermClose`, so
/// the watcher has seen it exit.
fn a_job_that_exits(sock: &Path, script: &str) {
    // The RPC budget exceeds the Lua wait, so a slow close is waited out in
    // Neovim and one that never closes raises a clear error.
    lua_within(
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
if not vim.wait(10000, function()
  return closed
end) then
  error('the job never closed')
end"#
        ),
        Duration::from_secs(15),
    );
}

/// Run `script` in a hidden terminal buffer; returns its channel, for
/// [`enter`].
fn a_terminal(sock: &Path, script: &str) -> u64 {
    lua(
        sock,
        &format!(
            r#"local buf = vim.api.nvim_create_buf(true, false)
vim.api.nvim_buf_call(buf, function()
  vim.fn.jobstart({{ 'sh', '-c', [=[{script}]=] }}, {{ term = true }})
end)
return vim.bo[buf].channel"#
        ),
    )
    .as_u64()
    .expect("its channel")
}

/// Press Enter in the terminal on `chan`, ending its program's `read`.
fn enter(sock: &Path, chan: u64) {
    lua(sock, &format!("vim.api.nvim_chan_send({chan}, '\\r')"));
}

/// Terminals with something in the watcher's table `name`: `parts`, their
/// unfinished kitty notifications, or `commands`, their shells' commands.
/// Nothing else shows them.
fn held(sock: &Path, name: &str) -> u64 {
    lua(
        sock,
        &format!(
            r"local f = vim.api.nvim_get_autocmds({{ group = 'nvmux_notes', event = 'TermRequest' }})[1].callback
for i = 1, math.huge do
  local name, value = debug.getupvalue(f, i)
  if name == nil or name == '{name}' then
    return value and vim.tbl_count(value)
  end
end"
        ),
    )
    .as_u64()
    .expect("the watcher's table")
}

/// The session's Lua memory in KB, after a full collection.
fn lua_memory(sock: &Path) -> f64 {
    lua(
        sock,
        "collectgarbage('collect') collectgarbage('collect') return collectgarbage('count')",
    )
    .as_f64()
    .expect("a size")
}

/// Read until `want` holds, or 5 s pass.
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

/// Progress messages need Neovim 0.12.
fn has_progress(sock: &Path) -> bool {
    lua(sock, "return vim.fn.has('nvim-0.12') == 1") == Value::Boolean(true)
}

fn a_session(scratch: &Scratch, name: &str) -> (nvmux::session::Session, std::path::PathBuf) {
    let t = scratch.transport();
    let s = t
        .create_session(name, &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&s).expect("socket");
    // Wait out startup: the notes' reads have a 1 s budget, and Neovim only
    // answers main-loop calls once started, which is slow on a busy machine.
    lua(&sock, "return 1");
    (s, sock)
}

#[test]
fn a_session_says_what_runs_in_it_and_what_its_terminals_ask_for() {
    require_nvim!();
    let scratch = Scratch::new("notes-said");
    let (_, sock) = a_session(&scratch, "noted");

    // The first read installs the watcher.
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
        // The last-started message is shown, or either if both started in the
        // same second.
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

    // OSC 777, as an agent sends in Ghostty.
    in_a_terminal(
        &sock,
        r"\033]777;notify;Claude Code;Claude needs your permission\007",
    );
    let notified = read_until(&sock, |n| n.is_some_and(|n| n.kind == Kind::Notified))
        .expect("the notification");
    assert_eq!(notified.title, "Claude Code");
    assert_eq!(notified.text, "Claude needs your permission");

    // OSC 9 (iTerm2), and OSC 99 (kitty) in Claude Code's three
    // ST-terminated parts.
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

/// A server that dies mid-work never sends `end`, so exiting must clear it
/// too. The server is an in-process stand-in sending what clangd sends while
/// indexing.
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

/// A write's progress begins and ends within `:write`, and a failed one never
/// ends, which would leave the session busy forever. Neovim's own entries kept
/// by an older watcher are dropped on upgrade.
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

/// A failure is an OSC 133 `D` with a non-zero code (as fish sends it, with
/// the command line in `C`) or a job exiting non-zero.
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

    // Without a command line, the title is the terminal's program.
    notes::seen(&sock);
    in_a_terminal(&sock, r"\033]133;D;2\033\134");
    let unnamed = read_until(&sock, |n| n.is_some()).expect("the unnamed failure");
    assert_eq!(
        (unnamed.title.as_str(), unnamed.text.as_str()),
        ("sh", "a command exited with code 2")
    );

    // A success must not replace the notification before it.
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
}

/// A success (OSC 133 `D;0` after a `C`, or a job exiting 0) is kept until
/// seen but never replaces a notification or failure. A plugin-drawn terminal
/// also closes with 0, and one whose buffer is deleted closes with -1;
/// neither counts.
#[test]
fn a_command_that_finishes_in_a_terminal_is_kept() {
    require_nvim!();
    let scratch = Scratch::new("notes-finished");
    let (_, sock) = a_session(&scratch, "testing");
    assert_eq!(notes::read(&sock), Reading::Said(None));

    in_a_terminal(
        &sock,
        r"\033]133;C;cmdline_url=cargo%%20test\033\134\033]133;D;0\033\134",
    );
    let named = read_until(&sock, |n| n.is_some()).expect("the finish");
    assert_eq!(named.kind, Kind::Finished);
    assert_eq!(
        (named.title.as_str(), named.text.as_str()),
        ("cargo test", "exited with code 0")
    );

    // Without a command line, the title is the terminal's program.
    notes::seen(&sock);
    in_a_terminal(&sock, r"\033]133;C\033\134\033]133;D;0\033\134");
    let unnamed = read_until(&sock, |n| n.is_some()).expect("the unnamed finish");
    assert_eq!(
        (unnamed.title.as_str(), unnamed.text.as_str()),
        ("sh", "a command exited with code 0")
    );

    // A `D;0` with no `C` since the last one (first prompt, or after an
    // empty line) is not a finish.
    notes::seen(&sock);
    in_a_terminal(
        &sock,
        r"\033]133;C;cmdline_url=ls\033\134\033]133;D;0\033\134\033]133;D;0\033\134",
    );
    read_until(&sock, |n| n.is_some()).expect("the command before it");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        read_until(&sock, |n| n.is_some()).map(|n| n.title),
        Some("ls".to_string()),
        "a prompt where no command started"
    );

    notes::seen(&sock);
    a_job_that_exits(&sock, "exit 0");
    let finished = read_until(&sock, |n| n.is_some()).expect("the finish");
    assert_eq!(finished.kind, Kind::Finished);
    assert_eq!(
        (finished.title.as_str(), finished.text.as_str()),
        ("sh", "exited with code 0")
    );

    in_a_terminal(
        &sock,
        r"\033]777;notify;Claude Code;Claude needs your permission\007",
    );
    read_until(&sock, |n| n.is_some_and(|n| n.kind == Kind::Notified)).expect("the notification");
    a_job_that_exits(&sock, "exit 0");
    assert_eq!(
        read_until(&sock, |n| n.is_some()).map(|n| n.kind),
        Some(Kind::Notified),
        "a finish never takes the place of a notification"
    );
    notes::seen(&sock);
    assert_eq!(notes::read(&sock), Reading::Said(None), "seen, both go");

    lua(
        &sock,
        r"local closed = false
local buf = vim.api.nvim_create_buf(true, false)
vim.api.nvim_create_autocmd('TermClose', {
  buffer = buf,
  once = true,
  callback = function()
    closed = true
  end,
})
local chan = vim.api.nvim_open_term(buf, {})
vim.api.nvim_chan_send(chan, 'drawn by a plugin')
vim.fn.chanclose(chan)
vim.wait(5000, function()
  return closed
end)",
    );
    assert_eq!(
        notes::read(&sock),
        Reading::Said(None),
        "a terminal drawn into, not run in"
    );

    lua(
        &sock,
        r"local buf = vim.api.nvim_create_buf(true, false)
vim.api.nvim_buf_call(buf, function()
  vim.fn.jobstart({ 'sh', '-c', 'sleep 30' }, { term = true })
end)
vim.api.nvim_buf_delete(buf, { force = true })",
    );
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        notes::read(&sock),
        Reading::Said(None),
        "a terminal whose buffer went while its program ran"
    );
}

/// ConEmu's OSC 9 subcommands (e.g. `9;4` progress bars) are not
/// notifications.
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

/// Neovim passes a Latin-1 program's `é` on as the one byte it sent, which is
/// not UTF-8.
#[test]
fn a_notification_that_is_not_utf8_still_shows() {
    require_nvim!();
    let scratch = Scratch::new("notes-latin1");
    let (_, sock) = a_session(&scratch, "latin1");
    assert_eq!(notes::read(&sock), Reading::Said(None));
    in_a_terminal(&sock, r"\033]777;notify;Build;caf\351 failed\007");
    let n = read_until(&sock, |n| n.is_some()).expect("the notification");
    assert_eq!(
        (n.title.as_str(), n.text.as_str()),
        ("Build", "caf\u{fffd} failed")
    );
}

/// Programs in two terminals use the same id; within one terminal, a part
/// with another id starts a new notification.
#[test]
fn kitty_notifications_are_pieced_together_by_terminal_and_id() {
    require_nvim!();
    let scratch = Scratch::new("notes-kitty-own");
    let (_, sock) = a_session(&scratch, "pieced");
    assert_eq!(notes::read(&sock), Reading::Said(None));

    let building = a_terminal(
        &sock,
        r"printf '\033]99;i=1:d=0;Build\033\134\033]777;notify;x;begun\007'; read x; printf '\033]99;i=1:p=body;finished\033\134'; sleep 30",
    );
    read_until(&sock, |n| n.is_some_and(|n| n.text == "begun")).expect("its first part");
    notes::seen(&sock);
    in_a_terminal(&sock, r"\033]99;i=1:p=body;Tests passed\033\134");
    let other = read_until(&sock, |n| n.is_some()).expect("the other terminal's");
    assert_eq!(
        (other.title.as_str(), other.text.as_str()),
        ("", "Tests passed"),
        "nothing of the first terminal's"
    );

    notes::seen(&sock);
    enter(&sock, building);
    let own = read_until(&sock, |n| n.is_some()).expect("the first terminal's");
    assert_eq!(
        (own.title.as_str(), own.text.as_str()),
        ("Build", "finished"),
        "its first part kept through the other's"
    );

    notes::seen(&sock);
    in_a_terminal(
        &sock,
        r"\033]99;i=2:d=0;Abandoned\033\134\033]99;i=3;Fresh\033\134",
    );
    let fresh = read_until(&sock, |n| n.is_some()).expect("the new one");
    assert_eq!((fresh.title.as_str(), fresh.text.as_str()), ("Fresh", ""));
}

/// However many parts come, it holds at most what 200 characters can take;
/// the 200 shown start at the first that is not blank.
#[test]
fn an_unfinished_kitty_notification_is_bounded() {
    require_nvim!();
    let scratch = Scratch::new("notes-kitty-cut");
    let (_, sock) = a_session(&scratch, "cut");
    assert_eq!(notes::read(&sock), Reading::Said(None));

    // 1300 blanks, past the 1200 bytes kept, and 400 KB of title in parts of
    // 1000 characters; then a mark once all are in.
    let sending = a_terminal(
        &sock,
        r"read x; printf '\033]99;i=big:d=0;%1300sLong \033\134' ''; a=$(head -c 1000 /dev/zero | tr '\0' A); i=0; while [ $i -lt 400 ]; do printf '\033]99;i=big:d=0;%s\033\134' $a; i=$((i+1)); done; printf '\033]777;notify;x;sent\007'; read x; printf '\033]99;i=big:p=body;done\033\134'; sleep 30",
    );
    let before = lua_memory(&sock);
    enter(&sock, sending);
    read_until(&sock, |n| n.is_some_and(|n| n.text == "sent")).expect("all its parts");
    let held = lua_memory(&sock) - before;
    assert!(held < 100.0, "400 KB sent, {held:.0} KB held");

    notes::seen(&sock);
    enter(&sock, sending);
    let n = read_until(&sock, |n| n.is_some()).expect("the notification");
    assert_eq!(n.title, format!("Long {}", "A".repeat(195)));
    assert_eq!(n.text, "done");
}

/// Here the 200th character, the last shown, is split between base64 parts,
/// which kitty joins before decoding.
#[test]
fn a_character_split_between_kitty_parts_is_kept_whole() {
    require_nvim!();
    let scratch = Scratch::new("notes-kitty-split");
    let (_, sock) = a_session(&scratch, "split");
    assert_eq!(notes::read(&sock), Reading::Said(None));

    // In base64: 199 A's and the first two of 中's three bytes, then its last
    // and "BC".
    in_a_terminal(
        &sock,
        &format!(
            r"\033]99;i=u:d=0:e=1;{}QeS4\033\134\033]99;i=u:e=1;rUJD\033\134",
            "QUFB".repeat(66)
        ),
    );
    let n = read_until(&sock, |n| n.is_some()).expect("the notification");
    assert_eq!(n.title, format!("{}中", "A".repeat(199)));
}

/// The part and the command come after `TermClose`, as when output that
/// scrolled holds them back: a terminal's requests wait for the Lua that sent
/// them to return, and `chanclose` closes the terminal within it.
#[test]
fn an_unfinished_kitty_notification_goes_with_its_terminal() {
    require_nvim!();
    let scratch = Scratch::new("notes-kitty-gone");
    let (_, sock) = a_session(&scratch, "gone");
    assert_eq!(notes::read(&sock), Reading::Said(None));

    let buf = lua(
        &sock,
        r"local buf = vim.api.nvim_create_buf(true, false)
local chan = vim.api.nvim_open_term(buf, {})
vim.api.nvim_chan_send(chan, '\27]99;i=1:d=0;Unfinished\27\\\27]133;C;cmdline_url=exit\27\\')
vim.fn.chanclose(chan)
return buf",
    )
    .as_i64()
    .expect("its buffer");
    assert!(
        common::wait_until(Duration::from_secs(5), || {
            (held(&sock, "parts"), held(&sock, "commands")) == (1, 1)
        }),
        "the part and the command were never held"
    );
    lua(
        &sock,
        &format!("vim.api.nvim_buf_delete({buf}, {{ force = true }})"),
    );
    assert_eq!(
        (held(&sock, "parts"), held(&sock, "commands")),
        (0, 0),
        "gone with its buffer"
    );
}

/// The new terminal gets neither the old one's unfinished notification nor
/// its command, whose OSC 133;C came after `TermClose`, following output that
/// scrolled. nvim_open_term sets no 'channel', so only `TermOpen` tells the
/// two apart.
#[test]
fn a_buffer_given_a_new_terminal_forgets_the_old_one() {
    require_nvim!();
    let scratch = Scratch::new("notes-kitty-reused");
    let (_, sock) = a_session(&scratch, "reused");
    assert_eq!(notes::read(&sock), Reading::Said(None));

    let buf = common::a_job_that_exits(
        &sock,
        r"seq 1 300; printf '\033]99;i=1:d=0;Old\033\134\033]133;C;cmdline_url=exit\033\134\033]777;notify;x;sent\007'",
        false,
    );
    read_until(&sock, |n| n.is_some_and(|n| n.text == "sent")).expect("its last sequences");
    notes::seen(&sock);
    lua(
        &sock,
        &format!(
            r"local chan = vim.api.nvim_open_term({buf}, {{}})
vim.api.nvim_chan_send(chan, '\27]133;D;0\27\\\27]99;i=1;Fresh\27\\')"
        ),
    );
    let n = read_until(&sock, |n| n.is_some_and(|n| n.kind == Kind::Notified))
        .expect("the new notification");
    assert_eq!(n.title, "Fresh");
    let Heard::Said { news, .. } = notes::hear(&sock, None) else {
        panic!("the session said nothing");
    };
    assert_eq!(
        news.iter().map(|n| n.kind).collect::<Vec<_>>(),
        [Kind::Notified],
        "no finish of the old command: {news:?}"
    );
}

/// A `TermRequest` autocommand defined before the watcher's, as a user's or
/// a plugin's is, wipes each terminal's buffer at its first request, before
/// the watcher sees it. 0.11 sends the first terminal's later requests for
/// buffer 0; 0.12 drops them.
#[test]
fn a_notification_shows_though_its_buffer_was_wiped_first() {
    require_nvim!();
    let scratch = Scratch::new("notes-kitty-wiped");
    let (_, sock) = a_session(&scratch, "wiped");
    lua(
        &sock,
        r"vim.v.errmsg = ''
vim.api.nvim_create_autocmd('TermRequest', {
  callback = function(ev)
    if ev.buf > 0 then
      vim.api.nvim_buf_delete(ev.buf, { force = true })
    end
  end,
})",
    );
    assert_eq!(notes::read(&sock), Reading::Said(None));
    let wiped = |printf: &str| {
        let buf = lua(
            &sock,
            &format!(
                r#"local buf = vim.api.nvim_create_buf(true, false)
vim.api.nvim_buf_call(buf, function()
  vim.fn.jobstart({{ 'sh', '-c', [=[printf '{printf}'; sleep 30]=] }}, {{ term = true }})
end)
return buf"#
            ),
        )
        .as_i64()
        .expect("its buffer");
        assert!(
            common::wait_until(Duration::from_secs(5), || {
                lua(&sock, &format!("return vim.api.nvim_buf_is_valid({buf})"))
                    == Value::Boolean(false)
            }),
            "{printf} never wiped its buffer"
        );
    };
    wiped(
        r"\033]99;i=1:d=0;Part\033\134\033]133;C;cmdline_url=make\033\134\033]99;i=2:d=0;More\033\134",
    );
    wiped(r"\033]133;C;cmdline_url=make\033\134");
    wiped(r"\033]99;i=3;Hi\033\134");
    let n = read_until(&sock, |n| n.is_some()).expect("the notification");
    assert_eq!(n.title, "Hi");
    assert_eq!(
        (held(&sock, "parts"), held(&sock, "commands")),
        (0, 0),
        "kept for a buffer gone"
    );
    assert_eq!(lua(&sock, "return vim.v.errmsg"), Value::from(""));
}

/// The old program's part came before `TermClose`, as nothing scrolled, and a
/// terminal a `TermClose` autocommand starts fires no `TermOpen` unless the
/// autocommand is nested.
#[test]
fn a_terminal_started_from_termclose_forgets_the_old_ones_part() {
    require_nvim!();
    let scratch = Scratch::new("notes-kitty-restart");
    let (_, sock) = a_session(&scratch, "restarted");
    assert_eq!(notes::read(&sock), Reading::Said(None));

    let old = lua(
        &sock,
        r"local buf = vim.api.nvim_create_buf(true, false)
vim.api.nvim_create_autocmd('TermClose', {
  buffer = buf,
  once = true,
  callback = function()
    vim.bo[buf].modified = false
    vim.api.nvim_buf_call(buf, function()
      vim.fn.jobstart({ 'sh', '-c', [=[printf '\033]99;i=1;Fresh\033\134'; sleep 30]=] }, { term = true })
    end)
  end,
})
vim.api.nvim_buf_call(buf, function()
  vim.fn.jobstart({ 'sh', '-c', [=[printf '\033]99;i=1:d=0;Old\033\134'; read x]=] }, { term = true })
end)
return vim.bo[buf].channel",
    )
    .as_u64()
    .expect("its channel");
    assert!(
        common::wait_until(Duration::from_secs(5), || held(&sock, "parts") == 1),
        "the old program's part never came"
    );
    enter(&sock, old);
    let n = read_until(&sock, |n| n.is_some_and(|n| n.kind == Kind::Notified))
        .expect("the new notification");
    assert_eq!(n.title, "Fresh");
}

/// Its last part, sent after output that scrolled just before the program
/// exited, comes after `TermClose`; the part before must still be kept.
#[test]
fn a_kitty_notification_finished_as_its_program_exits_is_whole() {
    require_nvim!();
    let scratch = Scratch::new("notes-kitty-late");
    let (_, sock) = a_session(&scratch, "late");
    assert_eq!(notes::read(&sock), Reading::Said(None));

    let building = a_terminal(
        &sock,
        r"printf '\033]99;i=1:d=0;Build\033\134\033]777;notify;x;begun\007'; read x; seq 1 300; printf '\033]99;i=1:p=body;finished\033\134'",
    );
    read_until(&sock, |n| n.is_some_and(|n| n.text == "begun")).expect("its first part");
    notes::seen(&sock);
    enter(&sock, building);
    let n = read_until(&sock, |n| n.is_some_and(|n| n.kind == Kind::Notified))
        .expect("the notification");
    assert_eq!((n.title.as_str(), n.text.as_str()), ("Build", "finished"));
}

/// An older watcher version is replaced, keeping what it recorded.
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
        Some(if has_progress(&sock) { 6 } else { 5 }),
        "the autocommands replaced, not added to"
    );
}

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

/// The picker's reader reads every session, clearing the one it was opened
/// from first.
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
