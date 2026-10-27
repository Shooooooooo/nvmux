#!/bin/sh
# Spawn a detached Neovim that survives our disconnection.
#
# Usage: sh spawn.sh <runtime_dir> <id> <cwd> <cmd> [args...]
#
# The command is one word per argument, with the socket path already in it
# (validated and split by src/launch.rs). It runs as-is: no shell, no
# expansion. <cwd> arrives absolute with `~` already expanded (src/session.rs).
#
# Output:
#   PID <pid>        the *validated* pid of the new nvim, or empty if we could
#                    not confirm it (see the note on `pid` below)
#   SOCK ok|timeout  whether the listen socket appeared
#   NVMUX_END
[ -n "${NVMUX_PRELUDE:-}" ] || . "$(dirname -- "$0")/_prelude.sh"

usage='usage: spawn.sh <runtime_dir> <id> <cwd> <cmd> [args...]'
dir="${1:?$usage}"
id="${2:?$usage}"
cwd="${3:?$usage}"
: "${4:?$usage}"
shift 3

sock="$dir/$id.sock"
log="$dir/$id.log"

# No `umask` here, deliberately: the editor writes the user's files, so it
# keeps the mask nvmux was launched with (locally the one src/proc.rs restores,
# remotely the host's login default). Neovim creates its socket with
# `0777 & ~umask`; the 0700 directory checked below and the later `chmod 600`
# protect it instead.

# 0700 from the start: a `chmod` after `mkdir` leaves a window, under the
# caller's mask, for others to plant files that outlive it. The mask stays in
# the `$( )`. One another user made since the test fails the check below.
why=
if [ ! -e "$dir" ] && [ ! -L "$dir" ]; then
  why=$(umask 077 && mkdir "$dir" 2>&1)
fi
require_private_dir "$dir" || fail "could not create runtime directory $dir${why:+: $why}"

# Required: `nvim --listen` refuses to start if anything exists at the path,
# with a misleading `address already in use`. Done here because std::fs cannot
# unlink over ssh.
rm -f "$sock"

# The nested-launch marker read by src/nested.rs; exported, it reaches every
# job the editor starts, `:terminal` shells included. Set here rather than over
# RPC after spawn: it is in place before nvim starts and cannot fail silently.
export NVMUX="$sock"

# Enter <cwd> before `command -v` below, which resolves a relative command
# like `./nvim` against the current directory.
#
# Two steps, for two errors: not a directory, or one we cannot enter. Both
# refuse rather than launch somewhere else, which would look right and be
# wrong.
[ -d "$cwd" ] || fail "$cwd: not a directory"
cd -- "$cwd" || fail "$cwd: could not enter it"

# Fail at once on a missing command rather than wait five seconds for a socket
# and blame the session; the shell's own "not found" only reaches the log.
# `command -v` accepts a path as well as a name.
command -v -- "$1" >/dev/null 2>&1 || fail "$1: not found"

# The redirections below would create the log under the caller's mask; set
# its mode so it is private on its own (not only via the 0700 directory) and
# the same on every host.
: >>"$log" 2>/dev/null || true
chmod 600 "$log" 2>/dev/null || true

# Detach from this shell's process group, so no hang-up reaches the editor.
# `setsid` (util-linux) is missing on macOS, so fall back to `nohup`: enough
# there, as this shell already leads a session with no terminal (sshd's, or
# src/proc.rs's locally).
#
# All three fds are redirected in both branches: `ssh host 'cmd &'` with
# stdout attached hangs the client until its timeout (12s versus 0.23s).
if command -v setsid >/dev/null 2>&1; then
  setsid "$@" </dev/null >>"$log" 2>&1 &
  guess=$!
else
  nohup "$@" </dev/null >>"$log" 2>&1 &
  guess=$!
  # Not a POSIX builtin, and absent in dash. `nohup` already did the work.
  disown 2>/dev/null || true
fi

# Wait up to five seconds for the socket: every hundredth of a second for the
# first tenth (when Neovim usually binds it), then every twentieth. Readiness
# is checked separately by the caller over RPC (src/rpc.rs).
sock_state=timeout
i=0
while [ "$i" -lt 500 ]; do
  if [ -S "$sock" ]; then
    # Its only protection besides the directory; never trust the caller's
    # mask.
    chmod 600 "$sock" 2>/dev/null || true
    sock_state=ok
    break
  fi
  # Neither sleep is POSIX; fall back to a whole second rather than spin.
  if [ "$i" -lt 10 ]; then
    sleep 0.01 2>/dev/null || sleep 1
    i=$((i + 1))
  else
    sleep 0.05 2>/dev/null || sleep 1
    i=$((i + 5))
  fi
done

# Report the pid only once confirmed as the nvim serving this socket: a later
# `kill -KILL` targets it, and pids are reused. `$!` may be setsid's own pid,
# as it forks when the caller already leads a process group.
pid=''
if [ -n "$guess" ] && kill -0 "$guess" 2>/dev/null; then
  if owns_socket "$guess" "$sock"; then
    pid=$guess
  fi
fi

# Otherwise find the process actually serving this socket.
if [ -z "$pid" ]; then
  pid=$(serving_pid "$sock")
fi

printf 'PID %s\n' "$pid"
printf 'SOCK %s\n' "$sock_state"
finish
