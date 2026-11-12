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

# Zero if runtime directory $1 is ours and private, nonzero if missing, else
# `fail`. Once it passes, /tmp's sticky bit keeps it ours; a missing one anyone
# can create right after, so a caller that reads or writes there must stop.
#
# `ls -ldn`: `test` cannot check an owner, and `stat` and `find -perm` differ
# between macOS and Linux. An empty answer is a refusal.
require_private_dir() {
  [ -e "$1" ] || [ -L "$1" ] || return 1
  nvmux_dir=$1
  nvmux_info=$(ls -ldn "$nvmux_dir" 2>/dev/null)
  [ -n "$nvmux_info" ] || fail "could not inspect runtime directory $nvmux_dir"
  # Split in the shell, not by `cut` and `awk`: this runs on every listing.
  # `-f`: the name at the end of the line is not a pattern.
  set -f
  set -- $nvmux_info
  set +f
  case "$1" in
    d*) ;;
    *) fail "runtime directory $nvmux_dir is not a directory (or is a symlink)" ;;
  esac
  [ "${3:-}" = "$(id -u)" ] || fail "runtime directory $nvmux_dir is not owned by us"
  # Characters 5-10 are the group and other bits. Only setgid and sticky (S,
  # T) may show, as in src/paths.rs: they let nobody in. An ACL or xattr
  # marker may follow.
  case "$1" in
    d???--[-S]--[-T]*) ;;
    *) fail "runtime directory $nvmux_dir is accessible to other users" ;;
  esac
}

# Is pid $1 the nvim serving socket $2? By `serving_pid`'s rule, in the shell:
# a `grep` would put the flag and path on a command line `serving_pid` reads.
# Quoted: the path is data, not a pattern.
owns_socket() {
  ours "$1" || return 1
  case " $(cmdline "$1") " in
    *" --listen $2 "*) return 0 ;;
  esac
  return 1
}

# Replace $1/$2.json with $3 atomically (temp file plus rename), so a
# concurrent listing never sees a half-written file. Nonzero on failure, with
# the temp file removed.
#
# `$$` shows in `ps`, so the temp name is guessable. `set -C` refuses anything
# already there except a FIFO or device (or a symlink to one), which only the
# private directory keeps out.
write_json() {
  nvmux_tmp="$1/$2.json.tmp$$"
  set -C
  printf '%s\n' "$3" > "$nvmux_tmp"
  nvmux_rc=$?
  set +C
  [ "$nvmux_rc" -eq 0 ] || { rm -f "$nvmux_tmp"; return 1; }
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

# The effective uid of a pid, unsigned as `id -u` prints it; fails if there is
# no such process. Linux owns /proc/<pid> by it, unlike the files inside, which
# a process that is not dumpable gives to root. macOS's `ps` prints a uid past
# 2^31-1 negative (`%d`).
uid_of() {
  if [ -r /proc/self/cmdline ]; then
    [ -d "/proc/$1" ] || return 1
    set -- $(ls -ldn "/proc/$1" 2>/dev/null)
    [ -n "${3:-}" ] && printf '%s\n' "$3"
  else
    # Parenthesized: BWK awk, macOS's, takes no bare `<` in printf's arguments.
    set -- $(ps -o uid= -p "$1" 2>/dev/null |
      awk '{ printf "%.0f\n", ($1 < 0 ? $1 + 4294967296 : $1) }')
    [ -n "${1:-}" ] && printf '%s\n' "$1"
  fi
}

# Is this pid ours? Only our processes can be sessions. Anyone's can name our
# socket: session ids show in `ps`.
ours() {
  nvmux_owner=$(uid_of "$1") && [ "$nvmux_owner" = "$(id -u)" ]
}

# The pid serving this socket, or nothing.
#
# The first process of ours whose command line has `--listen`, this path, then
# a space or its end: `a.sock` is not `a.sock2`. The socket is a session's
# identity: pids are reused, paths are not.
#
# `ps` first: one call reads every command line, where a `/proc` loop forks
# per process (seconds on a host with a thousand). `/proc` is the fallback for
# minimal containers without `ps`. `-A`, not `-e`, which on macOS shows the
# environment. One `-o` per keyword: FreeBSD and NetBSD read all of an `-o`
# argument after its first `=` as that column's header. The uid is matched in
# the `awk`: procps lists the union of `-A` and `-u`, every process. It is
# compared modulo 2^32, as procps and macOS print one past 2^31-1 negative
# (`%d`) and `id -u` unsigned. Callers test for empty output: the `awk`
# succeeds whether or not it matched.
#
# The flag is split, so the `awk`'s own command line, which `ps` may catch,
# cannot match. Nor can the shell's: nvmux runs `sh -s`, arguments on stdin.
# `owns_socket` starts no process that could: it matches in the shell. The
# path comes from the environment because `-v` would process its backslashes.
# Greps are skipped: one for the socket (a user's `pgrep -f`, say) names it
# too, and no session's argv[0] is a grep.
#
# Only lines naming the socket are split or matched: macOS's awk exits when a
# regex meets a byte its locale cannot decode, which its `ps` prints raw, and
# mawk 1.3.3 when a line splits into more than 32767 fields.
serving_pid() {
  if command -v ps >/dev/null 2>&1; then
    ps -ww -A -o uid= -o pid= -o args= 2>/dev/null \
      | nvmux_sock=$1 awk -v uid="$(id -u)" '
          BEGIN { want = " --lis" "ten " ENVIRON["nvmux_sock"] " " }
          !index($0 " ", want) || ($1 < 0 ? $1 + 4294967296 : $1) != uid { next }
          $3 !~ /(^|\/)[efp]?grep$/ { print $2; exit }'
    return
  fi

  if [ -r /proc/self/cmdline ]; then
    for c in /proc/[0-9]*/cmdline; do
      nvmux_args=$(tr '\0' ' ' 2>/dev/null < "$c")
      case "/${nvmux_args%% *}" in
        */grep | */egrep | */fgrep | */pgrep) continue ;;
      esac
      case " $nvmux_args " in
        *" --listen $1 "*)
          c=${c#/proc/}
          c=${c%/cmdline}
          ours "$c" || continue
          printf '%s\n' "$c"
          return 0
          ;;
      esac
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

  # `ps` first, as in `serving_pid`; `proc_listening` gives `/proc` the shape
  # of its lines, so one extractor reads both.
  if command -v ps >/dev/null 2>&1; then
    NVMUX_LISTENING=$(ps -ww -A -o uid= -o args= 2>/dev/null | listen_args)
  elif [ -r /proc/self/cmdline ]; then
    NVMUX_LISTENING=$(proc_listening | listen_args)
  fi

  NVMUX_LISTENING="
$NVMUX_LISTENING
"
}

# Each /proc command line that names a `--listen`, as a `ps -o uid= -o args=`
# line: the uid, then the command line joined by `tr`, newlines included, or
# an argument holding one could start a line with our uid. Not inline in a
# `$( )`: bash 3.2, macOS's sh, ends that at a `case` pattern's `)`.
proc_listening() {
  for c in /proc/[0-9]*/cmdline; do
    nvmux_args=$(tr '\0\n' '  ' 2>/dev/null < "$c")
    case $nvmux_args in
      *" --listen "*)
        c=${c#/proc/}
        nvmux_owner=$(uid_of "${c%/cmdline}") &&
          printf '%s %s\n' "$nvmux_owner" "$nvmux_args"
        ;;
    esac
  done
}

# The `--listen` argument of each command line of ours on stdin, where every
# line is a uid and then a command line. The uid is compared as in
# `serving_pid`.
#
# Taken to end of line, not the next space: `spawn.sh` puts the socket last
# and a runtime directory may contain a space. The trailing trim is for
# `/proc`, whose command lines end in a separator. The flag is split in two
# so this `awk`'s own command line, which `ps` may catch, cannot match itself.
listen_args() {
  awk -v uid="$(id -u)" '
    BEGIN { flag = " --lis" "ten " }
    {
      at = index($0, flag)
      if (!at || ($1 < 0 ? $1 + 4294967296 : $1) != uid) next
      path = substr($0, at + length(flag))
      sub(/[ 	]*$/, "", path)
      if (path != "") print path
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
