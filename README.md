# nvmux

A tmux-style session manager for Neovim. Create named Neovim sessions, attach to
them, detach, and come back later — with the editor running on a remote host
while every keystroke and every pixel of rendering happens on your own terminal.

```
                                                              
                      1  api-server                           
                    ▸ 2  dotfiles                             
                      3  scratch                              
                      4  notes                                
                                                              
   ↑↓ move   ⏎ 1-9 attach   c new   r rename   x kill   q quit
```

nvmux is a **thin multiplexer**: it does not render Neovim's UI. Neovim already
ships a client that does, so nvmux runs it and passes the bytes through
untouched — which is why bracketed paste, the kitty keyboard protocol,
truecolor, OSC 52 clipboard and DA1/XTGETTCAP round-trips all just work.

```
LOCAL                                     REMOTE
┌───────────────────────────────┐         ┌────────────────────────────────┐
│ nvmux                         │         │ nvim --headless --listen <sock>│
│  ├─ picker UI (ratatui)       │         │   (detached, survives SSH drop)│
│  ├─ PTY proxy (Ctrl-t prefix) │         │ nvim --headless --listen <sock>│
│  └─ child: nvim --server …    │         │ nvim --headless --listen <sock>│
│            --remote-ui        │         │                                │
└───────────────────────────────┘         └────────────────────────────────┘
         │                                            ▲
         │  one persistent ssh master (ControlMaster/ControlPersist);
         │  `ssh -O forward` adds a unix-socket forward per session
         └──────────────────── SSH ───────────────────┘
```

## Requirements

| Where | Needs |
|---|---|
| Local | `nvim` >= 0.11, `ssh` >= 6.7 |
| Remote | `nvim` >= 0.11 |

0.11 is where `:detach` and `:connect` landed; 6.7 is where ssh gained
unix-socket forwarding. Both are checked at startup and reported plainly.

macOS and Linux only.

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

### In the picker

| Key | Action |
|---|---|
| `j` `k` `↓` `↑` | move (wraps) |
| `g` `G` | first / last |
| `1`–`9` | attach to that session |
| `Enter` | attach |
| `c` | name a new session, then attach |
| `r` | rename the selected session |
| `x` | kill |
| `/` | filter |
| `Esc` | clear the filter, or cancel a prompt |
| `q` `Ctrl-c` | quit |

### While attached

`Ctrl-t` is the prefix.

| Keys | Action |
|---|---|
| `Ctrl-t` `d` | detach — leaves the session running, exits nvmux |
| `Ctrl-t` `t` | back to the picker, session still attached |
| `Ctrl-t` `1`–`9` | switch straight to that session |
| `Ctrl-t` `c` | name a new session and attach to it — `Esc` goes back |
| `Ctrl-t` `?` | show these keys — `Esc` goes back |
| `Ctrl-t` `Ctrl-t` | send a literal `Ctrl-t` to Neovim |

Everything else goes to Neovim untouched — including `Ctrl-c`, `Ctrl-z` and
`Ctrl-s`, which reach the editor as ordinary keys rather than becoming signals
for nvmux. Digits are the exception: `Ctrl-t 1` is a command now, so
`Ctrl-t Ctrl-t 1` is how you send that to the editor.

## Configuration

nvmux needs no configuration and has none by default. If you want to tune the
transitions or move the prefix key, it reads an optional TOML file, in this
order:

1. `$NVMUX_CONFIG` — an exact path. If set, it **must** exist.
2. `$XDG_CONFIG_HOME/nvmux/config.toml`
3. `$HOME/.config/nvmux/config.toml`

A missing file, an empty file, or any omitted field keeps the built-in default,
so a partial file only overrides what it names. A file that exists but does not
parse, names an unknown key, or fails validation is a startup error, reported
with its path — a typo is never silently ignored.

Every value below is its default:

```toml
# ~/.config/nvmux/config.toml

[fade]
enabled        = true   # master switch for the dip-to-black transitions.
                        # NO_COLOR forces this off regardless of this setting.
frames         = 8      # steps per direction (must be >= 1).
frame_delay_ms = 12     # milliseconds between frames.
hold_ms        = 30     # milliseconds held fully black across a hand-off.
excursions     = true   # also fade the quick Ctrl-t ? / Ctrl-t c screens.
raw_dissolve   = true   # dissolve an attached session cell by cell, rather
                        # than an instant blackout (cheaper over a slow link).

[keys]
prefix     = "Ctrl-t"   # the prefix key, written like "C-t" or "Ctrl-a"
                        # (case-insensitive).
timeout_ms = 500        # how long a lone prefix or a half-typed number waits.
```

The prefix must be a `Ctrl-<letter>` chord. `C-m`, `C-j`, `C-i` and `C-h` are
rejected (they are Enter, newline, Tab and Backspace on the wire); `C-c` and
`C-z` are allowed, but then that key stops reaching Neovim. `--help` always
spells the default `Ctrl-t`, since it is printed before the config is read — the
`Ctrl-t ?` screen shows the key you actually set.

## Session numbers

Every session gets a number when it is created and keeps it for life, so a
number you have learned goes on meaning the same session. They start at 1 and
fill gaps: kill session 3 and the next one you create becomes 3 again.

Numbers longer than one digit work by typing the digits together — `12` for the
twelfth session. A single digit acts immediately unless a longer number could
still be meant, which only happens once you have more than nine sessions.

## Leaving a session

There are two ways out, and they do different things.

**`Ctrl-t d` detaches.** The session keeps running with all its buffers, undo
history and jumplist. Reattach later, from this machine or another one.

**`:q` ends the session.** The editor *is* the session, so `:q` in the last
window terminates the server, not just your view — as do `:qa`, `ZZ`, `ZQ`,
`:x`, `:wq` and `<C-w>q`. That is the ordinary way to finish with a session and
keep your work: save as usual, then quit as usual. If you expected `:q` to close
only your local view, that is the one thing to unlearn. Each session also gets a
`:Detach` alias for `:detach`, if you would rather type that than the prefix.

**Killing is unconditional.** `x` asks `kill "name"? [y/N]` and then kills — it
does not ask the session about unsaved buffers. Use `:q` for the editor's own
save prompts.
