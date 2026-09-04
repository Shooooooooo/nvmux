#!/bin/sh
# Spawn a detached headless Neovim that survives our disconnection.
#
# Usage: sh spawn.sh <runtime_dir> <id>
# Prints: the pid we backgrounded, as a hint (see the caveat below).
#
# POSIX sh only. Wired up in milestone 2.

set -u

dir="${1:?usage: spawn.sh <runtime_dir> <id>}"
id="${2:?usage: spawn.sh <runtime_dir> <id>}"

sock="$dir/$id.sock"
log="$dir/$id.log"

# Neovim creates its listen socket with 0777 & ~umask -- it is not 0755 by
# default. Under a permissive umask the socket would be world-writable, and any
# local user could then connect and run nvim_command("!sh") as us. The 0700
# directory is the primary control; this closes the second hole.
umask 077
mkdir -p "$dir" || exit 1
chmod 700 "$dir" 2>/dev/null || true

# MANDATORY. `nvim --listen` refuses to start if *anything* exists at the path
# -- a live socket, a stale socket, or a plain file -- and all three produce the
# identical, misleading message:
#
#   nvim: Failed to --listen: address already in use: "<path>"
#
# This unlink lives in the script rather than in Rust because on a remote host
# there is no other way to reach the file: std::fs cannot unlink over ssh.
rm -f "$sock"

# Detach from the ssh session's process group. When the connection closes the
# kernel SIGHUPs the foreground process group, and the new nvim must not be in
# it.
#
# `setsid` is the cleanest way to do that, but it is util-linux and **does not
# exist on macOS**, which is a first-class target here rather than an edge case.
# So probe for it and fall back to `nohup`.
#
# All three file descriptors are redirected in both branches. That is not
# hygiene: `ssh host 'cmd &'` with stdout still attached hangs the ssh client
# until its timeout, because ssh waits for the pipe to close, not for the
# command to exit. Measured at 12s versus 0.23s.
if command -v setsid >/dev/null 2>&1; then
  setsid nvim --headless --listen "$sock" </dev/null >>"$log" 2>&1 &
  pid=$!
else
  nohup nvim --headless --listen "$sock" </dev/null >>"$log" 2>&1 &
  pid=$!
  # Not a POSIX builtin; present in bash and zsh, absent in dash. `nohup` has
  # already done the work, so failure here is fine.
  disown 2>/dev/null || true
fi

# Wait for the socket to appear so the caller gets a useful error instead of a
# race. Readiness is a separate question -- nvim answers nvim_get_api_info
# within milliseconds while the user's init.lua is still sourcing -- so the
# caller still has to probe with a deferred RPC call before declaring success.
i=0
while [ "$i" -lt 100 ]; do
  if [ -S "$sock" ]; then
    # Belt and braces alongside the umask above: never trust the caller's mask.
    chmod 600 "$sock" 2>/dev/null || true
    break
  fi
  # `sleep 0.05` is not POSIX, but every sh we target accepts it. If it fails,
  # fall back to a whole second rather than spinning.
  sleep 0.05 2>/dev/null || sleep 1
  i=$((i + 1))
done

# A hint only. `$!` is not reliably nvim's pid: setsid execs directly only when
# the caller is not already a process group leader, and otherwise forks, making
# $! setsid's pid. nvmux overwrites this with the pid the server reports about
# itself once it is reachable.
printf '%s\n' "$pid"
