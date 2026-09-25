//! The relay's waits, on Windows: the Unix `poll`s of `crate::sys::unix`,
//! as waits on handles.
//!
//! What a Unix relay polls — stdin, the pty master, `SIGWINCH`'s self-pipe —
//! is, here, the console's input handle (signalled while there are records,
//! a resize among them) and the event the pseudoconsole's reader sets while
//! there is output (see [`super::conpty::Output`]). One wait over both is the
//! same single wait the Unix relay makes, for the reasons `pty::relay` gives.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::conpty::{Output, Pty};
use super::console::ConsoleInput;
use super::{wait_any, Event, Woke};
use crate::error::Result;
use crate::sys::{ChildState, Order, ParkWake, Peek, PtySize, Wake};

impl Pty {
    /// There is no stopping a console process the way `SIGTSTP` stops one on
    /// Unix, so a client is running or it is gone.
    pub(crate) fn peek(&self) -> Peek {
        if self.try_wait() {
            Peek::Gone
        } else {
            Peek::Running
        }
    }

    /// The relay's idle look at its client: see the Unix one, which also
    /// revives a client that stopped itself — which this one cannot do.
    pub(crate) fn check(&mut self) -> ChildState {
        if self.try_wait() {
            ChildState::Gone
        } else {
            ChildState::Running
        }
    }

    pub(crate) fn wait_output(&self, timeout: Duration) -> io::Result<bool> {
        Ok(self.output().wait(timeout))
    }

    /// `Ok(0)` is the end — the pseudoconsole closed, which it is once the
    /// client has exited — and nothing to read yet is `WouldBlock`.
    pub(crate) fn read_output(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.output().read(buf)
    }

    pub(crate) fn hung_up(&self) -> bool {
        self.try_wait()
    }

    pub(crate) fn discard_pending(&self, mut saw: impl FnMut(&[u8])) {
        let output = self.output();
        let mut buf = [0u8; 8192];
        while let Ok(n) = output.read(&mut buf) {
            if n == 0 {
                return;
            }
            saw(&buf[..n]);
        }
    }
}

/// The relay's one wait: the console's input and the client's output.
pub(crate) struct Waiter {
    input: ConsoleInput,
    output: Arc<Output>,
    /// The size last seen, so a resize the console made no record of is still
    /// noticed on the next wake.
    size: PtySize,
}

impl Waiter {
    pub(crate) fn new(pty: &Pty) -> Result<Self> {
        Ok(Self {
            input: ConsoleInput::new(),
            output: pty.output(),
            size: crate::term::terminal_size(),
        })
    }

    /// Wait up to `timeout_ms`. Never interrupted, so never `None` — the
    /// `Option` is the Unix one's.
    pub(crate) fn wait(&mut self, timeout_ms: u64) -> io::Result<Option<Wake>> {
        // Something already in hand is not waited for: bytes a previous drain
        // took and the relay has not read, or output the reader has queued.
        let busy = self.input.has_bytes() || self.output.ready();
        let wait = if busy {
            0
        } else {
            timeout_ms.min(u64::from(u32::MAX - 1)) as u32
        };
        let woke = wait_any(&[self.input.handle(), self.output.event()], wait)?;
        if self.input.waiting()? > 0 {
            self.input.drain()?;
        }
        // However the console said so — a record, or not at all — a new size
        // is a resize.
        let now = crate::term::terminal_size();
        let resized = self.input.take_resized() || now != self.size;
        self.size = now;
        let stdin = self.input.has_bytes();
        let child = self.output.ready();
        Ok(Some(Wake {
            stdin,
            child,
            resized,
            idle: woke == Woke::TimedOut && !stdin && !child && !resized,
        }))
    }

    pub(crate) fn read_child(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.output.read(buf)
    }

    pub(crate) fn read_stdin(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Ok(self.input.read(buf))
    }
}

/// A parked client's stop channel: an event, and what setting it meant.
struct Stop {
    event: Event,
    retire: AtomicBool,
}

/// The owner's end. Dropping it is the client wanted back, unless it was told
/// to retire first — the same meanings the Unix channel's byte and end have.
pub(crate) struct StopTx(Arc<Stop>);

/// The parked thread's end.
pub(crate) struct StopRx(Arc<Stop>);

pub(crate) fn stop_pair() -> io::Result<(StopTx, StopRx)> {
    let stop = Arc::new(Stop {
        event: Event::new()?,
        retire: AtomicBool::new(false),
    });
    Ok((StopTx(Arc::clone(&stop)), StopRx(stop)))
}

impl StopTx {
    pub(crate) fn retire(&mut self) {
        self.0.retire.store(true, Ordering::SeqCst);
        self.0.event.set();
    }
}

impl Drop for StopTx {
    fn drop(&mut self) {
        self.0.event.set();
    }
}

impl StopRx {
    pub(crate) fn order(&self) -> Order {
        if self.0.retire.load(Ordering::SeqCst) {
            Order::Retire
        } else {
            Order::Back
        }
    }
}

/// A parked client's one wait: its owner, and its output.
pub(crate) fn park_wait(stop: &StopRx, pty: &Pty, timeout_ms: u64) -> io::Result<ParkWake> {
    let output = pty.output();
    let wait = if output.ready() {
        0
    } else {
        timeout_ms.min(u64::from(u32::MAX - 1)) as u32
    };
    let woke = wait_any(&[stop.0.event.handle(), output.event()], wait)?;
    Ok(ParkWake {
        stop: woke == Woke::Handle(0),
        output: output.ready(),
        idle: woke == Woke::TimedOut,
    })
}

/// The console's input, for a question asked of the terminal with a deadline
/// on the answer (see [`crate::palette`]). Only ever read from raw mode.
pub(crate) struct TermInput {
    input: ConsoleInput,
}

impl TermInput {
    pub(crate) fn new() -> Self {
        Self {
            input: ConsoleInput::new(),
        }
    }

    /// Whether anything is waiting already — a key typed before the question.
    pub(crate) fn readable_now(&mut self) -> bool {
        self.input.has_bytes() || self.input.waiting().is_ok_and(|n| n > 0)
    }

    /// Read what arrives within `timeout`: `Ok(None)` for nothing in time.
    pub(crate) fn read_within(
        &mut self,
        timeout: Duration,
        buf: &mut [u8],
    ) -> io::Result<Option<usize>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.input.has_bytes() {
                return Ok(Some(self.input.read(buf)));
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if wait_any(&[self.input.handle()], super::millis(left))? == Woke::TimedOut {
                return Ok(None);
            }
            // Records that were not keystrokes — a focus change, a resize —
            // are taken and say nothing; the wait goes on for the rest.
            self.input.drain()?;
        }
    }
}
