#!/bin/sh
# Helpers shared by the nvmux session-host scripts.
#
# src/shell.rs prepends this to each script, so the host gets one program on
# stdin. Run from a checkout, a script sources it when the marker below is
# unset:
#
#   sh scripts/list.sh /tmp/nvmux-$(id -u)
#
# POSIX sh only: /bin/sh is dash on Debian and Ubuntu.
NVMUX_PRELUDE=1

set -u

# Every script ends with this line; the Rust side treats its absence as an
# error. `ssh -n` with `sh -s` runs an empty script and exits 0, which would
# otherwise look like a script that had nothing to say.
finish() {
  printf 'NVMUX_END\n'
}

# Print the reason and stop. A bare exit reaches the user as an ssh connection
# problem (a shell "Permission denied" reads as an auth failure). The
# terminator still goes out: the script ran.
fail() {
  printf 'ERROR %s\n' "$1"
  finish
  exit 1
}

# Is pid $1 the nvim serving socket $2? `grep -F`: the path is data.
owns_socket() {
  cmdline "$1" | grep -q -F -- "--listen $2"
}

# Replace $1/$2.json with $3 atomically (temp file plus rename), so a
# concurrent listing never sees a half-written file. Nonzero on failure, with
# the temp file removed.
write_json() {
  nvmux_tmp="$1/$2.json.tmp$$"
  printf '%s\n' "$3" > "$nvmux_tmp" || return 1
  mv -f "$nvmux_tmp" "$1/$2.json" || { rm -f "$nvmux_tmp"; return 1; }
}

# The command line of a pid, or empty. /proc first for Linux boxes without
# ps; `-ww` stops macOS truncating long socket paths to terminal width.
cmdline() {
  if [ -r "/proc/$1/cmdline" ]; then
    tr '\0' ' ' < "/proc/$1/cmdline" 2>/dev/null
  else
    ps -ww -o args= -p "$1" 2>/dev/null
  fi
}

# The pid serving this socket, or nothing. The socket is a session's identity:
# pids are reused, paths are not.
#
# `ps` first: one call reads every command line, where a `/proc` loop forks
# twice per process (seconds on a host with a thousand). `/proc` is the
# fallback for minimal containers without `ps`. `-A`, not `-e`, which on macOS
# shows the environment. Callers test for empty output: the `awk` succeeds
# whether or not it matched.
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

# Every socket our processes listen on, read from the process table once and
# framed by newlines, so asking about each session is free. Per-session
# `serving_pid` made twelve sessions take 815ms on a 1500-process box; the
# snapshot takes 70ms whatever the session count.
NVMUX_LISTENING=
NVMUX_LISTENING_TAKEN=

take_listening() {
  [ -z "$NVMUX_LISTENING_TAKEN" ] || return 0
  NVMUX_LISTENING_TAKEN=1

  # `ps` first, as in `serving_pid`. `tr` gives `/proc` entries the
  # space-joined shape of `ps -o args=`, so one extractor reads both.
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
# Taken to end of line, not the next space: `spawn.sh` puts the socket last
# and a runtime directory may contain a space. The trailing trim is for
# `/proc`, whose command lines end in a separator. The flag is split in two
# so this `awk`'s own command line, which `ps` may catch, cannot match itself
# (what `grep -v grep` does above).
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
# A snapshot can only miss entries (odd `ps` output, a session another nvmux
# just spawned), and `list.sh` deletes files on this answer, so a miss is
# confirmed against the process table. Only for sockets: nothing else can be a
# live session, and plain files are never swept.
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

# The sockets under one directory that something has bound, read once from
# the kernel's list: 3ms, against 80ms for `serving` on a thousand-process host.
#
# Linux only, and never the last word: where the file is absent (macOS, BSD)
# or nvim is in another network namespace, this finds nothing and `serving`
# decides.
NVMUX_BOUND=
NVMUX_BOUND_TAKEN=

take_bound() {
  [ -z "$NVMUX_BOUND_TAKEN" ] || return 0
  NVMUX_BOUND_TAKEN=1
  [ -r /proc/net/unix ] || return 0
  # Field 8 to end of line, as in `listen_args`: a path may contain a space.
  # Only the one directory, so the test below stays on short strings.
  #
  # The prefix test is a negation because the POSIX scan in src/shell.rs
  # rejects a spaced `==` (a bashism in `test`, not in awk).
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

# Has anything bound this socket? A "yes" is conclusive; a "no" means
# nothing, so callers must ask `serving`, which decides if a session is gone.
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
# callers must fail closed rather than delete or report absence.
can_inspect() {
  [ -r /proc/self/cmdline ] && return 0
  ps -ww -o args= -p $$ >/dev/null 2>&1
}

# Read a file into $nvmux_flat with its newlines removed, nothing else.
#
# Replaces `tr -d '\n'`, a fork per session: 88-110 ms of a five-session
# listing where a fork costs 15 ms. Tabs and CRs are kept: the file is the
# last field of its record, so a tab splits nothing (see `parse_listing`), and
# both are JSON whitespace. A file of only newlines flattens to nothing.
flatten() {
  nvmux_flat=''
  while IFS= read -r nvmux_line || [ -n "$nvmux_line" ]; do
    nvmux_flat="$nvmux_flat$nvmux_line"
  done < "$1"
}

# Session ids only: 8 lowercase RFC 4648 base32 characters. Nothing else in the
# runtime directory is ours to touch, and an id becomes a path.
is_session_id() {
  case "$1" in
    [a-z2-7][a-z2-7][a-z2-7][a-z2-7][a-z2-7][a-z2-7][a-z2-7][a-z2-7]) return 0 ;;
    *) return 1 ;;
  esac
}

# Remove a session's three files. Only once the session is known gone:
# deleting while nvim runs orphans it, with no socket left to find.
purge() {
  rm -f "$1/$2.sock" "$1/$2.json" "$1/$2.log"
}
