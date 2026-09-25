//! Named pipes: a session's local endpoint on Windows (see [`crate::ipc`]).
//!
//! What a unix socket in a 0700 directory is on Unix, a pipe is here by its
//! own access list: one entry, for the user nvmux runs as, and nothing
//! inherited — so no other account on the machine can open it, which matters
//! because whatever opens it reaches a session, and a session is a shell as
//! that user. The first instance is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`,
//! so a pipe of the same name made first by someone else is refused rather than
//! joined, and remote clients are rejected outright.
//!
//! Every handle is overlapped, and every read and write waits on an event of
//! its own. That is what gives a read a timeout — [`crate::rpc`] bounds most of
//! its calls — and what lets another thread end a read in progress by
//! cancelling it, which is how [`crate::rpc::Interrupt`] stops a probe parked
//! on a session that will not answer. A pipe's read and write can also be in
//! flight at once from two threads, which a synchronous handle serialises.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    GetLastError, LocalFree, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
    GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileType, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE,
    FILE_FLAG_OVERLAPPED, FILE_TYPE_PIPE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use super::{check, millis, owned, wide, Event};

/// How long a connect waits for a pipe whose every instance is busy — the
/// moment between one connection being accepted and the next instance being
/// made.
const BUSY_WAIT: Duration = Duration::from_secs(2);

/// Each direction's buffer, a hint to the system.
const BUFFER: u32 = 64 * 1024;

/// A security descriptor granting the current user, and nobody else, full
/// access — with the attributes that carry it to `CreateNamedPipeW`.
struct Private {
    descriptor: PSECURITY_DESCRIPTOR,
    attributes: SECURITY_ATTRIBUTES,
}

// SAFETY: the descriptor is immutable once made, and freed once, on drop.
unsafe impl Send for Private {}
unsafe impl Sync for Private {}

impl Private {
    fn to_current_user() -> io::Result<Self> {
        let sid = current_user_sid()?;
        // A protected DACL (`P`: nothing inherited from a parent) with one
        // entry: generic-all, for this user.
        let sddl = wide(std::ffi::OsStr::new(&format!("D:P(A;;GA;;;{sid})")));
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a NUL-terminated SDDL string and an out-pointer.
        check(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        })?;
        Ok(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
        })
    }
}

impl Drop for Private {
    fn drop(&mut self) {
        // SAFETY: allocated by the conversion above, freed once.
        unsafe { LocalFree(self.descriptor) };
    }
}

/// The current user's SID, as a string.
fn current_user_sid() -> io::Result<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the pseudo-handle for this process, and an out-pointer.
    check(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) })?;
    let token = owned(token)?;
    let mut len = 0u32;
    // SAFETY: the documented size query, which fails and says how much.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut len,
        )
    };
    // u64s, for the TOKEN_USER's pointer alignment.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: a buffer of the size asked for.
    check(unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
        )
    })?;
    // SAFETY: the call filled the buffer with a TOKEN_USER.
    let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: a SID inside `buf`, which outlives the call, and an out-pointer.
    check(unsafe { ConvertSidToStringSidW(sid, &mut text) })?;
    // SAFETY: a NUL-terminated wide string the call allocated.
    let string = unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        let s = OsString::from_wide(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        s
    };
    Ok(string.to_string_lossy().into_owned())
}

/// A pipe name served by this process, accepting connections one at a time.
pub struct PipeListener {
    name: Vec<u16>,
    path: PathBuf,
    private: Private,
    /// The instance the next client connects to, made ahead of the accept.
    next: Mutex<Option<OwnedHandle>>,
}

impl std::fmt::Debug for PipeListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeListener")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

fn instance(name: &[u16], private: &Private, first: bool) -> io::Result<OwnedHandle> {
    let mut open = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        open |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // SAFETY: a NUL-terminated name and attributes that outlive the call.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER,
            BUFFER,
            0,
            &private.attributes,
        )
    };
    owned(handle)
}

impl PipeListener {
    /// Serve `path`, a name under `\\.\pipe\`. Refused if the name is taken —
    /// by this user or any other.
    pub fn bind(path: &Path) -> io::Result<Self> {
        let name = wide(path.as_os_str());
        let private = Private::to_current_user()?;
        let first = instance(&name, &private, true)?;
        Ok(Self {
            name,
            path: path.to_path_buf(),
            private,
            next: Mutex::new(Some(first)),
        })
    }

    /// Wait for the next client, and hand back the connection.
    pub fn accept(&self) -> io::Result<PipeStream> {
        let ready = self.next.lock().unwrap_or_else(|p| p.into_inner()).take();
        let handle = match ready {
            Some(handle) => handle,
            None => instance(&self.name, &self.private, false)?,
        };
        let event = Event::new()?;
        // SAFETY: plain data; the event is set below.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = event.handle();
        // SAFETY: a server handle and an OVERLAPPED that outlives the wait.
        let connected = unsafe { ConnectNamedPipe(handle.as_raw_handle(), &mut ov) };
        if connected == 0 {
            // SAFETY: the thread's last error, read at once.
            match unsafe { GetLastError() } {
                // A client got in between the instance being made and here.
                ERROR_PIPE_CONNECTED => {}
                ERROR_IO_PENDING => {
                    let mut n = 0u32;
                    // SAFETY: waiting for the operation `ov` describes.
                    check(unsafe { GetOverlappedResult(handle.as_raw_handle(), &ov, &mut n, 1) })?;
                }
                code => return Err(io::Error::from_raw_os_error(code as i32)),
            }
        }
        // The next instance now, so a client arriving while this one is being
        // served finds somewhere to connect.
        if let Ok(next) = instance(&self.name, &self.private, false) {
            *self.next.lock().unwrap_or_else(|p| p.into_inner()) = Some(next);
        }
        Ok(PipeStream::new(handle, true))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// One end of a connected pipe. Cheap to clone: every clone is the same
/// connection, as a `UnixStream`'s `try_clone` is.
#[derive(Clone)]
pub struct PipeStream {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for PipeStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeStream")
            .field("server", &self.inner.server)
            .finish_non_exhaustive()
    }
}

struct Inner {
    handle: OwnedHandle,
    /// The serving end, which can put its client off with `DisconnectNamedPipe`.
    server: bool,
    /// Set by [`PipeStream::shutdown`]: every read is its end from then on.
    closed: AtomicBool,
    read_timeout: Mutex<Option<Duration>>,
    write_timeout: Mutex<Option<Duration>>,
}

impl PipeStream {
    fn new(handle: OwnedHandle, server: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                handle,
                server,
                closed: AtomicBool::new(false),
                read_timeout: Mutex::new(None),
                write_timeout: Mutex::new(None),
            }),
        }
    }

    /// Connect to the pipe at `path`. A pipe that is not there is
    /// `ErrorKind::NotFound`, and something there that is not a pipe — an
    /// ordinary file, which `CreateFileW` would open just as readily — is
    /// `ErrorKind::ConnectionRefused`: both are what a caller reads as nothing
    /// listening.
    pub fn connect(path: &Path) -> io::Result<Self> {
        let name = wide(path.as_os_str());
        let deadline = Instant::now() + BUSY_WAIT;
        loop {
            // SAFETY: a NUL-terminated name; the handle is checked below.
            // Identification only: the server may learn who connected, and
            // may not act as them.
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    std::ptr::null_mut(),
                )
            };
            match owned(handle) {
                Ok(handle) => {
                    // SAFETY: a handle this owns.
                    if unsafe { GetFileType(handle.as_raw_handle()) } != FILE_TYPE_PIPE {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionRefused,
                            "not a pipe",
                        ));
                    }
                    return Ok(Self::new(handle, false));
                }
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(e);
                    }
                    // SAFETY: a NUL-terminated name.
                    unsafe {
                        WaitNamedPipeW(name.as_ptr(), millis(left.min(Duration::from_millis(100))))
                    };
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(self.clone())
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *self
            .inner
            .read_timeout
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = timeout;
        Ok(())
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *self
            .inner
            .write_timeout
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = timeout;
        Ok(())
    }

    /// End the connection from this side, now: whatever any clone is in the
    /// middle of reading or writing is cancelled, and every read from here on
    /// is the end. The serving end also puts its client off, which is what
    /// the client then reads as the end; a client's own end goes when its
    /// last clone does.
    pub fn shutdown(&self, _how: std::net::Shutdown) -> io::Result<()> {
        self.inner.closed.store(true, Ordering::SeqCst);
        let handle = self.inner.handle.as_raw_handle();
        // SAFETY: a handle this owns; a null OVERLAPPED cancels every
        // operation on it, from whichever thread.
        unsafe { CancelIoEx(handle, std::ptr::null()) };
        if self.inner.server {
            // SAFETY: as above.
            unsafe { DisconnectNamedPipe(handle) };
        }
        Ok(())
    }

    fn timeout(slot: &Mutex<Option<Duration>>) -> Option<Duration> {
        *slot.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Run one overlapped operation to its end, or to `timeout`, which
    /// cancels it. The OVERLAPPED is on this stack frame, so an operation is
    /// never left in flight past it: a cancelled one is waited out too.
    fn complete(
        &self,
        timeout: Option<Duration>,
        start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> i32,
    ) -> io::Result<usize> {
        let handle = self.inner.handle.as_raw_handle();
        let event = Event::new()?;
        // SAFETY: plain data; the event is set below.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = event.handle();
        if start(handle, &mut ov) == 0 {
            // SAFETY: the thread's last error, read at once.
            let code = unsafe { GetLastError() };
            if code != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(code as i32));
            }
            let wait = timeout.map_or(INFINITE, millis);
            // SAFETY: an event this owns.
            if unsafe { WaitForSingleObject(event.handle(), wait) } == WAIT_TIMEOUT {
                // SAFETY: cancelling the one operation `ov` describes, then
                // waiting for the cancellation to land before `ov` goes.
                unsafe {
                    CancelIoEx(handle, &ov);
                    let mut n = 0u32;
                    GetOverlappedResult(handle, &ov, &mut n, 1);
                }
                return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
            }
        }
        let mut n = 0u32;
        // SAFETY: the operation `ov` describes, which has completed.
        check(unsafe { GetOverlappedResult(handle, &ov, &mut n, 1) })?;
        Ok(n as usize)
    }
}

/// The errors that mean the other end has gone, which a read reports as its
/// end — as a socket's reads do.
fn is_gone(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error().map(|c| c as u32),
        Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA)
    )
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }
}

impl Read for &PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.inner.closed.load(Ordering::SeqCst) || buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len().min(u32::MAX as usize) as u32;
        let timeout = PipeStream::timeout(&self.inner.read_timeout);
        let result = self.complete(timeout, |handle, ov| {
            // SAFETY: a buffer of `len` bytes that outlives the operation,
            // which `complete` waits out.
            unsafe { ReadFile(handle, buf.as_mut_ptr(), len, std::ptr::null_mut(), ov) }
        });
        match result {
            Err(e) if is_gone(&e) => Ok(0),
            Err(e)
                if e.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32)
                    && self.inner.closed.load(Ordering::SeqCst) =>
            {
                Ok(0)
            }
            other => other,
        }
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for &PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let len = buf.len().min(u32::MAX as usize) as u32;
        let timeout = PipeStream::timeout(&self.inner.write_timeout);
        let result = self.complete(timeout, |handle, ov| {
            // SAFETY: a buffer of `len` bytes that outlives the operation.
            unsafe { WriteFile(handle, buf.as_ptr(), len, std::ptr::null_mut(), ov) }
        });
        match result {
            Err(e) if is_gone(&e) => Err(io::Error::new(io::ErrorKind::BrokenPipe, e)),
            other => other,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(tag: &str) -> PathBuf {
        PathBuf::from(format!(
            r"\\.\pipe\nvmux-test-{}-{tag}-{}",
            std::process::id(),
            crate::ids::nonce().expect("nonce")
        ))
    }

    /// Bytes both ways, and each end's going read as the end by the other.
    #[test]
    fn a_pipe_carries_bytes_both_ways() {
        let path = name("both");
        let listener = PipeListener::bind(&path).expect("bind");
        let server = std::thread::spawn(move || {
            let mut conn = listener.accept().expect("accept");
            let mut buf = [0u8; 5];
            conn.read_exact(&mut buf).expect("read");
            conn.write_all(&buf).expect("echo");
            conn.shutdown(std::net::Shutdown::Both).expect("shutdown");
        });
        let mut client = PipeStream::connect(&path).expect("connect");
        client.write_all(b"hello").expect("write");
        let mut buf = [0u8; 5];
        client.read_exact(&mut buf).expect("read");
        assert_eq!(&buf, b"hello");
        server.join().expect("server");
        let mut rest = [0u8; 1];
        assert_eq!(client.read(&mut rest).expect("the end"), 0);
    }

    /// A name that nothing serves is not there, as a socket path is not.
    #[test]
    fn a_pipe_nothing_serves_is_not_found() {
        let err = PipeStream::connect(&name("absent")).expect_err("nothing there");
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
    }

    /// The name is this run's alone: a second listener on it is refused.
    #[test]
    fn a_name_already_served_is_refused() {
        let path = name("twice");
        let _first = PipeListener::bind(&path).expect("bind");
        assert!(
            PipeListener::bind(&path).is_err(),
            "a second first instance"
        );
    }

    /// A read with a budget gives up; one with none is ended from another
    /// thread by a shutdown — the rpc interrupt.
    #[test]
    fn a_read_times_out_and_a_shutdown_ends_one() {
        let path = name("timeout");
        let listener = PipeListener::bind(&path).expect("bind");
        let accepted = std::thread::spawn(move || listener.accept().expect("accept"));
        let client = PipeStream::connect(&path).expect("connect");
        let _server = accepted.join().expect("server");

        client
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("timeout");
        let mut buf = [0u8; 1];
        let err = (&client).read(&mut buf).expect_err("nothing to read");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);

        client.set_read_timeout(None).expect("no timeout");
        let reader = client.try_clone().expect("clone");
        let parked = std::thread::spawn(move || (&reader).read(&mut [0u8; 1]));
        std::thread::sleep(Duration::from_millis(50));
        client.shutdown(std::net::Shutdown::Both).expect("shutdown");
        assert_eq!(parked.join().expect("reader").expect("the end"), 0);
    }
}
