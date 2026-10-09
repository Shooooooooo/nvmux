# nvmux

**Detachable Neovim sessions on a remote host, drawn by your own terminal.**

[![CI](https://github.com/Shooooooooo/nvmux/actions/workflows/ci.yml/badge.svg)](https://github.com/Shooooooooo/nvmux/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](Cargo.toml)

Create named Neovim sessions, attach to them, detach, and come back later —
with the editor running on a remote host while every keystroke and every pixel
of rendering happens on your own terminal.

![Opening the nvmux picker on three sessions; creating a fourth, blog, and moving it to the end of the list; attaching to api-server, scrolling as the text slides and opening a split that flies in from the side; switching to the next session with the prefix key, and to another buffer as it fades over; detaching, then relaunching nvmux to find the list in the order it was left, and reattaching to find the split where it was. Each key is shown as it is pressed.](https://raw.githubusercontent.com/Shooooooooo/nvmux/main/assets/demo.gif)

Make a session, put it where you want it in the list, work, hop to another,
detach, come back: the sessions, their windows and their buffers are where you
left them — and nvmux's own client animates the scrolls, splits and switches
on the way.

[Who it's for](#who-its-for) · [Why not tmux?](#why-not-tmux) · [Install](#install) · [Use](#use) · [How it works](#how-it-works) · [Configuration](#configuration)

## Who it's for

nvmux is a very opinionated program. It suits folks who already run their
terminal programs — shells, REPLs, coding agents — in Neovim terminal buffers,
so that a Neovim session *is* the workspace, and nvmux only has to keep that
session alive and get you back into it.

Pair it with [agent.nvim](https://github.com/Shooooooooo/agent.nvim) and a
coding agent running in one of those buffers gains access to all your Neovim
buffers, making it more powerful than the same agent in a standalone terminal.

## Why not tmux?

| | tmux over ssh | nvmux |
|---|---|---|
| Who draws the screen | tmux re-renders the editor from its own grid | a Neovim client: nvmux's own, animated, or Neovim's, straight to your terminal |
| Terminal features | have to survive a trip through tmux; some need coaxing | negotiated by that client with your terminal, through a relay that never rewrites them |
| What it holds | any program, in panes and windows | Neovim sessions, and nothing else |

That last row is the trade: nvmux is not a tmux replacement. It does one thing,
which is why it can draw Neovim from Neovim's own account of its screen — or
hand Neovim your terminal outright — rather than an imitation of one.

## Install

### Requirements

| Where | Needs |
|---|---|
| Local | `nvim` >= 0.11 for local sessions, and `ssh` for `nvmux <host>` |
| Remote | `nvim` >= 0.11 |

0.11 specifically, on both ends: that is the release where `:detach` and
`:connect` landed. `nvmux <host>` needs no local `nvim` — unless sessions are
drawn by Neovim's own client (`[client] ui = "nvim"`), which is `nvim
--remote-ui`.

### Building

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

### What a session has to say

Beside each name the picker shows, dim, what that session has to report:

| Sign | Means |
|---|---|
| `⠹` (turning) | something in it is busy: a progress message is running, which is how a plugin such as [agent.nvim](https://github.com/Shooooooooo/agent.nvim) says an agent is working |
| `⡆` | the same, with the percentage the progress gives |
| `∗` | a program in one of its terminal buffers asked for a desktop notification — an agent wanting your permission, say |
| `!` | a progress message failed |

The selected session's note is spelt out under the list —
`Claude Code · Claude needs your permission · 2m` — and a notification stays
until you next open its session. `[picker] notes = "column"` puts the notes in
the rows instead, after the names; `"off"` shows none.

To know, nvmux leaves a watcher in each session — one augroup, `nvmux_notes` —
that records Neovim's progress messages and the notifications programs in
terminal buffers send: OSC 777 (Ghostty's), OSC 9 (iTerm2's) and OSC 99
(kitty's). It stays when nvmux leaves, so a session that wanted you overnight
says so in the morning; `"off"` takes it out again.

> [!NOTE]
> A program only sends a notification to a terminal it thinks will show it.
> Claude Code goes by `TERM_PROGRAM`, which a session on this machine inherits
> from your terminal and a session on a remote host does not. There, or in a
> terminal Claude Code sends nothing to — Windows Terminal, WezTerm — set its
> Notifications setting (`/config`) to "Ghostty (OSC 777)".

## How it works

nvmux is a **thin multiplexer**: each session is drawn by a client of its own,
on a pty whose bytes nvmux passes through untouched — which is why bracketed
paste, the kitty keyboard protocol, truecolor, OSC 52 clipboard and
DA1/XTGETTCAP round-trips all just work. That client is nvmux's own, unless
`[client] ui = "nvim"` makes it Neovim's (see
[An animated client](#an-animated-client)).

```
LOCAL                                    REMOTE
nvmux                                    nvim --headless --listen <sock>
 ├─ picker UI (ratatui)                  nvim --headless --listen <sock>
 ├─ PTY proxy (watches for <prefix>)     nvim --headless --listen <sock>
 └─ clients: nvmux --client …              (detached, survive an SSH drop)
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

By default nvmux draws the editor with its own client rather than Neovim's,
so that it can animate it: scrolls slide, split windows fly in, fly out and
resize, floats glide as in [Neovide](https://neovide.dev), and a smeared cursor,
particles and a fading blink can be turned on under `[effects]`. Set
`ui = "nvim"` under `[client]` for Neovim's own client instead: with it, what
the terminal can do reaches the editor as Neovim negotiates it, not through
nvmux.

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
ui          = "nvmux" # who draws a session: nvmux's own client, animated; or "nvim"
per_session = true    # keep each session's client; a switch back reuses it
lazy        = true    # start a client on its first visit; false starts all at launch
predict     = true    # over a slow link, scrolls show before Neovim answers

[picker]
notes = "signs"   # what a session reports beside its name: "signs", "column" or "off"

[effects]
enabled = true   # master switch: false turns every effect below off

[effects.fade]
enabled     = true   # dissolve between screens; NO_COLOR forces this off
duration_ms = 200    # the whole dissolve, out and back; the notice pays it too
session     = true   # Neovim's own screen dissolves too, and backs the notice

[effects.move]
enabled = true   # stars off a session being moved, rows stepping aside, its landing

[effects.cursor]
enabled = true   # the row the cursor leaves fades back from the bar

[effects.back]
enabled = true   # back from a session, a hollow ▹ on its row in the picker

[effects.attach]
enabled = true   # the picker closes onto the name; the session opens out of it

[effects.kill]
enabled = true   # a line through the name at [y/N]; then the row is erased

[effects.filter]
enabled = true   # rows a filter keystroke drops fade out before the list closes

[effects.create]
enabled = true   # c opens a gap where the new session goes, then the prompt

# The rest are for nvmux's own client, the default ui.

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
