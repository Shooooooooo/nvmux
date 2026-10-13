//! nvmux — a tmux-style session manager for Neovim.
//!
//! A thin multiplexer: the multiplexer does not render Neovim's UI. A client
//! does, as a child on a PTY whose bytes pass through untouched — nvmux's own,
//! `nvmux --client`, which animates the editor ([`client`]), or with `[client]
//! ui = "nvim"` Neovim's, `nvim --server <addr> --remote-ui`. What is left is a
//! picker ([`ui`]) and a PTY proxy ([`pty`]) that watches stdin for a
//! `<prefix>` prefix ([`keys`]), however the terminal spells it ([`keyseq`]) —
//! plus a row along the bottom naming what the next key does while that prefix
//! waits ([`hint`]), a way back into the session you were in when the link to
//! its host drops ([`reconnect`]), and what each session has to report beside
//! its name in the picker — busy, failed, finished, or a notification — from a
//! watcher left in it ([`notes`]). Cards in the corner are the rest ([`card`]),
//! drawn by nvmux's own client: on a change of session which one you landed
//! in, and as it happens what the others have to report ([`tell`]) — the
//! notice once a box of nvmux's own, which is kept ([`announce`]). All of what
//! nvmux draws itself is in the terminal's own colours, or with its text in
//! one colour of the user's ([`theme`]).
//!
//! macOS and Linux only.

// The docs link private items on purpose — they are written for whoever reads
// the source, and resolve under `cargo doc --document-private-items`.
#![allow(rustdoc::private_intra_doc_links)]

pub mod announce;
pub mod boundary;
pub mod card;
pub mod cli;
pub mod client;
pub mod config;
pub mod dirs;
pub mod error;
pub mod fade;
pub mod handoff;
pub mod hint;
pub mod ids;
pub mod keys;
pub mod keyseq;
pub mod launch;
pub mod ledger;
pub mod logging;
pub mod nested;
pub mod notes;
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
pub mod tell;
pub mod term;
pub mod theme;
pub mod transport;
pub mod ui;
pub mod winch;

#[cfg(test)]
pub(crate) mod test_support;

pub use error::{NvmuxError, Result};
