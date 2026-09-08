//! nvmux — a tmux-style session manager for Neovim.
//!
//! A thin multiplexer: it does not render Neovim's UI. `nvim --server <addr>
//! --remote-ui` does that, as a child on a PTY whose bytes pass through
//! untouched. What is left is a picker ([`ui`]) and a PTY proxy ([`pty`]) that
//! watches stdin for a `<prefix>` prefix ([`keys`]), however the terminal spells
//! it ([`keyseq`]).
//!
//! macOS and Linux only.

pub mod cli;
pub mod config;
pub mod error;
pub mod ids;
pub mod keys;
pub mod keyseq;
pub mod logging;
pub mod nvim;
pub mod paths;
pub mod proc;
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
