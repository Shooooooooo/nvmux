//! Sessions on another host, reached through the relay (see [`crate::mux`]).
//!
//! The ssh transport's twin ([`crate::transport::remote`]): the same scripts,
//! run in the same kind of shell over there, reading and writing the same
//! files — the bodies the two share are in [`crate::transport`]. What differs
//! is how the host is reached. There, a ControlMaster, and a forward added to
//! it per session; here, one plain ssh connection with the relay at the far
//! end, and a channel per shell and per socket inside it. That is the only
//! way there is on Windows, whose ssh cannot multiplex, and a way round a host
//! that refuses unix-socket forwards.
//!
//! Where the ssh transport hands the client the local end of a forward, this
//! serves an endpoint of its own per session ([`crate::ipc`]) — a unix socket
//! under a directory only this run uses, or a named pipe only this user can
//! open — and carries whatever connects there over a fresh channel to the
//! session's socket. Everything downstream of [`Transport::local_socket_for`]
//! is then as it is for either other transport: a path `nvim --server` and
//! [`crate::rpc`] can connect to.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::{NvimError, NvmuxError, Result};
use crate::ids;
use crate::ipc;
use crate::launch::Launch;
use crate::mux::{self, Link, ShellPipes};
use crate::nvim;
use crate::paths;
use crate::proc::{Output, Shell};
use crate::session::Session;
use crate::shell;
use crate::ssh::{self, classify};
use crate::transport::{self, protocol, Host, Location, Reconnect, Transport};

/// The link every stream to one host rides on, replaceable: a reconnection
/// puts a new one here, and every endpoint's next connection opens its channel
/// on whatever is here when it arrives.
#[derive(Default)]
pub struct LinkSlot(Mutex<Option<Link>>);

impl std::fmt::Debug for LinkSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("LinkSlot").field(&*self.lock()).finish()
    }
}

impl LinkSlot {
    fn lock(&self) -> MutexGuard<'_, Option<Link>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The link, if there is one and it is up.
    pub fn current(&self) -> Option<Link> {
        self.lock().clone().filter(Link::is_up)
    }

    /// A shell on the far side, on the link as it is — never a reconnection,
    /// which a caller nobody is watching must not start: it could ask for a
    /// passphrase on a terminal that belongs to the picker.
    pub fn open_shell(&self) -> Result<Shell> {
        let link = self.current().ok_or_else(|| {
            NvmuxError::Io(std::io::Error::other("the relay's connection is down"))
        })?;
        open_shell(&link)
    }
}

/// How a link to the host is made, given the bound on connecting if there is
/// one: `ssh` in the field ([`ssh::relay_command`]), a shell here in the tests.
type Connector = Arc<dyn Fn(Option<u64>) -> std::process::Command + Send + Sync>;

pub struct RelayTransport {
    location: Location,
    host: String,
    connect: Connector,
    link: Arc<LinkSlot>,
    /// The shell every script runs in, on the link. Replaced, with the link if
    /// that is what went, by [`Self::run_script`] once it is found dead.
    shell: Mutex<Option<Shell>>,
    /// Short, stable, filename-safe token for this host, naming its endpoints.
    host_token: String,
    /// Where this run's endpoints are served; see [`Place`].
    place: Place,
    /// The endpoint serving each session reached so far, by id.
    endpoints: Mutex<HashMap<String, Endpoint>>,
    /// The runtime directory on the host that owns the nvim processes.
    remote_dir: String,
    /// That host's home directory, from the greeting, or empty.
    remote_home: String,
    /// The listing that came back with the greeting, for the first listing to
    /// take; see the ssh transport's field of the same name.
    first_listing: Mutex<Option<String>>,
}

/// Needed by `Result::expect_err` in the tests; never logged.
impl std::fmt::Debug for RelayTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayTransport")
            .field("host", &self.host)
            .field("remote_dir", &self.remote_dir)
            .finish_non_exhaustive()
    }
}

impl RelayTransport {
    pub fn new(host: String) -> Result<Self> {
        Self::with_dir(host, paths::ensure_runtime_dir()?)
    }

    /// Serve this run's endpoints from under `local_dir` rather than the
    /// runtime directory, so a test can keep its own apart. The same security
    /// check applies.
    pub fn with_dir(host: String, local_dir: PathBuf) -> Result<Self> {
        let named = host.clone();
        let connect: Connector = Arc::new(move |timeout| ssh::relay_command(&named, timeout));
        Self::with_connector(host, local_dir, connect)
    }

    /// The same, with the link made by `connect` rather than by `ssh`.
    fn with_connector(host: String, local_dir: PathBuf, connect: Connector) -> Result<Self> {
        paths::ensure_dir_secure(&local_dir)?;
        let link = Link::connect(connect(None), &host)?;
        match Self::greet(host, local_dir, connect, &link) {
            Ok(transport) => Ok(transport),
            Err(e) => {
                link.close();
                Err(e)
            }
        }
    }

    /// Everything after the link is up: the greeting, the version gate, and a
    /// place to serve endpoints from — any of which failing leaves a link for
    /// the caller to hang up.
    fn greet(host: String, local_dir: PathBuf, connect: Connector, link: &Link) -> Result<Self> {
        let mut shell = open_shell(link)?;

        // One round trip for the runtime directory, the remote Neovim version
        // and the listing the picker is about to draw — as over a master.
        let out = checked_script(link, &mut shell, shell::HELLO_SCRIPT, &[])?;
        let probe = protocol::parse_probe(&out.stdout)?;
        if probe.nvim_banner.is_empty() {
            return Err(NvimError::NotFound {
                where_: host.clone(),
                min: nvim::MIN_VERSION,
            }
            .into());
        }
        let version = nvim::check_banner(&host, &probe.nvim_banner)?;
        tracing::info!(%host, %version, dir = %probe.runtime_dir, "remote host ready (relay)");

        let place = Place::claim(&local_dir)?;
        let slot = LinkSlot::default();
        *slot.lock() = Some(link.clone());
        Ok(Self {
            location: Location::Ssh(host.clone()),
            host_token: ids::host_token(&host),
            host,
            connect,
            link: Arc::new(slot),
            shell: Mutex::new(Some(shell)),
            place,
            endpoints: Mutex::new(HashMap::new()),
            remote_dir: probe.runtime_dir,
            remote_home: probe.home,
            first_listing: Mutex::new(Some(out.stdout)),
        })
    }

    /// The process carrying the link — the local `ssh` — while there is one.
    /// For diagnostics, and for the tests that take a link away to see it
    /// come back.
    pub fn link_pid(&self) -> Option<u32> {
        self.link.lock().as_ref().map(Link::pid)
    }

    fn remote_sock(&self, id: &str) -> Result<PathBuf> {
        transport::remote_sock(&self.remote_dir, id)
    }

    fn endpoints(&self) -> MutexGuard<'_, HashMap<String, Endpoint>> {
        self.endpoints.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The link as it is now, up or not.
    fn link(&self) -> Result<Link> {
        self.link
            .lock()
            .clone()
            .ok_or_else(|| NvmuxError::Io(std::io::Error::other("the relay has no connection")))
    }

    /// Run one of the shared scripts on the host, in its shell.
    ///
    /// A shell found dead is replaced *before* a run and never after one — a
    /// script that was half way through must not run twice — and the link it
    /// rode on is brought back first if that is what went, as the ssh
    /// transport restores its master.
    fn run_script(&self, script: &str, args: &[&str]) -> Result<Output> {
        let mut slot = transport::claim_shell(&self.shell)?;
        if slot.as_mut().is_none_or(|shell| !shell.is_alive()) {
            *slot = None;
            self.restore_link(None)?;
            *slot = Some(open_shell(&self.link()?)?);
        }
        let link = self.link()?;
        let shell = slot.as_mut().expect("just started");
        let result = checked_script(&link, shell, script, args);
        // A shell that died is dropped now; one that merely ran a script that
        // failed is kept.
        if result.is_err() && slot.as_mut().is_some_and(|shell| !shell.is_alive()) {
            *slot = None;
        }
        result
    }

    /// Bring the link back if it has gone, and say which of the two it found.
    ///
    /// Asking is a flag and a lock: whether the link is up is what its reader
    /// saw of ssh's stdout, so there is no fork to pay for it, and it can be
    /// asked on every attach. What it cannot see is a link that is wedged
    /// rather than gone; that is what the connection's `ServerAlive` options
    /// are for, as they are a master's.
    ///
    /// `connect_timeout` bounds the connection attempt, for the one caller
    /// that retries — see [`crate::reconnect`].
    fn restore_link(&self, connect_timeout: Option<u64>) -> Result<Reconnect> {
        let mut slot = self.link.lock();
        if slot.as_ref().is_some_and(Link::is_up) {
            return Ok(Reconnect::Unneeded);
        }
        if let Some(old) = slot.take() {
            old.close();
        }
        let link = Link::connect((self.connect)(connect_timeout), &self.host)?;
        *slot = Some(link);
        tracing::info!(host = %self.host, "reconnected (relay)");
        Ok(Reconnect::Restored)
    }

    /// Stop serving endpoints for sessions that no longer exist.
    ///
    /// Deliberately not called when a listing fails: "the host did not
    /// answer" must never be mistaken for "you have no sessions".
    fn sweep_endpoints(&self, live: &[Session]) {
        self.endpoints().retain(|id, _| {
            let keep = live.iter().any(|s| &s.id == id);
            if !keep {
                tracing::debug!(%id, "stopped serving a session that has gone");
            }
            keep
        });
    }
}

impl Drop for RelayTransport {
    /// Endpoints first, so nothing new arrives; then the shell, which asks its
    /// channel to go; then the link, which takes whatever is left with it.
    fn drop(&mut self) {
        self.endpoints().clear();
        if let Ok(mut shell) = self.shell.lock() {
            shell.take();
        }
        if let Some(link) = self.link.lock().take() {
            link.close();
        }
    }
}

/// A shell on the far side of `link`, as a [`Shell`] like any other.
fn open_shell(link: &Link) -> Result<Shell> {
    let chan = link
        .open_shell(mux::OPEN_TIMEOUT)
        .map_err(|e| link_error(link, e))?;
    Ok(Shell::over(
        Box::new(ShellPipes::new(chan)),
        format!("relay {}", link.host()),
    )?)
}

/// An I/O failure on the link, as the link's own account of why it went where
/// it has one — ssh's words — and as itself otherwise.
fn link_error(link: &Link, e: std::io::Error) -> NvmuxError {
    match link.why_down() {
        Some(why) => why.into(),
        None => NvmuxError::Io(e),
    }
}

/// Run a script and insist it actually ran: see the ssh transport's function
/// of the same name, whose rules these are. A shell that went because the link
/// went is reported as the link going.
fn checked_script(link: &Link, shell: &mut Shell, script: &str, args: &[&str]) -> Result<Output> {
    let out = match shell.run(script, args) {
        Ok(out) => out,
        Err(died) => {
            return Err(match link.why_down() {
                Some(why) => why.into(),
                None => classify(link.host(), died.status, &died.stderr).into(),
            });
        }
    };
    if !out.ok() && out.stdout.trim().is_empty() {
        return Err(classify(link.host(), out.status, &out.stderr).into());
    }
    Ok(out)
}

impl Host for RelayTransport {
    fn location(&self) -> &Location {
        &self.location
    }

    fn dir(&self) -> &str {
        &self.remote_dir
    }

    fn run(&self, script: &str, args: &[&str]) -> Result<Output> {
        self.run_script(script, args)
    }

    fn take_listing(&self) -> Option<String> {
        self.first_listing.lock().ok().and_then(|mut f| f.take())
    }

    fn settle(&self, listed: Vec<Session>) -> Result<Vec<Session>> {
        let listed = transport::keep_serving(listed);
        self.sweep_endpoints(&listed);
        Ok(listed)
    }

    fn host_sock(&self, id: &str) -> Result<PathBuf> {
        self.remote_sock(id)
    }

    /// Serve an endpoint for the session, if one is not already being served:
    /// the relay's forward. Proves nothing about the session — nothing is
    /// opened on the far side until something connects — which is what
    /// [`transport::create`]'s wait for an answer is for.
    fn reach(&self, id: &str, host_sock: &Path) -> Result<PathBuf> {
        let mut endpoints = self.endpoints();
        if let Some(endpoint) = endpoints.get(id).filter(|e| e.is_serving()) {
            return Ok(endpoint.address.clone());
        }
        let address = self.place.address(&self.host_token, id)?;
        let endpoint = Endpoint::serve(
            address.clone(),
            host_sock.to_string_lossy().into_owned(),
            Arc::clone(&self.link),
        )?;
        endpoints.insert(id.to_string(), endpoint);
        Ok(address)
    }

    fn unreach(&self, id: &str) {
        self.endpoints().remove(id);
    }

    fn write_meta(&self, session: &Session) -> Result<()> {
        transport::write_meta_remotely(self, session)
    }

    fn log_path(&self, id: &str) -> PathBuf {
        PathBuf::from(format!("{}:{}/{}.log", self.host, self.remote_dir, id))
    }
}

impl Transport for RelayTransport {
    fn location(&self) -> &Location {
        &self.location
    }

    fn list_sessions(&self) -> Result<Vec<Session>> {
        transport::list(self)
    }

    fn home(&self) -> &str {
        &self.remote_home
    }

    /// A shell on the link this transport already has, opened by the
    /// completion worker when it wants one: no new connection, and so no
    /// prompt anyone would have to answer.
    fn dir_source(&self) -> crate::dirs::DirSource {
        crate::dirs::DirSource::Relay(Arc::clone(&self.link))
    }

    fn create_session(&self, name: &str, launch: &Launch, directory: &str) -> Result<Session> {
        transport::create(self, name, launch, directory)
    }

    fn kill_session(&self, s: &Session) -> Result<()> {
        transport::kill(self, s)
    }

    fn rename_session(&self, s: &Session, new_name: &str) -> Result<()> {
        transport::rename_remotely(self, &self.list_sessions()?, s, new_name)
    }

    fn renumber(&self, sessions: &[Session]) -> Result<()> {
        transport::renumber_remotely(self, sessions)
    }

    /// The link first — cheap, see [`RelayTransport::restore_link`] — so a
    /// link that went while the picker was up is back before an endpoint is
    /// handed out that nothing could be carried through.
    fn local_socket_for(&self, s: &Session) -> Result<PathBuf> {
        let remote = self.remote_sock(&s.id)?;
        self.restore_link(None)?;
        self.reach(&s.id, &remote)
    }

    /// Bounded, because it is retried: see the ssh transport's.
    fn reconnect(&self) -> Result<Reconnect> {
        self.restore_link(Some(RECONNECT_TIMEOUT_SECS))
    }
}

/// How long one reconnection attempt may spend reaching the host; the ssh
/// transport's number, for its reason.
const RECONNECT_TIMEOUT_SECS: u64 = 10;

/// Where one run serves its endpoints from.
///
/// On Unix, a directory of its own under the runtime directory — 0700, as that
/// one is, and named for this process and a nonce, so two runs never share one
/// and a run that died without tidying up can be told from one still going. On
/// Windows, a tag in every pipe's name, for the same reason: the pipe namespace
/// is the machine's, and a pipe goes with the process that made it.
struct Place {
    dir: PathBuf,
    tag: String,
}

impl Place {
    fn claim(runtime_dir: &Path) -> Result<Self> {
        let tag = ids::nonce()?;
        #[cfg(unix)]
        {
            sweep_stale_places(runtime_dir);
            let dir = runtime_dir.join(format!("relay-{}-{tag}", std::process::id()));
            paths::ensure_dir_secure(&dir)?;
            Ok(Self { dir, tag })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                dir: runtime_dir.to_path_buf(),
                tag,
            })
        }
    }

    /// The address one session's endpoint is served at, checked against the
    /// socket budget where it is a socket.
    fn address(&self, host_token: &str, id: &str) -> Result<PathBuf> {
        if !ids::is_valid_id(id) {
            return Err(crate::error::PathError::MalformedId(id.to_string()).into());
        }
        let address = ipc::address(&self.dir, &self.tag, host_token, id);
        #[cfg(unix)]
        paths::check_sock_path(&address)?;
        Ok(address)
    }
}

#[cfg(unix)]
impl Drop for Place {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Remove the endpoint directories of runs that are no longer running: a run
/// that was killed leaves its directory behind, with sockets nothing listens
/// on in it. Only ever a directory named for a process that is gone, in a
/// runtime directory only this user can write to.
#[cfg(unix)]
fn sweep_stale_places(runtime_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("relay-"))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<i32>().ok())
        else {
            continue;
        };
        if pid <= 0 || pid == std::process::id() as i32 {
            continue;
        }
        let gone = matches!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
            Err(nix::errno::Errno::ESRCH)
        );
        if gone && entry.file_type().is_ok_and(|t| t.is_dir()) {
            tracing::debug!(dir = %entry.path().display(), "removing a dead run's endpoints");
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// One session's endpoint: a listener, and the thread that accepts on it.
struct Endpoint {
    address: PathBuf,
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl Endpoint {
    fn serve(address: PathBuf, remote: String, link: Arc<LinkSlot>) -> Result<Self> {
        let listener = ipc::Listener::bind(&address)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("nvmux-endpoint".into())
            .spawn({
                let stop = Arc::clone(&stop);
                move || accept(listener, &remote, &link, &stop)
            })?;
        Ok(Self {
            address,
            stop,
            thread,
        })
    }

    fn is_serving(&self) -> bool {
        !self.thread.is_finished()
    }
}

/// Stopping is a flag and a knock: the thread is in `accept`, which nothing
/// else interrupts, and wakes to find the flag up. Not joined — it is gone as
/// soon as it looks — and the connections it already made stay up: they are
/// on threads of their own, and end with their channel or their client.
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        ipc::wake(&self.address);
    }
}

/// Accept connections until told to stop, and carry each over a channel of
/// its own to `remote` — opened on a thread of its own too, since an open is
/// a round trip over the link, and the attach probe and the client connect
/// at the same moment on purpose (see `pty::spawn_client`).
fn accept(listener: ipc::Listener, remote: &str, link: &Arc<LinkSlot>, stop: &AtomicBool) {
    loop {
        let conn = listener.accept();
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let conn = match conn {
            Ok(conn) => conn,
            Err(e) => {
                tracing::debug!(error = %e, "endpoint: accept failed");
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
        };
        let remote = remote.to_string();
        let link = Arc::clone(link);
        let spawned = std::thread::Builder::new()
            .name("nvmux-endpoint-conn".into())
            .spawn(move || {
                // A link that is down, or a socket the far side refuses,
                // hangs up on the connection — which is what a forward whose
                // far end has gone does, and what the attach path already
                // reads as a session that is not there.
                let Some(link) = link.current() else {
                    tracing::debug!("endpoint: no link to carry a connection over");
                    return;
                };
                match link.open_socket(&remote, mux::OPEN_TIMEOUT) {
                    Ok(chan) => splice(conn, chan),
                    Err(e) => {
                        tracing::debug!(%remote, error = %e, "endpoint: the far side refused")
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "endpoint: no thread for a connection");
        }
    }
}

/// Carry bytes both ways between a local connection and a channel until
/// either end goes, then close both.
fn splice(conn: ipc::Stream, chan: mux::Channel) {
    let up = match conn.try_clone() {
        Ok(up) => up,
        Err(e) => {
            tracing::debug!(error = %e, "endpoint: could not clone a connection");
            return;
        }
    };
    let up_chan = chan.clone();
    let spawned = std::thread::Builder::new()
        .name("nvmux-endpoint-up".into())
        .spawn(move || {
            let mut conn = up;
            let mut buf = [0u8; 32 * 1024];
            loop {
                match conn.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if up_chan.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
            up_chan.shutdown();
            let _ = conn.shutdown(std::net::Shutdown::Both);
        });
    if spawned.is_err() {
        chan.shutdown();
        return;
    }
    let mut conn = conn;
    let mut buf = [0u8; 32 * 1024];
    loop {
        match chan.read(&mut buf, None) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if conn.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = conn.shutdown(std::net::Shutdown::Both);
    chan.shutdown();
}
