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
 └─ clients: nvim --server … --remote-ui   (detached, survive an SSH drop)
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

### An animated client

Neovim's own client draws each cell where it goes, at once. With

```toml
[client]
ui = "nvmux"
```

nvmux draws the editor itself instead, with its own client in the same place
on the same pty, and animates what [Neovide](https://neovide.dev) does as far
as a grid of character cells allows:

- the cursor travels between cells, its leading edge ahead of its trailing one,
  so a long jump smears across the screen — drawn in block elements, at a
  quarter of a cell or finer, and fading towards its tail (off unless turned
  on);
- sparks, rings or an outline off the cursor as it moves (Neovide's `railgun`,
  `torpedo`, `pixiedust`, `sonicboom`, `ripple` and `wireframe`; off unless
  turned on);
- a scroll slides the text through its window a row at a time;
- a float, the message area, or windows rearranged (`<C-w>x`, `<C-w>r`,
  `<C-w>H` …) slide to where they move to;
- a blinking block cursor can fade rather than flash.

Split windows move the way
[animate.nvim](https://github.com/Shooooooooo/animate.nvim)'s window module
moves them instead: a new split flies in from its side — from the right or
below with `'splitright'` or `'splitbelow'` — as its text fades in out of its
background; a closed one flies back out into its side, its text dimming away
as it goes; a window changing size moves its separators there; and a window
that shows another buffer fades the old text out and the new text in.

What a terminal cannot do is not imitated: no blur, no motion of text finer
than a cell. And the trade is the one the table at the top is about: the screen
is drawn from nvmux's own copy of the editor's grid, and what the terminal can
do reaches the editor through nvmux's client rather than as Neovim negotiated
it — which is why Neovim's own client stays the default. Each animation has a
table of its own under `[effects]`, below.

## Configuration

nvmux needs no configuration. An optional TOML file — `$NVMUX_CONFIG` if set,
else `$XDG_CONFIG_HOME/nvmux/config.toml`, else `~/.config/nvmux/config.toml` —
overrides the defaults below; an unknown key or a bad value is a startup error.

<details>
<summary>Every setting, at its default</summary>

```toml
[keys]
prefix     = "Ctrl-Space"   # Ctrl-Space, or a Ctrl-<letter> chord
timeout_ms = 1000           # how long a lone prefix or half-typed number waits

[session]
command = "nvim --headless --listen {sock}"   # {sock} is required

[client]
ui          = "nvim" # who draws a session: Neovim's own client, or "nvmux"'s, animated
per_session = true   # keep each session's client; a switch back reuses it
lazy        = true   # start a client on its first visit; false starts all at launch

[effects]
enabled = true   # master switch: false turns every effect below off

[effects.fade]
enabled     = true   # dissolve between screens; NO_COLOR forces this off
duration_ms = 200    # the whole dissolve, out and back; the notice pays it too
session     = true   # Neovim's own screen dissolves too, and backs the notice

[effects.move]
enabled = true   # stars off a session being moved, rows stepping aside, its landing

[effects.cursor]
enabled = true   # the row the cursor leaves fades back; a glint crosses the next

[effects.back]
enabled = true   # back from a session, rings pulse from its row in the picker

[effects.attach]
enabled = true   # the picker closes onto the name; the session opens out of it

[effects.kill]
enabled = true   # a line through the name at [y/N]; then the row is erased

[effects.filter]
enabled = true   # rows a filter keystroke drops fade out before the list closes

[effects.create]
enabled = true   # c opens a gap where the new session goes, then the prompt

# The rest are for the animated client, [client] ui = "nvmux".

[effects.smear]
enabled = false   # the cursor travels between cells, smearing across a long jump

[effects.particles]
enabled = false       # what flies off the cursor as it moves
mode    = "railgun"   # railgun, torpedo, pixiedust, sonicboom, ripple, wireframe

[effects.scroll]
enabled = true   # a scroll slides the text through its window

[effects.windows]
enabled = true   # windows and floats move rather than jump

[effects.blink]
enabled = false   # a blinking block cursor fades out and back in
```

</details>
