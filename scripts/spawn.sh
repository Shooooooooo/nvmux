#!/bin/sh
# Spawn a detached Neovim that survives our disconnection.
#
# Usage: sh spawn.sh <runtime_dir> <id> <cwd> <cmd> [args...]
#
# The command is the caller's, one word per argument, already carrying this
# session's socket path -- src/launch.rs is what validated and split it, and
# nothing here re-parses it. It is run as-is: no shell, so no expansion, and no
# string this script assembled.
#
# The working directory is the caller's too, and arrives already absolute:
# src/session.rs refused a relative one and expanded any leading `~` against the
# home directory hello.sh reported, so there is nothing here to expand either.
#
# Output:
#   PID <pid>        the *validated* pid of the new nvim, or empty if we could
#                    not confirm it (see the safety note below)
#   SOCK ok|timeout  whether the listen socket appeared
#   NVMUX_END
#
[ -n "${NVMUX_PRELUDE:-}" ] || . "$(dirname -- "$0")/_prelude.sh"

usage='usage: spawn.sh <runtime_dir> <id> <cwd> <cmd> [args...]'
dir="${1:?$usage}"
id="${2:?$usage}"
cwd="${3:?$usage}"
: "${4:?$usage}"
shift 3

sock="$dir/$id.sock"
log="$dir/$id.log"

# No `umask` here, deliberately. The session's editor writes the user's files,
# so it runs under the umask the user launched nvmux with -- locally that is
# the mask src/proc.rs puts back on this shell, remotely the one this host
# gives its own logins. Neovim does create its listen socket with
# `0777 & ~umask`, which is what a mask here once guarded: what guards it now
# is the 0700 runtime directory, checked just below before anything is
# spawned, so a socket is unreachable for as long as it exists whatever mode
# it is born with -- and the `chmod 600` that follows the socket's appearance.
ensure_private_dir "$dir"

# MANDATORY. `nvim --listen` refuses to start if *anything* exists at the path
# -- live socket, stale socket or plain file -- all with the same misleading
# `address already in use` message. In the script rather than in Rust because
# std::fs cannot unlink over ssh.
rm -f "$sock"

# The nested-launch marker. Exported here it reaches the editor and every job
# the editor starts -- a `:terminal` shell included, which is exactly where
# someone would type `nvmux` again. src/nested.rs is what reads it.
#
# Here rather than over RPC once the session answers, which would reach both
# transports just as well: this is in place before nvim's first line runs, it
# cannot quietly fail the way a best-effort post-spawn call does, and it does
# not build a command out of a path -- see src/shell.rs on why that matters.
export NVMUX="$sock"

# Where the session runs. Before the `command -v` below, not after: that resolves
# a *relative* command path against the current directory, so `./nvim` would
# otherwise be looked for in the directory nvmux happened to be started in rather
# than the one the session is about to run in. Everything else this script names
# -- $dir, $sock, $log -- is absolute and does not care.
#
# Two steps rather than one `cd`, because their failures are different questions:
# a path that is not a directory (a typo, or a file) and one that is but cannot be
# entered (a mode of 000, or a component we may not traverse). Both refuse rather
# than launch somewhere else, which would be far worse than not launching: the
# session would come up, look right, and be wrong.
[ -d "$cwd" ] || fail "$cwd: not a directory"
cd -- "$cwd" || fail "$cwd: could not enter it"

# A command that is not there would otherwise be a five-second wait for a socket
# that was never going to appear, and a timeout message blaming the session. The
# shell writes its own "not found" to the log, but only the caller reads that,
# and only after giving up. `command -v` accepts a path as readily as a name.
command -v -- "$1" >/dev/null 2>&1 || fail "$1: not found"

# The redirections below create the log under the caller's mask, so its mode is
# set here rather than left to that mask. Like the socket it sits in the 0700
# directory, which is what keeps it unreadable; this keeps it private in its own
# right as well, and keeps the mode the same on every host.
: >>"$log" 2>/dev/null || true
chmod 600 "$log" 2>/dev/null || true

# Detach from the ssh session's process group, which the kernel SIGHUPs when the
# connection closes. `setsid` is cleanest but is util-linux and does not exist
# on macOS, so probe for it and fall back to `nohup`.
#
# All three file descriptors are redirected in both branches, and that is not
# hygiene: `ssh host 'cmd &'` with stdout still attached hangs the ssh client
# until its timeout. Measured at 12s versus 0.23s.
if command -v setsid >/dev/null 2>&1; then
  setsid "$@" </dev/null >>"$log" 2>&1 &
  guess=$!
else
  nohup "$@" </dev/null >>"$log" 2>&1 &
  guess=$!
  # Not a POSIX builtin, and absent in dash. `nohup` already did the work.
  disown 2>/dev/null || true
fi

# Wait for the socket to appear. Readiness is a *separate* question the caller
# settles with a deferred RPC call -- see src/rpc.rs.
sock_state=timeout
i=0
while [ "$i" -lt 100 ]; do
  if [ -S "$sock" ]; then
    # The socket's own control, and now the only one it has besides the
    # directory: never trust the caller's mask.
    chmod 600 "$sock" 2>/dev/null || true
    sock_state=ok
    break
  fi
  # `sleep 0.05` is not POSIX; fall back to a whole second rather than spin.
  sleep 0.05 2>/dev/null || sleep 1
  i=$((i + 1))
done

# Confirm the pid really is the nvim serving *this* socket before reporting it.
#
# `$!` is not necessarily nvim's pid: `setsid` execs directly only when the
# caller is not already a process group leader, and otherwise forks, leaving $!
# pointing at setsid. The pid is what a later `kill -KILL` targets, and pids get
# reused, so an unvalidated one could name an unrelated process.
pid=''
if [ -n "$guess" ] && kill -0 "$guess" 2>/dev/null; then
  if owns_socket "$guess" "$sock"; then
    pid=$guess
  fi
fi

# If the guess did not check out, find the process actually serving this socket
# rather than report something unsafe to kill.
if [ -z "$pid" ]; then
  pid=$(serving_pid "$sock")
fi

printf 'PID %s\n' "$pid"
printf 'SOCK %s\n' "$sock_state"
finish
