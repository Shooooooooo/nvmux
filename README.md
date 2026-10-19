# nvmux

**Detachable Neovim sessions on a remote host, drawn by your own terminal.**

[![CI](https://github.com/Shooooooooo/nvmux/actions/workflows/ci.yml/badge.svg)](https://github.com/Shooooooooo/nvmux/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](Cargo.toml)

Create named Neovim sessions, attach to them, detach, and come back later —
with the editor running on a remote host while every keystroke and every pixel
of rendering happens on your own terminal.

![Running nvmux devbox and opening the picker on the three sessions on that host, an agent's spinner turning beside api-server and a check mark beside dotfiles, whose install script finished; creating a fourth, blog, and moving it to the end of the list; attaching to api-server, scrolling as the text slides, and starting a release build in a terminal whose split flies in from the side; switching to the next session with the prefix key while it builds, the row of session names in the top right margin moving the way it went, and to another buffer as it fades over; a note coming up in the margin as the build finishes, the prefix key held saying the key under it, and Enter going back to it; detaching, then running nvmux devbox again to find the list in the order it was left, and reattaching to find the build's split where it was. Each key is shown under the window as it is pressed.](https://raw.githubusercontent.com/Shooooooooo/nvmux/main/assets/demo.gif)

Connect to a host, make a session, put it where you want it in the list, start
a build, hop to another session while it runs, go back the moment it finishes,
detach, come back: the sessions, their windows and their buffers are still
running on the host, where you left them — and nvmux's own client animates the
scrolls, splits and switches on the way.

[Who it's for](#who-its-for) · [Install](#install) · [Use](#use) · [How it works](#how-it-works) · [Configuration](#configuration)

## Who it's for

nvmux is opinionated: it is for people who run their shells, REPLs and coding
agents in Neovim terminal buffers. The session *is* the workspace; nvmux keeps
it alive and gets you back into it.

### Why not tmux?

tmux redraws Neovim from its own grid, and your terminal's features have to
survive the trip through it. In nvmux, a Neovim client draws straight to your
terminal. The trade: nvmux holds Neovim sessions and nothing else. It is not a
tmux replacement.

Inside a session, your programs run in terminal buffers, not panes:

- **They are text.** Search, yank and move through them with your own keys,
  not a copy mode's.
- **They are windows.** Split, float or tab them with the keys you edit with.
- **They reach the editor.** A program in one can drive Neovim through
  `$NVIM`. [agent.nvim](https://github.com/Shooooooooo/agent.nvim) uses this to
  give a coding agent all your buffers.
- **They report back.** Notifications and exit codes reach Lua, so nvmux knows
  when a build finishes.

Their weak spot is libvterm, the emulator behind them: it does not reflow
scrollback on resize. Neovim plans to replace it with Ghostty's libghostty-vt
([neovim#33155](https://github.com/neovim/neovim/issues/33155)).

## Install

### Requirements

| Where | Needs |
|---|---|
| Local | Linux or macOS; `nvim` >= 0.11 for local sessions, and `ssh` for `nvmux <host>` |
| Remote | `nvim` >= 0.11 |

0.11 specifically, on both ends: that is the release where `:detach` landed.
`nvmux <host>` needs no local `nvim` — unless sessions are drawn by Neovim's
own client (`[client] ui = "nvim"`), which is `nvim --remote-ui`.

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

`<prefix>` is `Ctrl-Space`, and a key after it runs a command. Each command
is on the key below unless the [config](#configuration) moves it, by name,
under `[keys.bind]` — `detach = "x"` makes it `<prefix> x` — as
`[keys] prefix` moves the prefix.

| Command | Default key | Action |
|---|---|---|
| `detach` | `<prefix>` `d` | leave the session running and exit nvmux |
| `picker` | `<prefix>` `Space` | the picker, over the session — `Esc` goes back |
| `next` | `<prefix>` `n` | the next session by number (wraps) |
| `prev` | `<prefix>` `p` | the previous session by number (wraps) |
| `new` | `<prefix>` `c` | set up a new session and attach to it — `Esc` goes back |
| `go` | `<prefix>` `Enter` | go where the newest note in the margin is about: its session, or a terminal of this one |
| `help` | `<prefix>` `?` | show these keys, as you have them, over the session — `Esc` goes back |
| `settings` | `<prefix>` `,` | change nvmux's settings while it runs, over the session — `Esc` goes back |

The picker, the help and the settings float over the session, which goes on
running behind them, dimmed; `float = false` under `[style]` has each take
the whole screen instead. Flipped in the settings, it moves them at once.

Two keys after the prefix are not commands, and stay where they are: a number
— `<prefix> 1`, `<prefix> 12` — switches straight to that session, and
`<prefix>` again sends a literal `<prefix>` to Neovim.

Pressing `<prefix>` puts a one-row reminder of the commands, on the keys you
have them on, along the bottom of the screen; it goes as soon as the next key
resolves it. A switch, by key or out of the picker, says where you have
landed: in the top right margin, the session's name between the ones `prev`
and `next` go to, which moves the way you went and goes a moment later. It is
up, and moving, from the moment you press the key, and stays on the screen
while the sessions fade.

### Leaving a session

> [!IMPORTANT]
> **`:q` ends the session.** The editor *is* the session, so quitting the last
> window terminates the server, not just your view. That is the one thing to
> unlearn.

- **`detach` (`<prefix> d`) leaves it running**, with all its buffers, undo
  history and jumplist; reattach later, from this machine or another one.
- **`x` in the picker kills**, without asking the session about unsaved
  buffers. Use `:q` for the editor's own save prompts.

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

### What a session has to say

The picker shows a sign beside each session that has something to report:

| Sign | Means |
|---|---|
| `⠹` | busy: a plugin (Neovim 0.12+) or a language server is working |
| `∗` | a program in a terminal buffer sent a notification |
| `!` | something failed — a command in a terminal buffer, say |
| `✓` | a command in a terminal buffer succeeded |

A command typed at a shell prompt counts only if the shell reports how it went,
as fish 4 does.

While you are in a session, the others' news comes up in the top right
margin, and goes, leaving its sign there until you open that session.
`go` (`<prefix> Enter`) goes to the newest:

```
              ✓ api-server  3s
cargo test: exited with code 0
```

Notes need nvmux's own client; with `ui = "nvim"` there are none. nvmux asks
each session for its news every two seconds; set `push_news = true` under
`[session]` to have each one say when it has some instead, and the note comes
as it happens.

> [!TIP]
> If Claude Code's notifications never show, as on a remote host, set its
> Notifications setting (`/config`) to "Ghostty (OSC 777)".

### An animated client

By default nvmux draws the editor with its own client rather than Neovim's,
so that it can animate it: scrolls slide, split windows fly in, fly out and
resize, floats glide as in [Neovide](https://neovide.dev), and a smeared cursor,
particles and a fading blink can be turned on under `[effects]`. Set
`ui = "nvim"` under `[client]` for Neovim's own client instead: with it, what
the terminal can do reaches the editor as Neovim negotiates it, not through
nvmux.

### A wide terminal

On a terminal wider than its windows need, nvmux's own client can draw each
window's text in the middle of the window: set `centre` under `[client]` to
`"smart"` or `"fixed"`.

`centre = "off"`, the default:

![Two windows side by side across a terminal as wide as an ultra-wide monitor, code on the left and a page of prose on the right, each at the left of its window, the prose in lines as long as its window is wide](https://raw.githubusercontent.com/Shooooooooo/nvmux/main/assets/wide-off.png)

`centre = "smart"`:

![The same two windows across the whole terminal, the code and the prose each in the middle of its own window, with the words of its status line over it](https://raw.githubusercontent.com/Shooooooooo/nvmux/main/assets/wide-on.png)

With `"smart"`, how wide a window's text is depends on what the window
shows: its buffer's `'textwidth'` where it has one, and room for its long
lines where they are not wrapped — all of them with `'wrap'` off, and with
it on those of a Markdown table or code block. Otherwise, and always with
`"fixed"`, it is `window_width`, 120 columns by default. A window showing a
terminal keeps its whole width either way, since that is the width the
program in it is told it has; so does a window whose text is in its middle
already, as a start screen's is, since it was laid out for the whole window.

## Configuration

nvmux needs no configuration. To change a setting, press `,` in the picker or
`<prefix> ,` in a session; changes apply at once, and `w` saves them.

Settings are saved as TOML to `$NVMUX_CONFIG`, else
`$XDG_CONFIG_HOME/nvmux/config.toml`, else `~/.config/nvmux/config.toml`,
which you can also edit by hand.

<details>
<summary>Every setting, at its default</summary>

```toml
[keys]
prefix     = "Ctrl-Space"   # Ctrl-Space, or a Ctrl-<letter> chord
timeout_ms = 1000           # how long a lone prefix or half-typed number waits

[keys.bind]          # the key after the prefix: one character, "Space" or "Enter"
picker   = "Space"   # back to the picker, session still attached
detach   = "d"       # detach, leaving the session running
new      = "c"       # set up a new session and attach to it
next     = "n"       # the next session by number, wrapping
prev     = "p"       # the previous session by number, wrapping
go       = "Enter"   # where the newest note in the margin is about
help     = "?"       # show these keys
settings = ","       # change nvmux's settings while it runs

[session]
command = "nvim --headless --listen {sock}"   # {sock} is required
quick_list = false   # true lists sessions without waiting three seconds on a busy one
push_news  = false   # true has the sessions say when they have news, not be asked every two seconds

[client]
ui             = "nvmux"  # who draws a session: nvmux's own client, animated; or "nvim"
per_session    = true     # keep each session's client; a switch back reuses it
lazy           = false    # start every client at launch; true waits for each session's first visit
revive         = false    # true starts again, once a dropped link is back, the clients it took
throttle       = false    # true draws a session out of sight once a second, not as it changes
predict        = "auto"   # over ssh, scrolls show before Neovim answers; "on" over any slow link
centre         = "off"    # on a wide terminal, each window's text in its middle: "smart" or "fixed"
window_width   = 120      # with centre on, how many columns a window's text gets; 0 for no limit
lean_output    = true     # fewer bytes per frame, the same screen; false makes every move absolute
scroll_regions = false    # true has the terminal move the rows a scroll moved; the same screen
partial_frames = false    # true puts a frame together from the rows that changed alone; the same screen

[style]
color = "terminal"   # or "#rrggbb": nvmux's text and bars in that colour; dim text stays dim
float = true         # the picker, settings and help over the session; false takes the screen

[effects]
enabled = true   # master switch: false turns every effect below off

[effects.fade]
enabled     = true   # dissolve between screens; NO_COLOR forces this off
duration_ms = 200    # the whole dissolve, out and back
session     = true   # Neovim's own screen dissolves too

[effects.move]
enabled = true   # stars off a session being moved, rows stepping aside, its landing

[effects.cursor]
enabled = true   # the row the cursor leaves fades back from the bar

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
