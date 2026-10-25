//! What a session reports to the picker: busy, failed or finished work, and
//! desktop notifications from its programs.
//!
//! None of this reaches nvmux unaided: Neovim only fires local autocommands,
//! a `--headless` server never emits OSC 9;4 progress, and UI events reach
//! only UIs attached at the time. So nvmux installs a watcher in each session
//! ([`WATCHER`], augroup `nvmux_notes`) that records:
//! - `Progress` messages (0.12+, except Neovim's own) and `LspProgress`;
//! - via `TermRequest`: OSC 777 `notify` (Ghostty, urxvt), OSC 9 (iTerm2),
//!   OSC 99 (kitty, multi-part), and OSC 133 shell command exits;
//! - via `TermClose`: a terminal program's exit, positive failed, 0 finished.
//!
//! A program's own OSC 9;4 is ignored (Claude Code's stays busy through its
//! permission prompts). The watcher outlives nvmux, since recording unattended
//! is its job; it sets no options, maps no keys, opens no windows and prints
//! nothing.
//!
//! The picker reads it ([`read`]) every [`POLL`] on a [`Poller`] thread,
//! asking the mode first as [`crate::rpc::probe`] does, so a session blocked
//! on a key keeps its old note instead of stalling.
//!
//! Notifications, failures and finished commands are news until seen: cleared
//! ([`SEEN`]) in the session when it comes to the front ([`touch`]) and when it
//! leaves ([`seen`]), so other nvmux instances stop showing them too. Busy is
//! state, shown while it lasts.
//!
//! For [`crate::tell`]'s cards nvmux listens instead of polling ([`listen`]):
//! the watcher sends [`NEWS`] to each listener channel when its news changes.
//! A read naming the channel ([`hear`]) registers it; until then a watcher
//! installed by another read or nvmux version tells nobody.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use rmpv::Value;

use crate::error::RpcError;
use crate::rpc;

/// Watcher version. A session with any other version gets this watcher,
/// keeping its record.
pub const VERSION: i64 = 9;

/// Picker read interval.
pub const POLL: Duration = Duration::from_secs(2);

/// Limit for one read, connect to answer; a slower session keeps its note.
const BUDGET: Duration = Duration::from_secs(1);

/// Delay before reading newly handed sessions, so a listing's sessions are
/// read in one round.
const SETTLE: Duration = Duration::from_millis(30);

/// The watcher, run with `nvim_exec_lua` (args: version, optional listener
/// channel); returns the record as [`READ`] does. Reinstalling replaces the
/// autocommands and keeps the record, except LSP work, which restarts since
/// another version's watcher may have missed its end.
///
/// It records:
/// - running progress by source and id (title, text, start time, percent;
///   Neovim 0.12 sends 0 for none);
/// - LSP work by client and token: server name as title, e.g.
///   `indexing 3/120` as text (a report carries only changes, so a missing
///   message or percentage stands). Kept until it ends or the server goes,
///   checked on read, as a dying server never ends its work;
/// - the last notification or failure (failed progress, non-zero terminal
///   exit or shell command) and, separately, the last finished command, each
///   until [`SEEN`].
///
/// Text has control characters blanked and is cut to 200 characters.
///
/// A command is named by the shell's OSC 133;C `cmdline_url` if given, else by
/// the terminal's program from the buffer name (`term://…//1234:cargo build`,
/// directory dropped), with body `exited with code 101`, or `a command exited
/// with code 101` for a shell command without a name.
///
/// - Exit 0 is "finished" only for job terminals; `nvim_open_term` ones also
///   close with 0. A buffer deleted while running closes with -1: neither.
/// - A shell's exit 0 counts only after its OSC 133;C: hand-configured shells
///   may report a status at every prompt, 0 when nothing ran.
/// - Neovim's own progress (`source` `nvim`) is skipped: writes and completion
///   scans start and end within one command, and a failed one never ends,
///   leaving the session busy forever. Older watchers kept them, so they are
///   purged on install.
///
/// When news arrives or is cleared it sends [`NEWS`] to every listener a read
/// registered ([`READ`], [`listen`]). Failing channels are dropped (channel ids
/// are never reused); listeners survive reinstalling. A listener passed here
/// is registered, but the record does not report it as `listening`, since the
/// replaced watcher may not have told it.
const WATCHER: &str = r#"local version, listener = ...
local api = vim.api
local N = package.loaded['nvmux.notes']
if type(N) ~= 'table' then
  N = { progress = {} }
  package.loaded['nvmux.notes'] = N
end
N.version = version
N.progress = type(N.progress) == 'table' and N.progress or {}
for key in pairs(N.progress) do
  if key:sub(1, 5) == 'nvim\0' then
    N.progress[key] = nil
  end
end
N.lsp = {}
N.seq = type(N.seq) == 'number' and N.seq or 0
N.listeners = type(N.listeners) == 'table' and N.listeners or {}
local group = api.nvim_create_augroup('nvmux_notes', { clear = true })
local function clean(s)
  if type(s) == 'table' then
    local parts = {}
    for _, c in ipairs(s) do
      parts[#parts + 1] = type(c) == 'table' and tostring(c[1] or '') or tostring(c)
    end
    s = table.concat(parts)
  end
  if type(s) ~= 'string' then
    return ''
  end
  s = s:gsub('[%z\1-\31\127]', ' '):gsub('\194[\128-\159]', ' ')
  return vim.fn.strcharpart(vim.trim(s), 0, 200)
end
local function shown(buf)
  if not buf or not api.nvim_buf_is_valid(buf) then
    return nil
  end
  for _, win in ipairs(api.nvim_tabpage_list_wins(0)) do
    if api.nvim_win_get_buf(win) == buf then
      return true
    end
  end
  return false
end
local function tell()
  for chan in pairs(N.listeners) do
    if not pcall(vim.rpcnotify, chan, 'nvmux_news') then
      N.listeners[chan] = nil
    end
  end
end
local function note(kind, title, body, buf)
  title, body = clean(title), clean(body)
  if title ~= '' or body ~= '' then
    N.seq = N.seq + 1
    local kept = {
      kind = kind,
      title = title,
      body = body,
      at = os.time(),
      seq = N.seq,
      shown = shown(buf),
      buf = buf,
    }
    if kind == 'finished' then
      N.finished = kept
    else
      N.note = kept
    end
    tell()
  end
end
pcall(api.nvim_create_autocmd, 'Progress', {
  group = group,
  callback = function(ev)
    local d = ev.data
    if type(d) ~= 'table' or d.id == nil or d.source == 'nvim' then
      return
    end
    local key = tostring(d.source) .. '\0' .. tostring(d.id)
    if d.status == 'running' then
      local was = N.progress[key]
      local p = tonumber(d.percent)
      N.progress[key] = {
        title = clean(d.title),
        text = clean(d.text),
        percent = p and p > 0 and math.min(100, math.floor(p)) or nil,
        at = was and was.at or os.time(),
      }
    else
      N.progress[key] = nil
      if d.status == 'failed' then
        note('failed', d.title, d.text)
      end
    end
  end,
})
api.nvim_create_autocmd('LspProgress', {
  group = group,
  callback = function(ev)
    local d = ev.data
    local params = type(d) == 'table' and d.params
    local v = type(params) == 'table' and params.value
    if type(v) ~= 'table' or d.client_id == nil or params.token == nil then
      return
    end
    local key = tostring(d.client_id) .. '\0' .. tostring(params.token)
    if v.kind == 'end' then
      N.lsp[key] = nil
    elseif v.kind == 'begin' or v.kind == 'report' then
      local was = v.kind == 'report' and N.lsp[key]
      if not was then
        local client = vim.lsp.get_client_by_id(d.client_id)
        was = { title = clean(client and client.name), message = '', at = os.time() }
      end
      local message = type(v.message) == 'string' and clean(v.message) or was.message
      local p = tonumber(v.percentage) or was.percent
      N.lsp[key] = {
        client = d.client_id,
        title = was.title,
        text = clean((type(v.title) == 'string' and v.title or '') .. ' ' .. message),
        message = message,
        percent = p and p > 0 and math.min(100, math.floor(p)) or nil,
        at = was.at,
      }
    end
  end,
})
local function program(buf)
  local name = api.nvim_buf_is_valid(buf) and api.nvim_buf_get_name(buf) or ''
  local command = name:match('^term://.-//%d+:(.*)$') or name
  return (command:gsub('^%S*/', ''))
end
local function ran(buf)
  local chan = api.nvim_buf_is_valid(buf) and vim.bo[buf].channel or 0
  return chan > 0 and api.nvim_get_chan_info(chan).stream == 'job'
end
local parts, commands = {}, {}
api.nvim_create_autocmd('TermRequest', {
  group = group,
  callback = function(ev)
    local seq = type(ev.data) == 'table' and ev.data.sequence or ev.data
    if type(seq) ~= 'string' or seq:sub(1, 2) ~= '\27]' then
      return
    end
    local osc = seq:sub(3)
    local title, body = osc:match('^777;notify;([^;]*);?(.*)$')
    if title then
      return note('notify', title, body, ev.buf)
    end
    local mark, args = osc:match('^133;(%a)(.*)$')
    if mark == 'C' then
      local url = args:match(';cmdline_url=([^;]*)')
      commands[ev.buf] = url and vim.uri_decode(url) or false
      return
    elseif mark == 'D' then
      local status = tonumber(args:match('^;(%d+)'))
      local command = commands[ev.buf]
      commands[ev.buf] = nil
      if status and (status > 0 or command ~= nil) then
        local kind = status > 0 and 'failed' or 'finished'
        if command then
          note(kind, command, 'exited with code ' .. status, ev.buf)
        else
          note(kind, program(ev.buf), 'a command exited with code ' .. status, ev.buf)
        end
      end
      return
    end
    local text = osc:match('^9;(.*)$')
    if text then
      if not text:match('^%d+;') and not text:match('^%d+$') then
        note('notify', '', text, ev.buf)
      end
      return
    end
    local meta, payload = osc:match('^99;([^;]*);(.*)$')
    if meta then
      local m = {}
      for k, v in meta:gmatch('([^:=]+)=([^:]*)') do
        m[k] = v
      end
      if m.e == '1' then
        local ok, decoded = pcall(vim.base64.decode, payload)
        payload = ok and decoded or ''
      end
      local id = m.i or ''
      local p = parts[id] or { title = '', body = '' }
      if (m.p or 'title') == 'title' then
        p.title = p.title .. payload
      elseif m.p == 'body' then
        p.body = p.body .. payload
      end
      if m.d == '0' then
        parts[id] = p
      else
        parts[id] = nil
        note('notify', p.title, p.body, ev.buf)
      end
    end
  end,
})
api.nvim_create_autocmd('TermClose', {
  group = group,
  callback = function(ev)
    commands[ev.buf] = nil
    local status = vim.v.event.status
    if type(status) == 'number' and status > 0 then
      note('failed', program(ev.buf), 'exited with code ' .. status, ev.buf)
    elseif status == 0 and ran(ev.buf) then
      note('finished', program(ev.buf), 'exited with code 0', ev.buf)
    end
  end,
})
local function tab_of(buf)
  for _, tab in ipairs(api.nvim_list_tabpages()) do
    for _, win in ipairs(api.nvim_tabpage_list_wins(tab)) do
      if api.nvim_win_get_buf(win) == buf then
        return api.nvim_tabpage_get_number(tab)
      end
    end
  end
  return nil
end
function N.snapshot()
  local now = os.time()
  local busy
  for _, p in pairs(N.progress) do
    if not busy or p.at > busy.at then
      busy = p
    end
  end
  for key, p in pairs(N.lsp) do
    if not vim.lsp.get_client_by_id(p.client) then
      N.lsp[key] = nil
    elseif not busy or p.at > busy.at then
      busy = p
    end
  end
  local out = { v = N.version }
  if busy then
    out.busy = { title = busy.title, text = busy.text, percent = busy.percent, age = now - busy.at }
  end
  for _, slot in ipairs({ 'note', 'finished' }) do
    local n = N[slot]
    if n then
      local buf = type(n.buf) == 'number' and api.nvim_buf_is_valid(n.buf) and n.buf or nil
      out[slot] = {
        kind = n.kind,
        title = n.title,
        body = n.body,
        age = now - n.at,
        seq = n.seq,
        shown = n.shown,
        buf = buf,
        tab = buf and tab_of(buf),
      }
    end
  end
  return out
end
function N.seen()
  local kept = N.note or N.finished
  N.note = nil
  N.finished = nil
  if kept then
    tell()
  end
end
if listener then
  N.listeners[listener] = true
end
return N.snapshot()
"#;

/// Returns the record map, or `false` if the watcher is missing or another
/// version. Keys, each present only when set: `busy` (latest-started progress
/// or LSP work: `title`, `text`, `percent`, `age` in seconds); `note` (`kind`
/// `notify` or `failed`) and `finished`, both with `title`, `body`, `age`,
/// `seq`, `shown`, `buf`, `tab`.
///
/// With a listener channel ([`listen`]) it registers it and reports whether it
/// already was (`listening`).
const READ: &str = r#"local version, listener = ...
local N = package.loaded['nvmux.notes']
if type(N) ~= 'table' or N.version ~= version or type(N.snapshot) ~= 'function' then
  return false
end
local out = N.snapshot()
if listener then
  out.listening = N.listeners[listener] == true
  N.listeners[listener] = true
end
return out
"#;

/// Sent to listeners when the watcher's news changes (see [`listen`]).
const NEWS: &str = "nvmux_news";

/// Clears the kept notification or failure and finished command.
const SEEN: &str = r#"local N = package.loaded['nvmux.notes']
if type(N) == 'table' and type(N.seen) == 'function' then
  N.seen()
end
return true
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A terminal program asked for a desktop notification.
    Notified,
    /// A progress message or a terminal command failed.
    Failed,
    /// A terminal command exited with 0.
    Finished,
    /// Progress or LSP work running, with its percentage if reported.
    Busy(Option<u8>),
}

/// The one note shown per session, by priority: notification or failure,
/// then finished command, then busy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub kind: Kind,
    /// Notification or progress title (often the source, e.g. `Claude Code`),
    /// the LSP server name (`clangd`), or the command (`cargo build`).
    pub title: String,
    /// Body, progress text (`indexing 3/120`) or exit (`exited with code 101`).
    pub text: String,
    /// Age at read time by the session's clock (a remote host's may differ).
    pub age: Duration,
    /// Local time of the read, for [`Note::age_now`].
    pub read_at: Instant,
}

impl Note {
    pub fn age_now(&self) -> Duration {
        self.age + self.read_at.elapsed()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reading {
    Said(Option<Note>),
    /// No answer now (blocked on a key, busy, or slow); keep the old note.
    Unknown,
    /// Refused or reset: nothing listens on its socket, or on macOS its
    /// accept queue is full.
    Gone,
}

/// Read the session on `sock`, installing the watcher if missing or another
/// version.
pub fn read(sock: &Path) -> Reading {
    let mut client = match rpc::Client::connect(sock, BUDGET) {
        Ok(c) => c,
        Err(e) if e.is_definitely_dead() => return Reading::Gone,
        Err(_) => return Reading::Unknown,
    };
    match snapshot(&mut client) {
        Ok(Some(v)) => Reading::Said(note_of(&v, Instant::now())),
        Ok(None) => Reading::Unknown,
        Err(e) if e.is_definitely_dead() => Reading::Gone,
        Err(_) => Reading::Unknown,
    }
}

/// A notification, failure or finished command kept until seen, numbered so
/// a reader can tell it from one it already had (see [`crate::tell`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct News {
    pub kind: Kind,
    /// As a [`Note`]'s.
    pub title: String,
    /// As a [`Note`]'s.
    pub text: String,
    /// From the session's increasing counter; 0 from a watcher too old to
    /// keep one.
    pub seq: u64,
    /// Whether its terminal buffer was in a window of the current tab when it
    /// came, so the front session is not told what it showed; `None` if not
    /// from a buffer (progress).
    pub shown: Option<bool>,
    /// Seconds since it came, as of the read.
    pub age: u64,
    /// Its terminal buffer, while still valid.
    pub buf: Option<i64>,
    /// Number of a tab page showing that buffer at read time.
    pub tab: Option<u32>,
}

/// Result of [`hear`].
#[derive(Debug, PartialEq, Eq)]
pub enum Heard {
    /// Kept news, oldest first (up to two), and whether the named listener
    /// was already registered (it is now).
    Said { news: Vec<News>, listening: bool },
    /// No answer now; nothing was left queued in the session.
    Unknown,
    /// Refused or reset: nothing listens on its socket, or on macOS its
    /// accept queue is full.
    Gone,
}

/// Read the session's news, installing the watcher if needed; with `listener`
/// ([`listen`]'s channel), the watcher notifies it from now on. Unlike
/// [`read`], the record is requested only once the mode shows the editor is
/// free: one more round trip, but nothing queues up in a session blocked all
/// night.
pub fn hear(sock: &Path, listener: Option<u64>) -> Heard {
    let mut client = match rpc::Client::connect(sock, BUDGET) {
        Ok(c) => c,
        Err(e) if e.is_definitely_dead() => return Heard::Gone,
        Err(_) => return Heard::Unknown,
    };
    match carefully(&mut client, listener) {
        Ok(Some(v)) => Heard::Said {
            news: news_of(&v),
            listening: listener.is_some()
                && field(&v, "listening").and_then(Value::as_bool) == Some(true),
        },
        Ok(None) => Heard::Unknown,
        Err(e) if e.is_definitely_dead() => Heard::Gone,
        Err(_) => Heard::Unknown,
    }
}

/// [`snapshot`] one request at a time: `None`, asking nothing more, if the
/// editor is blocked. A freshly installed watcher also gets the listener.
fn carefully<S: Read + Write>(
    client: &mut rpc::Client<S>,
    listener: Option<u64>,
) -> Result<Option<Value>, RpcError> {
    if client.get_mode()?.blocking {
        return Ok(None);
    }
    let record = client.call("nvim_exec_lua", lua_naming(READ, listener))?;
    if record == Value::Boolean(false) {
        return client
            .call("nvim_exec_lua", lua_naming(WATCHER, listener))
            .map(Some);
    }
    Ok(Some(record))
}

/// Events from [`listen`].
#[derive(Debug)]
pub enum Pushed {
    /// Connected as session channel `chan`, which [`hear`] must name for the
    /// watcher to notify it. `stop` ends listening from any thread.
    Listening { chan: u64, stop: rpc::Interrupt },
    /// News arrived or was seen; a read says which.
    News,
}

/// Pass [`Pushed`] events from the session on `sock` to `heard` until it
/// returns `false`, the session goes, or the connection is stopped.
///
/// No time budget once connected: a session may be quiet all day. If the
/// editor is blocked on a key this returns at once, to be retried. Registering
/// the channel with the watcher is left to [`hear`].
pub fn listen(sock: &Path, mut heard: impl FnMut(Pushed) -> bool) -> Result<(), RpcError> {
    let mut client = rpc::Client::connect(sock, BUDGET)?;
    let mode = client.ask_mode()?;
    // `0` means this channel; inside `nvim_exec_lua` it would be the
    // session's own, which has no id.
    let asked = client.ask("nvim_get_chan_info", vec![Value::from(0)])?;
    if client.mode_reply(mode)?.blocking {
        return Ok(());
    }
    let info = client.reply_to(asked)?;
    let chan = field(&info, "id")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::Protocol("a channel with no number".into()))?;
    let stop = client.interrupt()?;
    client.set_budget(None)?;
    if !heard(Pushed::Listening { chan, stop }) {
        return Ok(());
    }
    loop {
        let (method, _) = client.next_notification()?;
        if method == NEWS && !heard(Pushed::News) {
            return Ok(());
        }
    }
}

/// On a session coming to the front: install the watcher if missing and
/// clear its news. Best effort; one that cannot answer gets both next time.
pub fn touch(sock: &Path) {
    let Ok(mut client) = rpc::Client::connect(sock, BUDGET) else {
        return;
    };
    if matches!(snapshot(&mut client), Ok(Some(_))) {
        if let Err(e) = client.call("nvim_exec_lua", lua(SEEN)) {
            tracing::debug!(error = %e, "notes: could not clear on arrival");
        }
    }
}

/// Clear the news of a session just looked at. Best effort.
pub fn seen(sock: &Path) {
    if let Err(e) = unblocked(sock, SEEN) {
        tracing::debug!(error = %e, "notes: could not clear");
    }
}

/// Run [`touch`] or [`seen`] on its own thread so a switch does not wait.
pub fn later(sock: PathBuf, what: fn(&Path)) {
    let spawned = std::thread::Builder::new()
        .name("nvmux-notes-seen".into())
        .spawn(move || what(&sock));
    if let Err(e) = spawned {
        tracing::debug!(error = %e, "notes: no thread to clear on");
    }
}

/// Start [`seen`] on its own thread at exit; [`Seeing::wait_until`] waits for
/// it after the rest of shutdown, so the two overlap.
pub fn seen_begin(sock: PathBuf) -> Seeing {
    let (done, finished) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("nvmux-notes-seen".into())
        .spawn(move || {
            seen(&sock);
            let _ = done.send(());
        });
    Seeing(spawned.is_ok().then_some(finished))
}

/// A [`seen_begin`] in progress; `None` if no thread could be spawned.
pub struct Seeing(Option<mpsc::Receiver<()>>);

impl Seeing {
    /// Wait for it until `deadline`; exiting would otherwise kill the thread
    /// first.
    pub fn wait_until(self, deadline: Instant) {
        if let Some(said) = self.0 {
            let _ = said.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        }
    }
}

/// The record, installing the watcher if missing or another version; `None`
/// while the editor is blocked on a key (deferred calls go unanswered).
///
/// The read is pipelined with the mode: one round trip. If blocked, it runs
/// harmlessly once the key comes (it only drops LSP work of dead servers); the
/// watcher, which writes the record, is sent only after an answer.
fn snapshot<S: Read + Write>(client: &mut rpc::Client<S>) -> Result<Option<Value>, RpcError> {
    let mode = client.ask_mode()?;
    let asked = client.ask("nvim_exec_lua", lua(READ))?;
    if client.mode_reply(mode)?.blocking {
        return Ok(None);
    }
    let record = client.reply_to(asked)?;
    if record == Value::Boolean(false) {
        return client.call("nvim_exec_lua", lua(WATCHER)).map(Some);
    }
    Ok(Some(record))
}

/// Run `code` in the session without waiting if the editor is blocked on a
/// key; it then runs once the key comes, if still connected.
fn unblocked(sock: &Path, code: &'static str) -> Result<(), RpcError> {
    let mut client = rpc::Client::connect(sock, BUDGET)?;
    let mode = client.ask_mode()?;
    let asked = client.ask("nvim_exec_lua", lua(code))?;
    if client.mode_reply(mode)?.blocking {
        return Ok(());
    }
    client.reply_to(asked).map(drop)
}

/// `nvim_exec_lua` arguments: the chunk, with [`VERSION`] as its argument.
fn lua(code: &str) -> Vec<Value> {
    lua_naming(code, None)
}

/// [`lua`], plus a listener channel after the version if given.
fn lua_naming(code: &str, listener: Option<u64>) -> Vec<Value> {
    let mut args = vec![Value::from(VERSION)];
    args.extend(listener.map(Value::from));
    vec![Value::from(code), Value::Array(args)]
}

/// The record's note, by [`Note`]'s priority.
fn note_of(record: &Value, read_at: Instant) -> Option<Note> {
    let kept = |n: &Value, kind| Note {
        kind,
        title: text(field(n, "title")),
        text: text(field(n, "body")),
        age: seconds(field(n, "age")),
        read_at,
    };
    if let Some(n) = field(record, "note") {
        let kind = match field(n, "kind").and_then(Value::as_str) {
            Some("failed") => Kind::Failed,
            _ => Kind::Notified,
        };
        return Some(kept(n, kind));
    }
    if let Some(n) = field(record, "finished") {
        return Some(kept(n, Kind::Finished));
    }
    let busy = field(record, "busy")?;
    let percent = field(busy, "percent")
        .and_then(number)
        .filter(|p| *p > 0)
        .map(|p| p.min(100) as u8);
    Some(Note {
        kind: Kind::Busy(percent),
        title: text(field(busy, "title")),
        text: text(field(busy, "text")),
        age: seconds(field(busy, "age")),
        read_at,
    })
}

/// The record's news, oldest first.
fn news_of(record: &Value) -> Vec<News> {
    let mut news: Vec<News> = ["note", "finished"]
        .into_iter()
        .filter_map(|slot| {
            let n = field(record, slot)?;
            let kind = match (slot, field(n, "kind").and_then(Value::as_str)) {
                ("finished", _) => Kind::Finished,
                (_, Some("failed")) => Kind::Failed,
                _ => Kind::Notified,
            };
            Some(News {
                kind,
                title: text(field(n, "title")),
                text: text(field(n, "body")),
                seq: field(n, "seq").and_then(number).unwrap_or(0),
                shown: field(n, "shown").and_then(Value::as_bool),
                age: field(n, "age").and_then(number).unwrap_or(0),
                buf: field(n, "buf").and_then(Value::as_i64),
                tab: field(n, "tab")
                    .and_then(number)
                    .and_then(|t| u32::try_from(t).ok()),
            })
        })
        .collect();
    news.sort_by_key(|n| n.seq);
    news
}

fn field<'a>(map: &'a Value, key: &str) -> Option<&'a Value> {
    map.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// A record string, safe to draw. The watcher already strips control
/// characters, but the text comes from whoever sent the notification, so it is
/// not trusted.
fn text(v: Option<&Value>) -> String {
    let s = v.and_then(Value::as_str).unwrap_or_default();
    let clean: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    clean.trim().to_string()
}

/// A record count. Lua numbers arrive as integers when whole, else floats.
/// Negative (a clock gone back) reads as `None`.
fn number(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
}

fn seconds(v: Option<&Value>) -> Duration {
    Duration::from_secs(v.and_then(number).unwrap_or(0))
}

enum Command {
    Watch(String, PathBuf),
}

/// The picker's reads on their own thread, every [`POLL`] in parallel,
/// yielding a session id to note map. Dropping it never joins the thread, so
/// leaving the picker never waits on a session.
pub struct Poller {
    commands: Sender<Command>,
    updates: Receiver<HashMap<String, Note>>,
}

impl Poller {
    /// `seen` is the session the picker was opened from; it is cleared before
    /// its first read.
    pub fn start(seen: Option<String>) -> Self {
        let (commands, inbox) = mpsc::channel();
        let (outbox, updates) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("nvmux-notes".into())
            .spawn(move || poll(seen, &inbox, &outbox));
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "notes: no thread to read the sessions on");
        }
        Self { commands, updates }
    }

    pub fn watch(&self, id: &str, sock: PathBuf) {
        let _ = self.commands.send(Command::Watch(id.to_string(), sock));
    }

    /// The latest notes of all sessions, if updated since last asked.
    pub fn take(&self) -> Option<HashMap<String, Note>> {
        self.updates.try_iter().last()
    }
}

fn poll(
    mut seen_first: Option<String>,
    inbox: &Receiver<Command>,
    outbox: &Sender<HashMap<String, Note>>,
) {
    let mut watched: Vec<(String, PathBuf)> = Vec::new();
    let mut notes: HashMap<String, Note> = HashMap::new();
    let mut due: Option<Instant> = None;
    loop {
        let next = match due {
            Some(at) => inbox.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match next {
            Ok(Command::Watch(id, sock)) => {
                if seen_first.as_deref() == Some(id.as_str()) {
                    seen(&sock);
                    seen_first = None;
                }
                if !watched.iter().any(|(w, _)| *w == id) {
                    watched.push((id, sock));
                }
                let soon = Instant::now() + SETTLE;
                due = Some(due.map_or(soon, |d| d.min(soon)));
            }
            Err(RecvTimeoutError::Timeout) => {
                let readings: Vec<(String, Reading)> = std::thread::scope(|s| {
                    let reads: Vec<_> = watched
                        .iter()
                        .map(|(id, sock)| s.spawn(move || (id.clone(), read(sock))))
                        .collect();
                    reads.into_iter().filter_map(|r| r.join().ok()).collect()
                });
                for (id, reading) in readings {
                    match reading {
                        Reading::Said(Some(note)) => {
                            notes.insert(id, note);
                        }
                        Reading::Said(None) => {
                            notes.remove(&id);
                        }
                        Reading::Unknown => {}
                        Reading::Gone => {
                            notes.remove(&id);
                            watched.retain(|(w, _)| *w != id);
                        }
                    }
                }
                if outbox.send(notes.clone()).is_err() {
                    return;
                }
                due = Some(Instant::now() + POLL);
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: Vec<(&str, Value)>) -> Value {
        Value::Map(
            pairs
                .into_iter()
                .map(|(k, v)| (Value::from(k), v))
                .collect(),
        )
    }

    /// A session that accepts but never answers holds exit until the deadline;
    /// a missing one does not hold it at all.
    #[test]
    fn the_way_out_waits_for_seen_until_its_deadline_and_no_longer() {
        let path = crate::test_support::scratch_sock("notes-seen-deadline");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let started = Instant::now();
        let seeing = seen_begin(path.clone());
        let (_server, _) = listener.accept().expect("accept");
        seeing.wait_until(started + Duration::from_millis(150));
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(150) && took < Duration::from_secs(1),
            "waited {took:?} for a deadline 150 ms out"
        );
        let _ = std::fs::remove_file(&path);

        let gone = crate::test_support::scratch_sock("notes-seen-gone");
        let started = Instant::now();
        seen_begin(gone).wait_until(started + Duration::from_secs(5));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "waited {:?} for a session that is not there",
            started.elapsed()
        );
    }

    #[test]
    fn a_notification_comes_before_what_is_running() {
        let at = Instant::now();
        let record = map(vec![
            ("v", Value::from(VERSION)),
            (
                "busy",
                map(vec![
                    ("title", Value::from("claude")),
                    ("text", Value::from("working")),
                    ("age", Value::from(3)),
                ]),
            ),
            (
                "note",
                map(vec![
                    ("kind", Value::from("notify")),
                    ("title", Value::from("Claude Code")),
                    ("body", Value::from("Claude needs your permission")),
                    ("age", Value::from(120)),
                ]),
            ),
        ]);
        let note = note_of(&record, at).expect("a note");
        assert_eq!(note.kind, Kind::Notified);
        assert_eq!(note.title, "Claude Code");
        assert_eq!(note.text, "Claude needs your permission");
        assert_eq!(note.age, Duration::from_secs(120));
    }

    #[test]
    fn a_failure_is_kept_like_a_notification() {
        let record = map(vec![(
            "note",
            map(vec![
                ("kind", Value::from("failed")),
                ("title", Value::from("claude")),
                ("body", Value::from("exited with code 1")),
                ("age", Value::from(5)),
            ]),
        )]);
        let note = note_of(&record, Instant::now()).expect("a note");
        assert_eq!(note.kind, Kind::Failed);
        assert_eq!(note.text, "exited with code 1");
    }

    /// Both kept items, oldest first; an older watcher's items lack `seq` and
    /// `shown`.
    #[test]
    fn the_news_is_both_of_what_is_kept_by_number() {
        let record = map(vec![
            (
                "note",
                map(vec![
                    ("kind", Value::from("notify")),
                    ("title", Value::from("Claude Code")),
                    ("body", Value::from("Claude needs your permission")),
                    ("seq", Value::from(3)),
                    ("shown", Value::from(false)),
                    ("age", Value::from(12)),
                    ("buf", Value::from(5)),
                    ("tab", Value::from(2)),
                ]),
            ),
            (
                "finished",
                map(vec![
                    ("kind", Value::from("finished")),
                    ("title", Value::from("cargo test")),
                    ("body", Value::from("exited with code 0")),
                    ("seq", Value::from(2)),
                    ("shown", Value::from(true)),
                ]),
            ),
            ("busy", map(vec![("title", Value::from("claude"))])),
        ]);
        let news = news_of(&record);
        assert_eq!(
            news.iter()
                .map(|n| (n.kind, n.seq, n.shown))
                .collect::<Vec<_>>(),
            [
                (Kind::Finished, 2, Some(true)),
                (Kind::Notified, 3, Some(false))
            ]
        );
        assert_eq!(
            (news[1].title.as_str(), news[1].text.as_str()),
            ("Claude Code", "Claude needs your permission")
        );
        assert_eq!(
            (news[1].age, news[1].buf, news[1].tab),
            (12, Some(5), Some(2)),
            "how old, and the terminal it came from and where that is"
        );
        assert_eq!(
            (news[0].buf, news[0].tab),
            (None, None),
            "a record that says neither"
        );

        let older = map(vec![(
            "note",
            map(vec![
                ("kind", Value::from("failed")),
                ("body", Value::from("x")),
            ]),
        )]);
        assert_eq!(
            news_of(&older)
                .iter()
                .map(|n| (n.kind, n.seq, n.shown))
                .collect::<Vec<_>>(),
            [(Kind::Failed, 0, None)]
        );
        assert!(news_of(&Value::Boolean(false)).is_empty());
    }

    #[test]
    fn a_finished_command_comes_between_a_notification_and_what_is_running() {
        let finished = (
            "finished",
            map(vec![
                ("title", Value::from("cargo test")),
                ("body", Value::from("exited with code 0")),
                ("age", Value::from(30)),
            ]),
        );
        let busy = (
            "busy",
            map(vec![
                ("title", Value::from("claude")),
                ("age", Value::from(3)),
            ]),
        );
        let note =
            note_of(&map(vec![finished.clone(), busy.clone()]), Instant::now()).expect("a note");
        assert_eq!(note.kind, Kind::Finished);
        assert_eq!(
            (note.title.as_str(), note.text.as_str()),
            ("cargo test", "exited with code 0")
        );
        assert_eq!(note.age, Duration::from_secs(30));

        let notified = (
            "note",
            map(vec![
                ("kind", Value::from("notify")),
                ("body", Value::from("Claude needs your permission")),
                ("age", Value::from(5)),
            ]),
        );
        assert_eq!(
            note_of(&map(vec![finished, busy, notified]), Instant::now()).map(|n| n.kind),
            Some(Kind::Notified)
        );
    }

    /// Integer or float, capped at 100; 0 means none.
    #[test]
    fn busy_carries_its_percentage_when_it_has_one() {
        let busy = |percent: Option<Value>| {
            let mut fields = vec![("title", Value::from("lua_ls")), ("age", Value::from(1))];
            if let Some(p) = percent {
                fields.push(("percent", p));
            }
            note_of(&map(vec![("busy", map(fields))]), Instant::now()).map(|n| n.kind)
        };
        assert_eq!(busy(None), Some(Kind::Busy(None)));
        assert_eq!(busy(Some(Value::from(42))), Some(Kind::Busy(Some(42))));
        assert_eq!(busy(Some(Value::from(42.0))), Some(Kind::Busy(Some(42))));
        assert_eq!(busy(Some(Value::from(250))), Some(Kind::Busy(Some(100))));
        assert_eq!(busy(Some(Value::from(0))), Some(Kind::Busy(None)));
    }

    #[test]
    fn a_record_with_nothing_in_it_is_no_note() {
        assert_eq!(
            note_of(&map(vec![("v", Value::from(VERSION))]), Instant::now()),
            None
        );
        assert_eq!(note_of(&Value::Boolean(false), Instant::now()), None);
    }

    #[test]
    fn control_characters_never_reach_the_picker() {
        let record = map(vec![(
            "note",
            map(vec![
                ("kind", Value::from("notify")),
                ("title", Value::from("\u{1b}]52;c;aGk=\u{7}x")),
                ("body", Value::from("a\u{9c}b\u{7f}c\nd")),
                ("age", Value::from(-4)),
            ]),
        )]);
        let note = note_of(&record, Instant::now()).expect("a note");
        assert!(
            !note.title.chars().any(char::is_control),
            "{:?}",
            note.title
        );
        assert_eq!(note.text, "a b c d");
        assert_eq!(note.age, Duration::ZERO, "a clock that went back");
    }

    #[test]
    fn a_note_ages_from_when_it_was_read() {
        let read_at = Instant::now() - Duration::from_secs(10);
        let note = Note {
            kind: Kind::Notified,
            title: String::new(),
            text: String::new(),
            age: Duration::from_secs(50),
            read_at,
        };
        assert!(note.age_now() >= Duration::from_secs(60));
    }
}
