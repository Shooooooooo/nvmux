//! The Windows half of what nvmux needs from its platform.
//!
//! Three things, each a stand-in for a Unix one the rest of the crate was
//! written against:
//!
//! * [`conpty`] — a pseudoconsole for the `--remote-ui` client, where Unix has
//!   a pty. Its own rather than `portable-pty`'s, for the flags: that one asks
//!   the pseudoconsole to inherit the cursor (which makes it query the terminal
//!   and wait for the answer before it draws) and to put the terminal in
//!   win32-input-mode (which changes how every key reaches nvmux, and so how
//!   the prefix is spelled). A relay wants neither.
//! * [`console`] — the terminal nvmux itself runs in: raw mode, the input the
//!   user types as bytes, and a resize noticed without `SIGWINCH`.
//! * [`pipe`] — named pipes, which are what a session's local endpoint is here
//!   (see [`crate::ipc`]): private to the user by their access list, as a unix
//!   socket is by the directory it is in.

pub mod conpty;
pub mod console;
pub mod pipe;
pub(crate) mod relay;

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent};

/// `s` as the NUL-terminated UTF-16 the wide Win32 calls take.
pub(crate) fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Take ownership of a handle a Win32 call returned, or the error it set.
/// Both spellings of failure count: some calls return null, others
/// `INVALID_HANDLE_VALUE`.
pub(crate) fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a valid handle this process was just given and nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// A Win32 `BOOL` result, as an `io::Result`.
pub(crate) fn check(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// A manual-reset event: what a thread here sets to wake one waiting in
/// `WaitForMultipleObjects` — the Windows side of a self-pipe.
#[derive(Debug)]
pub(crate) struct Event(OwnedHandle);

impl Event {
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: no attributes, no name; the handle is checked below.
        let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        owned(handle).map(Self)
    }

    pub(crate) fn set(&self) {
        // SAFETY: a valid event handle this owns.
        unsafe { SetEvent(self.handle()) };
    }

    pub(crate) fn reset(&self) {
        // SAFETY: as above.
        unsafe { ResetEvent(self.handle()) };
    }

    pub(crate) fn handle(&self) -> HANDLE {
        self.0.as_raw_handle()
    }
}

/// What `WaitForMultipleObjects` said: which handle, or none in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Woke {
    Handle(usize),
    TimedOut,
}

/// Wait up to `timeout_ms` for any one of `handles` to be signalled.
pub(crate) fn wait_any(handles: &[HANDLE], timeout_ms: u32) -> io::Result<Woke> {
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::WaitForMultipleObjects;
    // SAFETY: a slice of valid handles, which the caller holds for the call.
    let rc =
        unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, timeout_ms) };
    if rc == WAIT_TIMEOUT {
        return Ok(Woke::TimedOut);
    }
    let index = rc.wrapping_sub(WAIT_OBJECT_0) as usize;
    if index < handles.len() {
        return Ok(Woke::Handle(index));
    }
    Err(io::Error::last_os_error())
}

/// A timeout as the milliseconds a Win32 wait takes, rounded up so a wait for
/// a deadline never wakes a fraction of a millisecond short of it, and capped
/// below `INFINITE`.
pub(crate) fn millis(timeout: std::time::Duration) -> u32 {
    timeout
        .as_micros()
        .div_ceil(1000)
        .min(u128::from(u32::MAX - 1)) as u32
}
