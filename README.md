# nvmux

A tmux-style session manager for Neovim. Create named Neovim sessions, attach to
them, detach, and come back later — with the editor running on a remote host
while every keystroke and every pixel of rendering happens on your own terminal.

```
                                                              
                        api-server                            
                      ▸ dotfiles                              
                        scratch                               
                        notes                                 
                                                              
  ↑↓ move   ⏎ attach   c new   r rename   x kill   q quit     
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
| `Ctrl-t` `c` | name a new session and attach to it — `Esc` goes back |
| `Ctrl-t` `?` | show these keys — `Esc` goes back |
| `Ctrl-t` `Ctrl-t` | send a literal `Ctrl-t` to Neovim |

Everything else goes to Neovim untouched — including `Ctrl-c`, `Ctrl-z` and
`Ctrl-s`, which reach the editor as ordinary keys rather than becoming signals
for nvmux.

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
