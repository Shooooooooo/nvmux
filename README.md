# nvmux

**Detachable Neovim sessions on a remote host, drawn by your own terminal.**

[![CI](https://github.com/Shooooooooo/nvmux-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/Shooooooooo/nvmux-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](Cargo.toml)

Create named Neovim sessions, attach to them, detach, and come back later —
with the editor running on a remote host while every keystroke and every pixel
of rendering happens on your own terminal.

![Creating a session from the nvmux picker, typing in Neovim, detaching with the prefix key, then relaunching nvmux and reattaching to the same buffer, with each key shown as it is pressed.](assets/demo.gif)

Make a session, type in it, detach, come back: the editor, its buffers and its
undo history are where you left them.

[Why not tmux?](#why-not-tmux) · [Requirements](#requirements) · [Install](#install) · [Use](#use) · [How it works](#how-it-works) · [Configuration](#configuration)

## Why not tmux?

| | tmux over ssh | nvmux |
|---|---|---|
| Who draws the screen | tmux re-renders the editor from its own grid | Neovim's own client draws straight to your terminal |
| Terminal features | have to survive a trip through tmux; some need coaxing | negotiated between Neovim and your terminal directly |
| What it holds | any program, in panes and windows | Neovim sessions, and nothing else |

That last row is the trade: nvmux is not a tmux replacement. It does one thing,
which is why it can hand Neovim your terminal rather than an imitation of one.

## Requirements

| Where | Needs |
|---|---|
| Local | `nvim` >= 0.11, and `ssh` for `nvmux <host>` |
| Remote | `nvim` >= 0.11 |

0.11 specifically, on both ends: that is the release where `:detach` and
`:connect` landed.

## Install

```sh
cargo install --git https://github.com/Shooooooooo/nvmux-rs
```

Or from a checkout:

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

### Leaving a session

> [!IMPORTANT]
> **`:q` ends the session.** The editor *is* the session, so quitting the last
> window terminates the server, not just your view. That is the one thing to
> unlearn.

- **`<prefix> d` detaches.** The session keeps running with all its buffers,
  undo history and jumplist; reattach later, from this machine or another one.
- **`x` in the picker kills**, without asking the session about unsaved
  buffers. Use `:q` for the editor's own save prompts.

## How it works

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

When that master goes — the laptop slept, the Wi-Fi changed — the sessions on
the far side never notice, and nvmux brings the link back on its own: six
attempts over about a minute, then the picker with the reason and `Enter` to
retry by hand.

## Configuration

nvmux needs no configuration. An optional TOML file — `$NVMUX_CONFIG` if set,
else `$XDG_CONFIG_HOME/nvmux/config.toml`, else `~/.config/nvmux/config.toml` —
overrides the defaults below; an unknown key or a bad value is a startup error.

<details>
<summary>Every setting, at its default</summary>

```toml
[keys]
prefix     = "Ctrl-Space"   # Ctrl-Space, or a Ctrl-<letter> chord
timeout_ms = 500            # how long a lone prefix or half-typed number waits

[session]
command = "nvim --headless --listen {sock}"   # {sock} is required

[fade]
enabled     = true   # dissolve between screens; NO_COLOR forces this off
duration_ms = 100    # each way — a switch pays it out, in, and for the notice
session     = true   # Neovim's own screen dissolves too, and backs the notice
excursions  = true   # so do the <prefix> ? and <prefix> c screens
```

</details>
