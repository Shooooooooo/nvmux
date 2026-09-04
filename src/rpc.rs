//! A minimal synchronous msgpack-RPC client for Neovim.
//!
//! # Scope
//!
//! Four methods, and no more:
//!
//! * `nvim_get_api_info`   — reachability and version
//! * `nvim_list_bufs`      — readiness, and the dirty-buffer check
//! * `nvim_get_option_value` — the `modified` flag per buffer
//! * `nvim_command`        — graceful `qa!`
//!
//! There is **no `nvim_ui_attach` anywhere in this crate**, and there must never
//! be one. nvmux does not render Neovim's UI; `nvim --server ... --remote-ui`
//! does, as a child process. See [`crate::pty`] for why that split matters, and
//! the note in [`Client::list_uis`] for what attaching a second UI would do to
//! the session you are editing in.
//!
//! # Why synchronous
//!
//! Neovim never pushes unsolicited traffic on a bare RPC channel — only a
//! channel that has attached a UI or registered for autocmds receives
//! notifications — so there is nothing to demultiplex in the background and no
//! reader task to own. A blocking `UnixStream` with a read timeout is the whole
//! transport, and it works identically on a local socket and on the local end of
//! an SSH forward.
//!
//! # Reachable is not the same as ready
//!
//! `nvim_get_api_info` is answered off the main loop. It replies in
//! milliseconds while `init.lua` is still sourcing, and — measured — it still
//! replies in 3ms while the editor is blocked inside `call system('sleep 3')`,
//! at which point `nvim_list_bufs` on a *separate connection* times out
//! completely.
//!
//! That asymmetry drives the whole liveness model:
//!
//! * `api_info` answering means the socket has a real Neovim behind it, as
//!   distinct from an SSH forward that accepts and then resets.
//! * only a *deferred* call answering means the session is actually serving.
//! * a deferred call timing out means [`Liveness::Busy`] — the user is running
//!   a build — and must never be treated as death, or nvmux would reap live
//!   sessions out from under anyone who typed `:!make`.

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

/// The read timeout for probes that run behind the picker.
///
/// Three seconds rather than one: three of the four methods above block for the
/// entire duration of any `system()` call in the user's editor, so a one-second
/// budget would mark healthy sessions dead whenever someone is compiling.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The read timeout for something the user explicitly asked for.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// A connected RPC channel.
///
/// Generic over the byte stream so the same code serves a local socket, the
/// local end of an SSH forward, and an in-memory pipe in tests.
pub struct Client<S: Read + Write> {
    io: BufReader<S>,
    next_msgid: u32,
    /// Once set, every call fails fast. See [`Client::poison`].
    poisoned: Option<String>,
    /// The read timeout currently set on the stream.
    ///
    /// Tracked so a timeout error reports the budget that actually elapsed. A
    /// hardcoded constant here would misreport a 250ms failure as a 3s one and
    /// send anyone reading the log looking in the wrong place.
    read_timeout: Duration,
}

impl<S: Read + Write> std::fmt::Debug for Client<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("next_msgid", &self.next_msgid)
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

impl Client<UnixStream> {
    /// Connect to a session socket.
    /// `read_timeout` bounds each *reply*, not the connect.
    ///
    /// Connecting to a unix socket is effectively instantaneous — the kernel
    /// either finds a listener or does not — so the meaningful budget is how
    /// long to wait for Neovim to answer. Passing a connect-sized value here is
    /// what caused live sessions to be classified dead: see [`probe`].
    pub fn connect(path: &Path, read_timeout: Duration) -> Result<Self, RpcError> {
        // Length is validated before we ever get here, but a caller could hand
        // us a path from metadata written by an older build, and std's error for
        // this case is `InvalidInput` with `raw_os_error() == None` — nothing a
        // caller could match on. Check explicitly so the message is useful.
        crate::config::check_sock_path(path).map_err(|e| RpcError::Protocol(e.to_string()))?;

        let stream = UnixStream::connect(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::ConnectionRefused => {
                RpcError::ConnectionRefused(path.to_path_buf())
            }
            std::io::ErrorKind::NotFound => RpcError::ConnectionRefused(path.to_path_buf()),
            _ => RpcError::Io(e),
        })?;
        stream.set_read_timeout(Some(read_timeout))?;
        stream.set_write_timeout(Some(read_timeout))?;
        let mut client = Self::new(stream);
        client.read_timeout = read_timeout;
        Ok(client)
    }

    /// Change the read timeout on an established connection.
    pub fn set_timeout(&mut self, timeout: Duration) -> Result<(), RpcError> {
        self.io.get_ref().set_read_timeout(Some(timeout))?;
        self.read_timeout = timeout;
        Ok(())
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

    /// Mark this connection unusable.
    ///
    /// Any timeout or decode failure has to be terminal for the channel. rmpv
    /// reads from the stream incrementally, so a call that gives up partway
    /// leaves unconsumed bytes in the socket. The next `read_value` would then
    /// decode a *well-formed-looking* value out of the tail of the previous
    /// frame and hand it back as an answer — silently wrong data rather than an
    /// error. The only safe recovery is to close and reconnect.
    fn poison(&mut self, why: impl Into<String>) -> RpcError {
        let why = why.into();
        self.poisoned = Some(why.clone());
        RpcError::Protocol(why)
    }

    /// Turn an error into one the connection cannot be used after, when it left
    /// unread bytes behind.
    ///
    /// A timeout is the dangerous case and the easy one to miss. rmpv consumes
    /// the stream incrementally, so abandoning a read part-way leaves the tail of
    /// that frame in the socket; the *next* call would then decode something
    /// well-formed out of it and return it as an answer to a different question.
    /// `dirty_buffer_count` issues many calls on one connection, so this would
    /// surface as a wrong unsaved-buffer count rather than as an error.
    ///
    /// The error the caller sees is preserved — only the connection is condemned.
    fn poison_if_fatal(&mut self, e: RpcError, method: &str) -> RpcError {
        match &e {
            RpcError::Timeout(_) | RpcError::Protocol(_) => {
                self.poisoned = Some(format!("{method}: {e}"));
            }
            // A reset or refused connection is already unusable; nothing to
            // condemn, and the caller distinguishes these to decide liveness.
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
        // not `bin`; rmpv's Value::String encodes as str, which is why the
        // method name is built this way rather than from bytes.
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

        // An overall deadline as well as a per-read one. Without it, a peer that
        // keeps sending frames we skip (notifications, or replies to requests
        // that already timed out) restarts the per-read budget every time and
        // this loop never returns.
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
                // A notification, or a response to a request that timed out
                // earlier. Skip it and keep reading.
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
                    // Not ours. Keep reading rather than mismatching an answer
                    // to the wrong question.
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
                // Neovim asking *us* something. We register no handlers, so
                // there is nothing to answer; ignoring it is correct.
                tracing::debug!("ignoring an inbound request from nvim");
                Ok(None)
            }
            other => Err(self.poison(format!("unknown msgpack-rpc message type {other:?}"))),
        }
    }

    /// `nvim_get_api_info` — reachability and version.
    ///
    /// Answered off the main loop, so this proves only that a real Neovim is
    /// behind the socket. It does **not** prove the session is usable; see the
    /// module docs.
    pub fn api_info(&mut self) -> Result<ApiInfo, RpcError> {
        let v = self.call("nvim_get_api_info", vec![])?;
        let arr = v
            .as_array()
            .ok_or_else(|| RpcError::Protocol("api_info was not an array".into()))?;
        let channel = arr
            .first()
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::Protocol("api_info had no channel id".into()))?;
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
            channel,
            major: field("major").unwrap_or(0),
            minor: field("minor").unwrap_or(0),
            patch: field("patch").unwrap_or(0),
            api_level: field("api_level").unwrap_or(0),
        })
    }

    /// `nvim_list_bufs` — a *deferred* call, and therefore the real readiness test.
    pub fn list_bufs(&mut self) -> Result<Vec<Value>, RpcError> {
        let v = self.call("nvim_list_bufs", vec![])?;
        Ok(v.as_array()
            .ok_or_else(|| RpcError::Protocol("list_bufs was not an array".into()))?
            .to_vec())
    }

    /// How many UIs are currently attached.
    ///
    /// Checked before ever attaching. Neovim's `ui_attach_impl` does
    /// `if (ui_count == MAX_UI_COUNT) { abort(); }` with `MAX_UI_COUNT == 16` —
    /// an `abort()`, not an error return, so a seventeenth attach SIGABRTs the
    /// whole server and destroys the session.
    pub fn list_uis(&mut self) -> Result<usize, RpcError> {
        let v = self.call("nvim_list_uis", vec![])?;
        Ok(v.as_array().map(|a| a.len()).unwrap_or(0))
    }

    /// Whether a buffer has unsaved changes.
    ///
    /// The handle must be passed back **exactly as received**. See
    /// [`ext_to_handle`] for why turning it into an integer first is a trap.
    pub fn buf_modified(&mut self, buf: &Value) -> Result<bool, RpcError> {
        let opts = Value::Map(vec![(Value::String("buf".into()), buf.clone())]);
        let v = self.call(
            "nvim_get_option_value",
            vec![Value::String("modified".into()), opts],
        )?;
        Ok(v.as_bool().unwrap_or(false))
    }

    /// Count buffers with unsaved changes.
    ///
    /// A buffer that disappears between `nvim_list_bufs` and reading its
    /// `modified` flag is skipped rather than failing the whole count: the
    /// editor is live and the user may well be closing buffers while we ask.
    /// Aborting there would turn an ordinary race into "could not check", and
    /// the kill prompt would stop mentioning unsaved work at exactly the moment
    /// someone is busy editing.
    ///
    /// A transport-level failure (timeout, reset) still propagates — that is a
    /// genuine "we do not know", and the caller must not read it as zero.
    pub fn dirty_buffer_count(&mut self) -> Result<usize, RpcError> {
        let bufs = self.list_bufs()?;
        let mut n = 0;
        for b in &bufs {
            match self.buf_modified(b) {
                Ok(true) => n += 1,
                Ok(false) => {}
                // "Invalid buffer id" and friends: it went away underneath us.
                Err(RpcError::Nvim(msg)) => {
                    tracing::debug!(error = %msg, "skipping a buffer that vanished mid-count");
                }
                Err(e) => return Err(e),
            }
        }
        Ok(n)
    }

    /// `nvim_command`. Used for `qa!` and for injecting the `:Detach` alias.
    pub fn command(&mut self, cmd: &str) -> Result<(), RpcError> {
        self.call("nvim_command", vec![Value::String(cmd.into())])?;
        Ok(())
    }
}

/// The parts of `nvim_get_api_info`'s version map that nvmux uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiInfo {
    /// Our channel id on this connection.
    pub channel: u64,
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Gates features far more precisely than the version triple does.
    pub api_level: u64,
}

impl ApiInfo {
    /// nvmux needs 0.11: that is where `:detach` and `:connect` landed.
    pub fn is_supported(&self) -> bool {
        (self.major, self.minor) >= (0, 11)
    }
}

impl std::fmt::Display for ApiInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Decode a Neovim handle (buffer, window, tabpage) into its integer id.
///
/// Neovim encodes these as msgpack **EXT** values, not as plain integers, and
/// the payload is itself a msgpack-encoded integer rather than a raw byte.
/// Reading `data[0]` directly appears to work for every handle below 128 and
/// then silently returns nonsense: buffer 128 decodes as 204, because 0xcc is
/// the `uint8` marker byte.
///
/// Use this only for display and logging. When passing a handle **back** to
/// Neovim, send the original [`Value::Ext`] verbatim — the type code is
/// validated on the way in, and an integer that happens to be 0 means "the
/// current buffer" and returns a plausible answer for the wrong buffer.
pub fn ext_to_handle(v: &Value) -> Option<i64> {
    match v {
        Value::Ext(_type_code, data) => rmpv::decode::read_value(&mut &data[..])
            .ok()
            .and_then(|v| v.as_i64()),
        // Tolerated so tests and any future API change that returns plain
        // integers keep working.
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
/// # The rule that matters
///
/// **Only `Dead` causes files to be deleted, so `Dead` must mean "we proved
/// nothing is listening" — never "we did not get an answer".** Every other
/// outcome degrades to [`Liveness::Busy`], which is never reaped.
///
/// This is not hypothetical caution. An earlier version ran `api_info` under a
/// 250ms budget and mapped any error to `Dead`, and while `api_info` is answered
/// off the main loop during `system()` calls, it is *not* answered during
/// CPU-bound Lua — measured at 3.7s, fifteen times over that budget. A plain
/// `nvmux` invocation would then unlink the socket of a session that was merely
/// busy, leaving a running Neovim that nothing could ever reach again.
///
/// # Two stages, answering different questions
///
/// 1. `api_info` — is there a Neovim here at all? Usually answered even while
///    the editor is blocked, which is what distinguishes a real server from an
///    SSH forward that accepts and immediately resets.
/// 2. `list_bufs` — is it actually serving? Blocks with the main loop, so a
///    timeout here means busy, not dead.
pub fn probe(path: &Path) -> Liveness {
    // The budget is a *reply* budget, not a connect budget. Connecting to a
    // unix socket either finds a listener immediately or does not.
    let mut client = match Client::connect(path, PROBE_TIMEOUT) {
        Ok(c) => c,
        Err(e) if e.is_definitely_dead() => return Liveness::Dead,
        Err(e) => {
            // Anything else — EACCES, too many open files, a path we cannot
            // even name — says nothing about the session. Under fd exhaustion
            // the alternative would reap every session at once.
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

    match client.list_bufs() {
        Ok(_) => Liveness::Alive,
        Err(e) if e.is_definitely_dead() => Liveness::Dead,
        // Reachable but not answering: almost certainly inside a `:!make`, or
        // still sourcing a slow init.lua. Never reap this.
        Err(_) => Liveness::Busy,
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
            channel: 1,
            major,
            minor,
            patch: 0,
            api_level: 0,
        };
        assert!(!at(0, 9).is_supported());
        assert!(!at(0, 10).is_supported());
        assert!(at(0, 11).is_supported(), "0.11 is the documented minimum");
        assert!(at(0, 12).is_supported());
        assert!(at(1, 0).is_supported());
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
