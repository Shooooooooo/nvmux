#!/bin/sh
# List the nvmux sessions on this host.
#
# Usage: sh list.sh <runtime_dir>
#
# Output is one TSV record per session:
#
#   S <TAB> <id> <TAB> <pid-alive: 0|1> <TAB> <one-line json>
#
# followed by a literal NVMUX_END line.
#
# Batch-shaped: one invocation returns every session, because each ssh round
# trip costs roughly 230ms even to localhost.
[ -n "${NVMUX_PRELUDE:-}" ] || . "$(dirname -- "$0")/_prelude.sh"

dir="${1:?usage: list.sh <runtime_dir>}"

# A missing directory is not an error: the answer is the empty list, and the
# terminator still proves the script ran.
if [ -d "$dir" ]; then
  for sock in "$dir"/*.sock; do
    # An unmatched glob expands to itself in POSIX sh.
    [ -e "$sock" ] || continue

    id=${sock##*/}
    id=${id%.sock}

    # The local end of every SSH forward also lives here, named
    # `<host_token>-<id>.sock`, and under `nvmux localhost` the remote listing
    # runs in this very directory. Without this guard the sweep below finds a
    # forwarded socket, sees nothing `--listen`ing on it, and deletes the live
    # forward of the session the user is attached to.
    is_session_id "$id" || continue

    json_path="$dir/$id.json"

    # An incomplete create or a half-reaped session: report it with empty
    # metadata and let nvmux decide.
    json=''
    if [ -f "$json_path" ]; then
      # One line, in case someone hand-wrote pretty-printed metadata.
      json=$(tr -d '\n\r\t' < "$json_path")
    fi

    # Asked by socket, not by the recorded pid: pids get reused, the socket is
    # the identity, and a busy session is still found because the process
    # exists whether or not it is answering.
    alive=0
    if serving "$sock"; then
      alive=1
    elif can_inspect && [ -S "$sock" ]; then
      # Stale: nvim unlinks its own socket on a clean exit, so one left behind
      # means an unclean death.
      #
      # Two guards, both required. `can_inspect` because "no process found" is
      # not evidence when we cannot look. `-S` because this deletes files, and
      # nothing is "serving" an ordinary file that happens to be named
      # `<id>.sock` either.
      purge "$dir" "$id"
      continue
    fi

    printf 'S\t%s\t%s\t%s\n' "$id" "$alive" "$json"
  done
fi

# Sweep metadata whose socket is gone: sessions are discovered through *.sock,
# so otherwise a self-exited session's <id>.json and unbounded <id>.log sit here
# forever. Safe because the socket is created before the metadata is written, so
# a .json with no .sock always means the session is over, never that it is
# starting.
if [ -d "$dir" ]; then
  for meta in "$dir"/*.json; do
    [ -e "$meta" ] || continue
    mid=${meta##*/}
    mid=${mid%.json}
    is_session_id "$mid" || continue
    if [ ! -e "$dir/$mid.sock" ]; then
      rm -f "$meta" "$dir/$mid.log"
    fi
  done
fi

finish
