# nvmux

A tmux-style session manager for Neovim. Create named Neovim sessions, attach to
them, detach, and come back later — with the editor running on a remote host
while every keystroke and every pixel of rendering happens on your own terminal.

```
                                                              
                      1  api-server                           
                    ▸ 2  dotfiles                             
                      3  scratch                              
                      4  notes                                
                                                              
   ↑↓ move  ⏎ attach  c new  r rename  x kill  / filter  q quit
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
| Local | `nvim` >= 0.11, and `ssh` >= 6.7 for `nvmux <host>` |
| Remote | `nvim` >= 0.11 |

0.11 is where `:detach` and `:connect` landed; 6.7 is where ssh gained
unix-socket forwarding. Both are checked at startup and reported plainly; `ssh`
is only needed, and only checked, when a host is given. macOS and Linux only.

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
| `j` `k` `↓` `↑` `Ctrl-n` `Ctrl-p` | move (wraps) |
| `g` `G` `Home` `End` | first / last |
| `1`, `2`, … `12` | attach to the session with that number |
| `Enter` | attach (or end a number early) |
| `c` | name a new session, then attach |
| `r` | rename the selected session |
| `x` | kill |
| `/` | filter |
| `?` | show the `<prefix>` keys |
| `Esc` | clear the filter, or cancel a prompt |
| `q` `Ctrl-c` | quit |

A session name is at most 64 bytes, has no leading or trailing whitespace and
no control characters, and must not be in use — compared without regard to
case. Pressing `Enter` on an empty name prompt takes the suggested `session N`.

**Numbers are for life.** A session keeps the number it was created with, so a
number you have learned goes on meaning the same session. They start at 1 and
fill gaps: kill session 3 and the next one you create becomes 3 again. Type the
digits together for a number past 9 — `12` for the twelfth. A single digit acts
immediately unless a longer number could still be meant, which only happens
once you have more than nine sessions.

### While attached

`<prefix>` is `Ctrl-t` unless you change it in the [config](#configuration).

| Keys | Action |
|---|---|
| `<prefix>` `d` | detach — leaves the session running, exits nvmux |
| `<prefix>` `t` | back to the picker, session still attached |
| `<prefix>` `1`, `2`, … `12` | switch straight to that session |
| `<prefix>` `c` | name a new session and attach to it — `Esc` goes back |
| `<prefix>` `?` | show these keys — `Esc` goes back |
| `<prefix>` `<prefix>` | send a literal `<prefix>` to Neovim |

Everything else goes to Neovim untouched — including `Ctrl-c`, `Ctrl-z` and
`Ctrl-s`, which reach the editor as ordinary keys rather than becoming signals
for nvmux. Digits are the exception: `<prefix> 1` is a command now, so
`<prefix> <prefix> 1` is how you send that to the editor.

### Leaving a session

Three ways out, and they do different things.

- **`<prefix> d` detaches.** The session keeps running with all its buffers,
  undo history and jumplist; reattach later, from this machine or another one.
  Each session also gets a `:Detach` alias for `:detach`.
- **`:q` ends the session.** The editor *is* the session, so `:q` in the last
  window terminates the server, not just your view — as do `:qa`, `ZZ`, `ZQ`,
  `:x`, `:wq` and `<C-w>q`. That is the ordinary way to finish and keep your
  work: save as usual, then quit as usual. If you expected `:q` to close only
  your local view, that is the one thing to unlearn.
- **`x` in the picker kills, unconditionally.** It asks `kill "name"? [y/N]`
  and then kills, without asking the session about unsaved buffers. Use `:q`
  for the editor's own save prompts.

## Configuration

nvmux needs no configuration and has none by default. To tune the transitions
or move the prefix key, it reads an optional TOML file, in this order:

1. `$NVMUX_CONFIG` — an exact path. If set, it **must** exist.
2. `$XDG_CONFIG_HOME/nvmux/config.toml`
3. `$HOME/.config/nvmux/config.toml`

A missing file, an empty file, or any omitted field keeps the built-in default,
so a partial file only overrides what it names. A file that exists but does not
parse, names an unknown key, or fails validation is a startup error, reported
with its path — a typo is never silently ignored.

**First run.** The first time you start nvmux at a terminal with no config file,
it asks you to pick a prefix: press the key you want (or `Enter` to keep
`Ctrl-t`), and nvmux writes the file below for you, so it only ever asks once.
`Esc` skips and leaves things unset — you'll be asked again next time. This never
happens for a non-interactive run or when `$NVMUX_CONFIG` is set.

Every value below is its default:

```toml
# ~/.config/nvmux/config.toml

[fade]
enabled        = true   # master switch for the dip-to-black transitions.
                        # NO_COLOR forces this off regardless of this setting.
frames         = 8      # steps per direction (must be >= 1).
frame_delay_ms = 12     # milliseconds between frames.
hold_ms        = 30     # milliseconds held fully black across a hand-off.
excursions     = true   # also fade the quick <prefix> ? / <prefix> c screens.
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
`<prefix> ?` screen shows the key you actually set.

## Logs

Everything nvmux runs lives under `/tmp/nvmux-<uid>` (the same rule on both
ends, so a session's files stay in one place across logouts); nvmux's own log
is `nvmux.log` there, and each session's server output is `<id>.log`. The
verbosity comes from `$NVMUX_LOG`, in `RUST_LOG` syntax, and defaults to
warnings only. Keystrokes are never logged.
