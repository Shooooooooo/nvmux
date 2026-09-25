#!/bin/sh
# Put the relay (scripts/relay.lua) in place of this shell.
#
# Usage: sh boot.sh <secret> <hash>
#
# The first thing written to the `sh -s` that ssh starts through the user's
# login shell, when nvmux reaches the host through the relay rather than
# through a ControlMaster -- see src/mux.rs. Written raw, and not in the frame
# every other script runs in: it ends by `exec`ing the relay, which then owns
# this shell's stdin and stdout for the rest of the connection, and in a frame
# it would be a subshell with its stdin taken from /dev/null.
#
# src/shell.rs joins the relay's source on where @RELAY@ stands, so this runs
# as it is only once assembled; `<secret>` is the other end's, and `<hash>`
# names the relay's source.
#
# Output, once the login shell has said whatever it says:
#   <empty line>
#   NVMUX_BOOT_<secret> <first line of `nvim --version`, or nothing>
# and then the relay's own first line, from the Neovim that replaced this
# shell. With no `nvim` on $PATH the line ends after the secret and the shell
# exits; with a runtime directory that is not ours it never comes, and
#   ERROR <why>
#   NVMUX_END
# is printed instead.
[ -n "${NVMUX_PRELUDE:-}" ] || . "$(dirname -- "$0")/_prelude.sh"

usage='usage: boot.sh <secret> <hash>'
nvmux_secret="${1:?$usage}"
nvmux_hash="${2:?$usage}"

# The directory every session on this host is in, checked the way spawn.sh
# checks it: the relay's source is written here and then run, so a directory
# another user could write to is one they could have us run their Lua from.
nvmux_dir="/tmp/nvmux-$(id -u)"
ensure_private_dir "$nvmux_dir"

# Named for what is in it, so a newer nvmux that ships a different relay
# writes a file of its own rather than running an older one's, and one that
# ships the same relay finds it already there and writes nothing.
nvmux_relay="$nvmux_dir/relay-$nvmux_hash.lua"
if [ ! -f "$nvmux_relay" ]; then
  nvmux_tmp="$nvmux_relay.tmp$$"
  if ! cat > "$nvmux_tmp" <<'NVMUX_RELAY_EOF'
@RELAY@
NVMUX_RELAY_EOF
  then
    rm -f "$nvmux_tmp"
    fail "could not write $nvmux_relay"
  fi
  chmod 600 "$nvmux_tmp" 2>/dev/null || true
  mv -f "$nvmux_tmp" "$nvmux_relay" || { rm -f "$nvmux_tmp"; fail "could not write $nvmux_relay"; }
fi

# Said before the relay starts, so that a Neovim too old to run it -- or none
# at all -- is reported as that rather than as a relay that never answered.
if command -v nvim >/dev/null 2>&1; then
  nvmux_banner=$(nvim --version 2>/dev/null | head -1)
else
  nvmux_banner=''
fi
printf '\nNVMUX_BOOT_%s %s\n' "$nvmux_secret" "$nvmux_banner"
[ -n "$nvmux_banner" ] || exit 0

# `--clean`: none of the user's configuration, plugins or shada. The relay is
# nvmux's, and runs as nothing else.
exec nvim --clean --headless -l "$nvmux_relay" "$nvmux_secret"
