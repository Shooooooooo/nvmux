#!/bin/sh
# Terminate a session and remove its files.
#
# Usage: sh kill.sh <runtime_dir> <id> <pid>
#
# Output:
#   RESULT killed|absent|refused|orphaned
#   NVMUX_END
#
# where:
#   killed    the process is gone and the files were removed
#   absent    there was no such process; the files were removed
#   refused   the pid did not belong to this session, so nothing was signalled
#             and NOTHING was removed
#   orphaned  we signalled it and it is still alive; nothing was removed
#
# The graceful path -- an RPC nvim_command("qa!") -- is nvmux's job, because only
# it can speak msgpack. By the time this runs, that has either already happened
# or was not possible. This is the escalation.
#
# POSIX sh only.

set -u

dir="${1:?usage: kill.sh <runtime_dir> <id> <pid>}"
id="${2:?usage: kill.sh <runtime_dir> <id> <pid>}"
pid="${3:-}"

sock="$dir/$id.sock"

# Is this pid really the nvim serving THIS session's socket?
#
# The pid was validated when the session was spawned, but that could have been
# days ago, and pids are reused. Re-checking here is the difference between
# terminating a session and terminating whatever unrelated process inherited
# that number in the meantime.
#
# `-o args=` is the POSIX spelling and `-ww` stops macOS truncating to terminal
# width. If `ps` is missing or unusable we must not guess: signalling on an
# unverified pid is exactly the mistake this guards against.
owns_socket() {
  ps -ww -o args= -p "$1" 2>/dev/null | grep -q -F -- "--listen $sock"
}

# A process that has exited but not yet been reaped by its parent.
#
# This matters more than it sounds. Sessions are spawned detached, so their
# parent is pid 1, and an init that does not reap promptly -- which is the norm
# inside containers -- leaves the exited nvim as a zombie for an unbounded time.
# `kill -0` SUCCEEDS on a zombie and `ps -o args=` reports "[nvim] <defunct>",
# so a naive liveness-then-ownership check concludes "alive, but not ours" and
# refuses to clean up a session that is in fact already gone.
#
# A zombie holds no socket and serves nothing, so it counts as dead.
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

result=absent

if [ -n "$pid" ] && [ "$pid" -gt 1 ] 2>/dev/null \
   && kill -0 "$pid" 2>/dev/null && ! is_zombie "$pid"; then
  if owns_socket "$pid"; then
    # SIGTERM, never SIGINT. On SIGTERM nvim exits and unlinks its own socket;
    # on SIGINT it dies from the signal and leaves the socket behind, which then
    # looks exactly like a crashed session to the next listing.
    kill -TERM "$pid" 2>/dev/null

    # Generous, because this is where VimLeavePre, ShaDa writes and session
    # files happen. Cutting it short to feel responsive corrupts exit-time
    # state, which is the opposite of what a session manager is for.
    i=0
    while [ "$i" -lt 100 ]; do
      gone "$pid" && break
      sleep 0.1 2>/dev/null || sleep 1
      i=$((i + 1))
    done

    if ! gone "$pid"; then
      kill -KILL "$pid" 2>/dev/null
      i=0
      while [ "$i" -lt 20 ]; do
        gone "$pid" && break
        sleep 0.1 2>/dev/null || sleep 1
        i=$((i + 1))
      done
    fi

    if gone "$pid"; then
      result=killed
    else
      result=orphaned
    fi
  else
    # The pid is live but belongs to something else, or ps could not tell us.
    # Removing the files now would strand a possibly-running nvim with no
    # socket for nvmux to ever find again, so we touch nothing.
    result=refused
  fi
fi

# Only remove the files once the session is genuinely gone. Deleting them while
# nvim still runs orphans it permanently: its socket disappears from the
# listing, so nvmux can never show it, probe it, or kill it again.
if [ "$result" = killed ] || [ "$result" = absent ]; then
  rm -f "$sock" "$dir/$id.json" "$dir/$id.log"
fi

printf 'RESULT %s\n' "$result"
printf 'NVMUX_END\n'
