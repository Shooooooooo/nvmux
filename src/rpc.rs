//! A minimal synchronous msgpack-RPC client for Neovim.
//!
//! Never `nvim_ui_attach` through this client (see [`Client::list_uis`]): the
//! UI is [`crate::client`] (or `nvim --remote-ui`) on the pty, in its own
//! process and channel.
//!
//! Synchronous because Neovim sends nothing unsolicited on a bare RPC channel;
//! only UI or autocmd subscribers get notifications. The one listener channel
//! is read by [`Client::next_notification`]. [`Client::ask`] pipelines
//! requests so two calls cost one round trip (the whole delay over SSH); a
//! reply arriving while another is awaited is parked for its own waiter.
//!
//! # Reachable is not ready
//!
//! `nvim_get_api_info` is answered off the main loop: it replies in 3 ms while
//! the editor is blocked in `call system('sleep 3')` and `nvim_list_bufs`
//! times out. So:
//!
//! * `api_info` answering proves a real Neovim, not an SSH forward that
//!   accepts and resets.
//! * only a deferred call answering proves the session is serving.
//! * a deferred call timing out is [`Liveness::Busy`] (e.g. `:!make`), never
//!   death, or live sessions would be reaped.
//!
//! # Fast calls and the hit-enter prompt
//!
//! `FUNC_API_FAST` functions (`nvim_get_api_info`, `nvim_get_mode`,
//! `nvim_input`) run from the socket read callback; the rest queue for the main
//! loop. At a hit-enter prompt that queue is off, so deferred calls
//! (`nvim_list_bufs`, `nvim_list_uis`, `nvim_command`, `nvim_eval`) get no
//! answer until it ends (0.12.5: `nvim_get_mode` answers in 0.1 ms with
//! `blocking = true`). With `'cmdheight'` 0 (AstroNvim) any one-line error
//! opens it; with 1, any message that scrolls. So [`probe`] and the attach
//! probe (`pty::probe_on`) read the mode before anything deferred, and attach
//! and resume in [`crate::pty`] send `<CR>` first. `nvim_get_mode` is queued
//! too during `:!cmd` and CPU-bound Lua, so every call still has a budget.

use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use rmpv::Value;

use crate::error::RpcError;
use crate::session::Liveness;

/// msgpack-rpc message kinds.
const REQUEST: u64 = 0;
const RESPONSE: u64 = 1;
const NOTIFICATION: u64 = 2;

/// Short enough that a dead forward does not stall the picker.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

/// Read timeout for probes nobody watches, e.g. waiting for a new session. 3 s
/// because a deferred call blocks for any `system()` in the editor, and a
/// shorter budget marks healthy sessions dead. The attach probe has none (the
/// user can cancel; see [`crate::pty::Probe`]); listings use
/// [`QUICK_PROBE_TIMEOUT`].
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Read timeout for a listing's probes. An idle editor answers in 1-2 ms; one
/// that cannot is Busy either way, and Busy is never reaped, so a longer wait
/// would only catch a session dying meanwhile (listed until the next listing).
pub const QUICK_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// An RPC channel over any byte stream: a local socket, an SSH forward's local
/// end, or a pipe in tests.
pub struct Client<S: Read + Write> {
    io: BufReader<S>,
    next_msgid: u32,
    /// Once set, every call fails fast. See [`Client::poison`].
    poisoned: Option<String>,
    /// So timeout errors report the real budget. `None`: no budget; a call
    /// waits until answered or the socket is shut down (see [`Interrupt`]).
    read_timeout: Option<Duration>,
    /// Msgids sent and not yet read. Only decides which stray reply to park,
    /// so an id left by an abandoned [`Pending`] is harmless.
    in_flight: Vec<u32>,
    /// Replies that arrived while another was awaited, kept for their waiters.
    /// Bounded by `in_flight`.
    parked: Vec<(u32, Result<Value, String>)>,
}

/// A sent request whose reply is unread, from [`Client::ask`] for
/// [`Client::reply_to`]. Not `Clone`: a second read would take the next
/// call's reply.
///
/// Dropping one is harmless here, but the request was sent, so Neovim still
/// runs it. See `pty::probe_on`, which abandons a `nvim_list_uis`.
#[derive(Debug)]
#[must_use = "a request that is sent and never read leaves its reply on the socket"]
pub struct Pending {
    msgid: u32,
    /// For error messages. Method names are literals, so no allocation.
    method: &'static str,
}

/// Needed by `Result::expect_err` in the tests; `Client` is never logged.
impl<S: Read + Write> std::fmt::Debug for Client<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("next_msgid", &self.next_msgid)
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

impl Client<UnixStream> {
    /// `read_timeout` bounds each reply, not the connect (a unix connect finds
    /// a listener or fails at once); a connect-sized value marks live sessions
    /// dead.
    pub fn connect(path: &Path, read_timeout: Duration) -> Result<Self, RpcError> {
        Self::connect_with(path, Some(read_timeout))
    }

    /// [`Client::connect`] with an optional budget. With `None` a call blocks
    /// until answered, and only an [`Interrupt`] from another thread ends it
    /// sooner: use it only for waits the user watches and can cancel.
    pub fn connect_with(path: &Path, read_timeout: Option<Duration>) -> Result<Self, RpcError> {
        // std's error here is an `InvalidInput` with no OS error to match on;
        // check first for a useful message.
        crate::paths::check_sock_path(path).map_err(|e| RpcError::Protocol(e.to_string()))?;

        let stream = UnixStream::connect(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::ConnectionRefused => {
                RpcError::ConnectionRefused(path.to_path_buf())
            }
            std::io::ErrorKind::NotFound => RpcError::ConnectionRefused(path.to_path_buf()),
            // A non-socket at the path: ECONNREFUSED on Linux, ENOTSOCK on
            // macOS/BSD. Nothing can listen there, so it is dead. Whether it
            // may be deleted is the reaper's call (`lstat`).
            _ if e.raw_os_error() == Some(libc::ENOTSOCK) => {
                RpcError::ConnectionRefused(path.to_path_buf())
            }
            _ => RpcError::Io(e),
        })?;
        stream.set_read_timeout(read_timeout)?;
        stream.set_write_timeout(read_timeout)?;
        let mut client = Self::new(stream);
        client.read_timeout = read_timeout;
        Ok(client)
    }

    /// Replace the budget, e.g. so a deferred call sent behind a fast one
    /// waits no longer than on its own connection.
    pub fn set_read_timeout(&mut self, read_timeout: Duration) -> Result<(), RpcError> {
        let stream = self.io.get_ref();
        stream.set_read_timeout(Some(read_timeout))?;
        stream.set_write_timeout(Some(read_timeout))?;
        self.read_timeout = Some(read_timeout);
        Ok(())
    }

    /// A handle for ending a call from another thread (see [`Interrupt`]).
    pub fn interrupt(&self) -> std::io::Result<Interrupt> {
        Ok(Interrupt(self.io.get_ref().try_clone()?))
    }

    /// Like [`Client::set_read_timeout`], but `None` removes the budget: for a
    /// channel kept open for notifications after its setup calls (see
    /// [`crate::notes::listen`]).
    pub fn set_budget(&mut self, budget: Option<Duration>) -> std::io::Result<()> {
        let stream = self.io.get_ref();
        stream.set_read_timeout(budget)?;
        stream.set_write_timeout(budget)?;
        self.read_timeout = budget;
        Ok(())
    }
}

/// Ends a call in progress on another thread.
///
/// A blocking read cannot be cancelled, but `shutdown` acts on the socket, not
/// the handle, so the other thread's read gets EOF at once and the call fails
/// as [`RpcError::Reset`]. Dropping a cloned handle would only close an fd.
/// The connection is unusable afterwards.
#[derive(Debug)]
pub struct Interrupt(UnixStream);

impl Interrupt {
    /// Errors are ignored: the caller is abandoning the connection anyway.
    pub fn fire(&self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

impl<S: Read + Write> Client<S> {
    pub fn new(stream: S) -> Self {
        Self {
            io: BufReader::new(stream),
            next_msgid: 1,
            poisoned: None,
            read_timeout: Some(PROBE_TIMEOUT),
            in_flight: Vec::new(),
            parked: Vec::new(),
        }
    }

    /// Mark the connection unusable: rmpv reads incrementally, so a call
    /// abandoned partway leaves bytes the next read would misdecode as a
    /// plausible value.
    fn poison(&mut self, why: impl Into<String>) -> RpcError {
        let why = why.into();
        self.poisoned = Some(why.clone());
        RpcError::Protocol(why)
    }

    /// Poison the connection if `e` may have left unread bytes (a timeout
    /// especially); `e` is returned unchanged.
    fn poison_if_fatal(&mut self, e: RpcError, method: &str) -> RpcError {
        match &e {
            RpcError::Timeout(_) | RpcError::Protocol(_) => {
                self.poisoned = Some(format!("{method}: {e}"));
            }
            // Already unusable; the caller tells these apart for liveness.
            _ => {}
        }
        e
    }

    pub fn call(&mut self, method: &'static str, params: Vec<Value>) -> Result<Value, RpcError> {
        let pending = self.ask(method, params)?;
        self.reply_to(pending)
    }

    /// Send a request without waiting, so two calls cost one round trip (over a
    /// 150 ms link, 151 ms instead of 302 ms). Replies are read with
    /// [`Client::reply_to`] in any order. Only `pty::probe_on` uses it.
    pub fn ask(&mut self, method: &'static str, params: Vec<Value>) -> Result<Pending, RpcError> {
        if let Some(why) = &self.poisoned {
            return Err(RpcError::Protocol(format!("connection poisoned: {why}")));
        }

        let msgid = self.next_msgid;
        self.next_msgid = self.next_msgid.wrapping_add(1);

        // [type, msgid, method, params]. The method must be a msgpack `str`,
        // not `bin` — rmpv's Value::String encodes as str, bytes would not.
        let request = Value::Array(vec![
            Value::from(REQUEST),
            Value::from(msgid),
            Value::String(method.into()),
            Value::Array(params),
        ]);

        // Encode, then write once: rmpv writes each marker separately, and on
        // a socket each is a syscall and a session wake-up (17 per notes read).
        // A refused write maps like a read error (a reset if hung up).
        let mut bytes = Vec::with_capacity(64);
        rmpv::encode::write_value(&mut bytes, &request)
            .map_err(|e| self.poison(format!("encoding {method}: {e}")))?;
        let sent = self.io.get_mut().write_all(&bytes);
        if let Err(e) = sent.and_then(|()| self.io.get_mut().flush()) {
            let mapped = map_io(e, self.read_timeout);
            return Err(self.poison_if_fatal(mapped, method));
        }
        self.in_flight.push(msgid);
        Ok(Pending { msgid, method })
    }

    /// Wait for the reply to a request [`Client::ask`] sent. The budget covers
    /// this wait, not the time since the request.
    pub fn reply_to(&mut self, pending: Pending) -> Result<Value, RpcError> {
        let Pending { msgid, method } = pending;
        if let Some(why) = &self.poisoned {
            return Err(RpcError::Protocol(format!("connection poisoned: {why}")));
        }
        if let Some(i) = self.parked.iter().position(|(id, _)| *id == msgid) {
            let (_, result) = self.parked.remove(i);
            self.forget(msgid);
            return result.map_err(RpcError::Nvim);
        }

        // An overall deadline too: a peer sending frames we skip would restart
        // the per-read timeout each time. No budget, no deadline.
        let budget = self.read_timeout;
        let started = std::time::Instant::now();

        loop {
            if let Some(budget) = budget {
                if started.elapsed() >= budget {
                    return Err(self.poison(format!("{method} exceeded its {budget:?} budget")));
                }
            }
            let decoded = rmpv::decode::read_value(&mut self.io).map_err(|e| match e {
                rmpv::decode::Error::InvalidMarkerRead(io)
                | rmpv::decode::Error::InvalidDataRead(io) => map_io(io, budget),
                other => RpcError::Protocol(format!("decoding a reply to {method}: {other}")),
            });
            let value = match decoded {
                Ok(v) => v,
                Err(e) => return Err(self.poison_if_fatal(e, method)),
            };

            match self.classify(value, msgid)? {
                Some(result) => {
                    self.forget(msgid);
                    return result.map_err(RpcError::Nvim);
                }
                None => continue,
            }
        }
    }

    /// The reply was delivered; a late duplicate is now a stray, not parked.
    fn forget(&mut self, msgid: u32) {
        self.in_flight.retain(|id| *id != msgid);
    }

    /// `Some` if the frame answers `want`. Other replies we are owed are
    /// parked; anything else is ignored.
    #[allow(clippy::type_complexity)]
    fn classify(
        &mut self,
        value: Value,
        want: u32,
    ) -> Result<Option<Result<Value, String>>, RpcError> {
        let arr = match value.as_array() {
            Some(a) => a,
            None => return Err(self.poison("top-level frame was not an array")),
        };

        match arr.first().and_then(Value::as_u64) {
            Some(RESPONSE) => {
                if arr.len() != 4 {
                    return Err(self.poison(format!("response had {} elements, want 4", arr.len())));
                }
                let got = arr[1]
                    .as_u64()
                    .ok_or_else(|| self.poison("response msgid was not an integer"))?;
                let result = if arr[2].is_nil() {
                    Ok(arr[3].clone())
                } else {
                    Err(describe_nvim_error(&arr[2]))
                };
                if got == u64::from(want) {
                    return Ok(Some(result));
                }
                // Park it if we are owed it; otherwise it is a stray (a
                // duplicate, or a peer answering unasked questions).
                let ours = u32::try_from(got).is_ok_and(|id| {
                    self.in_flight.contains(&id) && !self.parked.iter().any(|(p, _)| *p == id)
                });
                if ours {
                    self.parked.push((got as u32, result));
                } else {
                    tracing::debug!(got, want, "skipping a response for another request");
                }
                Ok(None)
            }
            Some(NOTIFICATION) => Ok(None),
            Some(REQUEST) => {
                // Neovim asking *us* something. We register no handlers.
                tracing::debug!("ignoring an inbound request from nvim");
                Ok(None)
            }
            other => Err(self.poison(format!("unknown msgpack-rpc message type {other:?}"))),
        }
    }

    /// Wait for the next notification (e.g. a session watcher's, see
    /// [`crate::notes::listen`]) and return its method and args. Replies
    /// meanwhile are parked; requests are not answered. Bounded by the
    /// connection's budget, if any.
    pub fn next_notification(&mut self) -> Result<(String, Vec<Value>), RpcError> {
        if let Some(why) = &self.poisoned {
            return Err(RpcError::Protocol(format!("connection poisoned: {why}")));
        }
        let budget = self.read_timeout;
        loop {
            let decoded = rmpv::decode::read_value(&mut self.io).map_err(|e| match e {
                rmpv::decode::Error::InvalidMarkerRead(io)
                | rmpv::decode::Error::InvalidDataRead(io) => map_io(io, budget),
                other => RpcError::Protocol(format!("decoding a notification: {other}")),
            });
            let value = match decoded {
                Ok(v) => v,
                Err(e) => return Err(self.poison_if_fatal(e, "a notification")),
            };
            let notified = value
                .as_array()
                .filter(|a| a.first().and_then(Value::as_u64) == Some(NOTIFICATION));
            if let Some(a) = notified {
                let method = a.get(1).and_then(Value::as_str).unwrap_or_default();
                let args = a.get(2).and_then(Value::as_array).cloned();
                return Ok((method.to_string(), args.unwrap_or_default()));
            }
            // Msgids start at 1, so `want = 0` matches no reply.
            self.classify(value, 0)?;
        }
    }

    /// `nvim_get_api_info`: reachability and version. Answered off the main
    /// loop, so it does not prove the session is usable (see module docs).
    pub fn api_info(&mut self) -> Result<crate::nvim::Version, RpcError> {
        let v = self.call("nvim_get_api_info", vec![])?;
        let arr = v
            .as_array()
            .ok_or_else(|| RpcError::Protocol("api_info was not an array".into()))?;
        // Element 0 is our channel id (unused). Requiring an integer makes a
        // non-nvim peer fail here, cleanly.
        if arr.first().and_then(Value::as_u64).is_none() {
            return Err(RpcError::Protocol("api_info had no channel id".into()));
        }
        let meta = arr
            .get(1)
            .and_then(Value::as_map)
            .ok_or_else(|| RpcError::Protocol("api_info had no metadata map".into()))?;
        let version = meta
            .iter()
            .find(|(k, _)| k.as_str() == Some("version"))
            .and_then(|(_, v)| v.as_map())
            .ok_or_else(|| RpcError::Protocol("api_info had no version".into()))?;

        let field = |name: &str| -> Option<u64> {
            version
                .iter()
                .find(|(k, _)| k.as_str() == Some(name))
                .and_then(|(_, v)| v.as_u64())
        };

        Ok(crate::nvim::Version {
            major: field("major").unwrap_or(0),
            minor: field("minor").unwrap_or(0),
            patch: field("patch").unwrap_or(0),
            // The map has `prerelease` as a bool and the dev tag under
            // `build`; neither is needed here.
            prerelease: None,
        })
    }

    /// `nvim_list_bufs`: a deferred call, so the real readiness test.
    pub fn list_bufs(&mut self) -> Result<Vec<Value>, RpcError> {
        let v = self.call("nvim_list_bufs", vec![])?;
        Ok(v.as_array()
            .ok_or_else(|| RpcError::Protocol("list_bufs was not an array".into()))?
            .to_vec())
    }

    /// Number of attached UIs, checked before attaching: Neovim's
    /// `ui_attach_impl` calls `abort()` at `MAX_UI_COUNT` (16), so a 17th
    /// attach kills the server.
    ///
    /// Also why nvmux never attaches a preview UI: Neovim sizes the grid to the
    /// smallest attached UI, shrinking the one being edited in.
    pub fn list_uis(&mut self) -> Result<usize, RpcError> {
        let pending = self.ask_uis()?;
        self.uis_reply(pending)
    }

    /// [`Client::list_uis`] without waiting. The attach probe reads it only
    /// once the mode shows deferred calls are served (see `pty::probe_on`).
    pub fn ask_uis(&mut self) -> Result<Pending, RpcError> {
        self.ask("nvim_list_uis", vec![])
    }

    pub fn uis_reply(&mut self, pending: Pending) -> Result<usize, RpcError> {
        let v = self.reply_to(pending)?;
        Ok(v.as_array().map(|a| a.len()).unwrap_or(0))
    }

    /// `nvim_command`: used for the `:Detach` alias and for `:mode`, the one
    /// command that clears and repaints the grid.
    pub fn command(&mut self, cmd: &str) -> Result<(), RpcError> {
        self.call("nvim_command", vec![Value::String(cmd.into())])?;
        Ok(())
    }

    /// `nvim_eval`, deferred like [`Client::command`]. Used on resume for the
    /// mouse setting.
    pub fn eval(&mut self, expr: &str) -> Result<Value, RpcError> {
        self.call("nvim_eval", vec![Value::String(expr.into())])
    }

    /// `nvim_get_mode`: fast, so answered even at a prompt.
    pub fn get_mode(&mut self) -> Result<Mode, RpcError> {
        let pending = self.ask_mode()?;
        self.mode_reply(pending)
    }

    /// [`Client::get_mode`] without waiting. Being fast, it is answered even
    /// with a deferred call queued behind it, as the attach probe does.
    pub fn ask_mode(&mut self) -> Result<Pending, RpcError> {
        self.ask("nvim_get_mode", vec![])
    }

    pub fn mode_reply(&mut self, pending: Pending) -> Result<Mode, RpcError> {
        let v = self.reply_to(pending)?;
        let map = v
            .as_map()
            .ok_or_else(|| RpcError::Protocol("get_mode was not a map".into()))?;
        let field = |name: &str| {
            map.iter()
                .find(|(k, _)| k.as_str() == Some(name))
                .map(|(_, v)| v)
        };
        let mode = field("mode")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::Protocol("get_mode had no mode".into()))?
            .to_string();
        // Missing reads as not blocking (the key exists since 0.2): erring
        // that way costs a wait, the other way could end a prompt.
        let blocking = field("blocking").and_then(Value::as_bool).unwrap_or(false);
        Ok(Mode { mode, blocking })
    }

    /// `nvim_input`: a fast call, so keys are queued at once even at a prompt.
    /// nvmux only sends a lone `<CR>` to a hit-enter prompt (see
    /// [`Mode::at_hit_enter`]); integration tests send more.
    pub fn input(&mut self, keys: &str) -> Result<(), RpcError> {
        // The reply (bytes queued) is not useful.
        self.call("nvim_input", vec![Value::String(keys.into())])?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mode {
    /// As `mode(1)` spells it: `n`, `i`, `c`, `r` (hit-enter prompt), `rm`
    /// (more-prompt), `r?` (`:confirm`), etc.
    pub mode: String,
    /// Waiting for a key with the event queue off: deferred calls are not
    /// served until it arrives.
    ///
    /// Measured on 0.12.5 with a UI attached: true at a hit-enter prompt (`r`),
    /// a more-prompt (`rm`) and after a half-typed `g` (`n`); false for a
    /// pending operator (`no`), `:confirm` (`r?`) and `getchar()`, which all
    /// serve deferred calls. Narrower than "waiting", but reliable when set.
    pub blocking: bool,
}

impl Mode {
    /// The hit-enter prompt only: mode `r` while blocking.
    ///
    /// The one prompt nvmux ends for the user: `wait_return()` consumes `<CR>`
    /// (other keys run as Normal-mode commands) and its message is already
    /// painted over. Not the more-prompt (`<CR>` scrolls), `:confirm` (`<CR>`
    /// answers it; only the mode string excludes it, as `blocking` is false),
    /// or a pending multi-key command in `n`/`no` (`<CR>` completes it).
    ///
    /// Advisory: a key from another UI just after this answer can end the
    /// prompt first, and the `<CR>` then runs in whatever mode follows.
    pub fn at_hit_enter(&self) -> bool {
        self.blocking && self.mode == "r"
    }
}

/// Decode a Neovim handle (buffer, window, tabpage) into its integer id.
///
/// Handles are msgpack EXT values whose payload is a msgpack-encoded integer:
/// reading `data[0]` breaks from 128 (buffer 128 decodes as 204, 0xcc being
/// the `uint8` marker).
///
/// For display only: pass handles back as the original [`Value::Ext`], since a
/// wrong integer such as 0 ("current buffer") answers plausibly. Unused outside
/// tests; kept as the reference for future handle-based calls.
#[cfg(test)]
fn ext_to_handle(v: &Value) -> Option<i64> {
    match v {
        Value::Ext(_type_code, data) => rmpv::decode::read_value(&mut &data[..])
            .ok()
            .and_then(|v| v.as_i64()),
        // Plain integers too, for tests and any future plain-integer API.
        other => other.as_i64(),
    }
}

fn describe_nvim_error(v: &Value) -> String {
    // Errors arrive as [code, message].
    if let Some(arr) = v.as_array() {
        if let Some(msg) = arr.get(1).and_then(Value::as_str) {
            return msg.to_string();
        }
    }
    v.to_string()
}

fn map_io(e: std::io::Error, timeout: Option<Duration>) -> RpcError {
    match e.kind() {
        // A read timeout is one of these, by platform. Never without a budget
        // (the socket is never non-blocking), so the zero default is unseen.
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
            RpcError::Timeout(timeout.unwrap_or_default())
        }
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof => RpcError::Reset,
        std::io::ErrorKind::BrokenPipe => RpcError::Reset,
        _ => RpcError::Io(e),
    }
}

/// Whether a session socket has a live, serving Neovim behind it.
///
/// **Only `Dead` leads to files being deleted, so it must mean "proved nothing
/// is listening", never "no answer".** Everything else is [`Liveness::Busy`],
/// which is never reaped. Even `api_info` goes unanswered during CPU-bound Lua
/// (measured 3.7 s).
pub fn probe(path: &Path) -> Liveness {
    probe_within(path, PROBE_TIMEOUT)
}

/// [`probe`] with `budget` per call instead of [`PROBE_TIMEOUT`].
pub fn probe_within(path: &Path, budget: Duration) -> Liveness {
    let mut client = match Client::connect(path, budget) {
        Ok(c) => c,
        Err(e) if e.is_definitely_dead() => return Liveness::Dead,
        Err(e) => {
            // EACCES, EMFILE, a bad path: nothing about the session. Treating
            // fd exhaustion as death would reap every session at once.
            tracing::warn!(path = %path.display(), error = %e, "probe inconclusive");
            return Liveness::Busy;
        }
    };

    match client.api_info() {
        Ok(_) => {}
        Err(e) if e.is_definitely_dead() => return Liveness::Dead,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "api_info did not answer");
            return Liveness::Busy;
        }
    }

    // The fast mode call first: at a hit-enter prompt a deferred call would
    // hold the picker for the whole budget. Blocking is Busy, like `:!make`:
    // reachable, never reaped.
    match client.get_mode() {
        Ok(mode) if mode.blocking => {
            tracing::debug!(
                path = %path.display(),
                mode = %mode.mode,
                "waiting for a key; deferred calls are not served"
            );
            return Liveness::Busy;
        }
        Ok(_) => {}
        Err(e) if e.is_definitely_dead() => return Liveness::Dead,
        Err(e) => {
            // Not answered even though it is fast: `:!cmd` or CPU-bound Lua.
            tracing::debug!(path = %path.display(), error = %e, "get_mode did not answer");
            return Liveness::Busy;
        }
    }

    match client.list_bufs() {
        Ok(_) => Liveness::Alive,
        Err(e) if e.is_definitely_dead() => Liveness::Dead,
        // Reachable but not answering: likely `:!make` or a slow init.lua.
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "list_bufs did not answer");
            Liveness::Busy
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_handles_decode_past_the_one_byte_boundary() {
        // Real encodings: 1 is a fixint; 128 needs the uint8 marker 0xcc,
        // which a naive `data[0]` misreads.
        let cases: &[(i64, &[u8])] = &[
            (1, &[0x01]),
            (127, &[0x7f]),
            (128, &[0xcc, 0x80]),
            (255, &[0xcc, 0xff]),
            (301, &[0xcd, 0x01, 0x2d]),
            (65536, &[0xce, 0x00, 0x01, 0x00, 0x00]),
        ];
        for (want, payload) in cases {
            let v = Value::Ext(0, payload.to_vec());
            assert_eq!(
                ext_to_handle(&v),
                Some(*want),
                "handle {want} decoded wrongly from {payload:02x?}"
            );
        }
    }

    #[test]
    fn naive_first_byte_decoding_would_be_wrong() {
        let v = Value::Ext(0, vec![0xcc, 0x80]);
        assert_eq!(ext_to_handle(&v), Some(128));
        assert_ne!(ext_to_handle(&v), Some(0xcc));
    }

    #[test]
    fn plain_integers_still_decode() {
        assert_eq!(ext_to_handle(&Value::from(7)), Some(7));
    }

    /// Replays canned reply bytes; records what was sent and in how many
    /// writes.
    struct Canned {
        reply: std::io::Cursor<Vec<u8>>,
        sent: Vec<u8>,
        writes: usize,
    }

    impl Read for Canned {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reply.read(buf)
        }
    }

    impl Write for Canned {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.sent.extend_from_slice(buf);
            self.writes += 1;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Answers msgid 1, a fresh client's first request, with `result`.
    fn answering(result: Value) -> Client<Canned> {
        answering_frames(&[(1, result)])
    }

    /// Replies in exactly this order, whatever is asked: a peer may answer a
    /// fast call before a deferred one asked first.
    fn answering_frames(replies: &[(u32, Value)]) -> Client<Canned> {
        let mut bytes = Vec::new();
        for (msgid, result) in replies {
            let frame = Value::Array(vec![
                Value::from(RESPONSE),
                Value::from(*msgid),
                Value::Nil,
                result.clone(),
            ]);
            rmpv::encode::write_value(&mut bytes, &frame).expect("encode");
        }
        Client::new(Canned {
            reply: std::io::Cursor::new(bytes),
            sent: Vec::new(),
            writes: 0,
        })
    }

    fn mode_map(mode: &str, blocking: Option<bool>) -> Value {
        let mut pairs = vec![(Value::from("mode"), Value::from(mode))];
        if let Some(b) = blocking {
            pairs.push((Value::from("blocking"), Value::from(b)));
        }
        Value::Map(pairs)
    }

    #[test]
    fn get_mode_decodes_a_hit_enter_prompt() {
        let mut c = answering(mode_map("r", Some(true)));
        let m = c.get_mode().expect("decode");
        assert_eq!(
            m,
            Mode {
                mode: "r".into(),
                blocking: true
            }
        );
        assert!(m.at_hit_enter());
        let sent = rmpv::decode::read_value(&mut &c.io.get_ref().sent[..]).expect("request");
        assert_eq!(sent[2].as_str(), Some("nvim_get_mode"));
    }

    /// Erring this way costs a 3 s wait; the other way would end prompts.
    #[test]
    fn a_missing_blocking_flag_reads_as_not_blocking() {
        let mut c = answering(mode_map("n", None));
        let m = c.get_mode().expect("decode");
        assert!(!m.blocking);
        assert!(!m.at_hit_enter());
    }

    /// The attach probe's two questions, the second answered first.
    #[test]
    fn a_reply_that_arrives_out_of_order_is_kept_for_its_own_waiter() {
        let mut c = answering_frames(&[(2, Value::from("uis")), (1, Value::from("mode"))]);
        let mode = c.ask("nvim_get_mode", vec![]).expect("sent");
        let uis = c.ask("nvim_list_uis", vec![]).expect("sent");

        // Reading the mode parks the UI count on the way.
        assert_eq!(c.reply_to(mode).expect("the mode").as_str(), Some("mode"));
        assert_eq!(c.reply_to(uis).expect("the count").as_str(), Some("uis"));
        assert!(
            c.parked.is_empty() && c.in_flight.is_empty(),
            "both replies were spent: {:?}, {:?}",
            c.parked,
            c.in_flight
        );
    }

    /// Duplicates and unasked replies are dropped, bounding parking to the
    /// requests in flight.
    #[test]
    fn a_reply_nobody_is_owed_is_skipped_rather_than_kept() {
        let mut c = answering_frames(&[(99, Value::from("stray")), (1, Value::from("mine"))]);
        let mine = c.ask("nvim_get_mode", vec![]).expect("sent");
        assert_eq!(c.reply_to(mine).expect("mine").as_str(), Some("mine"));
        assert!(c.parked.is_empty(), "kept a stray reply: {:?}", c.parked);
    }

    /// The attach probe abandons its UI count when the mode shows a key is
    /// awaited; later calls must still get their own replies.
    #[test]
    fn a_pending_nobody_reads_does_not_confuse_the_calls_after_it() {
        let mut c = answering_frames(&[
            (1, Value::from("mode")),
            (2, Value::from("uis")),
            (3, Value::from("4")),
        ]);
        let mode = c.ask("nvim_get_mode", vec![]).expect("sent");
        let _abandoned = c.ask("nvim_list_uis", vec![]).expect("sent");
        assert_eq!(c.reply_to(mode).expect("the mode").as_str(), Some("mode"));

        // The abandoned reply stays parked: one entry, on a connection about
        // to be dropped.
        let input = c.ask("nvim_input", vec![]).expect("sent");
        assert_eq!(c.reply_to(input).expect("the count").as_str(), Some("4"));
        assert_eq!(c.parked.len(), 1, "{:?}", c.parked);
    }

    #[test]
    fn a_non_map_mode_reply_is_a_protocol_error() {
        let mut c = answering(Value::from(7));
        assert!(matches!(c.get_mode(), Err(RpcError::Protocol(_))));
    }

    #[test]
    fn only_the_hit_enter_prompt_may_be_answered() {
        let mode = |mode: &str, blocking| Mode {
            mode: mode.into(),
            blocking,
        };
        assert!(mode("r", true).at_hit_enter());
        assert!(
            !mode("r", false).at_hit_enter(),
            "r without blocking is not a prompt"
        );
        assert!(
            !mode("rm", true).at_hit_enter(),
            "the more-prompt scrolls on <CR>"
        );
        assert!(
            !mode("r?", true).at_hit_enter(),
            "<CR> answers a :confirm question"
        );
        assert!(
            !mode("r?", false).at_hit_enter(),
            "what 0.12 actually reports for confirm(): the mode string is the guard"
        );
        assert!(!mode("n", false).at_hit_enter());
        assert!(!mode("c", false).at_hit_enter());
    }

    #[test]
    fn input_sends_nvim_input_and_ignores_the_count() {
        let mut c = answering(Value::from(4));
        c.input("<CR>").expect("input");
        let sent = rmpv::decode::read_value(&mut &c.io.get_ref().sent[..]).expect("request");
        assert_eq!(sent[2].as_str(), Some("nvim_input"));
        assert_eq!(sent[3][0].as_str(), Some("<CR>"));
    }

    /// Each write is a syscall and a session wake-up; a notes read was 17.
    #[test]
    fn a_request_is_written_once() {
        let mut c = answering(Value::Nil);
        let code = vec![Value::from("return 1"), Value::Array(vec![Value::from(8)])];
        c.call("nvim_exec_lua", code).expect("answered");
        assert_eq!(c.io.get_ref().writes, 1, "one write per request");
        let sent = rmpv::decode::read_value(&mut &c.io.get_ref().sent[..]).expect("request");
        assert_eq!(sent[2].as_str(), Some("nvim_exec_lua"));
        assert_eq!(sent[3][0].as_str(), Some("return 1"));
    }

    /// A peer that has hung up: nothing to read, and a write refused.
    struct HungUp;

    impl Read for HungUp {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }

    impl Write for HungUp {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Reset (gone), as a read would find it, not a protocol error.
    #[test]
    fn a_request_to_a_session_that_hung_up_is_a_reset() {
        let e = Client::new(HungUp).get_mode().expect_err("refused");
        assert!(matches!(e, RpcError::Reset), "want Reset, got {e:?}");
    }

    #[test]
    fn nvim_errors_are_unwrapped_to_their_message() {
        let e = Value::Array(vec![
            Value::from(0),
            Value::String("Key not found: pid".into()),
        ]);
        assert_eq!(describe_nvim_error(&e), "Key not found: pid");
    }

    #[test]
    fn connecting_to_a_missing_socket_reports_refused_not_io() {
        let err = Client::connect(Path::new("/tmp/nvmux-does-not-exist.sock"), CONNECT_TIMEOUT)
            .expect_err("must fail");
        assert!(
            err.is_definitely_dead(),
            "a missing socket must be classified as dead, got {err:?}"
        );
    }

    #[test]
    fn probing_a_missing_socket_is_dead_not_busy() {
        assert_eq!(probe(Path::new("/tmp/nvmux-nope.sock")), Liveness::Dead);
    }

    /// ECONNREFUSED on Linux, ENOTSOCK on macOS; `Busy` would list the file as
    /// a session.
    #[test]
    fn probing_a_regular_file_is_dead_not_busy() {
        let path = crate::test_support::scratch_sock("rpc-notasock");
        std::fs::write(&path, b"not a socket").expect("write");
        let liveness = probe(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(liveness, Liveness::Dead);
    }

    /// A session that accepts and says nothing (`:!make`, CPU-bound Lua) is
    /// Busy, never Dead, even on the short budget.
    #[test]
    fn a_silent_session_is_busy_at_the_end_of_a_short_budget() {
        let path = crate::test_support::scratch_sock("rpc-silent");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let started = std::time::Instant::now();
        let liveness = probe_within(&path, QUICK_PROBE_TIMEOUT);
        let took = started.elapsed();
        drop(listener);
        let _ = std::fs::remove_file(&path);
        assert_eq!(liveness, Liveness::Busy);
        assert!(
            took < Duration::from_secs(1),
            "a quarter of a second's budget took {took:?}"
        );
    }

    /// With no budget (as for the attach probe), only the interrupt can end an
    /// unanswered call, and it must do so promptly.
    #[test]
    fn an_interrupt_ends_an_unbounded_call_on_another_thread() {
        let path = crate::test_support::scratch_sock("rpc-interrupt");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let mut client = Client::connect_with(&path, None).expect("connect");
        let interrupt = client.interrupt().expect("a second handle");
        // Accepted and then ignored: a server that never answers.
        let (server, _) = listener.accept().expect("accept");

        let worker = std::thread::spawn(move || client.get_mode());
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !worker.is_finished(),
            "the call returned with no answer and no budget"
        );

        let started = std::time::Instant::now();
        interrupt.fire();
        let err = worker
            .join()
            .expect("the worker thread")
            .expect_err("the call was cut short");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the interrupt took {:?} to unpark the read",
            started.elapsed()
        );
        assert!(matches!(err, RpcError::Reset), "want Reset, got {err:?}");
        drop(server);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn overlong_paths_are_rejected_before_connect() {
        let long = std::path::PathBuf::from(format!("/tmp/{}.sock", "a".repeat(150)));
        let err = Client::connect(&long, CONNECT_TIMEOUT).expect_err("must fail");
        assert!(
            matches!(err, RpcError::Protocol(_)),
            "want a clear protocol error, got {err:?}"
        );
    }
}
