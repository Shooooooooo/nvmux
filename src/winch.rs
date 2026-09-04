//! SIGWINCH forwarding. Milestone 4.
//!
//! # The contract
//!
//! When nvmux's terminal is resized, it resizes the PTY master. That is
//! *sufficient*: the kernel then delivers SIGWINCH to the PTY's foreground
//! process group, the `--remote-ui` child calls `try_resize`, and the server
//! follows. nvmux never forwards the signal to the child itself — it only has to
//! notice its own.
//!
//! # Mechanism: a self-pipe
//!
//! The signal handler does one async-signal-safe `write(2)` of a single byte; a
//! dedicated thread blocks reading the other end and calls `master.resize()`.
//! Closing the write end gives that thread a clean EOF, which is exactly what
//! the detach path needs to shut it down.
//!
//! A `sigwait`-based version also works but is ordering-sensitive: every signal
//! in the waited set must be blocked in *every* thread before any thread waits,
//! and getting that wrong produces zero deliveries and a hang with no
//! diagnostic. The self-pipe has no such hazard.
