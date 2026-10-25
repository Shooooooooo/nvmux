#!/bin/sh
# Rewrite the `num` of sessions the user reordered in the picker.
#
# Usage: sh renumber.sh <runtime_dir> <id> <json> [<id> <json>]...
#
# Output:
#   ERROR <reason>   (on a refused runtime directory or a failed write)
#   NVMUX_END
#
# Batch-shaped: dragging a row down a list would otherwise cost one ssh round
# trip per row passed.
#
# Separate from write_meta.sh because this may only edit metadata, never
# create it. Each JSON is its own positional parameter, never re-parsed.

[ -n "${NVMUX_PRELUDE:-}" ] || . "$(dirname -- "$0")/_prelude.sh"

dir="${1:?usage: renumber.sh <runtime_dir> <id> <json> [<id> <json>]...}"
shift

# Missing: no metadata to edit, and one created since the check is unchecked.
if ! require_private_dir "$dir"; then
  finish
  exit 0
fi

while [ "$#" -ge 2 ]; do
  id="$1"
  json="$2"
  shift 2

  # An id becomes a path; nothing else in the runtime directory is ours.
  is_session_id "$id" || continue

  # Never create metadata: an orphan (live socket, no <id>.json) shows under a
  # made-up name, and writing it here would make that placeholder real.
  [ -f "$dir/$id.json" ] || continue

  write_json "$dir" "$id" "$json" || fail "could not write metadata for $id"
done

finish
