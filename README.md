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

The prefix is recognised however your terminal spells it. Neovim asks every
terminal for the kitty keyboard protocol (or xterm's `modifyOtherKeys`), and
one that has it — Windows Terminal from 1.25, kitty, Ghostty, WezTerm, xterm —
then sends `Ctrl-t` as an escape sequence rather than a control byte. nvmux
treats both as the prefix, and a literal `<prefix> <prefix>` replays whichever
the terminal sent.

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

nvmux needs no configuration. To move the prefix key or change how long it
waits, it reads an optional TOML file — `$NVMUX_CONFIG` if set, else
`$XDG_CONFIG_HOME/nvmux/config.toml`, else `~/.config/nvmux/config.toml`. Any
omitted field keeps its default; an unknown key or a bad value is a startup
error.

Every value below is its default:

```toml
[keys]
prefix     = "Ctrl-t"   # a Ctrl-<letter> chord
timeout_ms = 500        # how long a lone prefix or half-typed number waits
```

## Logs

Everything nvmux runs lives under `/tmp/nvmux-<uid>` (the same rule on both
ends, so a session's files stay in one place across logouts); nvmux's own log
is `nvmux.log` there, and each session's server output is `<id>.log`. The
verbosity comes from `$NVMUX_LOG`, in `RUST_LOG` syntax, and defaults to
warnings only. Keystrokes are never logged.
