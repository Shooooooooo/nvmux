#!/bin/sh
# Helpers shared by the nvmux session-host scripts.
#
# Prepended to each script by src/shell.rs, so what runs on the session host is
# one self-contained program delivered on stdin. Running a script straight from
# a checkout still works — each one sources this file when the marker below is
# unset, which is the case exactly when it was not prepended:
#
#   sh scripts/list.sh /tmp/nvmux-$(id -u)
#
# POSIX sh only -- /bin/sh is dash on Debian and Ubuntu, not bash.
NVMUX_PRELUDE=1

set -u

# Every script ends with this line, and its absence is an error on the Rust
# side. `ssh -n` combined with `sh -s` silently produces an *empty* result --
# stdin is /dev/null, sh reads an empty script and exits 0 -- which is otherwise
# indistinguishable from a script that ran and had nothing to say.
finish() {
  printf 'NVMUX_END\n'
}

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

# The pid serving this socket, or nothing. The socket, not the pid, is a
# session's identity: pids are reused, paths are not.
#
# `ps` first, not `/proc`: `ps` reads every process's command line in one call,
# in C. The obvious `/proc` translation — loop over `/proc/[0-9]*/cmdline`,
# `tr` and `grep` each one — instead forks two processes per entry in
# `/proc`, so listing sessions on a box with a thousand processes (a shared
# host, not even an unusual one) means two thousand-odd forks to answer "is
# anything listening on this socket", multiple seconds before the picker's
# first frame. `/proc` remains the fallback for the one case `ps` cannot
# cover — a minimal container with `/proc` mounted and no `ps` installed —
# where the process count is typically small enough that the per-entry forking
# does not hurt. `-A` not `-e`: on macOS `-e` means "show the environment".
# `grep -F` because the path is data, not a pattern. Callers test for empty
# output rather than the exit status, since the `awk` below succeeds whether
# or not it matched.
serving_pid() {
  if command -v ps >/dev/null 2>&1; then
    ps -ww -A -u "$(id -u)" -o pid=,args= 2>/dev/null \
      | grep -F -- "--listen $1" \
      | grep -v grep \
      | awk 'NR==1{print $1}'
    return
  fi

  if [ -r /proc/self/cmdline ]; then
    for c in /proc/[0-9]*/cmdline; do
      if tr '\0' ' ' < "$c" 2>/dev/null | grep -q -F -- "--listen $1"; then
        c=${c#/proc/}
        printf '%s\n' "${c%/cmdline}"
        return 0
      fi
    done
    return 1
  fi
}

# Every socket a process of ours is listening on, read from the process table
# **once** and held here framed by newlines, so that asking about one session
# costs nothing at all.
#
# `serving` used to be `serving_pid` per session, and `serving_pid` reads the
# whole process table -- so a listing read it once per session, and the picker's
# first frame cost time proportional to sessions times processes. Measured on a
# 1500-process box: twelve sessions took 815ms and one took 77ms. With a single
# snapshot the same listing takes 70ms whatever the session count.
NVMUX_LISTENING=
NVMUX_LISTENING_TAKEN=

take_listening() {
  [ -z "$NVMUX_LISTENING_TAKEN" ] || return 0
  NVMUX_LISTENING_TAKEN=1

  # `ps` first, and `/proc` only where there is no `ps`, for the reason
  # `serving_pid` gives. `tr` renders each `/proc` entry in the same
  # space-joined shape `ps -o args=` produces, so one extractor reads both.
  if command -v ps >/dev/null 2>&1; then
    NVMUX_LISTENING=$(ps -ww -A -u "$(id -u)" -o args= 2>/dev/null | listen_args)
  elif [ -r /proc/self/cmdline ]; then
    NVMUX_LISTENING=$(for c in /proc/[0-9]*/cmdline; do
      tr '\0' ' ' < "$c" 2>/dev/null
      printf '\n'
    done | listen_args)
  fi

  NVMUX_LISTENING="
$NVMUX_LISTENING
"
}

# The `--listen` argument of every command line on stdin, one per line.
#
# Taken to the end of the line rather than to the next space: `spawn.sh` runs
# `nvim --headless --listen "$sock"` with the socket last, and a runtime
# directory containing a space would otherwise be cut in half. The trailing
# trim is for `/proc`, whose command lines end in a separator.
#
# The flag is spelled in two pieces so that this `awk`'s own command line --
# which the `ps` beside it in the pipeline may well have caught -- cannot match
# itself. That is the job the `grep -v grep` above does.
listen_args() {
  awk '
    BEGIN { flag = " --lis" "ten " }
    {
      at = index($0, flag)
      if (at > 0) {
        path = substr($0, at + length(flag))
        sub(/[ 	]*$/, "", path)
        if (path != "") print path
      }
    }'
}

# Is anything serving this socket?
#
# A snapshot can only ever be missing an entry -- a `ps` that formatted
# something unexpectedly, a session spawned by another nvmux a moment ago -- so
# a miss is confirmed against the process table before it is believed. That
# keeps the answer this returns, which is the answer `list.sh` hides sessions
# and deletes files on, exactly the answer nvmux gave before the snapshot
# existed; a live session, the case every listing is mostly made of, costs
# nothing.
#
# Only for a socket, though. Neovim serves a session by binding one, so nothing
# else can be a live session however the snapshot was read -- and a plain file
# left in the runtime directory is never swept, so confirming that one would buy
# nothing and cost a second read of the process table on every listing forever.
serving() {
  take_listening
  case "$NVMUX_LISTENING" in
    *"
$1
"*) return 0 ;;
  esac
  [ -S "$1" ] || return 1
  [ -n "$(serving_pid "$1")" ]
}

# The sockets under one directory that something has actually bound, read from
# the kernel's own list of them, once.
#
# `serving` makes `ps` copy out every process's whole command line so that a
# shell can pick a few paths back out -- 80ms on a thousand-process host, and it
# grows with the host rather than with the sessions. This file is 3ms. A session
# *is* an nvim with its socket bound, so this is the more direct question as
# well as the cheaper one; `--listen` in a command line is evidence about it.
#
# Linux only, and never the last word. Where the file is absent (macOS, BSD), or
# where the session's nvim is in a different network namespace from the shell
# reading it, this finds nothing and `serving` answers exactly as it did before.
NVMUX_BOUND=
NVMUX_BOUND_TAKEN=

take_bound() {
  [ -z "$NVMUX_BOUND_TAKEN" ] || return 0
  NVMUX_BOUND_TAKEN=1
  [ -r /proc/net/unix ] || return 0
  # Field 8 to the end of the line, for the reason `listen_args` reads to the
  # end of its line: a runtime directory with a space in it is one path, not
  # two. Restricted to the one directory so the test below stays a test on a
  # short string however much else the host has bound.
  #
  # The prefix test is spelled as a negation because the POSIX scan in
  # src/shell.rs rejects a spaced `==`, which in `test` is a bashism and in awk
  # is not. Nothing else is meant by it.
  NVMUX_BOUND=$(awk -v dir="$1" '
    NF > 7 {
      path = substr($0, index($0, $8))
      if (index(path, dir) != 1) next
      print path
    }' /proc/net/unix)
  NVMUX_BOUND="
$NVMUX_BOUND
"
}

# Has anything bound this socket?
#
# A "yes" is conclusive and costs one read of one small file for a whole
# listing. A "no" means nothing at all -- callers must ask `serving`, which is
# what decides whether a session is gone.
bound() {
  take_bound "${1%/*}/"
  case "$NVMUX_BOUND" in
    *"
$1
"*) return 0 ;;
  esac
  return 1
}

# Can we inspect processes at all? If not, "nothing found" proves nothing, and
# every caller has to fail closed rather than delete or report absence.
can_inspect() {
  [ -r /proc/self/cmdline ] && return 0
  ps -ww -o args= -p $$ >/dev/null 2>&1
}

# A tab and a carriage return, for the one place that has to remove them.
#
# Empty until `flatten` needs it, and never read from the environment: this file
# runs inside the user's login shell, so a name it *uses* must be one it set.
NVMUX_BLANKS=

# Read a file into $nvmux_flat as one line, with newlines, tabs and carriage
# returns removed.
#
# `tr -d '\n\r\t' < "$file"` says this in one word, and costs a process for
# every session in the listing. That is the whole of what a listing scales by:
# on a host where a fork is a millisecond it is not worth the trouble, and on
# one where a fork is fifteen it was 88-110 ms of a listing of five sessions.
# This is the same work with one fork for the whole run, and none at all for
# the scripts that never call it.
#
# The splitting is how a POSIX shell deletes characters without a process:
# field-split the line on tab and carriage return, then join the fields with
# nothing. `set --` is safe here because a function has positional parameters of
# its own, and the argument is saved before it is clobbered. `set -f` is not
# optional: without it a `*` in the metadata would be expanded against the
# runtime directory.
flatten() {
  [ -n "$NVMUX_BLANKS" ] || NVMUX_BLANKS=$(printf '\t\r')
  nvmux_flat=''
  nvmux_path=$1
  while IFS= read -r nvmux_line || [ -n "$nvmux_line" ]; do
    nvmux_ifs=$IFS
    IFS=$NVMUX_BLANKS
    set -f
    # Deliberately unquoted: this is the split.
    # shellcheck disable=SC2086
    set -- $nvmux_line
    IFS=''
    nvmux_flat="$nvmux_flat$*"
    set +f
    IFS=$nvmux_ifs
  done < "$nvmux_path"
}

# Session ids only: 8 lowercase RFC 4648 base32 characters. Nothing else in the
# runtime directory is ours to touch, and an id becomes a path.
is_session_id() {
  case "$1" in
    [a-z2-7][a-z2-7][a-z2-7][a-z2-7][a-z2-7][a-z2-7][a-z2-7][a-z2-7]) return 0 ;;
    *) return 1 ;;
  esac
}

# Remove a session's three files. Only ever called once the session is known to
# be gone: deleting while nvim still runs orphans it permanently, with no socket
# left for any listing to find.
purge() {
  rm -f "$1/$2.sock" "$1/$2.json" "$1/$2.log"
}
