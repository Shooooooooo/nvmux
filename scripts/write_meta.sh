#!/bin/sh
# Write a session's metadata on the host that owns it.
#
# Usage: sh write_meta.sh <runtime_dir> <id> <json>
#
# Output:
#   NVMUX_END
#
# Locally nvmux writes this file itself; over ssh this script is the only way to
# reach it. The JSON arrives as a positional parameter and is never re-parsed,
# so a name containing quotes, spaces or $(...) is data, not code.

[ -n "${NVMUX_PRELUDE:-}" ] || . "$(dirname -- "$0")/_prelude.sh"

dir="${1:?usage: write_meta.sh <runtime_dir> <id> <json>}"
id="${2:?usage: write_meta.sh <runtime_dir> <id> <json>}"
json="${3:?usage: write_meta.sh <runtime_dir> <id> <json>}"

umask 077
mkdir -p "$dir" 2>/dev/null || true

# Temp file plus rename, so a concurrent listing sees the old name or the new
# one and never a half-written file. rename(2) within one directory is atomic.
tmp="$dir/$id.json.tmp$$"
printf '%s\n' "$json" > "$tmp" || exit 1
mv -f "$tmp" "$dir/$id.json" || { rm -f "$tmp"; exit 1; }

finish
