#!/bin/sh
# The subdirectories of one directory, for the create prompt's completion.
#
# Usage: sh dirs.sh <dir> [<prefix>]
#
# Output is one record per subdirectory of <dir> whose name starts with
# <prefix>:
#
#   D <TAB> <name>
#
# followed by `TRUNCATED` if there were more than LIMIT of them, and then a
# literal NVMUX_END line.
#
# Batch-shaped, like list.sh and for the same reason: an ssh round trip costs
# roughly 230ms even to localhost. One invocation answers for a whole directory,
# and the Rust side keeps the answer, so typing a path costs about one round
# trip per `/` rather than one per keystroke -- see src/ui/complete.rs.
[ -n "${NVMUX_PRELUDE:-}" ] || . "$(dirname -- "$0")/_prelude.sh"

dir="${1:?usage: dirs.sh <dir> [<prefix>]}"
prefix="${2:-}"

# The glob's order is the output's order, and `*` sorts by the collating
# sequence -- which under a UTF-8 locale is neither the byte order the Rust side
# compares in nor the same order on two hosts. C makes it one answer everywhere.
LC_ALL=C
export LC_ALL

# A directory with a hundred thousand entries would otherwise be a hundred
# thousand lines across an ssh connection to answer one keystroke. The cap is
# generous enough that no real directory reaches it, and `TRUNCATED` tells the
# caller its answer is partial -- which matters, because a partial answer must
# not be filtered down for a longer prefix.
LIMIT=500

# A literal newline, for the guard below. There is no `\n` escape a POSIX
# `case` pattern would honour.
nl='
'

n=0
# Both halves quoted, so only the trailing `*` is a metacharacter: a prefix
# containing `*`, `?` or `[` matches literally, which is what someone typing a
# directory actually named that expects. The shell does the filtering, which is
# what makes a huge directory cheap to ask about -- and it gives the dotfile
# rule for free, since `*` does not match a leading dot but `.*` does.
for entry in "$dir"/"$prefix"*; do
  # An unmatched glob expands to itself in POSIX sh, and a path ending in a
  # literal `*` is not a directory. The same guard covers a <dir> that is not
  # there at all: the answer is the empty list, not an error.
  [ -d "$entry" ] || continue

  name=${entry##*/}

  case "$name" in
    # `.` and `..` are matched by a prefix of `.`, and neither is ever a useful
    # completion: the field already holds the directory they name.
    .|..) continue ;;
    # This output is line-framed, so a name containing a newline would break the
    # framing and produce a record the caller would read as a second directory.
    # Skipped rather than escaped: src/session.rs already refuses to render such
    # a string, so it could not be shown or typed back anyway.
    *"$nl"*) continue ;;
  esac

  if [ "$n" -ge "$LIMIT" ]; then
    printf 'TRUNCATED\n'
    break
  fi
  n=$((n + 1))

  # Tab-separated like list.sh, so a name with spaces in it survives: everything
  # after the first tab is the name.
  printf 'D\t%s\n' "$name"
done

finish
