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
# Batch-shaped, like list.sh and for the same reason: an ssh round trip costs
# roughly 230ms even to localhost. One invocation answers for a whole directory,
# and the Rust side keeps the answer, so typing a path costs about one round
# trip per `/` rather than one per keystroke -- see src/ui/complete.rs.
[ -n "${NVMUX_PRELUDE:-}" ] || . "$(dirname -- "$0")/_prelude.sh"

dir="${1:?usage: dirs.sh <dir>}"

# Nothing here narrows the answer any more. Matching used to be this glob's job
# -- `"$dir"/"$prefix"*` -- but the Rust side ranks fuzzily now, and a
# subsequence match cannot be expressed as a shell pattern: the first character
# someone types need not be the first character of the name. So the whole
# directory comes back and src/ui/complete.rs scores it, which is also what lets
# one answer serve every keystroke inside a directory rather than just the ones
# that extend the same prefix.
#
# The order is the globs' -- plain names in collating order, then dotted ones --
# and the collating sequence under a UTF-8 locale is neither the byte order the
# Rust side compares in nor the same order on two hosts. C makes it one answer
# everywhere. It is no longer the order the user sees, though: the score decides
# that, and this is only the stable tiebreak beneath it.
LC_ALL=C
export LC_ALL

# The cap now matters. While the glob did the filtering it was documented as
# unreachable, because a prefix had already narrowed the directory before this
# counted anything; unfiltered, a `~/.cache` or a `/nix/store` reaches it easily.
# 2000 names is roughly 40KB on the wire -- one round trip, still cheap -- and
# `TRUNCATED` tells the caller its answer is partial so it can say so rather than
# showing a short list as if it were the whole one.
#
# Truncation is in C order, which is the wrong 2000 for a fuzzy query. That is
# the honest cost of a bound, and the alternative -- shipping the query and a
# matcher to the host -- buys a better 2000 for a great deal more machinery.
LIMIT=2000

# A literal newline, for the guard below. There is no `\n` escape a POSIX
# `case` pattern would honour.
nl='
'

n=0
# Two globs, because `*` does not match a leading dot. That asymmetry used to be
# the dotfile rule and came free with the prefix; with no prefix to carry it the
# rule moves to Rust, which hides dotted names until a query asks for one. Here
# the job is only to report everything that is there.
for entry in "$dir"/* "$dir"/.*; do
  # An unmatched glob expands to itself in POSIX sh, and a path ending in a
  # literal `*` is not a directory. The same guard covers a <dir> that is not
  # there at all: the answer is the empty list, not an error.
  [ -d "$entry" ] || continue

  name=${entry##*/}

  case "$name" in
    # `.` and `..` are matched by the second glob, and neither is ever a useful
    # completion: the field already holds the directory they name. Mandatory now
    # rather than a nicety -- with `.*` always globbed, every listing would carry
    # them otherwise.
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
