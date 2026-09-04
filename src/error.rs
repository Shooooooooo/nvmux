//! Typed errors.
//!
//! The rule from the design: `anyhow::Result` at boundaries, typed errors where
//! callers actually branch. Every variant below exists because some caller makes
//! a decision on it — a stale socket gets reaped, a busy session does not; a
//! dirty session changes the kill prompt; an old nvim gets a specific message
//! instead of a mysterious connection failure.

use std::path::PathBuf;

/// Failures constructing or validating paths.
///
/// Separate from `io::Error` because the length rule is ours, not the kernel's,
/// and because the kernel's version of this error is unusable — see
/// [`PathError::TooLong`].
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// A composed socket path exceeded the `sun_path` budget.
    ///
    /// This must be caught by us, before anyone calls `bind()`. The reason is
    /// that the two halves of nvmux disagree about overlong paths: nvim (via
    /// libuv) *silently truncates* — `uv_pipe_bind2` does
    /// `if (namelen > sizeof(saddr.up.path)) namelen = sizeof(saddr.up.path);`
    /// with no warning and exit 0 — while Rust's `UnixStream::connect` refuses
    /// with `InvalidInput`. So an overlong path yields a perfectly healthy nvim
    /// server that nvmux itself can never connect to, and nothing anywhere
    /// reports an error.
    ///
    /// Note the kernel error carries `raw_os_error() == None`, so there is no
    /// `ENAMETOOLONG` to match on. Never write `raw_os_error() == Some(ENAMETOOLONG)`.
    #[error(
        "unix socket path is {len} bytes, max {max}: {}\n\
         hint: nvmux composes paths under {}. A longer path would be silently \
         truncated by Neovim, leaving a session nvmux could never reach.",
        .path.display(), crate::config::RUNTIME_DIR_PREFIX
    )]
    TooLong {
        path: PathBuf,
        len: usize,
        max: usize,
    },

    /// The runtime directory exists but is not a directory.
    ///
    /// Checked on the `lstat` result, not `stat`: a *symlink to* a directory
    /// passes `create_dir_all` and `Metadata::is_dir()`, and that is exactly the
    /// shape of a `/tmp` symlink attack.
    #[error("{}: not a directory (or is a symlink to one, which we refuse)", .0.display())]
    NotADirectory(PathBuf),

    /// Someone else owns our runtime directory.
    ///
    /// `/tmp` is mode 1777. The sticky bit stops another user *deleting* our
    /// directory; it does not stop them *creating* `/tmp/nvmux-<uid>` first,
    /// with any ownership and mode they like. This check is mandatory.
    #[error("{}: owned by uid {owner}, expected {expected}", .path.display())]
    BadOwner {
        path: PathBuf,
        owner: u32,
        expected: u32,
    },

    /// The runtime directory is group- or world-accessible.
    ///
    /// This is a code-execution control, not a privacy nicety: nvim creates its
    /// listen socket with `0777 & ~umask`, so a reachable socket lets any local
    /// user run `nvim_command("!sh")` as us.
    #[error("{}: mode is {mode:04o}, refusing to use a directory accessible to other users", .path.display())]
    BadMode { path: PathBuf, mode: u32 },

    /// A session id that is not 8 base32 characters.
    ///
    /// Ids become paths, so this is a containment check rather than a
    /// formatting nicety.
    #[error("malformed session id {0:?}: expected 8 lowercase base32 characters")]
    MalformedId(String),

    #[error("{}: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Failures talking msgpack-RPC to a Neovim server.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// Nothing is listening. Locally this means the session is dead and the
    /// socket file is stale; through an SSH forward it means the *master* died,
    /// because ssh accepts first and resets later.
    #[error("connection refused: {}", .0.display())]
    ConnectionRefused(PathBuf),

    /// The peer went away mid-conversation. Through an SSH forward this is the
    /// remote nvim dying, as distinct from `ConnectionRefused`.
    #[error("connection reset by peer")]
    Reset,

    /// The call did not answer in time. This does *not* mean the session is
    /// dead — see the note on [`crate::rpc`] about `nvim_list_bufs` blocking for
    /// the entire duration of any `system()` call.
    #[error("timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// Neovim answered with an error object.
    #[error("nvim returned an error: {0}")]
    Nvim(String),

    /// We could not make sense of the bytes on the wire.
    ///
    /// Any occurrence of this poisons the connection: rmpv has already consumed
    /// a partial frame, so the next read would decode garbage from the tail of
    /// this one. The client closes itself when this happens.
    #[error("malformed msgpack from nvim: {0}")]
    Protocol(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl RpcError {
    /// Whether this error means "there is definitely no server here", as opposed
    /// to "the server did not answer in time".
    ///
    /// Reaping a socket on a timeout would delete live sessions whenever the
    /// user is running a build, so the distinction is load-bearing.
    pub fn is_definitely_dead(&self) -> bool {
        matches!(self, RpcError::ConnectionRefused(_) | RpcError::Reset)
    }
}

/// Failures about sessions as entities.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("no session named {0:?}")]
    NotFound(String),

    #[error("a session named {0:?} already exists")]
    Exists(String),

    /// Session names are display metadata, but they still end up in shell
    /// commands and terminal output, so they are validated at the boundary.
    #[error("invalid session name {name:?}: {reason}")]
    InvalidName { name: String, reason: &'static str },

    /// The session has unsaved buffers. The picker turns this into
    /// `kill "dotfiles"? 2 unsaved buffers [y/N]` rather than failing.
    #[error("session {name:?} has {count} unsaved buffer(s)")]
    Dirty { name: String, count: usize },

    /// The socket appeared but nothing ever answered on it.
    #[error("session {name:?} did not become ready within {timeout:?}\n--- tail of {} ---\n{log_tail}", .log.display())]
    NotReady {
        name: String,
        timeout: std::time::Duration,
        log: PathBuf,
        log_tail: String,
    },

    /// The kill did not take effect, and nvmux left the session's files alone.
    ///
    /// Removing them anyway would orphan a running Neovim: with no socket in the
    /// runtime directory it would never appear in a listing again, and nothing
    /// could reach it.
    #[error("could not kill session {name:?}: {reason}")]
    NotKilled { name: String, reason: &'static str },

    #[error("session metadata at {}: {source}", .path.display())]
    Metadata {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Problems with the `nvim` binary itself, on either machine.
///
/// These are checked and reported at startup rather than surfacing later as a
/// mysterious connection failure.
#[derive(Debug, thiserror::Error)]
pub enum NvimError {
    #[error("{where_}: `nvim` not found on $PATH\nhint: nvmux needs Neovim >= {min} on {where_}")]
    NotFound { where_: String, min: &'static str },

    #[error("{where_}: Neovim {found} is too old; nvmux needs >= {min}\nhint: {min} is where `:detach` and `:connect` landed, which nvmux relies on")]
    TooOld {
        where_: String,
        found: String,
        min: &'static str,
    },

    #[error("{where_}: could not parse `nvim --version` output: {raw:?}")]
    UnparsableVersion { where_: String, raw: String },
}

/// Problems driving the `ssh` client.
#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("ssh: no ControlMaster running for {0}")]
    NoMaster(String),

    #[error("ssh: the connection to {0} died")]
    MasterDied(String),

    #[error("ssh: could not forward {remote} to {local}: {stderr}", remote = .remote.display(), local = .local.display())]
    ForwardFailed {
        local: PathBuf,
        remote: PathBuf,
        stderr: String,
    },

    #[error("ssh: authentication failed for {0}\nhint: nvmux uses your ssh client, so `ssh {0}` must work on its own first")]
    AuthFailed(String),

    #[error("ssh: host {0} is unreachable")]
    Unreachable(String),

    #[error("ssh exited {code}: {stderr}")]
    Failed { code: i32, stderr: String },

    #[error("`ssh` not found on $PATH")]
    NotFound,

    #[error("ssh {found} is too old; nvmux needs >= {min} for unix-socket forwarding")]
    TooOld { found: String, min: &'static str },
}

/// The error type crossing the [`crate::transport::Transport`] boundary.
///
/// Deliberately declared in full at milestone 1, with most variants unused:
/// fixing the error surface now is what stops the SSH work at milestone 5 from
/// reshaping every signature.
#[derive(Debug, thiserror::Error)]
pub enum NvmuxError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Nvim(#[from] NvimError),
    #[error(transparent)]
    Ssh(#[from] SshError),
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A milestone boundary. Every one of these is a `todo!()` that reports
    /// itself politely instead of panicking.
    #[error("not implemented yet: {0}")]
    Unimplemented(&'static str),
}

pub type Result<T> = std::result::Result<T, NvmuxError>;
