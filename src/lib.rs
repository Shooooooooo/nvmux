//! nvmux — a tmux-style session manager for Neovim.
//!
//! A thin multiplexer: it does not render Neovim's UI. `nvim --server <addr>
//! --remote-ui` does that, as a child on a PTY whose bytes pass through
//! untouched. What is left is a picker ([`ui`]) and a PTY proxy ([`pty`]) that
//! watches stdin for a `<prefix>` prefix ([`keys`]), however the terminal spells
//! it ([`keyseq`]) — plus a row along the bottom naming what the next key does
//! while that prefix waits ([`hint`]), on a change of session a brief notice
//! saying which one you landed in ([`announce`]), and a way back into the
//! session you were in when the link to its host drops ([`reconnect`]).
//!
//! macOS and Linux only.

// The docs link private items on purpose — they are written for whoever reads
// the source, and resolve under `cargo doc --document-private-items`.
#![allow(rustdoc::private_intra_doc_links)]

pub mod announce;
pub mod boundary;
pub mod cli;
pub mod config;
pub mod dirs;
pub mod error;
pub mod fade;
pub mod hint;
pub mod ids;
pub mod ipc;
pub mod keys;
pub mod keyseq;
pub mod launch;
pub mod ledger;
pub mod logging;
pub mod mux;
pub mod nested;
pub mod nvim;
pub mod palette;
pub mod paths;
pub mod pool;
pub mod proc;
pub mod pty;
pub mod reconnect;
pub mod rpc;
pub mod session;
pub mod shadow;
pub mod shell;
pub mod ssh;
pub mod state;
pub mod term;
pub mod transport;
pub mod ui;
pub mod winch;

#[cfg(test)]
pub(crate) mod test_support;

pub use error::{NvmuxError, Result};
