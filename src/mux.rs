//! One ssh connection carrying many streams: the near end of the relay.
//!
//! # Why
//!
//! The ssh transport ([`crate::transport::remote`]) reaches a host through one
//! `ControlMaster` and adds a unix-socket forward to it per session with `ssh
//! -O forward`. That needs a client that can multiplex, and not every client
//! can: OpenSSH for Windows has no `ControlMaster` at all — its control socket
//! is a unix socket it cannot make ("getsockname failed: Not a socket") — and
//! a host can refuse the forwards themselves with `AllowStreamLocalForwarding
//! no`. Opening a fresh ssh connection for every shell and every socket would
//! work anywhere, and would cost a handshake apiece and, for a key that wants
//! one, a passphrase or a touch every time the user switched session.
//!
//! So this opens **one** connection, and does the multiplexing itself. What it
//! runs at the far end is `scripts/relay.lua`, under the Neovim every session
//! host has by definition: [`Link::connect`] starts the host's login shell as
//! the ssh transport does, writes it `scripts/boot.sh`, and the shell becomes
//! the relay. From then on the connection's stdin and stdout carry frames, and
//! each frame belongs to a channel: a `sh -s` the scripts run in, or a
//! connection to a session's socket.
//!
//! # The protocol
//!
//! A frame is a kind (one byte), a channel (four bytes, big-endian), a length
//! (four more) and that many bytes. This end opens every channel and picks its
//! number; the relay answers [`OPENED`] or [`REFUSED`]. [`DATA`] goes both ways;
//! [`STDERR`], [`EOF`] and [`EXIT`] only come back, from shells. [`CLOSE`] from
//! either side ends a channel.
//!
//! Each channel's traffic, each way, is bounded by [`WINDOW`]: a side may have
//! that many bytes of a channel sent and not yet taken, and a [`CREDIT`] frame
//! hands more back as they are. That is what keeps one channel from starving
//! the rest: a session repainting into a client nobody is reading stops at its
//! window, on the far side, rather than queueing without bound on this one —
//! and the reader here never blocks on any channel, so every other channel's
//! frames keep arriving behind it.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::error::{NvimError, NvmuxError, Result, SessionError, SshError};
use crate::proc::{Pipes, Ready, Streams};
use crate::shell;

/// The protocol's version, which the relay's first line carries. Bumped when
/// a frame's meaning changes; a relay speaking another one is refused rather
/// than guessed at.
pub const VERSION: u32 = 1;

// Frame kinds. The same numbers are in `scripts/relay.lua`.
const OPEN_SOCKET: u8 = 1;
const OPEN_SHELL: u8 = 2;
const OPENED: u8 = 3;
const REFUSED: u8 = 4;
const DATA: u8 = 5;
const STDERR: u8 = 6;
const EOF: u8 = 7;
const CLOSE: u8 = 8;
const EXIT: u8 = 9;
const CREDIT: u8 = 10;
const KILL: u8 = 11;

/// How many bytes of one channel may be on their way, one way, without having
/// been taken yet. The same number is in `scripts/relay.lua`.
///
/// A quarter of a megabyte: several full repaints of a large terminal, so a
/// channel that is being read never waits for its credit, and small enough
/// that a hundred idle channels could not tie up memory worth mentioning.
pub const WINDOW: u64 = 256 * 1024;

/// The most one [`DATA`] frame carries from this end. A write larger than this
/// goes as several, so a channel with a lot to say cannot hold the connection
/// for long while others wait to be written to it.
const MAX_CHUNK: usize = 32 * 1024;

/// The largest frame accepted from the relay. Its own reads are at most 64 KiB,
/// so this is only ever reached by a stream that is not the relay's at all —
/// which is then treated as the connection having gone.
const MAX_FRAME: usize = 1 << 20;

/// How long a channel waits to be opened. The relay opens one at once — a
/// unix socket connect or a fork — so this is a round trip over the link,
/// with room for a slow one.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a shell that has been told to go gets to report its exit.
const EXIT_WAIT: Duration = Duration::from_secs(2);

/// How much of ssh's stderr is kept: the end of it, which is where the reason
/// a connection ended is.
const STDERR_TAIL: usize = 16 * 1024;

/// One ssh connection with the relay at the far end of it.
///
/// Cheap to clone: every clone is the same connection. It is up until the
/// reader sees ssh's stdout end, and then it is down for good — a link is
/// never revived, only replaced by a new [`Link::connect`], and every channel
/// on it ends with it.
#[derive(Clone)]
pub struct Link {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("host", &self.shared.host)
            .field("up", &self.is_up())
            .finish_non_exhaustive()
    }
}

struct Shared {
    host: String,
    /// Where frames go: ssh's stdin. `None` once the link is down, or closed.
    out: Mutex<Option<ChildStdin>>,
    /// Every open channel, by number, for the reader to deliver to.
    chans: Mutex<HashMap<u32, Arc<Chan>>>,
    next_id: AtomicU32,
    child: Mutex<Child>,
    /// The end of what ssh has said on stderr.
    stderr: Arc<Mutex<Vec<u8>>>,
    /// Why the link went down, once it has: ssh's exit status and last words.
    down: Mutex<Option<(i32, String)>>,
}

/// One channel's state, shared between the reader, which fills it, and the
/// [`Channel`] that owns it.
struct Chan {
    id: u32,
    st: Mutex<ChanState>,
    cv: Condvar,
}

#[derive(Default)]
struct ChanState {
    /// `None` while the relay has not answered the open; then what it said.
    opened: Option<std::result::Result<u32, String>>,
    out: VecDeque<u8>,
    out_eof: bool,
    err: VecDeque<u8>,
    err_eof: bool,
    /// A shell's exit status, `-1` for a signal.
    exit: Option<i32>,
    /// The channel is over: the relay closed it, refused it, or the link went.
    closed: bool,
    /// What may still be sent before a credit comes back.
    window: u64,
    /// What has been taken here and not yet credited back to the relay.
    unacked: u64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic elsewhere with the lock held left state that is still sound for
    // every reader here: bytes queued, flags set.
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Chan {
    fn new(id: u32) -> Self {
        Self {
            id,
            st: Mutex::new(ChanState {
                window: WINDOW,
                ..ChanState::default()
            }),
            cv: Condvar::new(),
        }
    }

    fn state(&self) -> MutexGuard<'_, ChanState> {
        lock(&self.st)
    }

    /// End the channel from this side's point of view: nothing more will
    /// arrive, and nothing more may be sent. Wakes everything waiting on it.
    fn end(&self, why: &str) {
        let mut st = self.state();
        if st.opened.is_none() {
            st.opened = Some(Err(why.to_string()));
        }
        st.closed = true;
        st.out_eof = true;
        st.err_eof = true;
        drop(st);
        self.cv.notify_all();
    }
}

impl Link {
    /// Start `ssh` as `cmd` — a login shell's `sh -s` at the far end, as
    /// [`crate::ssh`] starts one — and turn that shell into the relay.
    ///
    /// Blocks for as long as the connection takes to come up, a passphrase or
    /// a key touch included: ssh asks on the terminal, which is still the
    /// user's at this point, exactly as it does for the ssh transport.
    ///
    /// The failures are told apart the way the far side lets them be: ssh's own
    /// (it exits before the shell says anything, and its stderr says why), the
    /// scripts' (`boot.sh` refused, and printed why), and Neovim's (missing, or
    /// too old to run the relay, which `boot.sh` reports before it tries).
    pub fn connect(mut cmd: Command, host: &str) -> Result<Link> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| spawn_error(e, host))?;
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(NvmuxError::Io(io::Error::other(
                "ssh was started without its pipes",
            )));
        };
        let tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_reader = spawn_stderr_reader(stderr, Arc::clone(&tail))?;

        let secret = crate::ids::nonce()?;
        let boot = format!(
            "set -- {}\n{}",
            shell::quote_all([secret.as_str(), &shell::relay_hash()]),
            shell::boot_script()
        );
        // A failure here is a connection that is already gone; the read below
        // finds its stdout closed and reports it with ssh's words.
        let _ = stdin
            .write_all(boot.as_bytes())
            .and_then(|()| stdin.flush());

        let mut stdout = BufReader::with_capacity(64 * 1024, stdout);
        let failed = |child: &mut Child, said: Vec<Vec<u8>>, banner: Option<String>| {
            let status = settle(child);
            let stderr = finish_stderr(&stderr_reader, &tail);
            boot_failure(host, status, &stderr, &said, banner)
        };

        // What the login shell prints, then the boot marker.
        let boot_mark = format!("NVMUX_BOOT_{secret}");
        let mut said = Vec::new();
        let banner = loop {
            match read_line(&mut stdout) {
                Ok(Some(line)) => match strip(&line, &boot_mark) {
                    Some(rest) => break String::from_utf8_lossy(rest).trim().to_string(),
                    None => said.push(line),
                },
                _ => return Err(failed(&mut child, said, None)),
            }
        };
        if banner.is_empty() {
            return Err(failed(&mut child, said, Some(banner)));
        }

        // Then the relay's first line, from the Neovim that replaced the shell.
        let relay_mark = format!("NVMUX_RELAY_{secret}");
        let version = loop {
            match read_line(&mut stdout) {
                Ok(Some(line)) => match strip(&line, &relay_mark) {
                    Some(rest) => break String::from_utf8_lossy(rest).trim().to_string(),
                    None => {
                        tracing::debug!(
                            line = %String::from_utf8_lossy(&line).trim_end(),
                            "relay: ignoring what came before its first line"
                        );
                    }
                },
                _ => return Err(failed(&mut child, Vec::new(), Some(banner))),
            }
        };
        if version != VERSION.to_string() {
            let _ = child.kill();
            settle(&mut child);
            return Err(SshError::RelayFailed {
                host: host.to_string(),
                why: format!("it speaks protocol {version}, and this nvmux speaks {VERSION}"),
            }
            .into());
        }

        let shared = Arc::new(Shared {
            host: host.to_string(),
            out: Mutex::new(Some(stdin)),
            chans: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            child: Mutex::new(child),
            stderr: tail,
            down: Mutex::new(None),
        });
        let reader = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("nvmux-relay-reader".into())
            .spawn(move || read_frames(reader, stdout))?;
        tracing::info!(%host, %banner, "relay up");
        Ok(Link { shared })
    }

    /// The host this link reaches, as the user named it.
    pub fn host(&self) -> &str {
        &self.shared.host
    }

    /// The local process carrying the link: `ssh`.
    pub fn pid(&self) -> u32 {
        lock(&self.shared.child).id()
    }

    /// Whether the link is still up. A link that is down stays down.
    pub fn is_up(&self) -> bool {
        lock(&self.shared.down).is_none()
    }

    /// Why the link is down, as the error an ssh failure is reported as — or
    /// `None` while it is up.
    pub fn why_down(&self) -> Option<SshError> {
        lock(&self.shared.down)
            .as_ref()
            .map(|(status, stderr)| link_error(&self.shared.host, *status, stderr))
    }

    /// Open a connection to the unix socket at `path` on the far side.
    pub fn open_socket(&self, path: &str, timeout: Duration) -> io::Result<Channel> {
        self.open(OPEN_SOCKET, path.as_bytes(), timeout)
    }

    /// Start a `sh -s` on the far side, with this machine's shells' manners:
    /// the same environment as the relay's own login shell gave it.
    pub fn open_shell(&self, timeout: Duration) -> io::Result<Channel> {
        self.open(OPEN_SHELL, b"", timeout)
    }

    fn open(&self, kind: u8, payload: &[u8], timeout: Duration) -> io::Result<Channel> {
        if !self.is_up() {
            return Err(down_error());
        }
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let chan = Arc::new(Chan::new(id));
        lock(&self.shared.chans).insert(id, Arc::clone(&chan));
        // Checked again with the channel in the map: a link that went down in
        // between ended every channel it knew of, which may not have been
        // this one.
        if !self.is_up() {
            chan.end("the relay's connection is down");
        }
        let channel = Channel(Arc::new(ChannelInner {
            link: self.clone(),
            chan,
            pid: AtomicU32::new(0),
        }));
        self.shared.send(kind, id, payload)?;
        let deadline = Instant::now() + timeout;
        let chan = &channel.0.chan;
        let mut st = chan.state();
        loop {
            match st.opened.clone() {
                Some(Ok(pid)) => {
                    drop(st);
                    channel.0.pid.store(pid, Ordering::Relaxed);
                    return Ok(channel);
                }
                Some(Err(why)) => {
                    return Err(io::Error::new(io::ErrorKind::ConnectionRefused, why));
                }
                None => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                // Dropping the channel closes it, which is also what the relay
                // is told of an open it answers after this has given up.
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the relay did not answer the open",
                ));
            }
            st = chan
                .cv
                .wait_timeout(st, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    /// Hang up: close the relay's stdin, so it lets go of every channel and
    /// exits, and ssh after it. Waits a moment for ssh to go, and makes it.
    pub fn close(&self) {
        drop(lock(&self.shared.out).take());
        let mut child = lock(&self.shared.child);
        let deadline = Instant::now() + EXIT_WAIT;
        while Instant::now() < deadline {
            if !matches!(child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Shared {
    /// Write one frame. One `write_all` per frame under the lock, so frames
    /// from different threads never interleave.
    fn send(&self, kind: u8, id: u32, payload: &[u8]) -> io::Result<()> {
        let mut out = lock(&self.out);
        let Some(pipe) = out.as_mut() else {
            return Err(down_error());
        };
        let mut frame = Vec::with_capacity(9 + payload.len());
        frame.push(kind);
        frame.extend_from_slice(&id.to_be_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        pipe.write_all(&frame).and_then(|()| pipe.flush())
    }

    fn deliver(&self, kind: u8, id: u32, payload: Vec<u8>) {
        let Some(chan) = lock(&self.chans).get(&id).cloned() else {
            // A channel this end has already let go of.
            return;
        };
        let mut st = chan.state();
        match kind {
            OPENED => {
                let pid = std::str::from_utf8(&payload)
                    .ok()
                    .and_then(|p| p.trim().parse().ok())
                    .unwrap_or(0);
                st.opened = Some(Ok(pid));
            }
            REFUSED => {
                st.opened = Some(Err(String::from_utf8_lossy(&payload).into_owned()));
                st.closed = true;
                st.out_eof = true;
                st.err_eof = true;
                lock(&self.chans).remove(&id);
            }
            DATA => st.out.extend(payload),
            STDERR => st.err.extend(payload),
            EOF => match payload.as_slice() {
                b"err" => st.err_eof = true,
                _ => st.out_eof = true,
            },
            EXIT => st.exit = Some(exit_status(&payload)),
            CLOSE => {
                st.closed = true;
                st.out_eof = true;
                st.err_eof = true;
                if st.opened.is_none() {
                    st.opened = Some(Err("the relay closed the channel".into()));
                }
                lock(&self.chans).remove(&id);
            }
            CREDIT => {
                if let Ok(bytes) = <[u8; 4]>::try_from(payload.as_slice()) {
                    st.window += u64::from(u32::from_be_bytes(bytes));
                }
            }
            other => tracing::debug!(kind = other, id, "relay: a frame of no kind this end knows"),
        }
        drop(st);
        chan.cv.notify_all();
    }

    /// The link has gone: say why, and end every channel on it.
    fn went_down(&self) {
        drop(lock(&self.out).take());
        let status = settle(&mut lock(&self.child));
        let stderr = String::from_utf8_lossy(&lock(&self.stderr)).into_owned();
        tracing::info!(host = %self.host, status, stderr = %stderr.trim(), "relay down");
        *lock(&self.down) = Some((status, stderr));
        let chans: Vec<_> = lock(&self.chans).drain().map(|(_, c)| c).collect();
        for chan in chans {
            chan.end("the relay's connection went down");
        }
    }
}

/// The reader: every frame the relay sends, delivered to its channel, until
/// ssh's stdout ends — which is the link going down, however it went.
fn read_frames(shared: Arc<Shared>, mut stdout: BufReader<ChildStdout>) {
    let mut header = [0u8; 9];
    loop {
        if stdout.read_exact(&mut header).is_err() {
            break;
        }
        let kind = header[0];
        let id = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        let len = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;
        if len > MAX_FRAME {
            tracing::warn!(
                len,
                "relay: a frame too large to be the relay's; hanging up"
            );
            break;
        }
        let mut payload = vec![0u8; len];
        if stdout.read_exact(&mut payload).is_err() {
            break;
        }
        shared.deliver(kind, id, payload);
    }
    shared.went_down();
}

/// A channel on a [`Link`]: a connection to a socket on the far side, or a
/// shell there. Cheap to clone, and every clone is the same channel; the last
/// one to go closes it.
#[derive(Clone)]
pub struct Channel(Arc<ChannelInner>);

struct ChannelInner {
    link: Link,
    chan: Arc<Chan>,
    /// A shell's pid on the far side; zero for a socket.
    pid: AtomicU32,
}

impl std::fmt::Debug for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Channel")
            .field("id", &self.0.chan.id)
            .finish_non_exhaustive()
    }
}

impl Drop for ChannelInner {
    fn drop(&mut self) {
        let was_open = !std::mem::replace(&mut self.chan.state().closed, true);
        lock(&self.link.shared.chans).remove(&self.chan.id);
        if was_open {
            let _ = self.link.shared.send(CLOSE, self.chan.id, b"");
        }
        self.chan.cv.notify_all();
    }
}

/// Which of a channel's two incoming streams a read takes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    Out,
    Err,
}

impl Channel {
    /// Read what has arrived, waiting up to `timeout` — `None` for as long as
    /// it takes — for something to. `Ok(0)` is the stream's end; a wait that
    /// runs out is `ErrorKind::TimedOut`.
    pub fn read(&self, buf: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
        let which = Streams::OUT;
        if !self.wait(which, timeout).out {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "nothing arrived in time",
            ));
        }
        Ok(self.take(Stream::Out, buf))
    }

    /// Write all of `data`, a frame at a time, waiting for credit where the
    /// window is spent. A channel that has ended is a broken pipe.
    pub fn write_all(&self, mut data: &[u8]) -> io::Result<()> {
        let chan = &self.0.chan;
        while !data.is_empty() {
            let n = {
                let mut st = chan.state();
                loop {
                    if st.closed {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "the channel has closed",
                        ));
                    }
                    if st.window > 0 {
                        break;
                    }
                    st = chan.cv.wait(st).unwrap_or_else(|p| p.into_inner());
                }
                let n = data.len().min(MAX_CHUNK).min(st.window as usize);
                st.window -= n as u64;
                n
            };
            self.0.link.shared.send(DATA, chan.id, &data[..n])?;
            data = &data[n..];
        }
        Ok(())
    }

    /// Say there is nothing more to write: a socket's write side is shut, a
    /// shell's stdin reaches its end.
    pub fn finish(&self) {
        if !self.0.chan.state().closed {
            let _ = self.0.link.shared.send(EOF, self.0.chan.id, b"");
        }
    }

    /// End the channel now, from here: nothing more is read or written, and
    /// the far side lets go of it. What any clone is waiting on returns.
    pub fn shutdown(&self) {
        let was_open = !std::mem::replace(&mut self.0.chan.state().closed, true);
        {
            let mut st = self.0.chan.state();
            st.out_eof = true;
            st.err_eof = true;
        }
        lock(&self.0.link.shared.chans).remove(&self.0.chan.id);
        if was_open {
            let _ = self.0.link.shared.send(CLOSE, self.0.chan.id, b"");
        }
        self.0.chan.cv.notify_all();
    }

    /// Wait up to `timeout` for one of `which` to have something to take:
    /// bytes, or its end.
    fn wait(&self, which: Streams, timeout: Option<Duration>) -> Ready {
        let chan = &self.0.chan;
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut st = chan.state();
        loop {
            let ready = Ready {
                out: which.out && (!st.out.is_empty() || st.out_eof),
                err: which.err && (!st.err.is_empty() || st.err_eof),
            };
            if ready.out || ready.err {
                return ready;
            }
            match deadline {
                None => st = chan.cv.wait(st).unwrap_or_else(|p| p.into_inner()),
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ready::default();
                    }
                    st = chan
                        .cv
                        .wait_timeout(st, left)
                        .unwrap_or_else(|p| p.into_inner())
                        .0;
                }
            }
        }
    }

    /// Take what has arrived on one stream, and credit it back once enough has
    /// been taken to be worth a frame.
    fn take(&self, which: Stream, buf: &mut [u8]) -> usize {
        let chan = &self.0.chan;
        let (n, credit) = {
            let mut st = chan.state();
            let queue = match which {
                Stream::Out => &mut st.out,
                Stream::Err => &mut st.err,
            };
            let n = queue.len().min(buf.len());
            for (slot, byte) in buf.iter_mut().zip(queue.drain(..n)) {
                *slot = byte;
            }
            st.unacked += n as u64;
            let credit = if st.unacked >= WINDOW / 2 && !st.closed {
                std::mem::take(&mut st.unacked)
            } else {
                0
            };
            (n, credit)
        };
        if credit > 0 {
            let _ = self
                .0
                .link
                .shared
                .send(CREDIT, chan.id, &(credit as u32).to_be_bytes());
        }
        n
    }

    /// A shell's exit status, once it has one. A shell whose link went down
    /// without saying is as gone as one that did, and has no status to give.
    fn exit(&self) -> Option<i32> {
        let st = self.0.chan.state();
        st.exit
            .or((st.closed && !self.0.link.is_up()).then_some(-1))
    }

    /// Send a shell a signal, by name: `sigkill`, `sighup`.
    fn signal(&self, name: &str) {
        let _ = self
            .0
            .link
            .shared
            .send(KILL, self.0.chan.id, name.as_bytes());
    }

    /// The link this channel is on.
    pub fn link(&self) -> &Link {
        &self.0.link
    }
}

/// A shell on the far side, as the [`Pipes`] a [`crate::proc::Shell`] runs its
/// scripts through: the same framing, handshake and markers as a shell here,
/// over a channel instead of a child's pipes.
pub struct ShellPipes {
    chan: Channel,
    /// stdin has been closed, once.
    finished: bool,
    /// stderr has been read to its end.
    err_done: bool,
}

impl ShellPipes {
    pub fn new(chan: Channel) -> Self {
        Self {
            chan,
            finished: false,
            err_done: false,
        }
    }
}

impl Pipes for ShellPipes {
    fn write_in(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.finished {
            return Err(io::Error::other("stdin already closed"));
        }
        self.chan.write_all(bytes)
    }

    fn close_in(&mut self) {
        if !std::mem::replace(&mut self.finished, true) {
            self.chan.finish();
        }
    }

    fn wait(&mut self, which: Streams, timeout: Option<Duration>) -> io::Result<Ready> {
        let which = Streams {
            out: which.out,
            err: which.err && !self.err_done,
        };
        if !which.out && !which.err {
            return Ok(Ready::default());
        }
        Ok(self.chan.wait(which, timeout))
    }

    fn read_out(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Ok(self.chan.take(Stream::Out, buf))
    }

    fn read_err(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.chan.take(Stream::Err, buf);
        if n == 0 {
            self.err_done = true;
        }
        Ok(n)
    }

    fn err_open(&self) -> bool {
        !self.err_done
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(self.chan.exit())
    }

    fn kill(&mut self) {
        self.chan.signal("sigkill");
    }

    fn wait_exit(&mut self) -> i32 {
        let deadline = Instant::now() + EXIT_WAIT;
        loop {
            if let Some(status) = self.chan.exit() {
                return status;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return -1;
            }
            let chan = &self.chan.0.chan;
            let st = chan.state();
            if st.closed && st.exit.is_none() {
                // Closed without saying how it ended — refused, or let go of
                // by the relay: there is no status coming.
                return -1;
            }
            drop(
                chan.cv
                    .wait_timeout(st, left)
                    .unwrap_or_else(|p| p.into_inner()),
            );
        }
    }

    fn pid(&self) -> u32 {
        self.chan.0.pid.load(Ordering::Relaxed)
    }
}

/// `"<code> <signal>"`, as the relay reports an exit: the code, or `-1` for a
/// shell a signal ended — as a child here reports one.
fn exit_status(payload: &[u8]) -> i32 {
    let text = String::from_utf8_lossy(payload);
    let mut words = text.split_whitespace();
    let code = words.next().and_then(|w| w.parse::<i32>().ok());
    let signal = words
        .next()
        .and_then(|w| w.parse::<i32>().ok())
        .unwrap_or(0);
    match code {
        Some(code) if signal == 0 => code,
        _ => -1,
    }
}

/// A line of stdout, newline and all, or `None` at its end.
fn read_line(stdout: &mut BufReader<ChildStdout>) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    // Bounded, so a login shell that prints something enormous with no newline
    // cannot make this grow for ever; a marker line is short.
    let n = stdout
        .by_ref()
        .take(64 * 1024)
        .read_until(b'\n', &mut line)?;
    Ok((n > 0).then_some(line))
}

/// What follows `mark` on `line`, if the line is that marker's.
fn strip<'a>(line: &'a [u8], mark: &str) -> Option<&'a [u8]> {
    line.strip_prefix(mark.as_bytes())
}

fn spawn_stderr_reader(
    mut stderr: std::process::ChildStderr,
    tail: Arc<Mutex<Vec<u8>>>,
) -> io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("nvmux-relay-stderr".into())
        .spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        let mut tail = lock(&tail);
                        tail.extend_from_slice(&buf[..n]);
                        if tail.len() > STDERR_TAIL {
                            let excess = tail.len() - STDERR_TAIL;
                            tail.drain(..excess);
                        }
                        // The one place what ssh says between connecting and
                        // hanging up goes: a warning, a banner, a host key
                        // notice. Logged, as the ssh transport's stderr is.
                        tracing::debug!(
                            said = %String::from_utf8_lossy(&buf[..n]).trim_end(),
                            "relay: ssh said"
                        );
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => return,
                }
            }
        })
}

/// Wait a moment for ssh to have exited, since its exit status is half of why
/// a connection ended, and make it go if it has not. `-1` for a signal.
fn settle(child: &mut Child) -> i32 {
    let deadline = Instant::now() + EXIT_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code().unwrap_or(-1),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                return child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
            }
        }
    }
}

/// Everything ssh said on stderr, once it has stopped saying it.
fn finish_stderr(reader: &std::thread::JoinHandle<()>, tail: &Mutex<Vec<u8>>) -> String {
    let deadline = Instant::now() + Duration::from_millis(200);
    while !reader.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    String::from_utf8_lossy(&lock(tail)).into_owned()
}

/// Why the relay did not come up, from what the far side got as far as.
///
/// `banner` is `None` when not even `boot.sh`'s marker arrived: ssh failed, or
/// the script refused — which it says on stdout, as every script does. With
/// the marker, an empty banner is no `nvim` there at all, and a banner is a
/// Neovim that could not run the relay: too old for `-l`, most likely, which
/// its version says.
fn boot_failure(
    host: &str,
    status: i32,
    stderr: &str,
    said: &[Vec<u8>],
    banner: Option<String>,
) -> NvmuxError {
    let Some(banner) = banner else {
        let refusal = said.iter().find_map(|line| {
            let line = String::from_utf8_lossy(line);
            line.trim_end()
                .strip_prefix("ERROR ")
                .map(|why| why.to_string())
        });
        return match refusal {
            Some(why) => SessionError::ScriptFailed(why).into(),
            None => crate::ssh::classify(host, status, stderr).into(),
        };
    };
    if banner.is_empty() {
        return NvimError::NotFound {
            where_: host.to_string(),
            min: crate::nvim::MIN_VERSION,
        }
        .into();
    }
    if let Err(e) = crate::nvim::check_banner(host, &banner) {
        return e.into();
    }
    SshError::RelayFailed {
        host: host.to_string(),
        why: match stderr.trim() {
            "" => format!("Neovim exited with status {status} and said nothing"),
            said => said.lines().last().unwrap_or(said).to_string(),
        },
    }
    .into()
}

/// A link that went down, as the error ssh's own exit would be reported as.
/// A connection ssh had up and lost exits 255 with nothing classifiable more
/// often than not, which is still a connection that died.
fn link_error(host: &str, status: i32, stderr: &str) -> SshError {
    match crate::ssh::classify(host, status, stderr) {
        SshError::Failed { .. } => SshError::MasterDied(host.to_string()),
        other => other,
    }
}

fn down_error() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "the relay's connection is down")
}

fn spawn_error(e: io::Error, host: &str) -> NvmuxError {
    if e.kind() == io::ErrorKind::NotFound {
        SshError::NotFound.into()
    } else {
        SshError::Failed {
            code: -1,
            stderr: format!("running ssh for {host}: {e}"),
        }
        .into()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A relay reached the way the ssh transport's tests reach a "remote" host:
    /// the command ssh would hand the far side's login shell, run by a shell
    /// here. Needs `nvim` on `$PATH`, like any host.
    fn local_link() -> Option<Link> {
        if !crate::test_support::have_nvim() {
            return None;
        }
        let remote = crate::ssh::remote_shell_command();
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(remote.replace("exec ", ""));
        Some(Link::connect(cmd, "localhost-relay").expect("the relay comes up"))
    }

    #[test]
    fn exit_statuses_read_as_a_child_would_report_them() {
        assert_eq!(exit_status(b"0 0"), 0);
        assert_eq!(exit_status(b"3 0"), 3);
        assert_eq!(exit_status(b"-1 9"), -1);
        assert_eq!(exit_status(b"0 15"), -1, "a signal has no code");
        assert_eq!(exit_status(b"garbage"), -1);
    }

    /// The same scripts, the same framing, the same answers — over a channel.
    #[test]
    fn a_shell_on_the_relay_runs_scripts_like_a_shell_here() {
        let Some(link) = local_link() else { return };
        let chan = link.open_shell(OPEN_TIMEOUT).expect("a shell");
        assert_ne!(ShellPipes::new(chan.clone()).pid(), 0, "a shell has a pid");
        let mut shell =
            crate::proc::Shell::over(Box::new(ShellPipes::new(chan)), "relay").expect("start");
        let out = shell
            .run(
                r#"printf '[%s]' "$1" "$2"; printf oops >&2; exit 3"#,
                &["a b", "it's"],
            )
            .expect("the shell is alive");
        assert_eq!(out.stdout, "[a b][it's]");
        assert_eq!(out.stderr, "oops");
        assert_eq!(out.status, 3);
        assert_eq!(
            shell.run("printf again", &[]).expect("alive").stdout,
            "again"
        );
        assert!(shell.is_alive());
        link.close();
    }

    /// A script that says more than a window's worth, on both streams at once,
    /// finishes: credit flows back as the output is taken.
    #[test]
    fn a_chatty_script_outlasts_its_window() {
        let Some(link) = local_link() else { return };
        let chan = link.open_shell(OPEN_TIMEOUT).expect("a shell");
        let mut shell =
            crate::proc::Shell::over(Box::new(ShellPipes::new(chan)), "relay").expect("start");
        let script = "i=0; while [ $i -lt 4000 ]; do printf '%0500d\\n' 0; printf 'e%0100d\\n' 0 >&2; i=$((i+1)); done";
        let out = shell.run(script, &[]).expect("alive");
        assert!(out.ok());
        assert_eq!(out.stdout.lines().count(), 4000);
        assert_eq!(out.stderr.lines().count(), 4000);
        assert!(
            out.stdout.len() as u64 > 4 * WINDOW,
            "the window was not exercised"
        );
        link.close();
    }

    /// A socket channel is a byte stream to a unix socket over there — here, a
    /// real Neovim answering a real request.
    #[test]
    fn a_socket_channel_reaches_a_neovim() {
        let Some(link) = local_link() else { return };
        let sock = crate::test_support::scratch_sock("mux-nvim");
        let mut nvim = std::process::Command::new("nvim")
            .args(["--clean", "--headless", "--listen"])
            .arg(&sock)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("nvim");
        assert!(crate::test_support::wait_until(
            Duration::from_secs(5),
            || sock.exists()
        ));

        let chan = link
            .open_socket(&sock.to_string_lossy(), OPEN_TIMEOUT)
            .expect("the socket opens");
        // [0, 1, "nvim_eval", ["6*7"]]
        let mut req = vec![0x94, 0x00, 0x01, 0xa9];
        req.extend_from_slice(b"nvim_eval");
        req.extend_from_slice(&[0x91, 0xa3]);
        req.extend_from_slice(b"6*7");
        chan.write_all(&req).expect("write");
        let mut got = Vec::new();
        let mut buf = [0u8; 64];
        while got.len() < 5 {
            let n = chan
                .read(&mut buf, Some(Duration::from_secs(5)))
                .expect("read");
            assert_ne!(n, 0, "the socket closed");
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, [0x94, 0x01, 0x01, 0xc0, 42]);

        let _ = nvim.kill();
        let _ = nvim.wait();
        // The session going is the channel ending.
        let mut buf = [0u8; 16];
        assert_eq!(
            chan.read(&mut buf, Some(Duration::from_secs(5)))
                .expect("read"),
            0
        );
        let _ = std::fs::remove_file(&sock);
        link.close();
    }

    /// A socket nothing listens on is refused, in the relay's words, and the
    /// link is none the worse for it.
    #[test]
    fn a_socket_that_is_not_there_is_refused() {
        let Some(link) = local_link() else { return };
        let err = link
            .open_socket("/nonexistent/nvmux.sock", OPEN_TIMEOUT)
            .expect_err("nothing is there");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused, "{err}");
        assert!(link.is_up());
        assert!(link.open_shell(OPEN_TIMEOUT).is_ok());
        link.close();
    }

    /// Hanging up ends every channel, and the link says it is down.
    #[test]
    fn closing_the_link_ends_its_channels() {
        let Some(link) = local_link() else { return };
        let chan = link.open_shell(OPEN_TIMEOUT).expect("a shell");
        link.close();
        assert!(crate::test_support::wait_until(
            Duration::from_secs(5),
            || !link.is_up()
        ));
        let mut buf = [0u8; 16];
        assert_eq!(
            chan.read(&mut buf, Some(Duration::from_secs(1)))
                .expect("read"),
            0
        );
        assert!(chan.write_all(b"x").is_err());
        assert!(link.open_shell(OPEN_TIMEOUT).is_err());
    }

    /// No shell to become the relay: what ssh would say, classified as ssh.
    #[test]
    fn a_connection_that_never_starts_is_ssh_failing() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "echo 'Permission denied (publickey).' >&2; exit 255"]);
        let err = Link::connect(cmd, "h").expect_err("no relay");
        assert!(
            matches!(err, NvmuxError::Ssh(SshError::AuthFailed(_))),
            "{err:?}"
        );
    }

    /// A host with no Neovim says so before the relay is tried.
    #[test]
    fn a_host_without_neovim_is_reported_as_one() {
        // A `$PATH` with the tools the boot script uses and no `nvim` — unless
        // this machine keeps one there, which leaves nothing to test.
        const PATH: &str = "/usr/bin:/bin";
        let has_one = Command::new("/bin/sh")
            .args(["-c", "command -v nvim"])
            .env("PATH", PATH)
            .output()
            .is_ok_and(|o| o.status.success());
        if has_one {
            eprintln!("skipping: nvim is in {PATH}");
            return;
        }
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exec sh -s"]).env("PATH", PATH);
        let err = Link::connect(cmd, "h").expect_err("no nvim");
        assert!(
            matches!(err, NvmuxError::Nvim(NvimError::NotFound { .. })),
            "{err:?}"
        );
    }
}
