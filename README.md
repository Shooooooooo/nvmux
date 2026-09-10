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

### In the picker

| Key | Action |
|---|---|
| `j` `k` `↓` `↑` `Ctrl-n` `Ctrl-p` | move (wraps) |
| `g` `G` `Home` `End` | first / last |
| `1`, `2`, … `12` | attach to the session with that number |
| `Enter` | attach (or end a number early) |
| `c` `r` `x` | new / rename / kill |
| `Space` | pick the session up; `↑↓` move it, `Space` places it, `Esc` puts it back |
| `/` | filter |
| `?` | show the `<prefix>` keys |
| `Esc` | clear the filter or a half-typed number, then go back to the session you came from |
| `q` `Ctrl-c` | quit |

`c` asks three things: what the session is called, how its Neovim is started, and
where it runs. The last two arrive pre-filled and editable — the
[command](#the-command-a-session-runs) you last used on that host, and your home
directory there. `Enter` submits the whole form from any field, so `c` `Enter`
creates a session in one keystroke.

The directory is one on whichever machine runs the session, so `nvmux myhost`
completes and starts paths on *myhost*, not here; one that is not there is
refused before anything is started. `Tab` opens a fuzzily ranked completion menu
— `nvmx` finds `nvmux-rs` — and listing runs beside the prompt, so typing is
never slower even when the answer is coming over ssh. Typing `//` discards
everything before it, so `/home/shu//etc` means `/etc`. A leading `~` is expanded
against the session host's home directory; nothing else is expanded, for the
reasons the command section gives.

A session name is at most 64 bytes, has no leading or trailing whitespace and
no control characters, and must not be in use — compared without regard to
case.

### While attached

`<prefix>` is `Ctrl-Space` unless you change it in the [config](#configuration).

| Keys | Action |
|---|---|
| `<prefix>` `d` | detach — leaves the session running, exits nvmux |
| `<prefix>` `Space` | back to the picker, session still attached — `Esc` goes back |
| `<prefix>` `1`, `2`, … `12` | switch straight to that session |
| `<prefix>` `n` / `p` | next / previous session by number — wraps at both ends |
| `<prefix>` `c` | set up a new session and attach to it — `Esc` goes back |
| `<prefix>` `?` | show these keys — `Esc` goes back |
| `<prefix>` `<prefix>` | send a literal `<prefix>` to Neovim |

Everything else goes to Neovim untouched — including `Ctrl-c`, `Ctrl-z` and
`Ctrl-s`, which reach the editor as ordinary keys rather than becoming signals
for nvmux. Digits, `n` and `p` are the exceptions: `<prefix> 1`, `<prefix> n`
and `<prefix> p` are commands now, so `<prefix> <prefix> n` is how you send one
of those to the editor.

Landing on a different session says so, in a box in the middle of the screen.
`popup.duration_ms` in the [config](#configuration) changes how long it stays,
and `0` turns it off.

nvmux will not start inside a session — the outer proxy sees every `<prefix>`
first, so an inner nvmux could be neither detached from nor left. To manage
another host's sessions anyway, `NVMUX= nvmux <host>` runs it, with the prefix
belonging to the outer session throughout. (`$NVMUX` is the session socket,
exported by the editor; it is what nvmux checks for.)

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
prefix     = "Ctrl-Space"   # Ctrl-Space, or a Ctrl-<letter> chord
timeout_ms = 500            # how long a lone prefix or half-typed number waits

[session]
command = "nvim --headless --listen {sock}"   # what a new session starts

[popup]
duration_ms = 1000          # how long the session notice stays; 0 turns it off
```

### The command a session runs

`session.command` is the whole command line, and it is what the create prompt
offers the first time. `{sock}` is required and stands for the session's socket:
it is what `nvim` binds and what every later listing and kill finds the session
by, so a line without it is refused rather than quietly repaired.

The line is split into words the way a shell splits them — `'…'`, `"…"` and `\`
all work — but **nothing is expanded**. There is no shell in the path to do it,
which is also what keeps a command safe to send over ssh. So no globs, no
`$VAR`, no `~`, and no leading `VAR=value`; write `env NAME=value nvim …` and
spell the home directory out.

```toml
[session]
command = "nvim --clean --headless --listen {sock}"
# command = "/opt/nvim-nightly/bin/nvim --headless --listen {sock}"
# command = "env NVIM_APPNAME=work nvim --headless --listen {sock}"
```

Neovim's version is checked as `nvim` on your `$PATH`, which is not necessarily
the binary a custom command runs.

### What nvmux remembers

Change the command at the prompt and the next new session on that host offers it
back. That is kept — per host, since a path to a nightly build on one machine
means nothing on another — in `$NVMUX_STATE` if set, else
`$XDG_STATE_HOME/nvmux/state.toml`, else `~/.local/state/nvmux/state.toml`.
Deleting it only loses the suggestion, and a broken one is ignored rather than
reported; the config file is the opposite on both counts, which is why they are
separate.

## Logs

Everything nvmux runs lives under `/tmp/nvmux-<uid>` (the same rule on both
ends, so a session's files stay in one place across logouts); nvmux's own log
is `nvmux.log` there, and each session's server output is `<id>.log` — beside
`<id>.json`, which records the command that produced it and the directory it
started in. The verbosity comes
from `$NVMUX_LOG`, in `RUST_LOG` syntax, and defaults to warnings only.
Keystrokes are never logged.
