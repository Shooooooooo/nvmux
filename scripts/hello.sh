#!/bin/sh
# Open a session host: everything nvmux needs to know about it, and its
# sessions, in ONE round trip.
#
# Usage: sh hello.sh
#
# Output:
#   DIR  <runtime directory>
#   NVIM <first line of `nvim --version`, or empty if not on PATH>
#
# followed by everything list.sh prints for the directory just named, that
# script's NVMUX_END included.
#
# Two files rather than one, joined by src/shell.rs and delivered as a single
# program: the listing a connection opens with is then produced by exactly the
# code every later refresh runs, rather than by a copy of it. There is
# deliberately no `finish` here — list.sh's terminator is the one that says the
# whole thing ran, and a second one printed halfway would say a listing had
# completed when it had not even started.
#
# This runs through the user's LOGIN shell, which is the whole point of asking
# rather than assuming: `ssh host nvim` finds nothing on most real setups,
# because nvim was put on $PATH by .zprofile or .bash_profile.
#
# Assigned before it can be read, and never with a `:-` default. This script
# runs inside the user's login shell, which exports whatever their profile sets
# and whatever ssh was asked to send, so a name this script *execs* must not be
# one the environment can supply. Namespaced for the same reason.
NVMUX_STANDALONE=

if [ -z "${NVMUX_PRELUDE:-}" ]; then
  . "$(dirname -- "$0")/_prelude.sh"
  # Run from a checkout the two files are still two files, so finish the job by
  # hand; delivered over ssh they are one stream and list.sh simply follows.
  NVMUX_STANDALONE="$(dirname -- "$0")/list.sh"
fi

# Matches the Rust side's rule exactly; see src/paths.rs for why
# $XDG_RUNTIME_DIR is not consulted.
dir="/tmp/nvmux-$(id -u)"
printf 'DIR %s\n' "$dir"

if command -v nvim >/dev/null 2>&1; then
  printf 'NVIM %s\n' "$(nvim --version 2>/dev/null | head -1)"
else
  printf 'NVIM\n'
fi

[ -z "$NVMUX_STANDALONE" ] || exec sh "$NVMUX_STANDALONE" "$dir"

# What list.sh, appended below, reads as its runtime directory.
set -- "$dir"
