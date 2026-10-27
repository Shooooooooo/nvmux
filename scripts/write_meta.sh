#!/bin/sh
# Write a session's metadata on the host that owns it.
#
# Usage: sh write_meta.sh <runtime_dir> <id> <json>
#
# Output:
#   ERROR <reason>   (on a refused runtime directory or a failed write)
#   NVMUX_END
#
# For ssh only; locally nvmux writes the file itself. The JSON is a positional
# parameter, never re-parsed.

[ -n "${NVMUX_PRELUDE:-}" ] || . "$(dirname -- "$0")/_prelude.sh"

dir="${1:?usage: write_meta.sh <runtime_dir> <id> <json>}"
id="${2:?usage: write_meta.sh <runtime_dir> <id> <json>}"
json="${3:?usage: write_meta.sh <runtime_dir> <id> <json>}"

umask 077
why=$(mkdir -p "$dir" 2>&1)
require_private_dir "$dir" || fail "could not create runtime directory $dir${why:+: $why}"

write_json "$dir" "$id" "$json" || fail "could not write metadata for $id"

finish
