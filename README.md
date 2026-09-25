# nvmux

**Detachable Neovim sessions on a remote host, drawn by your own terminal.**

[![CI](https://github.com/Shooooooooo/nvmux/actions/workflows/ci.yml/badge.svg)](https://github.com/Shooooooooo/nvmux/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](Cargo.toml)

Create named Neovim sessions, attach to them, detach, and come back later —
with the editor running on a remote host while every keystroke and every pixel
of rendering happens on your own terminal.

![Creating a session from the nvmux picker, typing in Neovim, detaching with the prefix key, then relaunching nvmux and reattaching to the same buffer, with each key shown as it is pressed.](https://raw.githubusercontent.com/Shooooooooo/nvmux/main/assets/demo.gif)

Make a session, type in it, detach, come back: the editor, its buffers and its
undo history are where you left them.

[Why not tmux?](#why-not-tmux) · [Requirements](#requirements) · [Install](#install) · [Use](#use) · [How it works](#how-it-works) · [Configuration](#configuration) · [Windows](#windows)

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

Local is macOS or Linux — or, experimentally, Windows, for sessions on a host
you reach with `ssh`: see [Windows](#windows). Remote is any host with a POSIX
`sh`.

## Install

```sh
cargo install --git https://github.com/Shooooooooo/nvmux
```

Or from a checkout:

```sh
cargo build --release
install -m 755 target/release/nvmux ~/.local/bin/
```

## Use

Two invocations, and no subcommands:

```sh
nvmux            # sessions on this machine (macOS and Linux)
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

Pressing `<prefix>` puts a one-row reminder of these keys along the bottom of
the screen; it goes as soon as the next key resolves it.

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
the far side never notice, and nvmux brings the link back on its own: one try
at once and then six retries over about a minute. If the host is still
unreachable after that, nvmux exits with the reason and a reminder that the
session is still running; `nvmux <host>` picks it up again.

Where ssh cannot multiplex — the OpenSSH that ships with Windows — or a host
refuses unix-socket forwards, `[ssh] transport = "relay"` reaches the host
another way: one plain `ssh` connection, multiplexed by nvmux itself, with a
small relay at the far end that runs under the host's own Neovim and needs
nothing installed. Each session is then served locally by nvmux rather than
forwarded by ssh; everything else is the same. See
[docs/windows.md](docs/windows.md#one-plain-ssh-connection-with-a-relay-at-the-far-end).

### One client per session

By default nvmux runs one `--remote-ui` client at a time: a switch retires it
and starts a new one for the next session, which has to connect and wait for
its server to send a whole screen — several round trips over ssh. With
`[client] per_session = true`, every session you visit keeps its client, parked
while another is in front and kept up to date. Switching back puts that
session's screen straight back on the terminal and asks its server for its own
on top: no new process and nothing to wait for, even while the editor is busy
or waiting on a prompt, which the screen then shows.

It is off by default because a parked client is still a UI of its session,
until nvmux exits:

- another UI on the same session shares its screen with it, at the smaller of
  the two sizes — including a second nvmux of your own, whose parked clients
  keep the size its terminal had when it last left each session;
- switching away no longer fires `UILeave` in the session, nor switching back
  `UIEnter`;
- every session visited keeps an idle `nvim` client (about 1.5 MB of its own,
  and a copy of its screen in nvmux) and its ssh forward, and a session that
  keeps redrawing — a `:terminal` running something — keeps sending it frames,
  over the link for a remote one;
- there is no limit on how many are kept: one per session visited.

## Configuration

nvmux needs no configuration. An optional TOML file — `$NVMUX_CONFIG` if set,
else `$XDG_CONFIG_HOME/nvmux/config.toml`, else `~/.config/nvmux/config.toml`
(on Windows, `~` is `%USERPROFILE%` unless `$HOME` is set) — overrides the
defaults below; an unknown key or a bad value is a startup error.

<details>
<summary>Every setting, at its default</summary>

```toml
[keys]
prefix     = "Ctrl-Space"   # Ctrl-Space, or a Ctrl-<letter> chord
timeout_ms = 1000           # how long a lone prefix or half-typed number waits

[session]
command = "nvim --headless --listen {sock}"   # {sock} is required

[client]
per_session = false   # keep a client per session; see "One client per session"

[fade]
enabled     = true   # dissolve between screens; NO_COLOR forces this off
duration_ms = 200    # the whole dissolve, out and back; the notice pays it too
session     = true   # Neovim's own screen dissolves too, and backs the notice
excursions  = true   # so do the <prefix> ? and <prefix> c screens

[ssh]
transport = "control-master"   # or "relay"; see "How it works". Windows: "relay"
```

</details>

## Windows

`nvmux <host>` builds and runs on Windows, as the local end of sessions on a
Linux or macOS host: through the `ssh` that ships with Windows, over the relay
transport, with each session's endpoint a named pipe and Neovim's client on a
pseudoconsole. It is **experimental**: the transport has been tested end to
end from a Windows build running under Wine, and the terminal side has not yet
been run on a real Windows machine.
Sessions on the Windows machine itself (`nvmux` with no host) are not
supported, and the fade is off there.

[docs/windows.md](docs/windows.md) has what works, how it is built, what has
been verified, and the known limitations.
