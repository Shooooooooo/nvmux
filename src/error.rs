//! Typed errors: `anyhow::Result` at boundaries, these where callers branch
//! (e.g. a stale socket is reaped, a busy one is not).
//!
//! Wrapping variants include the inner error in their message and do not
//! expose it as `source()`: `main` prints with anyhow's `{:#}`, which appends
//! every source, so doing both repeats the cause, and
//! [`NvmuxError::one_line`] prints only the message. The field is named
//! `error` because thiserror treats any field named `source` as the source.

use std::path::PathBuf;

/// Path construction and validation failures.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// A socket path exceeds the `sun_path` limit. Must be caught before
    /// `bind()`; see [`crate::paths`].
    #[error(
        "unix socket path is {len} bytes, max {max}: {}\n\
         hint: nvmux composes paths under {}. A longer path would be silently \
         truncated by Neovim, leaving a session nvmux could never reach.",
        .path.display(), crate::paths::RUNTIME_DIR_PREFIX
    )]
    TooLong {
        path: PathBuf,
        len: usize,
        max: usize,
    },

    /// Checked with `lstat`, not `stat`.
    #[error("{}: not a directory (or is a symlink to one, which we refuse)", .0.display())]
    NotADirectory(PathBuf),

    /// Another user owns the runtime directory. `/tmp`'s sticky bit stops
    /// others deleting it, not creating it first.
    #[error("{}: owned by uid {owner}, expected {expected}", .path.display())]
    BadOwner {
        path: PathBuf,
        owner: u32,
        expected: u32,
    },

    /// The runtime directory is group- or world-accessible. This is a
    /// code-execution control, not just privacy.
    #[error("{}: mode is {mode:04o}, refusing to use a directory accessible to other users", .path.display())]
    BadMode { path: PathBuf, mode: u32 },

    /// Ids become paths, so this is a containment check.
    #[error("malformed session id {0:?}: expected 8 lowercase base32 characters")]
    MalformedId(String),

    #[error("{}: {error}", .path.display())]
    Io {
        path: PathBuf,
        error: std::io::Error,
    },
}

/// msgpack-RPC failures talking to Neovim.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// Locally: nothing listens and the socket is stale, or, on macOS, a live
    /// session's accept queue is full, so the local reaper also asks the
    /// listing script. Through an SSH forward: the *master* died, since ssh
    /// accepts first and resets later.
    #[error("connection refused: {}", .0.display())]
    ConnectionRefused(PathBuf),

    /// Linux's answer to a full accept queue: a live session whose loop is
    /// not running (CPU-bound Lua, stopped), so never death.
    #[error("not accepting connections: {}", .0.display())]
    Busy(PathBuf),

    /// Through an SSH forward this means the remote nvim died.
    #[error("connection reset by peer")]
    Reset,

    /// Does *not* mean the session is dead: e.g. `nvim_list_bufs` blocks for
    /// the whole of any `system()` call (see [`crate::rpc`]).
    #[error("timed out after {0:?}")]
    Timeout(std::time::Duration),

    #[error("nvim returned an error: {0}")]
    Nvim(String),

    /// Poisons the connection: rmpv has consumed a partial frame, so the next
    /// read would decode garbage.
    #[error("malformed msgpack from nvim: {0}")]
    Protocol(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl RpcError {
    /// True only when no server is there; a timeout is not enough, since
    /// reaping on one would delete busy sessions.
    pub fn is_definitely_dead(&self) -> bool {
        matches!(self, RpcError::ConnectionRefused(_) | RpcError::Reset)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("no session named {0:?}")]
    NotFound(String),

    #[error("a session named {0:?} already exists")]
    Exists(String),

    /// A session-host script failed or refused. Its message is shown bare;
    /// wrapping it read badly.
    #[error("{0}")]
    ScriptFailed(String),

    /// Names reach shell commands and terminal output, so are validated.
    #[error("invalid session name {name:?}: {reason}")]
    InvalidName { name: String, reason: &'static str },

    /// The Neovim launch command. It must also carry the socket, which is how
    /// listing and kill find the session.
    #[error("invalid command {command:?}: {reason}")]
    InvalidCommand {
        command: String,
        reason: &'static str,
    },

    /// Neovim's start directory. Must be absolute: a relative one would
    /// resolve against the spawning shell's cwd.
    #[error("invalid working directory {directory:?}: {reason}")]
    InvalidDirectory {
        directory: String,
        reason: &'static str,
    },

    /// The socket never appeared, or nothing answered on it.
    #[error("session {name:?} did not become ready within {timeout:?}\n--- tail of {} ---\n{log_tail}", .log.display())]
    NotReady {
        name: String,
        timeout: std::time::Duration,
        log: PathBuf,
        log_tail: String,
    },

    /// The files were kept: removing them would orphan a running Neovim.
    #[error("could not kill session {name:?}: {reason}")]
    NotKilled { name: String, reason: &'static str },

    /// Neovim aborts (not errors) on its 17th UI; nvmux refuses well before.
    /// See [`crate::pty`].
    #[error("session {id} already has {attached} attached UIs; detach one before attaching again")]
    TooManyUis { id: String, attached: usize },

    #[error("session metadata at {}: {error}", .path.display())]
    Metadata {
        path: PathBuf,
        error: serde_json::Error,
    },
}

/// Problems with the `nvim` binary on either machine, checked at startup.
#[derive(Debug, thiserror::Error)]
pub enum NvimError {
    #[error("{where_}: `nvim` not found on $PATH\nhint: nvmux needs Neovim >= {min} on {where_}")]
    NotFound { where_: String, min: &'static str },

    #[error("{where_}: Neovim {found} is too old; nvmux needs >= {min}\nhint: {min} is where `:detach` landed, which nvmux relies on")]
    TooOld {
        where_: String,
        found: String,
        min: &'static str,
    },

    #[error("{where_}: could not parse `nvim --version` output: {raw:?}")]
    UnparsableVersion { where_: String, raw: String },
}

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

/// Config file loading failures. A missing file at a default path is not an
/// error (defaults apply); one named by `$NVMUX_CONFIG` is.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{}: config file not found (named by $NVMUX_CONFIG)", .0.display())]
    ExplicitMissing(PathBuf),

    /// Exists but unreadable (e.g. permissions, or a directory).
    #[error("{}: {error}", .path.display())]
    Read {
        path: PathBuf,
        error: std::io::Error,
    },

    /// Invalid TOML, an unknown key, or a value (e.g. the prefix) that did
    /// not parse.
    #[error("{}: {error}", .path.display())]
    Parse {
        path: PathBuf,
        error: toml::de::Error,
    },

    /// A value fails our own range checks (e.g. `keys.timeout_ms = 0` would
    /// spin the relay).
    #[error("{}: {message}", .path.display())]
    Invalid { path: PathBuf, message: String },

    /// The first-run config could not be written. Non-fatal: the chosen prefix
    /// is kept in memory.
    #[error("{}: {error}", .path.display())]
    Write {
        path: PathBuf,
        error: std::io::Error,
    },
}

/// Why a settings save wrote nothing (see [`crate::config::save`]). Messages
/// are one line, for the hint row, with the path as the user would type it.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    /// `$NVMUX_CONFIG`, `$XDG_CONFIG_HOME` and `$HOME` are all unset or empty.
    #[error("not saved: there is no $HOME to keep a config file in")]
    Nowhere,

    #[error("not saved: could not read {}: {error}", crate::config::shown(.path))]
    Read {
        path: PathBuf,
        error: std::io::Error,
    },

    /// The file no longer parses (hand-edited since it was read), so it is
    /// not overwritten.
    #[error("not saved: {} no longer reads: {message}", crate::config::shown(.path))]
    Unreadable { path: PathBuf, message: String },

    /// The `settings` being saved were also changed in the file since it was
    /// read. A second save overwrites them.
    #[error("{}", changed_too(.settings, .path))]
    Changed {
        path: PathBuf,
        settings: Vec<String>,
    },

    #[error("not saved: could not write {}: {error}", crate::config::shown(.path))]
    Write {
        path: PathBuf,
        error: std::io::Error,
    },

    /// The file's layout is one the editor cannot handle.
    #[error("not saved: {} could not be edited to hold the change", crate::config::shown(.path))]
    Unedited { path: PathBuf },
}

/// `keys.timeout_ms changed in config.toml too`, or with several, how many.
fn changed_too(settings: &[String], path: &std::path::Path) -> String {
    let what = match settings {
        [one] => one.clone(),
        many => format!("{} settings", many.len()),
    };
    let file = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    format!("{what} changed in {file} too")
}

impl SaveError {
    /// For [`SaveError::Changed`], how many settings a second save would
    /// overwrite.
    pub fn changed_in_file(&self) -> Option<usize> {
        match self {
            SaveError::Changed { settings, .. } => Some(settings.len()),
            _ => None,
        }
    }
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
}

impl NvmuxError {
    /// Join the message's lines, for one-row displays that would otherwise
    /// cut it at the first newline.
    pub fn one_line(&self) -> String {
        self.to_string().lines().collect::<Vec<_>>().join(" — ")
    }
}

pub type Result<T> = std::result::Result<T, NvmuxError>;

impl From<nix::errno::Errno> for NvmuxError {
    fn from(e: nix::errno::Errno) -> Self {
        NvmuxError::Io(std::io::Error::from(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn errors_are_flattened_to_one_line() {
        let e = NvmuxError::Session(SessionError::NotReady {
            name: "x".into(),
            timeout: Duration::from_secs(1),
            log: "x.log".into(),
            log_tail: "line one\nline two".into(),
        });
        assert!(
            e.to_string().contains('\n'),
            "the fixture must be multi-line"
        );
        let flat = e.one_line();
        assert!(!flat.contains('\n'), "{flat:?}");
        assert!(
            flat.contains("line one") && flat.contains("line two"),
            "nothing is lost on the way: {flat:?}"
        );
    }

    /// Both `{}` and anyhow's `{:#}` (which appends sources) must show the
    /// cause once.
    #[test]
    fn a_wrapped_error_is_said_once() {
        let denied = || std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let cases = [
            (
                anyhow::Error::from(ConfigError::Read {
                    path: "c.toml".into(),
                    error: denied(),
                }),
                "c.toml: permission denied",
            ),
            (
                anyhow::Error::from(ConfigError::Write {
                    path: "c.toml".into(),
                    error: denied(),
                }),
                "c.toml: permission denied",
            ),
            (
                anyhow::Error::from(PathError::Io {
                    path: "/tmp/nvmux-0".into(),
                    error: denied(),
                }),
                "/tmp/nvmux-0: permission denied",
            ),
            // serde_json exposes the I/O error as its own source.
            (
                anyhow::Error::from(SessionError::Metadata {
                    path: "x.json".into(),
                    error: serde_json::Error::io(denied()),
                }),
                "session metadata at x.json: permission denied",
            ),
        ];
        for (e, want) in cases {
            assert_eq!(format!("{e}"), want);
            assert_eq!(format!("{e:#}"), want, "the cause is said once");
        }
    }
}
