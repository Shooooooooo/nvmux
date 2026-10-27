#!/bin/sh
# List the nvmux sessions on this host.
#
# Usage: sh list.sh <runtime_dir>
#
# Output is one TSV record per session:
#
#   S <TAB> <id> <TAB> <pid-alive: 0|1> <TAB> <one-line json>
#
# followed by a literal NVMUX_END line. A runtime directory that is not ours
# and private gets `ERROR <reason>` instead of records.
#
# One invocation returns every session: each ssh round trip costs ~230ms even
# to localhost.
[ -n "${NVMUX_PRELUDE:-}" ] || . "$(dirname -- "$0")/_prelude.sh"

dir="${1:?usage: list.sh <runtime_dir>}"

# A missing directory is not an error: the answer is the empty list, and the
# terminator still proves the script ran. Given at once, without looking
# again: one created since the check is unchecked.
if ! require_private_dir "$dir"; then
  finish
  exit 0
fi

for sock in "$dir"/*.sock; do
  # An unmatched glob expands to itself in POSIX sh.
  [ -e "$sock" ] || continue

  id=${sock##*/}
  id=${id%.sock}

  # SSH forwards' local ends (`<host_token>-<id>.sock`) live here too, and
  # under `nvmux localhost` the remote listing runs in this directory: without
  # this guard the sweep would delete the attached session's live forward.
  is_session_id "$id" || continue

  json_path="$dir/$id.json"

  # An incomplete create or a half-reaped session: report it with empty
  # metadata and let nvmux decide.
  json=''
  if [ -f "$json_path" ]; then
    # One line, in case of hand-written pretty-printed metadata.
    flatten "$json_path"
    json=$nvmux_flat
  fi

  # By socket, not recorded pid: pids are reused, and a busy session's
  # process exists even when it is not answering. `bound` is cheap and its
  # "yes" final; only a "no" (also any host it cannot answer on) reaches the
  # process table.
  alive=0
  if bound "$sock" || serving "$sock"; then
    alive=1
  elif can_inspect && [ -S "$sock" ]; then
    # Stale: nvim unlinks its socket on a clean exit. Both guards matter:
    # "no process found" is no evidence without `can_inspect`, and `-S` keeps
    # an ordinary file named `<id>.sock` from being deleted.
    purge "$dir" "$id"
    continue
  fi

  printf 'S\t%s\t%s\t%s\n' "$id" "$alive" "$json"
done

# Sweep metadata whose socket is gone; otherwise a self-exited session's
# <id>.json and unbounded <id>.log stay forever. Safe because the socket is
# created before the metadata, so a .json without a .sock means the session is
# over.
for meta in "$dir"/*.json; do
  [ -e "$meta" ] || continue
  mid=${meta##*/}
  mid=${mid%.json}
  is_session_id "$mid" || continue
  if [ ! -e "$dir/$mid.sock" ]; then
    rm -f "$meta" "$dir/$mid.log"
  fi
done

finish
