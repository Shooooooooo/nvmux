//! Sessions on another host, reached over SSH. Milestone 5.
//!
//! This file is a full `impl Transport` from milestone 1 with every method
//! unimplemented, and that is deliberate: it forces [`Transport`] to be
//! object-safe for *both* implementations from the start, and makes any
//! local-only assumption that leaks into the trait a compile error now rather
//! than a redesign later.

use std::path::PathBuf;

use crate::error::{NvmuxError, Result};
use crate::ids;
use crate::session::Session;
use crate::transport::{Location, Transport};

pub struct SshTransport {
    location: Location,
    /// Short, stable, filename-safe token for this host, used to namespace the
    /// local end of each forward and the `ControlPath`.
    #[allow(dead_code)]
    host_token: String,
}

impl SshTransport {
    pub fn new(host: String) -> Result<Self> {
        let host_token = ids::host_token(&host);
        Ok(Self {
            location: Location::Ssh(host),
            host_token,
        })
    }
}

impl Transport for SshTransport {
    fn location(&self) -> &Location {
        &self.location
    }

    fn list_sessions(&self) -> Result<Vec<Session>> {
        Err(NvmuxError::Unimplemented("ssh list_sessions (milestone 5)"))
    }

    fn create_session(&self, _name: &str) -> Result<Session> {
        Err(NvmuxError::Unimplemented(
            "ssh create_session (milestone 5)",
        ))
    }

    fn kill_session(&self, _s: &Session, _force: bool) -> Result<()> {
        Err(NvmuxError::Unimplemented("ssh kill_session (milestone 5)"))
    }

    fn rename_session(&self, _s: &Session, _new_name: &str) -> Result<()> {
        Err(NvmuxError::Unimplemented(
            "ssh rename_session (milestone 5)",
        ))
    }

    fn local_socket_for(&self, _s: &Session) -> Result<PathBuf> {
        // Milestone 5: allocate <runtime_dir>/<host_token>-<id>.sock, unlink any
        // leftover file, then `ssh -O forward -L <local>:<remote>` onto the
        // existing ControlMaster.
        //
        // The unlink is not optional. `ssh -O cancel` exits 0 and leaves the
        // local socket file on disk; a later `-O forward` onto that path then
        // fails with rc 255 and `forwarding request failed`, which breaks
        // detach-then-reattach — the exact flow this tool exists for. Verified
        // both ways: the failure is real without `StreamLocalBindUnlink=yes` on
        // the *master* invocation, and setting it on the `-O forward` client
        // does nothing, because the master performs the bind.
        Err(NvmuxError::Unimplemented(
            "ssh local_socket_for (milestone 5)",
        ))
    }
}
