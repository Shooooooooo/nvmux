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
# The terminator is load-bearing, not decoration. `ssh -n` combined with
# `sh -s` silently produces an *empty* result: stdin comes from /dev/null, sh
# reads an empty script, and exits 0. Without a terminator that is
# indistinguishable from "this host has no sessions", so nvmux would quietly
# show an empty picker instead of reporting a broken connection. The parser
# treats a missing NVMUX_END as an error.
#
# Batch-shaped deliberately: one invocation returns every session. Each ssh
# round trip costs roughly 230ms even to localhost, so a per-session query
# would feel fine in local testing and be unusable over a real link.
#
# POSIX sh only -- this runs under whatever /bin/sh the remote host has, which
# on Debian and Ubuntu is dash, not bash.

set -u

dir="${1:?usage: list.sh <runtime_dir>}"

# A missing directory is not an error: it means no sessions have been created
# on this host yet, or a /tmp reaper removed it. Either way, the answer is the
# empty list, and the terminator still proves the script ran.
if [ -d "$dir" ]; then
  for sock in "$dir"/*.sock; do
    # An unmatched glob expands to itself in POSIX sh.
    [ -e "$sock" ] || continue

    id=${sock##*/}
    id=${id%.sock}
    json_path="$dir/$id.json"

    # A socket with no metadata is an incomplete create or a half-reaped
    # session. Report it with empty metadata and let nvmux decide; deleting
    # things from a listing operation would be a surprise.
    json=''
    if [ -f "$json_path" ]; then
      # Flatten to one line so the record stays parseable even if someone
      # hand-wrote pretty-printed metadata.
      json=$(tr -d '\n\r\t' < "$json_path")
    fi

    # A cheap prefilter only. The pid is a hint: pids get reused, and this says
    # nothing about whether nvim is actually serving its socket. nvmux decides
    # liveness with a real RPC round trip; this just lets the picker grey out
    # obviously dead rows without paying for a probe per session.
    alive=0
    pid=$(printf '%s' "$json" | sed -n 's/.*"pid"[[:space:]]*:[[:space:]]*\([0-9]*\).*/\1/p')
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      alive=1
    fi

    printf 'S\t%s\t%s\t%s\n' "$id" "$alive" "$json"
  done
fi

printf 'NVMUX_END\n'
