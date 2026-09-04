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

# Is any of our processes serving this socket?
#
# `-A` for "every process", not `-e`: on macOS `-e` means "show the
# environment". `-u` keeps the search within our own processes. `grep -F`
# because the path is data, not a pattern.
serving() {
  ps -ww -A -u "$(id -u)" -o args= 2>/dev/null \
    | grep -v grep \
    | grep -q -F -- "--listen $1"
}

# Can we inspect processes at all? If not, "not found" proves nothing.
can_inspect() {
  ps -ww -o args= -p $$ >/dev/null 2>&1
}

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

    # Is a process actually serving this socket?
    #
    # Asked by socket rather than by the pid recorded in the metadata: pids get
    # reused, and the recorded one can be stale, while the socket is the
    # session's identity. This also works unchanged over ssh, where nvmux
    # cannot connect to the socket to find out for itself.
    #
    # A busy session is still found — the process exists whether or not it is
    # answering — so this cannot mistake "compiling" for "dead".
    alive=0
    if serving "$sock"; then
      alive=1
    elif can_inspect && [ -S "$sock" ]; then
      # Nothing is serving it and we can see the process table, so the socket is
      # stale: nvim unlinks its own socket on a clean exit, and one left behind
      # means an unclean death. Sweep it.
      #
      # Two guards, both required. `can_inspect` because "no process found" is
      # not evidence when we have no way to look. `-S` because this deletes
      # files, and a *regular file* that merely happens to be named `<id>.sock`
      # must never be removed on the strength of "nothing is serving it" —
      # nothing is serving any ordinary file.
      rm -f "$sock" "$dir/$id.json" "$dir/$id.log"
      continue
    fi

    printf 'S\t%s\t%s\t%s\n' "$id" "$alive" "$json"
  done
fi

# Sweep metadata whose socket is gone.
#
# A session that exits on its own -- :qa, a crash, SIGTERM -- unlinks its own
# listen socket but nothing else. Since sessions are discovered through *.sock,
# such a session becomes invisible from that moment on, and its <id>.json and
# its unbounded <id>.log would sit in the runtime directory forever with nothing
# left that would ever look at them again.
#
# Safe because the socket is created before the metadata is written: a .json
# with no .sock always means the session is over, never that it is starting.
if [ -d "$dir" ]; then
  for meta in "$dir"/*.json; do
    [ -e "$meta" ] || continue
    mid=${meta##*/}
    mid=${mid%.json}
    if [ ! -e "$dir/$mid.sock" ]; then
      rm -f "$meta" "$dir/$mid.log"
    fi
  done
fi

printf 'NVMUX_END\n'
