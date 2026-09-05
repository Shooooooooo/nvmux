//! Typed errors: `anyhow::Result` at boundaries, these where callers branch.
//! Every variant exists because some caller makes a decision on it — a stale
//! socket gets reaped and a busy one does not, a kill that did not take effect
//! must not delete files.

use std::path::PathBuf;

/// Failures constructing or validating paths. Separate from `io::Error` because
/// the length rule is ours, not the kernel's.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// A composed socket path exceeded the `sun_path` budget. Must be caught
    /// before anyone calls `bind()` — see [`crate::config`] for why.
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

    /// The runtime directory exists but is not a directory. Checked on `lstat`,
    /// not `stat` — see [`crate::config`].
    #[error("{}: not a directory (or is a symlink to one, which we refuse)", .0.display())]
    NotADirectory(PathBuf),

    /// Someone else owns our runtime directory. `/tmp` is mode 1777: the sticky
    /// bit stops another user deleting it, not creating it first.
    #[error("{}: owned by uid {owner}, expected {expected}", .path.display())]
    BadOwner {
        path: PathBuf,
        owner: u32,
        expected: u32,
    },

    /// The runtime directory is group- or world-accessible — a code-execution
    /// control, not a privacy nicety. See [`crate::config`].
    #[error("{}: mode is {mode:04o}, refusing to use a directory accessible to other users", .path.display())]
    BadMode { path: PathBuf, mode: u32 },

    /// A session id that is not 8 base32 characters. Ids become paths, so this
    /// is a containment check.
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

    /// We could not make sense of the bytes on the wire. This poisons the
    /// connection: rmpv has already consumed a partial frame, so the next read
    /// would decode garbage from the tail of this one.
    #[error("malformed msgpack from nvim: {0}")]
    Protocol(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl RpcError {
    /// "There is definitely no server here", as opposed to "it did not answer in
    /// time". Reaping on a timeout would delete sessions that are merely busy.
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

    /// The socket appeared but nothing ever answered on it.
    #[error("session {name:?} did not become ready within {timeout:?}\n--- tail of {} ---\n{log_tail}", .log.display())]
    NotReady {
        name: String,
        timeout: std::time::Duration,
        log: PathBuf,
        log_tail: String,
    },

    /// The kill did not take effect, so the files were left alone. Removing them
    /// anyway would orphan a running Neovim that nothing could ever reach again.
    #[error("could not kill session {name:?}: {reason}")]
    NotKilled { name: String, reason: &'static str },

    #[error("session metadata at {}: {source}", .path.display())]
    Metadata {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Problems with the `nvim` binary itself, on either machine. Checked at startup
/// rather than surfacing later as a mysterious connection failure.
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

    /// A `todo!()` that reports itself politely instead of panicking.
    #[error("not implemented yet: {0}")]
    Unimplemented(&'static str),
}

pub type Result<T> = std::result::Result<T, NvmuxError>;
