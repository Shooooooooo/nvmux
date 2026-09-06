#!/bin/sh
# Report what nvmux needs to know about a session host, in one round trip.
#
# Usage: sh probe.sh
#
# Output:
#   DIR  <runtime directory>
#   NVIM <first line of `nvim --version`, or empty if not on PATH>
#   NVMUX_END
#
# One invocation rather than three, because every ssh round trip costs roughly
# 230ms even to localhost.
#
# This runs through the user's LOGIN shell, which is the whole point of asking
# rather than assuming: `ssh host nvim` finds nothing on most real setups,
# because nvim was put on $PATH by .zprofile or .bash_profile.
#
[ -n "${NVMUX_PRELUDE:-}" ] || . "$(dirname -- "$0")/_prelude.sh"

# Matches the Rust side's rule exactly; see src/paths.rs for why
# $XDG_RUNTIME_DIR is not consulted.
printf 'DIR /tmp/nvmux-%s\n' "$(id -u)"

if command -v nvim >/dev/null 2>&1; then
  printf 'NVIM %s\n' "$(nvim --version 2>/dev/null | head -1)"
else
  printf 'NVIM\n'
fi

finish
