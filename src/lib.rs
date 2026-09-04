//! nvmux — a tmux-style session manager for Neovim.
//!
//! # Shape of the thing
//!
//! nvmux is a **thin multiplexer**, and that is the load-bearing decision. It
//! does not render Neovim's UI. There is no `nvim_ui_attach` in this crate, no
//! `grid_line` handling, no highlight table, no grid diffing. Neovim already
//! ships a client that does all of that — `nvim --server <addr> --remote-ui` —
//! so nvmux runs it as a child on a PTY and passes bytes through untouched.
//!
//! What is left is two things:
//!
//! 1. a picker ([`ui`]) that lists, creates, kills and renames sessions, and
//! 2. a PTY proxy ([`pty`]) that sits in the byte stream so it can intercept a
//!    `Ctrl-t` prefix key ([`keys`]).
//!
//! ```text
//! LOCAL                                     REMOTE
//! ┌───────────────────────────────┐         ┌────────────────────────────────┐
//! │ nvmux                         │         │ nvim --headless --listen <sock>│
//! │  ├─ picker UI (ratatui)       │         │   (detached, survives SSH drop)│
//! │  ├─ PTY proxy (prefix key)    │         │ nvim --headless --listen <sock>│
//! │  └─ child: nvim --server ...  │         │ nvim --headless --listen <sock>│
//! │            --remote-ui        │         │                                │
//! └───────────────────────────────┘         └────────────────────────────────┘
//!          │                                            ▲
//!          │  one persistent ssh master (ControlMaster/ControlPersist);
//!          │  `ssh -O forward -L <lsock>:<rsock>` adds a unix-socket forward
//!          │  per session on demand, with no reconnection
//!          └──────────────────── SSH ───────────────────┘
//! ```
//!
//! # Platforms
//!
//! macOS and Linux. There is no Windows support, no named-pipe path, and no
//! cross-platform abstraction layer for one.

pub mod cli;
pub mod config;
pub mod error;
pub mod ids;
pub mod keys;
pub mod logging;
pub mod nvim;
pub mod pty;
pub mod rpc;
pub mod session;
pub mod shell;
pub mod ssh;
pub mod term;
pub mod transport;
pub mod ui;
pub mod winch;

pub use error::{NvmuxError, Result};
