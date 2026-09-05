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
#   unknown   we could not determine what owns the socket, so nothing was
#             signalled and nothing was removed
#
# This is the whole kill. There is no graceful RPC step: nvmux does not ask a
# session to quit, it terminates it. SIGTERM first so nvim can run VimLeavePre,
# write its ShaDa file and unlink its own socket, then SIGKILL if it will not go.
#
# POSIX sh only.

set -u

dir="${1:?usage: kill.sh <runtime_dir> <id> <pid>}"
id="${2:?usage: kill.sh <runtime_dir> <id> <pid>}"
pid="${3:-}"

sock="$dir/$id.sock"

# The command line of a pid, or empty if we cannot find out.
#
# /proc first so this still works on a Linux box with no ps at all; ps is the
# fallback and the only option on macOS. `-ww` stops macOS truncating to
# terminal width, which would silently break the match for exactly the long
# socket paths where identity matters most.
cmdline() {
  if [ -r "/proc/$1/cmdline" ]; then
    tr '\0' ' ' < "/proc/$1/cmdline" 2>/dev/null
  else
    ps -ww -o args= -p "$1" 2>/dev/null
  fi
}

# Is this pid the nvim serving THIS session's socket?
#
# `grep -F` because the path is data, not a pattern: a runtime directory
# containing `.` or `[` would otherwise match the wrong process, or nothing.
owns_socket() {
  cmdline "$1" | grep -q -F -- "--listen $sock"
}

# A process that has exited but not yet been reaped.
#
# Sessions are spawned detached, so their parent is pid 1, and an init that does
# not reap promptly -- the norm inside containers -- leaves an exited nvim as a
# zombie indefinitely. `kill -0` SUCCEEDS on a zombie and its command line reads
# "[nvim] <defunct>", so without this a session that is already gone looks
# "alive but not ours". A zombie holds no socket and serves nothing.
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

# Find whatever is serving this socket.
#
# The recorded pid is only a starting guess. It was validated at spawn time, but
# that could have been days ago and pids are reused, so it is used only if it
# still owns this socket. Otherwise we search by socket, because the socket --
# not the pid -- is the session's identity.
#
# `-A` for "every process", not `-e`: on macOS `-e` means "show the
# environment". `-u` keeps the search inside our own processes, so another
# user's command line can never be matched.
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
  # SIGTERM, never SIGINT. On SIGTERM nvim exits and unlinks its own socket; on
  # SIGINT it dies from the signal and leaves the socket behind, which then
  # looks exactly like a crashed session to the next listing.
  kill -TERM "$target" 2>/dev/null

  # Generous, because this is where VimLeavePre, ShaDa writes and session files
  # happen. Cutting it short to feel responsive corrupts exit-time state.
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
  # A socket is present and we have no way to see whether anything is serving
  # it. Deleting its files on that basis could orphan a live session forever,
  # so we do nothing and say so.
  result=unknown
fi

# Remove the files only once the session is genuinely gone. Deleting them while
# nvim still runs orphans it permanently: its socket disappears from the
# listing, so nvmux can never show it, probe it, or kill it again.
if [ "$result" = killed ] || [ "$result" = absent ]; then
  rm -f "$sock" "$dir/$id.json" "$dir/$id.log"
fi

printf 'RESULT %s\n' "$result"
printf 'NVMUX_END\n'
