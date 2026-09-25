# nvmux on Windows

**Status: experimental.** `nvmux <host>` builds and runs on Windows as a
client of sessions on a Linux or macOS host. Its transport has been exercised
end to end from a Windows build running under Wine; the part that draws the
editor on your terminal has not yet been run on a real Windows machine. See
[What has been verified](#what-has-been-verified).

## What works, and what does not

| | Windows |
|---|---|
| `nvmux <host>`, sessions on a Linux or macOS host | yes |
| The picker, create / rename / kill, directory completion | yes |
| Attach, detach, `<prefix>` keys, switching, `[client] per_session` | yes, not yet run on real Windows |
| Reconnecting after the link drops | yes |
| `nvmux` alone — sessions on the Windows machine itself | no: a session's scripts are POSIX `sh` |
| `[ssh] transport = "control-master"` | no: Windows' ssh cannot multiplex |
| The fade between screens | off: see [Limitations](#limitations) |

## Requirements

- Windows 10 1809 or later, for the pseudoconsole (ConPTY).
- A terminal that speaks VT sequences both ways: Windows Terminal, or
  WezTerm, Alacritty and the like. The classic console host has the same
  modes, but has not been tried.
- `nvim` >= 0.11 on `%PATH%` (`winget install Neovim.Neovim`), as the
  `--remote-ui` client.
- The OpenSSH client, which ships with Windows
  (`C:\Windows\System32\OpenSSH\ssh.exe`). Key-based authentication through
  `ssh-agent` is the smooth path; a password or passphrase prompt is asked on
  the console before the picker comes up, as on Unix, but has not been tried
  on Windows.
- On the host: `nvim` >= 0.11 and a POSIX `sh`, as on any other platform.

```powershell
cargo install --git https://github.com/Shooooooooo/nvmux
nvmux myhost
```

The config file is where it is everywhere else, relative to your home
directory: `$NVMUX_CONFIG`, else `$XDG_CONFIG_HOME\nvmux\config.toml`, else
`%USERPROFILE%\.config\nvmux\config.toml` (`$HOME` wins over `%USERPROFILE%`
when it is set). The log is in `%LOCALAPPDATA%\nvmux`.

## How it works

Everything that makes a session what it is runs on the host, exactly as it
does from macOS or Linux: the same scripts, the same `nvim --headless
--listen` servers, the same sockets. What differs is on this side, in four
places.

### One plain ssh connection, with a relay at the far end

On Unix nvmux keeps one `ControlMaster` per host and adds an `ssh -O forward`
unix-socket forward to it for each session. OpenSSH for Windows has no
multiplexing at all — asked to be a master it fails with `getsockname failed:
Not a socket` — so on Windows nvmux uses its other transport, the **relay**
(`[ssh] transport = "relay"`, the default there, and available on Unix too):

```
WINDOWS                                 HOST
nvmux                                   nvim --headless -l relay.lua
 ├─ \\.\pipe\nvmux-…-<id1> ─┐            ├─ <id1>'s socket
 ├─ \\.\pipe\nvmux-…-<id2> ─┼─ ssh -T ───┼─ <id2>'s socket
 └─ shells on the host ─────┘            └─ sh -s, one per shell

            one stream of frames, a channel for each
```

One `ssh -T` starts the same login shell and `sh -s` every other script runs
in, and is fed `scripts/boot.sh`, which writes `scripts/relay.lua` into the
host's private runtime directory (`/tmp/nvmux-<uid>`, checked as `spawn.sh`
checks it) and `exec`s a bare Neovim to run it. From then on the connection
carries frames — a kind, a channel and a length — and every session socket and
every shell nvmux wants over there is a channel of its own, with credit-based
flow control so one busy session cannot starve the rest. See `src/mux.rs` for the client
end and `scripts/relay.lua` for the far one.

### Named pipes for a session's endpoint

Each session nvmux reaches is served locally at a named pipe,
`\\.\pipe\nvmux-<run>-<host>-<id>`, which `nvim --server` accepts as readily
as a socket path. A pipe is private by its access list rather than by the
directory it is in: one entry, full access for the user nvmux runs as, and
nothing inherited. It is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a
name someone else made first is refused rather than joined, and remote clients
are rejected. Not a loopback TCP port, which any account on the machine could
connect to — and a session is a shell as its user. (`src/sys/windows/pipe.rs`)

### A pseudoconsole for the client

`nvim --server <pipe> --remote-ui` runs on a pseudoconsole, the Windows pty.
nvmux holds its own rather than `portable-pty`'s, for the flags: that one asks
the pseudoconsole to inherit the cursor, which makes it query the terminal and
draw nothing until the answer comes, and to switch the terminal into
win32-input-mode, which would change how every key — the prefix included —
reaches nvmux. A thread reads the pseudoconsole's output into a queue an event
makes waitable, and a second waits for the client to exit and closes the
pseudoconsole then, which is what ends its output: the moment a Unix pty would
report EOF. (`src/sys/windows/conpty.rs`)

### The console nvmux runs in

Raw mode is a pair of console modes: no line editing, no echo, `Ctrl-C` a key
rather than a signal, escape sequences interpreted on the way out, and — with
`ENABLE_VIRTUAL_TERMINAL_INPUT` — every key delivered as the bytes of its VT
sequence, which is what the prefix machine reads everywhere. A console
reports a resize as an input record rather than a signal, so the relay's one
wait is on the console's input and the client's output together, as on Unix
it is one `poll` over stdin, the pty and `SIGWINCH`'s self-pipe. In place of
the signal handler that puts the terminal back, a console control handler does
the same when the window is closed or interrupted. (`src/sys/windows/console.rs`,
`src/sys/windows/relay.rs`)

`Ctrl-Space`, the default prefix, is the one key a console record cannot carry
as a character: it is NUL. A key-down with no character is taken as NUL when
it has no key code either, or is the space or `2` key with `Ctrl` held.

## What has been verified

On Linux, and under Wine 9.0 for the Windows build (no real Windows machine was
available):

- **The Linux build is unchanged in behaviour.** Everything platform-specific
  moved behind `src/sys/` without changing what it does: the full test suite
  passes, and the real binary, driven on a pty, creates a session, types into
  it, detaches, reattaches with the buffer intact and kills it, over both
  transports.
- **The Windows build compiles clean** for `x86_64-pc-windows-msvc` and
  `-gnu`, `cargo clippy --all-targets -- -D warnings` included, and on the
  minimum Rust version, 1.88.
- **The transport works from a Windows build.** `tests/relay_windows.rs`, run
  under Wine with Microsoft's `ssh.exe` (OpenSSH_for_Windows 10.0p2) against a
  Linux `sshd`: sessions are created over the relay, reached through their
  named pipes — a request, a deferred call and a second channel beside the
  first — renamed, killed (which stops their pipe), found again by a second
  transport, and the host's directories are listed for completion.
- **The Windows unit tests** pass under Wine, 697 of 701, and the four that do
  not fail for want of Wine features: it ignores `FILE_FLAG_FIRST_PIPE_INSTANCE`
  (its `CreateNamedPipeW` always opens with `FILE_OPEN_IF`), and its
  pseudoconsole renders only a first frame and does not implement
  `ResizePseudoConsole`.

**Not verified:** anything that needs a working pseudoconsole or a real
console — attaching, typing, the `<prefix>` keys, resizing, the mouse. The
Windows CI job (`.github/workflows/ci.yml`) runs the unit tests on a real
Windows runner, the pseudoconsole's among them, but nothing yet drives a whole
attach there. That is the first thing to try on a real machine.

## Limitations

- **No sessions on the Windows machine itself.** A session is created,
  listed and killed by POSIX `sh` scripts, and its editor is found by its
  unix socket; none of that has a Windows counterpart yet.
- **The fade is off.** It needs the terminal's palette, which nvmux asks for
  with OSC queries followed by a DSR whose answer says the rest are in. On
  Windows the console host sits between nvmux and the terminal, answers some
  of those questions itself and passes others on, so that order is not
  guaranteed — and an answer that arrived after the wait would be read by the
  picker as keys.
- **A client is ended outright.** There is no hangup a console process acts on
  and still exits promptly, so detaching terminates the `--remote-ui` client —
  which leaves its server running, as it always does — and clears the screen it
  was drawing, since it never gets to restore it itself.
- **Keyboard protocols pass through two console hosts** — the one nvmux runs in
  and the pseudoconsole the client runs in — and whether the kitty keyboard
  protocol survives both is untested. nvmux reads the prefix in every encoding
  it knows (see `src/keyseq.rs`), so the risk is to the keys Neovim sees, not
  to detaching.

## Trying it

On Windows:

```powershell
cargo test                                   # unit tests, pseudoconsole included
$env:NVMUX_TEST_SSH_HOST = "myhost"          # a host ssh reaches unprompted
cargo test --test relay_windows              # the transport, end to end
cargo run --release -- myhost                # the real thing
```

From Linux, under Wine, with the mingw target installed and Microsoft's
OpenSSH unpacked somewhere:

```sh
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine WINEDEBUG=-all
export WINEPATH='Z:\path\to\OpenSSH-Win64'   # so the tests find ssh.exe
# ~/.ssh/config and the key go in the prefix: drive_c/users/$USER/.ssh
NVMUX_TEST_SSH_HOST=myhost cargo test --target x86_64-pc-windows-gnu --test relay_windows
```
