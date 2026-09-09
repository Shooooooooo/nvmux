//! A minimal synchronous msgpack-RPC client for Neovim.
//!
//! There is **no `nvim_ui_attach` anywhere in this crate**, and there must never
//! be one — see [`Client::list_uis`].
//!
//! Synchronous because Neovim never pushes unsolicited traffic on a bare RPC
//! channel: only one that has attached a UI or registered for autocmds gets
//! notifications, so there is nothing to demultiplex in the background.
//!
//! # Reachable is not the same as ready
//!
//! `nvim_get_api_info` is answered off the main loop. Measured, it still replies
//! in 3ms while the editor is blocked inside `call system('sleep 3')`, at which
//! point `nvim_list_bufs` on a separate connection times out completely. That
//! asymmetry drives the whole liveness model:
//!
//! * `api_info` answering means a real Neovim is behind the socket, as distinct
//!   from an SSH forward that accepts and then resets.
//! * only a *deferred* call answering means the session is actually serving.
//! * a deferred call timing out means [`Liveness::Busy`] — the user is running
//!   a build — and must never be treated as death, or nvmux would reap live
//!   sessions out from under anyone who typed `:!make`.
//!
//! # Fast calls, and a session waiting at a prompt
//!
//! Neovim runs a handful of API functions straight from the socket read
//! callback (`FUNC_API_FAST`: `nvim_get_api_info`, `nvim_get_mode`,
//! `nvim_input`) and queues every other one for its main loop. An editor
//! parked at a hit-enter prompt (`Press ENTER or type command to continue`)
//! waits for that key with the queue switched off, so a deferred call — any of
//! `nvim_list_bufs`, `nvim_list_uis`, `nvim_command`, `nvim_eval` — gets no
//! answer at all until the prompt ends, however long the budget. Measured
//! against 0.12.5: `nvim_get_mode` answers in 0.1 ms with `blocking = true`,
//! `nvim_list_bufs` never answers. With `'cmdheight'` at 0, which AstroNvim
//! sets, every one-line error opens that prompt; with the default 1, any
//! message that scrolls does. So [`probe`] asks the mode before it asks
//! anything deferred, and the attach and resume paths in [`crate::pty`] end
//! the prompt with the one key it consumes, `<CR>`, before they wait on the
//! editor. `nvim_get_mode` is only immediate while the editor is blocked or
//! idle: during `:!cmd` and CPU-bound Lua it is queued like everything else,
//! which is why every call here still carries a budget.

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

/// Long enough that an ordinary call never trips it, short enough that a dead
/// forward does not stall the picker.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

/// The read timeout for probes that run behind the picker. Three seconds rather
/// than one: a deferred call blocks for the whole of any `system()` call in the
/// user's editor, so a shorter budget marks healthy sessions dead.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// A connected RPC channel, generic over the byte stream so the same code serves
/// a local socket, the local end of an SSH forward, and a pipe in tests.
pub struct Client<S: Read + Write> {
    io: BufReader<S>,
    next_msgid: u32,
    /// Once set, every call fails fast. See [`Client::poison`].
    poisoned: Option<String>,
    /// Tracked so a timeout error reports the budget that actually elapsed,
    /// rather than misreporting a 250ms failure as a 3s one.
    read_timeout: Duration,
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
    /// Connect to a session socket. `read_timeout` bounds each *reply*, not the
    /// connect — a unix socket connect either finds a listener or does not.
    /// Passing a connect-sized value here classified live sessions as dead.
    pub fn connect(path: &Path, read_timeout: Duration) -> Result<Self, RpcError> {
        // std's error here is `InvalidInput` with `raw_os_error() == None` —
        // nothing a caller could match on. Check explicitly for a useful message.
        crate::paths::check_sock_path(path).map_err(|e| RpcError::Protocol(e.to_string()))?;

        let stream = UnixStream::connect(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::ConnectionRefused => {
                RpcError::ConnectionRefused(path.to_path_buf())
            }
            std::io::ErrorKind::NotFound => RpcError::ConnectionRefused(path.to_path_buf()),
            // A regular file (or directory) sitting at the path. Linux reports
            // that as ECONNREFUSED; macOS and the BSDs say ENOTSOCK. Either
            // way nothing can be listening on a non-socket inode, so it is as
            // dead as a missing one. Whether the file may be *deleted* is a
            // separate question the reaper answers with `lstat`.
            _ if e.raw_os_error() == Some(libc::ENOTSOCK) => {
                RpcError::ConnectionRefused(path.to_path_buf())
            }
            _ => RpcError::Io(e),
        })?;
        stream.set_read_timeout(Some(read_timeout))?;
        stream.set_write_timeout(Some(read_timeout))?;
        let mut client = Self::new(stream);
        client.read_timeout = read_timeout;
        Ok(client)
    }
}

impl<S: Read + Write> Client<S> {
    pub fn new(stream: S) -> Self {
        Self {
            io: BufReader::new(stream),
            next_msgid: 1,
            poisoned: None,
            read_timeout: PROBE_TIMEOUT,
        }
    }

    /// Mark this connection unusable. rmpv reads incrementally, so a call that
    /// gives up partway leaves bytes in the socket and the next read would
    /// decode a well-formed-looking value out of the previous frame's tail.
    fn poison(&mut self, why: impl Into<String>) -> RpcError {
        let why = why.into();
        self.poisoned = Some(why.clone());
        RpcError::Protocol(why)
    }

    /// Condemn the connection when an error left unread bytes behind — a
    /// timeout is the dangerous case and the easy one to miss. The error the
    /// caller sees is preserved; only the connection is condemned.
    fn poison_if_fatal(&mut self, e: RpcError, method: &str) -> RpcError {
        match &e {
            RpcError::Timeout(_) | RpcError::Protocol(_) => {
                self.poisoned = Some(format!("{method}: {e}"));
            }
            // Already unusable, and the caller distinguishes these for liveness.
            _ => {}
        }
        e
    }

    /// Issue a request and wait for its response.
    pub fn call(&mut self, method: &str, params: Vec<Value>) -> Result<Value, RpcError> {
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

        rmpv::encode::write_value(self.io.get_mut(), &request)
            .map_err(|e| self.poison(format!("encoding {method}: {e}")))?;
        let budget = self.read_timeout;
        if let Err(e) = self.io.get_mut().flush() {
            let mapped = map_io(e, budget);
            return Err(self.poison_if_fatal(mapped, method));
        }

        // An overall deadline as well as a per-read one: a peer that keeps
        // sending frames we skip restarts the per-read budget every time.
        let deadline = std::time::Instant::now() + budget;

        loop {
            if std::time::Instant::now() >= deadline {
                return Err(self.poison(format!("{method} exceeded its {budget:?} budget")));
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
                Some(result) => return result.map_err(RpcError::Nvim),
                None => continue,
            }
        }
    }

    /// Sort one decoded frame into "the answer we wanted" or "ignore it".
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
                if got != u64::from(want) {
                    // Not ours. Keep reading rather than answering the wrong
                    // question.
                    tracing::debug!(got, want, "skipping a response for another request");
                    return Ok(None);
                }
                if !arr[2].is_nil() {
                    return Ok(Some(Err(describe_nvim_error(&arr[2]))));
                }
                Ok(Some(Ok(arr[3].clone())))
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

    /// `nvim_get_api_info` — reachability and version. Answered off the main
    /// loop, so this does **not** prove the session is usable; see the module
    /// docs.
    pub fn api_info(&mut self) -> Result<ApiInfo, RpcError> {
        let v = self.call("nvim_get_api_info", vec![])?;
        let arr = v
            .as_array()
            .ok_or_else(|| RpcError::Protocol("api_info was not an array".into()))?;
        // Element 0 is our channel id, which nvmux never uses. Requiring it to be
        // an integer anyway is what makes a non-nvim peer fail here, cleanly,
        // rather than somewhere further downstream.
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

        Ok(ApiInfo {
            major: field("major").unwrap_or(0),
            minor: field("minor").unwrap_or(0),
            patch: field("patch").unwrap_or(0),
        })
    }

    /// `nvim_list_bufs` — a *deferred* call, and therefore the real readiness test.
    pub fn list_bufs(&mut self) -> Result<Vec<Value>, RpcError> {
        let v = self.call("nvim_list_bufs", vec![])?;
        Ok(v.as_array()
            .ok_or_else(|| RpcError::Protocol("list_bufs was not an array".into()))?
            .to_vec())
    }

    /// How many UIs are currently attached, checked before ever attaching.
    /// Neovim's `ui_attach_impl` does `if (ui_count == MAX_UI_COUNT) { abort(); }`
    /// with `MAX_UI_COUNT == 16` — an `abort()`, not an error return, so a
    /// seventeenth attach SIGABRTs the server and destroys the session.
    ///
    /// This is also why nvmux never attaches a second UI to preview a session:
    /// Neovim sizes the global grid to the per-dimension minimum across every
    /// attached UI, so a small preview would shrink the grid you are editing in.
    pub fn list_uis(&mut self) -> Result<usize, RpcError> {
        let v = self.call("nvim_list_uis", vec![])?;
        Ok(v.as_array().map(|a| a.len()).unwrap_or(0))
    }

    /// `nvim_command`. Used for `qa!`, for injecting the `:Detach` alias, and
    /// for `:mode`, the one command that clears the grid and repaints it.
    pub fn command(&mut self, cmd: &str) -> Result<(), RpcError> {
        self.call("nvim_command", vec![Value::String(cmd.into())])?;
        Ok(())
    }

    /// `nvim_get_mode` — a *fast* call, answered even while the editor is
    /// blocked at a prompt, which is exactly when it is worth asking. See the
    /// module docs.
    pub fn get_mode(&mut self) -> Result<Mode, RpcError> {
        let v = self.call("nvim_get_mode", vec![])?;
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
        // Absent means not blocking: the key has been there since 0.2, but a
        // missing flag must read as "go ahead", never as "blocked".
        let blocking = field("blocking").and_then(Value::as_bool).unwrap_or(false);
        Ok(Mode { mode, blocking })
    }

    /// `nvim_input` — a *fast* call: the keys land in the editor's input
    /// buffer at once, prompt or no prompt. nvmux itself only ever sends a
    /// lone `<CR>` to a hit-enter prompt this way (see [`Mode::at_hit_enter`]);
    /// the integration tests type more.
    pub fn input(&mut self, keys: &str) -> Result<(), RpcError> {
        // The reply is how many bytes were queued, which says nothing useful.
        self.call("nvim_input", vec![Value::String(keys.into())])?;
        Ok(())
    }
}

/// What `nvim_get_mode` reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mode {
    /// As `mode(1)` spells it: `n`, `i`, `c`, `r` for the hit-enter prompt,
    /// `rm` for the more-prompt, `r?` for a `:confirm` question, and so on.
    pub mode: String,
    /// The editor is waiting for a key with its event queue switched off, so
    /// no deferred call is served until that key arrives: a hit-enter or
    /// more-prompt, or the second key of a multi-key command (`g`, `d`, `"`).
    /// Not a `:confirm` question, on 0.12 at least — measured, it reports
    /// `r?` with this false and answers deferred calls as usual.
    pub blocking: bool,
}

impl Mode {
    /// The hit-enter prompt, and only that: mode `r` while blocking.
    ///
    /// The distinction matters because this is the one prompt nvmux is willing
    /// to end on the user's behalf. `wait_return()` consumes `<CR>` outright
    /// (most other keys it pushes back to run as a Normal-mode command), and
    /// what it was showing has been painted over by the time nvmux asks. The
    /// more-prompt (`rm`) scrolls a line on `<CR>`; a `:confirm` question
    /// (`r?`) takes `<CR>` as its default answer, which is never nvmux's call
    /// to make — and it is the mode string alone that keeps it out, since
    /// `blocking` is false there. A pending multi-key command is blocking in
    /// mode `n` or `no`, and `<CR>` would complete it.
    ///
    /// Advisory, not a lock: a key from another attached UI in the
    /// microseconds after this answer ends the prompt first, and a `<CR>`
    /// sent on the strength of it runs in whatever mode that key left.
    pub fn at_hit_enter(&self) -> bool {
        self.blocking && self.mode == "r"
    }
}

/// The parts of `nvim_get_api_info`'s version map that nvmux uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiInfo {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl ApiInfo {
    /// The same gate [`crate::nvim::Version::is_supported`] applies to a
    /// `--version` banner, sourced from the one constant so the two cannot drift.
    pub fn is_supported(&self) -> bool {
        (self.major, self.minor) >= crate::nvim::MIN
    }
}

impl std::fmt::Display for ApiInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Decode a Neovim handle (buffer, window, tabpage) into its integer id.
///
/// Neovim encodes these as msgpack **EXT** values whose payload is itself a
/// msgpack-encoded integer, not a raw byte. Reading `data[0]` directly appears
/// to work for every handle below 128 and then silently returns nonsense:
/// buffer 128 decodes as 204, because 0xcc is the `uint8` marker byte.
///
/// Display and logging only. Pass a handle **back** to Neovim as the original
/// [`Value::Ext`] verbatim — an integer that happens to be 0 means "the current
/// buffer" and answers plausibly for the wrong one.
///
/// nvmux makes no handle-based calls today, so this lives with its test as the
/// reference for whoever adds one.
#[cfg(test)]
pub fn ext_to_handle(v: &Value) -> Option<i64> {
    match v {
        Value::Ext(_type_code, data) => rmpv::decode::read_value(&mut &data[..])
            .ok()
            .and_then(|v| v.as_i64()),
        // Tolerated so tests and any future plain-integer API keep working.
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

fn map_io(e: std::io::Error, timeout: Duration) -> RpcError {
    match e.kind() {
        // A read timeout surfaces as one of these two depending on platform.
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => RpcError::Timeout(timeout),
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof => RpcError::Reset,
        std::io::ErrorKind::BrokenPipe => RpcError::Reset,
        _ => RpcError::Io(e),
    }
}

/// Decide whether a session socket has a live, serving Neovim behind it.
///
/// **Only `Dead` causes files to be deleted, so `Dead` must mean "we proved
/// nothing is listening" — never "we did not get an answer".** Everything else
/// degrades to [`Liveness::Busy`], which is never reaped. An earlier version
/// mapped any `api_info` error to `Dead` under a 250ms budget; `api_info` is
/// *not* answered during CPU-bound Lua, measured at 3.7s, so a plain `nvmux`
/// invocation unlinked the socket of a session that was merely busy.
pub fn probe(path: &Path) -> Liveness {
    let mut client = match Client::connect(path, PROBE_TIMEOUT) {
        Ok(c) => c,
        Err(e) if e.is_definitely_dead() => return Liveness::Dead,
        Err(e) => {
            // EACCES, too many open files, a path we cannot name: none of it
            // says anything about the session. Under fd exhaustion the
            // alternative would reap every session at once.
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

    // Before anything deferred: a session waiting for a key with its event
    // queue off — at a hit-enter prompt, most often — would hold the deferred
    // call for the whole budget, and it is the picker that waits with it, on
    // a cleared screen, once per such session. The mode is a fast call and
    // says so in a millisecond. Busy, not a state of its own: like a `:!make`
    // it is reachable and must never be reaped.
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
        // Reachable but not answering: almost certainly inside a `:!make`, or
        // still sourcing a slow init.lua. Never reap this.
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
        // The payload is a msgpack-encoded integer, so these are the encodings
        // Neovim actually sends. Handle 1 fits in a fixint; 128 needs a uint8
        // marker (0xcc), which is the case a naive `data[0]` gets wrong.
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
        // Guards the comment on ext_to_handle: this is the bug it prevents.
        let v = Value::Ext(0, vec![0xcc, 0x80]);
        assert_eq!(ext_to_handle(&v), Some(128));
        assert_ne!(ext_to_handle(&v), Some(0xcc));
    }

    #[test]
    fn plain_integers_still_decode() {
        assert_eq!(ext_to_handle(&Value::from(7)), Some(7));
    }

    #[test]
    fn version_gate_matches_the_documented_minimum() {
        let at = |major, minor| ApiInfo {
            major,
            minor,
            patch: 0,
        };
        assert!(!at(0, 9).is_supported());
        assert!(!at(0, 10).is_supported());
        assert!(at(0, 11).is_supported(), "0.11 is the documented minimum");
        assert!(at(0, 12).is_supported());
        assert!(at(1, 0).is_supported());
    }

    /// A peer that answers every request with one canned frame, and keeps
    /// what was sent to it.
    struct Canned {
        reply: std::io::Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }

    impl Read for Canned {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reply.read(buf)
        }
    }

    impl Write for Canned {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.sent.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A client whose first call is answered with `result`. The msgid is 1
    /// because that is what a fresh client sends first.
    fn answering(result: Value) -> Client<Canned> {
        let frame = Value::Array(vec![
            Value::from(RESPONSE),
            Value::from(1u32),
            Value::Nil,
            result,
        ]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &frame).expect("encode");
        Client::new(Canned {
            reply: std::io::Cursor::new(bytes),
            sent: Vec::new(),
        })
    }

    fn mode_map(mode: &str, blocking: Option<bool>) -> Value {
        let mut pairs = vec![(Value::from("mode"), Value::from(mode))];
        if let Some(b) = blocking {
            pairs.push((Value::from("blocking"), Value::from(b)));
        }
        Value::Map(pairs)
    }

    /// The shape Neovim answers with at a hit-enter prompt — and the one
    /// answer that took 3 s to not get before the mode was asked first.
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

    /// A missing flag must mean "not blocking": that way round a decoding
    /// gap costs a 3 s wait, the other way round it would end prompts.
    #[test]
    fn a_missing_blocking_flag_reads_as_not_blocking() {
        let mut c = answering(mode_map("n", None));
        let m = c.get_mode().expect("decode");
        assert!(!m.blocking);
        assert!(!m.at_hit_enter());
    }

    #[test]
    fn a_non_map_mode_reply_is_a_protocol_error() {
        let mut c = answering(Value::from(7));
        assert!(matches!(c.get_mode(), Err(RpcError::Protocol(_))));
    }

    /// Only the hit-enter prompt may be ended by nvmux; see `Mode::at_hit_enter`.
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

    /// The keys go out as the single `nvim_input` parameter, and the byte
    /// count that comes back is dropped.
    #[test]
    fn input_sends_nvim_input_and_ignores_the_count() {
        let mut c = answering(Value::from(4));
        c.input("<CR>").expect("input");
        let sent = rmpv::decode::read_value(&mut &c.io.get_ref().sent[..]).expect("request");
        assert_eq!(sent[2].as_str(), Some("nvim_input"));
        assert_eq!(sent[3][0].as_str(), Some("<CR>"));
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

    /// `connect()` on a regular file fails with ECONNREFUSED on Linux and
    /// ENOTSOCK on macOS. Both mean "nothing is listening": reporting `Busy`
    /// instead would list the file as a session.
    #[test]
    fn probing_a_regular_file_is_dead_not_busy() {
        let path = std::env::temp_dir().join(format!("nvmux-notasock-{}.sock", std::process::id()));
        std::fs::write(&path, b"not a socket").expect("write");
        let liveness = probe(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(liveness, Liveness::Dead);
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
