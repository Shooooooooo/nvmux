#!/bin/sh
# Open a session host: what nvmux needs to know about it, and its sessions, in
# one round trip.
#
# Usage: sh hello.sh
#
# Output:
#   DIR  <runtime directory>
#   HOME <the home directory of the user these sessions run as>
#   NVIM <first line of `nvim --version`, or empty if not on PATH>
#
# followed by everything list.sh prints for the directory just named, that
# script's NVMUX_END included.
#
# src/shell.rs joins this with list.sh, so the first listing runs the same code
# as every refresh. No `finish` here: list.sh's terminator must be the only
# one, or the listing would read as complete before it started.
#
# Runs in the user's login shell, which is what puts nvim on $PATH
# (.zprofile, .bash_profile).
#
# Assigned before use and never with a `:-` default: the login shell's
# environment is arbitrary, and this name is exec'd. Namespaced for the same
# reason.
NVMUX_STANDALONE=

if [ -z "${NVMUX_PRELUDE:-}" ]; then
  . "$(dirname -- "$0")/_prelude.sh"
  # From a checkout the two files are separate, so run list.sh by hand.
  NVMUX_STANDALONE="$(dirname -- "$0")/list.sh"
fi

# Matches the Rust side; see src/paths.rs for why $XDG_RUNTIME_DIR is unused.
dir="/tmp/nvmux-$(id -u)"
printf 'DIR %s\n' "$dir"

# The default cwd for new sessions and what a leading `~` expands to, asked of
# the host since its accounts are not ours. `$HOME`, not the password database,
# so it matches the user's own shell. Empty if unset or relative: the Rust side
# then offers no default rather than a guessed path.
case "${HOME:-}" in
  /*) printf 'HOME %s\n' "$HOME" ;;
  *) printf 'HOME\n' ;;
esac

if command -v nvim >/dev/null 2>&1; then
  printf 'NVIM %s\n' "$(nvim --version 2>/dev/null | head -1)"
else
  printf 'NVIM\n'
fi

[ -z "$NVMUX_STANDALONE" ] || exec sh "$NVMUX_STANDALONE" "$dir"

# What list.sh, appended below, reads as its runtime directory.
set -- "$dir"
