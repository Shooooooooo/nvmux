//! A local endpoint: an address on this machine that `nvim --server` and
//! nvmux's own RPC can connect to, served by nvmux.
//!
//! The relay ([`crate::transport::relay`]) has no ssh forward to hand the
//! client — the one connection it has is a stream of frames — so it serves
//! each session's address itself, and carries whatever connects there over a
//! channel of its own. This is that address: a unix socket, as everywhere else
//! nvmux runs on Unix, and a named pipe on Windows (see
//! [`crate::sys::windows::pipe`]). Neovim takes either for `--server`.
//!
//! Both are private to the user by the same kind of control: a socket in a
//! directory only its owner can enter, and a pipe whose access list names only
//! its owner. Not a loopback TCP port, which every account on the machine can
//! connect to — and a session is a shell as its user.

use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
pub use std::os::unix::net::UnixStream as Stream;

#[cfg(windows)]
pub use crate::sys::windows::pipe::{PipeListener as Listener, PipeStream as Stream};

/// A listening unix socket, removed again when it is dropped.
#[cfg(unix)]
pub struct Listener {
    inner: std::os::unix::net::UnixListener,
    path: PathBuf,
}

#[cfg(unix)]
impl Listener {
    /// Listen at `path`, which must be in a directory only this user can use:
    /// the socket is as private as that directory makes it, and no more.
    pub fn bind(path: &Path) -> io::Result<Self> {
        let inner = std::os::unix::net::UnixListener::bind(path)?;
        Ok(Self {
            inner,
            path: path.to_path_buf(),
        })
    }

    /// The next connection, waiting for one.
    pub fn accept(&self) -> io::Result<Stream> {
        self.inner.accept().map(|(stream, _)| stream)
    }

    /// Where this listens.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Wake a thread parked in [`Listener::accept`] at `path`, by connecting to it
/// and going away: how a listener's thread is told to look at its stop flag,
/// since an accept cannot otherwise be interrupted.
pub fn wake(path: &Path) {
    drop(Stream::connect(path));
}

/// The address a listener for one session is served at, under `dir` on Unix
/// or in the pipe namespace on Windows, where `dir` names nothing and `tag`
/// (unique to this nvmux) keeps two runs on one machine apart.
pub fn address(dir: &Path, tag: &str, host_token: &str, id: &str) -> PathBuf {
    #[cfg(unix)]
    {
        let _ = tag;
        dir.join(format!("{host_token}-{id}.sock"))
    }
    #[cfg(windows)]
    {
        let _ = dir;
        PathBuf::from(format!(r"\\.\pipe\nvmux-{tag}-{host_token}-{id}"))
    }
}
