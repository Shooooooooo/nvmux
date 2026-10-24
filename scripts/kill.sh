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
# No graceful RPC step. SIGTERM first, so nvim runs VimLeavePre, writes ShaDa
# and unlinks its socket; SIGKILL if it will not go.
[ -n "${NVMUX_PRELUDE:-}" ] || . "$(dirname -- "$0")/_prelude.sh"

dir="${1:?usage: kill.sh <runtime_dir> <id> <pid>}"
id="${2:?usage: kill.sh <runtime_dir> <id> <pid>}"
pid="${3:-}"

sock="$dir/$id.sock"

# Exited but unreaped. Sessions are detached (parent pid 1), and an init that
# reaps slowly, common in containers, leaves zombies indefinitely. `kill -0`
# succeeds on one and its command line reads "[nvim] <defunct>", so a gone
# session would look alive but not ours.
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

# Poll for up to $2 tenths of a second, every hundredth for the first tenth
# (when most sessions exit). Fractional sleep is not POSIX: fall back to 1s.
wait_gone() {
  i=0
  while [ "$i" -lt "$(($2 * 10))" ]; do
    gone "$1" && return 0
    if [ "$i" -lt 10 ]; then
      sleep 0.01 2>/dev/null || sleep 1
      i=$((i + 1))
    else
      sleep 0.1 2>/dev/null || sleep 1
      i=$((i + 10))
    fi
  done
  gone "$1"
}

# The recorded pid is used only if it still owns this socket: pids are reused.
target=''
if [ -n "$pid" ] && [ "$pid" -gt 1 ] 2>/dev/null && ! gone "$pid" && owns_socket "$pid" "$sock"; then
  target=$pid
else
  target=$(serving_pid "$sock")
fi

if [ -n "$target" ]; then
  # Never SIGINT: nvim dies from it and leaves the socket behind, which the
  # next listing reads as a crash.
  kill -TERM "$target" 2>/dev/null

  # Generous: VimLeavePre and ShaDa writes happen here, and cutting them
  # short corrupts exit-time state.
  if ! wait_gone "$target" 100; then
    kill -KILL "$target" 2>/dev/null
    wait_gone "$target" 20
  fi

  if gone "$target"; then
    result=killed
  else
    result=orphaned
  fi
elif can_inspect || [ ! -e "$sock" ]; then
  result=absent
else
  # A socket exists and we cannot tell if anything serves it; deleting could
  # orphan a live session.
  result=unknown
fi

# Only once the session is known gone (see `purge`).
if [ "$result" = killed ] || [ "$result" = absent ]; then
  purge "$dir" "$id"
fi

printf 'RESULT %s\n' "$result"
finish
