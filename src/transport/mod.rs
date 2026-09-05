//! The local/remote seam.
//!
//! Everything above this module — the picker, the attach path — is written once
//! against [`Transport`] and does not know whether the sessions it is listing
//! live on this machine or on the far end of an SSH connection.

pub mod exec;
pub mod local;
pub mod protocol;
pub mod remote;

use std::path::PathBuf;

use crate::error::{NvmuxError, Result};
use crate::session::Session;

/// Where a set of sessions lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Local,
    /// Passed to `ssh` **verbatim**, so a hostname, `user@host` or any
    /// `~/.ssh/config` alias works without nvmux understanding it.
    Ssh(String),
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Location::Local => f.write_str("local"),
            Location::Ssh(host) => f.write_str(host),
        }
    }
}

/// Session management, independent of where the sessions run.
///
/// Object-safe on purpose: the picker holds a `Box<dyn Transport>` and never
/// branches on which one it has.
pub trait Transport {
    fn location(&self) -> &Location;

    /// Every session on the host, with liveness already determined. One call,
    /// not one per session — see [`crate::shell::LIST_SCRIPT`].
    fn list_sessions(&self) -> Result<Vec<Session>>;

    fn create_session(&self, name: &str) -> Result<Session>;

    /// Terminate a session, unconditionally: nvmux never asks about unsaved
    /// buffers. The ordinary way out is `:q` in the session itself, which ends
    /// it because the editor *is* the session.
    fn kill_session(&self, s: &Session) -> Result<()>;

    /// Rename is a metadata edit and nothing more. The socket is never renamed
    /// or moved: its path is the session's stable identity and the display name
    /// is only data, so no SSH forward has to be rebuilt.
    fn rename_session(&self, s: &Session, new_name: &str) -> Result<()>;

    /// A socket path on **this** machine that `nvim --server` can use.
    ///
    /// The single seam that makes remote sessions work: locally the session
    /// socket itself, over SSH the local end of a forward. Everything downstream
    /// is identical in both cases.
    fn local_socket_for(&self, s: &Session) -> Result<PathBuf>;
}

/// Build the transport for a location.
pub fn open(location: Location) -> Result<Box<dyn Transport>> {
    match location {
        Location::Local => Ok(Box::new(local::LocalTransport::new()?)),
        Location::Ssh(host) => Ok(Box::new(remote::SshTransport::new(host)?)),
    }
}

/// Shared by both transports: turn a listing into sessions, sorted for display.
pub(crate) fn finish_listing(mut sessions: Vec<Session>) -> Result<Vec<Session>> {
    // Case-folded, so `Api` and `api` do not end up in surprising places.
    sessions.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(sessions)
}

impl From<NvmuxError> for std::io::Error {
    fn from(e: NvmuxError) -> Self {
        std::io::Error::other(e.to_string())
    }
}
