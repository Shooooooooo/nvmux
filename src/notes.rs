//! What a session has to report to the picker: that something in it is busy or
//! has failed, and the desktop notifications the programs in it send.
//!
//! # Where it comes from
//!
//! An agent in a terminal buffer tells its terminal when it wants you — Claude
//! Code sends `ESC ] 777 ; notify ; Claude Code ; Claude needs your
//! permission` — and a shell there can say how each command went: fish marks
//! where one starts and ends, and with what exit status (OSC 133). A plugin
//! that knows it is working says so with a progress message, and a language
//! server with LSP's own progress: clangd's `indexing`, say. None of it
//! reaches nvmux by itself. Neovim's terminal keeps what a program sends to
//! itself: it fires `TermRequest`, and `TermClose`, with the exit code, when
//! the program ends, and that is all. Its LSP client does the same with a
//! server's progress: it fires `LspProgress`, and makes no progress message
//! of it. And a session started `--headless` never draws its progress
//! messages as a terminal's progress bar (OSC 9;4), because Neovim sets that
//! up at startup only when a terminal UI is already attached, which a
//! server's never is. Whatever a session does send its UIs goes to the ones
//! attached at that moment, which for a session nobody has visited since
//! nvmux started is none.
//!
//! So the session keeps the record itself. nvmux leaves a watcher in it
//! ([`WATCHER`]): one augroup, `nvmux_notes`, with a `Progress` autocommand —
//! every progress message but Neovim's own, on 0.12 and later — an
//! `LspProgress` one, for what each language server says it is working on, a
//! `TermRequest` one, which takes the three notifications terminals know —
//! OSC 777 `notify` (Ghostty's, urxvt's), OSC 9 (iTerm2's) and OSC 99
//! (kitty's, sent in parts) — and a shell's word that a command failed, and a
//! `TermClose` one, for a terminal's program that exits: with a code other
//! than 0, it failed, and with 0, it finished. The picker reads it over plain
//! RPC ([`read`]), the way it already asks a session whether it is alive. A
//! program's own OSC 9;4 is not read: an agent's is no guide to whether it
//! wants you — Claude Code's stays busy through its permission prompts — and
//! what the session's progress messages say is.
//!
//! # It stays
//!
//! The agent nvmux's own client leaves in a session goes when that client
//! does (see [`crate::client`]). The watcher stays when nvmux leaves, because
//! recording while nobody is looking is what it is for. It sets no option,
//! maps no key, opens no window and prints nothing, and what it runs on an
//! event is a string match and a table write.
//!
//! # Read, not waited on
//!
//! A read asks the mode first, as [`crate::rpc::probe`] does — a fast call,
//! answered even at a hit-enter prompt — and does not wait on the deferred one
//! behind it when the editor is blocked, so a session waiting for a key keeps
//! the note it had rather than holding anything up. The picker's reads run on
//! a thread of their own ([`Poller`]), every [`POLL`] while it is up.
//!
//! # Seen
//!
//! A notification is news until its session has been looked at: nvmux clears
//! it ([`SEEN`]) when a session comes to the front ([`touch`]) and again when
//! it leaves ([`seen`]) — in the session, so that another nvmux, on another
//! machine, stops showing it too. A failure is kept the same way, and so,
//! apart from them, is a program that has finished: news too, but asking
//! nothing of you, so that a notification or a failure comes before it.
//! Whether something is busy is not news but state, and is shown for as long
//! as it lasts.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use rmpv::Value;

use crate::error::RpcError;
use crate::rpc;

/// The watcher's version. A session holding an older one is given this one,
/// the record it kept carried over; a newer nvmux does the same to this one.
pub const VERSION: i64 = 5;

/// How long the picker waits between reads of its sessions.
pub const POLL: Duration = Duration::from_secs(2);

/// What one read may take, from the connect to the answer. A session that is
/// slower than this keeps the note it had until the next read.
const BUDGET: Duration = Duration::from_secs(1);

/// How long the poller waits after a session is handed to it before reading,
/// so that the sessions of one listing, handed over one by one, are read in
/// one round of reads.
const SETTLE: Duration = Duration::from_millis(30);

/// The watcher, sent with `nvim_exec_lua` and its version as the argument.
/// Installing it again replaces the autocommands and keeps the record, but
/// for what the language servers were doing, which starts afresh: another
/// version's watcher, in the meantime, may have missed the end of it. Its
/// answer is the record, as [`READ`] gives it.
///
/// What it records: each progress message running, by source and id, with its
/// title, text, percentage — 0 is no percentage, which is what Neovim 0.12
/// gives a message that has none — and when it started; each language
/// server's work the same way, by client and token, its server's name for the
/// title and what it says it is doing for the text, `indexing 3/120` — a
/// report gives only what has changed, so what it leaves out stands — until
/// it ends or the server goes, which a read sees to, since a server that dies
/// mid-work never says it has finished; the last notification or failure — a
/// failed progress message, a terminal's program that exits with a code other
/// than 0, or a command its shell says did — and, apart from it, the last
/// terminal's program to exit with 0, each until [`SEEN`] clears it. Text is
/// kept without control characters and to 200 characters.
///
/// A failed command is named by what its shell said it was, where the shell
/// says — fish's `OSC 133 ; C` carries the command line, URL-encoded — and
/// otherwise by the terminal's program, from its buffer name
/// (`term://…//1234:cargo build`) and without its directory: `exited with
/// code 101`, or `a command exited with code 101` when it is the shell's.
///
/// A finished program is named the same way, from its terminal's buffer name:
/// `exited with code 0`. It has to be a job's: a terminal a plugin draws into
/// itself (`nvim_open_term`) closes with 0 too, though nothing ran in it. A
/// terminal whose buffer is deleted while its program runs closes with -1,
/// which is neither a failure nor a finish, and a command its shell says
/// succeeded is not kept at all: at a prompt, nearly every one does.
///
/// Neovim's own progress messages, `source` `nvim`, are left out: a write's
/// and a completion's scan begin and end within the command that says them,
/// so a read never finds one running — and a write that fails never says it
/// has ended, nor does a scan, which would leave the session busy for good.
/// An older watcher kept them, so they go from the record on the way in.
const WATCHER: &str = r#"local version = ...
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
local function note(kind, title, body)
  title, body = clean(title), clean(body)
  if title ~= '' or body ~= '' then
    N.note = { kind = kind, title = title, body = body, at = os.time() }
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
      return note('notify', title, body)
    end
    local mark, args = osc:match('^133;(%a)(.*)$')
    if mark == 'C' then
      local url = args:match(';cmdline_url=([^;]*)')
      commands[ev.buf] = url and vim.uri_decode(url) or nil
      return
    elseif mark == 'D' then
      local status = tonumber(args:match('^;(%d+)'))
      local command = commands[ev.buf]
      commands[ev.buf] = nil
      if status and status > 0 then
        if command then
          note('failed', command, 'exited with code ' .. status)
        else
          note('failed', program(ev.buf), 'a command exited with code ' .. status)
        end
      end
      return
    end
    local text = osc:match('^9;(.*)$')
    if text then
      if not text:match('^%d+;') and not text:match('^%d+$') then
        note('notify', '', text)
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
        note('notify', p.title, p.body)
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
      note('failed', program(ev.buf), 'exited with code ' .. status)
    elseif status == 0 and ran(ev.buf) then
      N.finished = { title = clean(program(ev.buf)), body = 'exited with code 0', at = os.time() }
    end
  end,
})
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
  if N.note then
    out.note = { kind = N.note.kind, title = N.note.title, body = N.note.body, age = now - N.note.at }
  end
  if N.finished then
    out.finished = { title = N.finished.title, body = N.finished.body, age = now - N.finished.at }
  end
  return out
end
function N.seen()
  N.note = nil
  N.finished = nil
end
return N.snapshot()
"#;

/// The record, as a map: `busy` (the progress message, or language server's
/// work, running that started last: `title`, `text`, `percent` when it has
/// one, and `age` in seconds), `note` (`kind` `notify` or `failed`, `title`,
/// `body`, `age`) and `finished` (the program that finished last: `title`,
/// `body`, `age`), each there only when the session has one. `false` where
/// there is no watcher, or one older or newer than this nvmux's.
const READ: &str = r#"local version = ...
local N = package.loaded['nvmux.notes']
if type(N) ~= 'table' or N.version ~= version or type(N.snapshot) ~= 'function' then
  return false
end
return N.snapshot()
"#;

/// Clear what the session was keeping: the notification, or failure, and the
/// program that finished.
const SEEN: &str = r#"local N = package.loaded['nvmux.notes']
if type(N) == 'table' and type(N.seen) == 'function' then
  N.seen()
end
return true
"#;

/// What a note is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A program in a terminal buffer asked for a desktop notification.
    Notified,
    /// A progress message ended in failure, or a command in a terminal buffer
    /// did.
    Failed,
    /// A program in a terminal buffer finished: it exited with 0.
    Finished,
    /// A progress message, or a language server's work, is running, and with
    /// a percentage where it says how far it has got.
    Busy(Option<u8>),
}

/// The one thing a session has to say: a notification or a failure, which
/// wait for you, then a program that has finished, and then anything running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub kind: Kind,
    /// The notification's title, or the progress message's: the program or
    /// plugin it came from, as often as not — `Claude Code`, `claude` — and
    /// for a language server's work, the server's name, `clangd`; for a
    /// command that failed, or a program that finished, the command:
    /// `cargo build`.
    pub title: String,
    /// The notification's body, the progress message's text, what the
    /// language server says it is doing — `indexing 3/120` — or how a command
    /// ended: `exited with code 101`.
    pub text: String,
    /// How old it was when it was read, by the session's own clock, which a
    /// remote host's need not agree with this one's on.
    pub age: Duration,
    /// When it was read, by this one's: the time since, added to `age`, is how
    /// old it is now (see [`Note::age_now`]).
    pub read_at: Instant,
}

impl Note {
    /// How old it is now.
    pub fn age_now(&self) -> Duration {
        self.age + self.read_at.elapsed()
    }
}

/// What a read of one session came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Reading {
    /// Its note, or that it has none.
    Said(Option<Note>),
    /// It could not say just now: waiting for a key, busy, or slow. The note
    /// it had stands.
    Unknown,
    /// Nothing is listening on its socket any more.
    Gone,
}

/// Read what the session on `sock` has to say, giving it the watcher first
/// where it has none, or another version's.
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

/// A session coming to the front: the watcher put in where it is missing, and
/// what it kept cleared, being looked at now. Best effort, and quiet: a
/// session that cannot answer now gets both on the next visit or listing.
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

/// A session that has just been looked at: what it kept, cleared. Best
/// effort, like [`touch`].
pub fn seen(sock: &Path) {
    if let Err(e) = unblocked(sock, SEEN) {
        tracing::debug!(error = %e, "notes: could not clear");
    }
}

/// [`touch`] or [`seen`] on a thread of its own, so that nothing waits for a
/// round trip a session's switch has no use for.
pub fn later(sock: PathBuf, what: fn(&Path)) {
    let spawned = std::thread::Builder::new()
        .name("nvmux-notes-seen".into())
        .spawn(move || what(&sock));
    if let Err(e) = spawned {
        tracing::debug!(error = %e, "notes: no thread to clear on");
    }
}

/// [`seen`], waited for, but no longer than `within`: for the way out of
/// nvmux, which would otherwise end the thread before it had said anything.
pub fn seen_within(sock: PathBuf, within: Duration) {
    let (done, finished) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("nvmux-notes-seen".into())
        .spawn(move || {
            seen(&sock);
            let _ = done.send(());
        });
    if spawned.is_ok() {
        let _ = finished.recv_timeout(within);
    }
}

/// The record, with the watcher installed first where it is missing or is
/// another version's; `None` while the editor waits for a key, which is when
/// a deferred call would not be answered.
///
/// The read is asked with the mode, before either is waited for, so the two
/// cost one round trip. Left unread when the editor is blocked: the session
/// runs it once the key comes, which costs nothing, since all a read writes
/// is to forget a language server that has gone, as true then as now — and
/// the watcher, which writes the record, is only sent once it has answered.
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

/// Run `code` in the session on `sock`, unless its editor is waiting for a
/// key; then it runs once the key comes, if it is still connected by then.
fn unblocked(sock: &Path, code: &'static str) -> Result<(), RpcError> {
    let mut client = rpc::Client::connect(sock, BUDGET)?;
    let mode = client.ask_mode()?;
    let asked = client.ask("nvim_exec_lua", lua(code))?;
    if client.mode_reply(mode)?.blocking {
        return Ok(());
    }
    client.reply_to(asked).map(drop)
}

/// The arguments of `nvim_exec_lua` for one of the chunks above: the chunk,
/// and the version as its one argument.
fn lua(code: &str) -> Vec<Value> {
    vec![Value::from(code), Value::Array(vec![Value::from(VERSION)])]
}

/// The note a record comes to, if any: the notification or failure it keeps
/// first, since those wait for you, then the program that finished, and then
/// whatever is running.
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

/// A key of a msgpack map.
fn field<'a>(map: &'a Value, key: &str) -> Option<&'a Value> {
    map.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// A string of the record, fit to be drawn: the watcher already keeps control
/// characters out, and a cell holding one would hand the terminal a sequence
/// of nvmux's choosing — or rather of whoever wrote the notification — so the
/// picker does not take that on trust.
fn text(v: Option<&Value>) -> String {
    let s = v.and_then(Value::as_str).unwrap_or_default();
    let clean: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    clean.trim().to_string()
}

/// A count of the record — Lua's numbers reach msgpack as integers when they
/// are whole, and as floats otherwise. Never negative: a clock that went back
/// says nothing about how long ago.
fn number(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
}

fn seconds(v: Option<&Value>) -> Duration {
    Duration::from_secs(v.and_then(number).unwrap_or(0))
}

/// What the picker hands its sessions to.
enum Command {
    /// Read this session, on this socket, from now on.
    Watch(String, PathBuf),
}

/// The picker's reads, on a thread of their own: every session it is handed
/// is read at once, then every [`POLL`], all of them at a time, and what they
/// say comes back as a map from session id to note.
///
/// Dropping it stops the thread without waiting for it: a read in flight, its
/// budget at most a second, finishes to nobody. Nothing is joined, so leaving
/// the picker never waits on a session.
pub struct Poller {
    commands: Sender<Command>,
    updates: Receiver<HashMap<String, Note>>,
}

impl Poller {
    /// Start reading. `seen` is the session the picker was opened from, which
    /// has just been looked at: it is cleared before it is first read.
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

    /// Read session `id` through `sock` from now on.
    pub fn watch(&self, id: &str, sock: PathBuf) {
        let _ = self.commands.send(Command::Watch(id.to_string(), sock));
    }

    /// What the sessions said, if they have said anything since this was last
    /// asked: the latest of it, every session at once.
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

    /// A notification or a failure waits for you, so it is the note even
    /// while something else in the session runs.
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

    /// A program that finished is news, so it comes before what is running;
    /// but it asks nothing of you, so a notification comes before it.
    #[test]
    fn a_finished_program_comes_between_a_notification_and_what_is_running() {
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

    /// Lua's numbers are whole or not: a percentage arrives either way, is
    /// held to 100, and 0 is no percentage at all.
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

    /// The picker draws what it is given straight into cells, so nothing in a
    /// notification may reach the terminal as a control character, whatever
    /// the watcher let through.
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
