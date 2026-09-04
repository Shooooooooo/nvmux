#!/bin/sh
# Terminate a session and remove its files.
#
# Usage: sh kill.sh <runtime_dir> <id> <pid>
#
# The graceful path -- an RPC nvim_command("qa!") -- is nvmux's job, because
# only it can speak msgpack. By the time this script runs, that has either
# already happened or was not possible. This is the escalation.
#
# POSIX sh only. Wired up in milestone 2.

set -u

dir="${1:?usage: kill.sh <runtime_dir> <id> <pid>}"
id="${2:?usage: kill.sh <runtime_dir> <id> <pid>}"
pid="${3:-}"

sock="$dir/$id.sock"

if [ -n "$pid" ] && [ "$pid" -gt 1 ] 2>/dev/null; then
  # SIGTERM, never SIGINT. On SIGTERM nvim exits and unlinks its own socket; on
  # SIGINT it dies from the signal and leaves the socket file behind, which then
  # looks exactly like a crashed session to the next listing.
  if kill -TERM "$pid" 2>/dev/null; then
    i=0
    while [ "$i" -lt 30 ]; do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.1 2>/dev/null || sleep 1
      i=$((i + 1))
    done
    # Grace expired.
    kill -0 "$pid" 2>/dev/null && kill -KILL "$pid" 2>/dev/null
  fi
fi

rm -f "$sock" "$dir/$id.json" "$dir/$id.log"
printf 'NVMUX_END\n'
