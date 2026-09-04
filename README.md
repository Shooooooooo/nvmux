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

## What it is, and what it deliberately is not

nvmux is a **thin multiplexer**. It does not render Neovim's UI — there is no
`nvim_ui_attach`, no `grid_line` handling, no highlight table and no grid
diffing anywhere in the codebase. Neovim already ships a client that does all of
that, so nvmux runs it:

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

The child's output reaches your terminal **byte for byte, unparsed**. That is
the whole reason bracketed paste, the kitty keyboard protocol, truecolor,
undercurl, terminal titles, OSC 52 clipboard and DA1/XTGETTCAP query/response
round-trips all just work: the child negotiates directly with your real
terminal, and nvmux is not in the way.

## Requirements

| Where | Needs |
|---|---|
| Local | `nvim` >= 0.11, `ssh` >= 6.7 |
| Remote | `nvim` >= 0.11 |

0.11 is the floor because that is where `:detach` and `:connect` landed. 6.7 is
where ssh gained unix-socket forwarding. Both are checked at startup and
reported plainly rather than failing later as a mysterious connection error.

macOS and Linux only. There is no Windows support and no abstraction layer
pretending otherwise.

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
| `c` | create, then attach |
| `r` | rename |
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
| `Ctrl-t` `c` | create a new session and attach to it |
| `Ctrl-t` `Ctrl-t` | send a literal `Ctrl-t` to Neovim |

Everything else goes to Neovim untouched — including `Ctrl-c`, `Ctrl-z` and
`Ctrl-s`, which reach the editor as ordinary keys rather than becoming signals
for nvmux.

## Leaving a session

There are two ways out, and they do different things.

**`Ctrl-t d` detaches.** The session keeps running with all its buffers, undo
history and jumplist. Reattach later, from this machine or another one.

**`:q` ends the session.** In a `--remote-ui` session the editor *is* the
session, so quitting the editor quits the session — `:q` in the last window
terminates the server, not just your view. That is the ordinary way to finish
with a session and keep your work: save as usual, then quit as usual. So do
`:qa`, `ZZ`, `ZQ`, `:x`, `:wq` and `<C-w>q`.

If you expected `:q` to close only your local view, that is the one thing to
unlearn. nvmux does not remap it: a `cnoreabbrev` guard would cover bare `:q`,
silently turn `:q!` into a no-op (`bang (!) not supported yet`), and miss every
other spelling above — worse than no guard at all. A `:Detach` command is
installed in each session as an alias for `:detach`, if you would rather type
that than the prefix key.

**Killing is unconditional.** `x` in the picker asks `kill "name"? [y/N]` and
then kills — it does not ask the session about unsaved buffers. Use `:q` if you
want the editor's own save prompts.

## Where things live

On the machine that runs the Neovim processes:

```
/tmp/nvmux-<uid>/<id>.sock     the listen socket
/tmp/nvmux-<uid>/<id>.json     {"id","name","created","pid"}
/tmp/nvmux-<uid>/<id>.log      the session's own output — read this first
                               when a session will not start
```

`<id>` is eight random base32 characters, never the name. Renaming rewrites the
name in the JSON and moves nothing, so the socket path — the session's real
identity — is stable and no SSH forward has to be rebuilt.

The metadata lives beside the socket on the session host, not on your laptop, so
attaching from a second machine shows the same names.

**`$XDG_RUNTIME_DIR` is deliberately ignored.** On Linux it is `/run/user/<uid>`,
which systemd destroys when your last login session ends unless
`loginctl enable-linger` is set — so sessions would silently die at logout,
which is precisely what nvmux exists to prevent. `/tmp/nvmux-<uid>` behaves the
same on both platforms and survives logout. It is created `0700` and checked on
every use; nvmux refuses to run if it is not a directory, not owned by you, or
readable by anyone else.

Paths are kept short on purpose. `sun_path` is 104 bytes on macOS and 108 on
Linux, and Neovim *silently truncates* an overlong socket path while Rust
refuses it outright — which would leave a perfectly healthy session that nvmux
could never reach, with no error printed anywhere. Anything over 100 bytes is
rejected up front.

## Logs

nvmux writes to `/tmp/nvmux-<uid>/nvmux.log`, never to stdout — stdout belongs to
the picker and then to the attached editor. Raise the level with:

```sh
NVMUX_LOG=nvmux=debug nvmux myhost
```

Keystrokes are never logged, at any level.

Each session's own output goes to `/tmp/nvmux-<uid>/<id>.log` on the session
host. Headless Neovim writes `:echomsg` *and* errors to stderr, which is why
that file exists and why it is the first thing to look at when a session will
not start.

## No preview pane, and there never will be

A preview of the highlighted session would mean attaching a second UI to it, and
Neovim sizes the global grid to the per-dimension **minimum** across every
attached UI:

```c
/* src/nvim/ui.c, ui_refresh(), v0.11.4 */
int width = INT_MAX;
int height = INT_MAX;
for (size_t i = 0; i < ui_count; i++) {
  RemoteUI *ui = uis[i];
  width  = MIN(ui->width,  width);
  height = MIN(ui->height, height);
}
screen_resize(width, height);
```

So a small preview UI would shrink the grid of the session you are actually
editing in and fire `VimResized`. There is no read-only or observer attach mode
that opts out — none of the `ui_options` makes an attachment non-sizing. nvmux
reads session state over plain RPC instead, and never attaches a second UI to a
live session.

## Development

```sh
cargo test          # unit + local integration tests
cargo clippy --all-targets -- -D warnings
```

The local integration tests spawn real `nvim --headless` processes in their own
scratch directories. The SSH tests need a host they can reach, taken from
`$NVMUX_TEST_SSH_HOST` and defaulting to `selftest`; where no such host answers
they skip rather than fail. Pointing that alias at localhost exercises the real
ssh client, a real ControlMaster, real unix-socket forwarding and a real remote
login shell — everything except latency:

```
Host selftest
  HostName 127.0.0.1
  User <you>
  IdentityFile ~/.ssh/id_ed25519
```

## Not in v0.1

Panes, splits and layouts; a config file; session persistence across reboots;
`$CWD`-based auto-naming; mouse support; multiple simultaneous UIs on one
session; Windows; mosh; password-based SSH auth.
