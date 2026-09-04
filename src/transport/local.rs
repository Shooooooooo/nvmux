//! Sessions on this machine. No SSH anywhere in this file.

use std::path::PathBuf;

use crate::config;
use crate::error::{NvmuxError, Result};
use crate::session::Session;
use crate::shell;
use crate::transport::exec::{Executor, LocalExecutor};
use crate::transport::{finish_listing, protocol, Location, Transport};

pub struct LocalTransport {
    location: Location,
    dir: PathBuf,
    exec: LocalExecutor,
}

impl LocalTransport {
    pub fn new() -> Result<Self> {
        Ok(Self {
            location: Location::Local,
            dir: config::ensure_runtime_dir()?,
            exec: LocalExecutor,
        })
    }

    pub fn runtime_dir(&self) -> &std::path::Path {
        &self.dir
    }
}

impl Transport for LocalTransport {
    fn location(&self) -> &Location {
        &self.location
    }

    fn list_sessions(&self) -> Result<Vec<Session>> {
        let dir = self.dir.to_string_lossy().into_owned();
        let out = self.exec.run_script(shell::LIST_SCRIPT, &[&dir])?;
        if !out.ok() {
            tracing::warn!(status = out.status, stderr = %out.stderr, "list.sh failed");
        }
        let mut sessions = protocol::rows_to_sessions(protocol::parse_listing(&out.stdout)?);

        // The script's `kill -0` result is only a hint. Locally the socket is
        // right there, so replace it with the real answer: a connect plus an
        // RPC round trip. This is also what distinguishes a running-but-busy
        // session from a dead one, which matters because stale cleanup must
        // never fire on the former.
        for s in &mut sessions {
            let paths = config::SessionPaths::new(&self.dir, &s.id)?;
            s.state.liveness = crate::rpc::probe(&paths.sock);
        }

        finish_listing(sessions)
    }

    fn create_session(&self, _name: &str) -> Result<Session> {
        Err(NvmuxError::Unimplemented("create_session (milestone 2)"))
    }

    fn kill_session(&self, _s: &Session, _force: bool) -> Result<()> {
        Err(NvmuxError::Unimplemented("kill_session (milestone 2)"))
    }

    fn rename_session(&self, _s: &Session, _new_name: &str) -> Result<()> {
        Err(NvmuxError::Unimplemented("rename_session (milestone 2)"))
    }

    fn local_socket_for(&self, s: &Session) -> Result<PathBuf> {
        // Locally this is the identity function: the session socket already is
        // a path on this machine. All the interesting work happens in the SSH
        // implementation, which is exactly why this is the seam.
        Ok(config::SessionPaths::new(&self.dir, &s.id)?.sock)
    }
}
