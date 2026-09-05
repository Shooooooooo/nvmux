#!/bin/sh
# Terminate a session and remove its files.
#
# Usage: sh kill.sh <runtime_dir> <id> <pid>
#
# Output:
#   RESULT killed|absent|orphaned|unknown
#   NVMUX_END
#
# where:
#   killed    the process is gone and the files were removed
#   absent    nothing was serving this socket; the files were removed
#   orphaned  it was signalled and is still alive; nothing was removed
#   unknown   what owns the socket could not be determined; nothing was
#             signalled and nothing was removed
#
# There is no graceful RPC step: nvmux terminates a session rather than asking
# it to quit. SIGTERM first, so nvim runs VimLeavePre, writes its ShaDa file and
# unlinks its own socket; SIGKILL if it will not go.
#
# POSIX sh only.

set -u

dir="${1:?usage: kill.sh <runtime_dir> <id> <pid>}"
id="${2:?usage: kill.sh <runtime_dir> <id> <pid>}"
pid="${3:-}"

sock="$dir/$id.sock"

# The command line of a pid, or empty if we cannot find out. /proc first so this
# works on a Linux box with no ps at all; `-ww` stops macOS truncating to
# terminal width, which would break the match for exactly the long socket paths
# where identity matters most.
cmdline() {
  if [ -r "/proc/$1/cmdline" ]; then
    tr '\0' ' ' < "/proc/$1/cmdline" 2>/dev/null
  else
    ps -ww -o args= -p "$1" 2>/dev/null
  fi
}

# Is this pid the nvim serving THIS session's socket? `grep -F` because the path
# is data, not a pattern.
owns_socket() {
  cmdline "$1" | grep -q -F -- "--listen $sock"
}

# A process that has exited but not yet been reaped. Sessions are spawned
# detached, so their parent is pid 1, and an init that does not reap promptly --
# the norm in containers -- leaves an exited nvim as a zombie indefinitely.
# `kill -0` SUCCEEDS on a zombie and its command line reads "[nvim] <defunct>",
# so without this a session that is already gone looks "alive but not ours".
is_zombie() {
  case "$(ps -o stat= -p "$1" 2>/dev/null)" in
    Z*) return 0 ;;
    *) return 1 ;;
  esac
}

# Exited, whether or not the process table has caught up.
gone() {
  kill -0 "$1" 2>/dev/null || return 0
  is_zombie "$1"
}

# Can we inspect processes at all? If not, "no process found" is not evidence.
can_inspect() {
  [ -r "/proc/$$/cmdline" ] && return 0
  ps -ww -o args= -p $$ >/dev/null 2>&1
}

# Find whatever is serving this socket. The recorded pid is only a starting
# guess -- it was validated at spawn time, but pids are reused -- so it is used
# only if it still owns this socket. The socket, not the pid, is the identity.
#
# `-A` not `-e`: on macOS `-e` means "show the environment". `-u` keeps the
# search inside our own processes.
target=''
if [ -n "$pid" ] && [ "$pid" -gt 1 ] 2>/dev/null && ! gone "$pid" && owns_socket "$pid"; then
  target=$pid
else
  target=$(ps -ww -A -u "$(id -u)" -o pid=,args= 2>/dev/null \
           | grep -F -- "--listen $sock" \
           | grep -v grep \
           | awk 'NR==1{print $1}')
fi

if [ -n "$target" ]; then
  # SIGTERM, never SIGINT: on SIGINT nvim dies from the signal and leaves the
  # socket behind, which looks exactly like a crash to the next listing.
  kill -TERM "$target" 2>/dev/null

  # Generous: this is where VimLeavePre and ShaDa writes happen, and cutting it
  # short to feel responsive corrupts exit-time state.
  i=0
  while [ "$i" -lt 100 ]; do
    gone "$target" && break
    sleep 0.1 2>/dev/null || sleep 1
    i=$((i + 1))
  done

  if ! gone "$target"; then
    kill -KILL "$target" 2>/dev/null
    i=0
    while [ "$i" -lt 20 ]; do
      gone "$target" && break
      sleep 0.1 2>/dev/null || sleep 1
      i=$((i + 1))
    done
  fi

  if gone "$target"; then
    result=killed
  else
    result=orphaned
  fi
elif can_inspect || [ ! -e "$sock" ]; then
  result=absent
else
  # A socket is present and we cannot see whether anything serves it. Deleting
  # on that basis could orphan a live session forever.
  result=unknown
fi

# Only once the session is genuinely gone: deleting while nvim still runs
# orphans it permanently, with no socket left for any listing to find.
if [ "$result" = killed ] || [ "$result" = absent ]; then
  rm -f "$sock" "$dir/$id.json" "$dir/$id.log"
fi

printf 'RESULT %s\n' "$result"
printf 'NVMUX_END\n'
