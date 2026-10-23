#!/bin/sh
# The subdirectories of one directory, for the create prompt's completion.
#
# Usage: sh dirs.sh <dir>
#
# Output is one record per subdirectory of <dir>, dotted ones included:
#
#   D <TAB> <name>
#
# followed by `TRUNCATED` if there were more than LIMIT of them, and then a
# literal NVMUX_END line.
#
# One invocation per directory (an ssh round trip is ~230ms); the Rust side
# keeps the answer, so typing a path costs one round trip per `/`.
[ -n "${NVMUX_PRELUDE:-}" ] || . "$(dirname -- "$0")/_prelude.sh"

dir="${1:?usage: dirs.sh <dir>}"

# The whole directory comes back unfiltered: src/ui/complete.rs ranks it
# fuzzily, which no shell pattern can express. C collation makes the glob
# order (only a tiebreak under the score) the same on every host.
LC_ALL=C
export LC_ALL

# A `~/.cache` or `/nix/store` reaches this easily. 2000 names is about 40KB,
# still one cheap round trip; `TRUNCATED` lets the caller say the list is
# partial. Truncation is in C order, not by fuzzy score.
LIMIT=2000

# A literal newline: a POSIX `case` pattern has no `\n` escape.
nl='
'

n=0
# Two globs, because `*` does not match a leading dot. Rust decides when to
# show dotted names.
for entry in "$dir"/* "$dir"/.*; do
  # An unmatched glob expands to itself, not a directory. A missing <dir>
  # thus yields the empty list, not an error.
  [ -d "$entry" ] || continue

  name=${entry##*/}

  case "$name" in
    # Matched by `.*`, and never useful completions.
    .|..) continue ;;
    # A newline would break the line framing. Skipped, not escaped:
    # src/session.rs refuses to render such a name anyway.
    *"$nl"*) continue ;;
  esac

  if [ "$n" -ge "$LIMIT" ]; then
    printf 'TRUNCATED\n'
    break
  fi
  n=$((n + 1))

  # Tab-separated like list.sh: everything after the first tab is the name.
  printf 'D\t%s\n' "$name"
done

finish
