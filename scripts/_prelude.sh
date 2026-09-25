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

# Say why, in the script's own words, and stop. A bare exit with nothing on
# stdout reaches the user as a diagnosis of the ssh connection instead -- and a
# "Permission denied" from the shell on stderr reads there as an authentication
# failure. The terminator still goes out: the script ran, and said this.
fail() {
  printf 'ERROR %s\n' "$1"
  finish
  exit 1
}

# Make sure $1 is a directory only we can use -- ours, private to us, and not a
# symlink -- creating it if it is not there. Fails the script otherwise.
#
# An existing directory must be OURS and private. Over ssh the scripts are the
# only check there is, and /tmp is world-writable, so another user could have
# created it first; a socket in it is then theirs to connect to, and a file in it
# -- the relay's source, see boot.sh -- theirs to replace before it runs.
#
# `ls -ldn` for both tests: POSIX `test` has no "-owned-by-me" operator, `stat`
# is spelled differently on macOS and Linux, and `find -perm /mode` (GNU) and
# `-perm +mode` (BSD) are not both accepted anywhere. `-n` gives the owner as a
# uid, so it compares with `id -u` without a name lookup. A check that cannot be
# made fails CLOSED: an empty answer is a refusal, not a pass.
ensure_private_dir() {
  if [ -e "$1" ] || [ -L "$1" ]; then
    nvmux_info=$(ls -ldn "$1" 2>/dev/null)
    [ -n "$nvmux_info" ] || fail "could not inspect runtime directory $1"
    nvmux_mode=$(printf '%s\n' "$nvmux_info" | cut -c1-10)
    nvmux_owner=$(printf '%s\n' "$nvmux_info" | awk '{print $3}')
    case "$nvmux_mode" in
      d*) ;;
      *) fail "runtime directory $1 is not a directory (or is a symlink)" ;;
    esac
    [ "$nvmux_owner" = "$(id -u)" ] || fail "runtime directory $1 is not owned by us"
    # Columns 5-10 are the group and other permissions; anything but dashes
    # there is a bit we would never have set.
    case "$(printf '%s\n' "$nvmux_mode" | cut -c5-10)" in
      ------) ;;
      *) fail "runtime directory $1 is accessible to other users" ;;
    esac
  else
    mkdir -p "$1" || fail "could not create runtime directory $1"
    # Only a directory we just created; tightening someone else's is worse than
    # refusing to use it.
    chmod 700 "$1" 2>/dev/null || true
  fi
}

# Is pid $1 the nvim serving socket $2? `grep -F` because the path is data, not
# a pattern.
owns_socket() {
  cmdline "$1" | grep -q -F -- "--listen $2"
}

# Replace $1/$2.json with $3: temp file plus rename, so a concurrent listing
# sees the old contents or the new and never a half-written file. rename(2)
# within one directory is atomic. Nonzero if either step failed, with the temp
# file gone.
write_json() {
  nvmux_tmp="$1/$2.json.tmp$$"
  printf '%s\n' "$3" > "$nvmux_tmp" || return 1
  mv -f "$nvmux_tmp" "$1/$2.json" || { rm -f "$nvmux_tmp"; return 1; }
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

# Read a file into $nvmux_flat as one line: its newlines removed, nothing else.
#
# `tr -d '\n' < "$file"` says this in one word, and costs a process for every
# session in the listing. That is the whole of what a listing scales by: on a
# host where a fork is a millisecond it is not worth the trouble, and on one
# where a fork is fifteen it was 88-110 ms of a listing of five sessions. This
# is the same work with no process at all.
#
# Tabs and carriage returns used to go too, and spelling them cost a `printf`
# fork per run — the one fork the shell nvmux keeps still paid for every
# script. Neither needs removing: the flattened file is the last field of its
# record, so a tab inside it splits nothing (see `parse_listing`), and both
# are whitespace to a JSON parser. A file that is only newlines flattens to
# nothing, which is what an empty file does.
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

# Remove a session's three files. Only ever called once the session is known to
# be gone: deleting while nvim still runs orphans it permanently, with no socket
# left for any listing to find.
purge() {
  rm -f "$1/$2.sock" "$1/$2.json" "$1/$2.log"
}
