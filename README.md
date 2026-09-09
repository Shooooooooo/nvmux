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
| `c` | set up a new session, then attach |
| `r` | rename the selected session |
| `Space` | pick the session up; `↑↓` move it, `Enter` places it, `Esc` puts it back |
| `x` | kill |
| `/` | filter |
| `?` | show the `<prefix>` keys |
| `Esc` | clear the filter, or cancel a prompt |
| `q` `Ctrl-c` | quit |

`c` asks three things: what the session is called, how its Neovim is started, and
where it runs. `Tab` moves between the fields and `Enter` submits the whole form
from any one of them, so `c` `Enter` still creates a session in one keystroke —
with the suggested `session N`, the command you last used, and your home
directory. Each field shows what `Enter` would take, dimmed; typing replaces it,
and `→` takes it into the field to be edited instead, which is usually what you
want for the command.

A session name is at most 64 bytes, has no leading or trailing whitespace and
no control characters, and must not be in use — compared without regard to
case.

The command is described under [Configuration](#the-command-a-session-runs).

**Where a session runs.** The third field is the directory its Neovim starts in —
what `:pwd` reports, and what everything keyed off the working directory follows.
It must be absolute, and it is a directory on whichever machine runs the session,
so `nvmux myhost` completes and starts paths on *myhost*, not here. A leading `~`
is expanded against that machine's home directory, which is also what the field
offers by default; nothing else is expanded, for the reasons the command section
gives. A directory that is not there is refused before anything is started.

**The path completes as you type.** Under the field is a menu of the directories
it could become, ranked fuzzily — `nvmx` finds `nvmux-rs`, so you need not know
how a directory starts to reach it. `↑` `↓` and `Ctrl-n` `Ctrl-p` move through
it; `Enter` takes the highlighted one and adds the `/`, so you can keep typing
and the menu drops a level with you. Directories starting with a dot appear once
you type a dot, as in a shell.

In that field the arrows belong to the menu, so `Tab` is how you leave it — which
is how you leave every other field too. `Enter` still submits the whole form from
an untouched field, so `c` `Enter` is still a session in one keystroke; it goes to
the menu only while you have typed something the form could not submit, or moved
the selection. `Esc` hands it back, and a second `Esc` leaves the prompt.

Completion never waits on your keystrokes. The listing runs beside the prompt, so
typing is never slower than typing even when the answer is coming over ssh — and
one listing serves a whole directory, so a path costs about one round trip per
`/` rather than one per key.

### While attached

`<prefix>` is `Ctrl-Space` unless you change it in the [config](#configuration).

| Keys | Action |
|---|---|
| `<prefix>` `d` | detach — leaves the session running, exits nvmux |
| `<prefix>` `Space` | back to the picker, session still attached |
| `<prefix>` `1`, `2`, … `12` | switch straight to that session |
| `<prefix>` `c` | set up a new session and attach to it — `Esc` goes back |
| `<prefix>` `?` | show these keys — `Esc` goes back |
| `<prefix>` `<prefix>` | send a literal `<prefix>` to Neovim |

Everything else goes to Neovim untouched — including `Ctrl-c`, `Ctrl-z` and
`Ctrl-s`, which reach the editor as ordinary keys rather than becoming signals
for nvmux. Digits are the exception: `<prefix> 1` is a command now, so
`<prefix> <prefix> 1` is how you send that to the editor.

The prefix is recognised however your terminal spells it. Neovim asks every
terminal for the kitty keyboard protocol (or xterm's `modifyOtherKeys`), and
one that has it — Windows Terminal from 1.25, kitty, Ghostty, WezTerm, xterm —
then sends `Ctrl-Space` as an escape sequence rather than the byte `NUL`. nvmux
treats both as the prefix, and a literal `<prefix> <prefix>` replays whichever
the terminal sent.

nvmux will not start inside a session. Run it in a `:terminal` there and it
says `already inside an nvmux session` and stops — the outer proxy sees every
`<prefix>` first, so an inner nvmux could be neither detached from nor left.
`<prefix> Space` is the way to the picker, `<prefix> c` the way to a new
session. If you do want a second one anyway — to manage another host's
sessions, say — `NVMUX= nvmux <host>` runs it, with the prefix belonging to the
outer session throughout. (`$NVMUX` is the session socket, exported by the
editor; it is what nvmux checks for.)

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
```

### The command a session runs

`session.command` is the whole command line, and it is what the create prompt
offers the first time. `{sock}` is required and stands for the session's socket:
it is what `nvim` binds and what every later listing and kill finds the session
by, so a line without it is refused rather than quietly repaired.

The line is split into words the way a shell splits them — `'…'`, `"…"` and `\`
all work, so a path with a space in it stays one word — but **nothing is
expanded**. There is no shell in the path to do it, which is also what keeps a
command safe to send over ssh. So no globs, no `$VAR`, no `~`, and no leading
`VAR=value`; write `env NAME=value nvim …` and spell the home directory out.

```toml
[session]
command = "nvim --clean --headless --listen {sock}"
# command = "/opt/nvim-nightly/bin/nvim --headless --listen {sock}"
# command = "env NVIM_APPNAME=work nvim --headless --listen {sock}"
```

Neovim's version is checked as `nvim` on your `$PATH`, which is not necessarily
the binary a custom command runs. A command that names nothing is reported as
soon as the session is created, not after a timeout.

### What nvmux remembers

Change the command at the prompt and the next new session on that host offers it
back. That is kept in `$NVMUX_STATE` if set, else `$XDG_STATE_HOME/nvmux/state.toml`,
else `~/.local/state/nvmux/state.toml` — per host, since a path to a nightly
build on one machine means nothing on another.

The file is a convenience and nothing more: nvmux writes it, deleting it only
loses the suggestion, and a broken one is ignored rather than reported. The
config file is the opposite on every count, which is why they are separate.

## Logs

Everything nvmux runs lives under `/tmp/nvmux-<uid>` (the same rule on both
ends, so a session's files stay in one place across logouts); nvmux's own log
is `nvmux.log` there, and each session's server output is `<id>.log` — beside
`<id>.json`, which records the command that produced it and the directory it
started in. The verbosity comes
from `$NVMUX_LOG`, in `RUST_LOG` syntax, and defaults to warnings only.
Keystrokes are never logged.
