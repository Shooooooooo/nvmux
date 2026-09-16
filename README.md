# nvmux

A tmux-style session manager for Neovim. Create named Neovim sessions, attach to
them, detach, and come back later — with the editor running on a remote host
while every keystroke and every pixel of rendering happens on your own terminal.

```
                                                                       
                              1  api-server                            
                            ▸ 2  dotfiles                              
                              3  scratch                               
                              4  notes                                 
                                                                       
 ↑↓ move  ⏎ attach  c new  r rename  x kill  ␣ order  / filter  q quit 
```

nvmux is a **thin multiplexer**: it does not render Neovim's UI. Neovim already
ships a client that does, so nvmux runs it and passes the bytes through
untouched — which is why bracketed paste, the kitty keyboard protocol,
truecolor, OSC 52 clipboard and DA1/XTGETTCAP round-trips all just work.

```
LOCAL                                    REMOTE
nvmux                                    nvim --headless --listen <sock>
 ├─ picker UI (ratatui)                  nvim --headless --listen <sock>
 ├─ PTY proxy (watches for <prefix>)     nvim --headless --listen <sock>
 └─ child: nvim --server … --remote-ui     (detached, survive an SSH drop)
      │                                             ▲
      └──── one persistent ssh master ──────────────┘
            (ControlMaster/ControlPersist), plus one
            `ssh -O forward` unix-socket forward per session
```

## Requirements

| Where | Needs |
|---|---|
| Local | `nvim` >= 0.11, and `ssh` for `nvmux <host>` |
| Remote | `nvim` >= 0.11 |

0.11 is where `:detach` and `:connect` landed; it is checked at startup on both
ends and reported plainly. `ssh` is only needed, and only checked, when a host
is given. macOS and Linux only.

## Install

```sh
cargo build --release
install -m 755 target/release/nvmux ~/.local/bin/
```

## Use

Two invocations, and no subcommands:

```sh
nvmux            # sessions on this machine
nvmux myhost     # sessions on myhost
```

The host string is handed to `ssh` verbatim, so a hostname, `user@host`, or any
`~/.ssh/config` alias works — including `ProxyJump`, agent forwarding and
hardware keys, because nvmux drives your own ssh client rather than
reimplementing it.

### While attached

`<prefix>` is `Ctrl-Space` unless you change it in the [config](#configuration).

| Keys | Action |
|---|---|
| `<prefix>` `d` | detach — leaves the session running, exits nvmux |
| `<prefix>` `Space` | back to the picker, session still attached — `Esc` goes back |
| `<prefix>` `1`, `2`, … `12` | switch straight to that session |
| `<prefix>` `n` | next session by number (wraps) |
| `<prefix>` `p` | previous session by number (wraps) |
| `<prefix>` `c` | set up a new session and attach to it — `Esc` goes back |
| `<prefix>` `?` | show these keys — `Esc` goes back |
| `<prefix>` `<prefix>` | send a literal `<prefix>` to Neovim |

The picker takes the mouse as well: point at a session to select it, click to
attach, scroll to move, and drag a row to change its place in the order.
Opened from a session with `<prefix> Space`, the picker takes the mouse the
same way, and puts that session's own `'mouse'` setting back when you return
to it.

A session's number is its **position in the list**, recalculated every time the
list is read. Kill session 2 of three and the old 3 becomes the new 2 — the
column has no holes in it, so the last session is always the number of sessions
there are. What a session keeps for life is its place in the order, not its
number; `␣` in the picker is how you change that place, and a new session goes
on the end.

### Leaving a session

- **`<prefix> d` detaches.** The session keeps running with all its buffers,
  undo history and jumplist; reattach later, from this machine or another one.
- **`:q` ends the session.** The editor *is* the session, so quitting the last
  window terminates the server, not just your view. That is the one thing to
  unlearn.
- **`x` in the picker kills**, without asking the session about unsaved
  buffers. Use `:q` for the editor's own save prompts.

## Configuration

nvmux needs no configuration. An optional TOML file — `$NVMUX_CONFIG` if set,
else `$XDG_CONFIG_HOME/nvmux/config.toml`, else `~/.config/nvmux/config.toml` —
overrides the defaults below; an unknown key or a bad value is a startup error.

```toml
[keys]
prefix     = "Ctrl-Space"   # Ctrl-Space, or a Ctrl-<letter> chord
timeout_ms = 500            # how long a lone prefix or half-typed number waits

[session]
command = "nvim --headless --listen {sock}"   # {sock} is required
```
