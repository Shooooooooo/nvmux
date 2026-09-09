#!/bin/sh
# Rewrite the `num` of sessions the user reordered in the picker.
#
# Usage: sh renumber.sh <runtime_dir> <id> <json> [<id> <json>]...
#
# Output:
#   ERROR <reason>   (on a write that failed)
#   NVMUX_END
#
# Batch-shaped, like list.sh: dragging a row the length of a list would
# otherwise cost one ssh round trip per row it passed.
#
# Separate from write_meta.sh, which may *create* a session's metadata, because
# this one may only edit it. Each JSON arrives as its own positional parameter
# and is never re-parsed, so a name containing quotes, spaces or $(...) is data.

[ -n "${NVMUX_PRELUDE:-}" ] || . "$(dirname -- "$0")/_prelude.sh"

dir="${1:?usage: renumber.sh <runtime_dir> <id> <json> [<id> <json>]...}"
shift

# Say why, in the script's own words. A bare exit with nothing on stdout reaches
# the user as a diagnosis of the ssh connection instead.
fail() {
  printf 'ERROR %s\n' "$1"
  finish
  exit 1
}

while [ "$#" -ge 2 ]; do
  id="$1"
  json="$2"
  shift 2

  # An id becomes a path; nothing else in the runtime directory is ours.
  is_session_id "$id" || continue

  # Never conjure metadata for a session that has none. An orphan is a live
  # socket with no <id>.json, and the picker shows it under a name it made up on
  # the spot -- writing that here would make the placeholder real.
  [ -f "$dir/$id.json" ] || continue

  # Temp file plus rename, so a concurrent listing sees the old number or the
  # new one and never a half-written file.
  tmp="$dir/$id.json.tmp$$"
  printf '%s\n' "$json" > "$tmp" || fail "could not write metadata for $id"
  mv -f "$tmp" "$dir/$id.json" || {
    rm -f "$tmp"
    fail "could not replace metadata for $id"
  }
done

finish
