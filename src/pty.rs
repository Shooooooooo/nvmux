//! The PTY proxy. Milestone 4.
//!
//! # The contract
//!
//! nvmux runs `nvim --server <local_sock> --remote-ui` as a child on a PTY and
//! sits in the byte stream between the user's terminal and that child:
//!
//! ```text
//! stdin  -> [ prefix state machine ] -> pty master
//! stdout <-      (untouched)        <- pty master
//! ```
//!
//! **The child-to-terminal direction is never parsed, buffered by line, or
//! rewritten.** That is the entire reason this design works: bracketed paste,
//! the kitty keyboard protocol, truecolor, undercurl, terminal title sequences,
//! OSC 52 clipboard and DA1/XTGETTCAP query/response round-trips all function
//! because the child negotiates directly with the real terminal. Any
//! "improvement" that inspects this direction breaks a subset of them.
//!
//! nvmux does **not** implement a Neovim UI. There is no `nvim_ui_attach`, no
//! `grid_line` handling, no highlight table and no grid diffing anywhere in this
//! crate — that is thousands of lines of the most bug-prone code in this space,
//! and `nvim --remote-ui` already is that client.
//!
//! # Things milestone 4 must get right
//!
//! * **Read the real terminal size before spawning.** `PtySize::default()` is
//!   24x80. Note `crossterm::terminal::size()` returns `(columns, rows)` while
//!   `PtySize` is `{ rows, cols }` — passing them positionally transposes the
//!   screen. `window_size()` also gives pixel dimensions, without which sixel
//!   and kitty image protocols break inside the session.
//! * **Drop the slave after `spawn_command`**, or the master reader never sees
//!   EOF and the relay thread hangs forever after the child exits.
//! * **Take the writer exactly once.** `MasterPty::take_writer()` errors on a
//!   second call.
//! * **Never run two readers on the master.** `try_clone_reader()` dups the same
//!   open file description, so two readers race and split the stream — which in
//!   a proxy means randomly deleting chunks of the user's screen. Read once and
//!   tee in userspace if logging is ever wanted.
//! * **RPC-ping the session before spawning the child.** Against a dead socket
//!   the client prints `Remote ui failed to start: connection refused` — but
//!   through an SSH forward that message is empty, after ~165 bytes of escape
//!   sequences have already hit the terminal.
//! * **The child's exit code carries no information.** Server killed while
//!   attached gives 0; nvmux terminating the child to detach gives 1; a failed
//!   attach gives 1. Drive teardown off the master read result and nvmux's own
//!   detach state, then probe the socket afterwards to tell "detached" from
//!   "session ended".
//! * **Teardown differs by platform.** When the slave closes, the master read
//!   fails with `EIO` on Linux and returns `Ok(0)` on macOS. Both mean detached.
//! * **Let the child restore the terminal.** It emits its own
//!   `...\x1b[?1049l\x1b[23;0;0t\x1b[?25h` on exit. Pass those bytes through and
//!   only *then* leave raw mode; emitting a competing reset corrupts the display.
//! * `portable-pty` vendors its own `nix`, so never pass a `nix` type across
//!   that boundary — `MasterPty::get_termios()` returns *its* `Termios`, not ours.
//!
//! # `:q` ends the session, and that is intended
//!
//! In a `--remote-ui` session `:q` in the last window terminates the *server*,
//! not just the local view — the editor is the session. That is the documented
//! way to finish with a session and keep your work: save as usual, then quit as
//! usual. `Ctrl-t d` is the other exit, and leaves the session running.
//!
//! So this is a thing to explain rather than to guard. A `cnoreabbrev` guard
//! would also be a poor one: it covers bare `:q`, turns `:q!` into a silent
//! no-op (`bang (!) not supported yet`), and misses `:qa`, `ZZ`, `ZQ`, `:x`,
//! `:wq` and `<C-w>q` entirely.

/// Placeholder so the module has a shape. Milestone 4 fills this in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// `Ctrl-t t` — the user wants the picker; the child is still running.
    ToPicker,
    /// `Ctrl-t d` — detach and exit, leaving the server running.
    Detached,
    /// `Ctrl-t c` — create a new session and attach to it.
    CreateNew,
    /// The child exited on its own.
    ChildExited,
}
