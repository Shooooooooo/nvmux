#!/bin/sh
# Spawn a detached headless Neovim that survives our disconnection.
#
# Usage: sh spawn.sh <runtime_dir> <id>
#
# Output:
#   PID <pid>        the *validated* pid of the new nvim, or empty if we could
#                    not confirm it (see the safety note below)
#   SOCK ok|timeout  whether the listen socket appeared
#   NVMUX_END
#
# POSIX sh only -- this runs under whatever /bin/sh the session host has, which
# on Debian and Ubuntu is dash, not bash.

set -u

dir="${1:?usage: spawn.sh <runtime_dir> <id>}"
id="${2:?usage: spawn.sh <runtime_dir> <id>}"

sock="$dir/$id.sock"
log="$dir/$id.log"

# Neovim creates its listen socket with 0777 & ~umask, so under a permissive
# umask it would be world-writable. The 0700 directory is the primary control;
# this closes the second hole.
umask 077

refuse() {
  printf 'ERROR %s\n' "$1"
  printf 'NVMUX_END\n'
  exit 1
}

if [ -e "$dir" ] || [ -L "$dir" ]; then
  # An existing directory must be OURS and private. Over ssh this script is the
  # only check there is, and /tmp is world-writable, so another user could have
  # created it first.
  #
  # `ls -ldn` for both tests: POSIX `test` has no "-owned-by-me" operator,
  # `stat` is spelled differently on macOS and Linux, and `find -perm /mode`
  # (GNU) and `-perm +mode` (BSD) are not both accepted anywhere. `-n` gives
  # the owner as a uid, so it compares with `id -u` without a name lookup. A
  # check that cannot be made fails CLOSED: an empty answer is a refusal, not
  # a pass.
  info=$(ls -ldn "$dir" 2>/dev/null)
  [ -n "$info" ] || refuse "could not inspect runtime directory $dir"
  mode=$(printf '%s\n' "$info" | cut -c1-10)
  owner=$(printf '%s\n' "$info" | awk '{print $3}')
  case "$mode" in
    d*) ;;
    *) refuse "runtime directory $dir is not a directory (or is a symlink)" ;;
  esac
  [ "$owner" = "$(id -u)" ] || refuse "runtime directory $dir is not owned by us"
  # Columns 5-10 are the group and other permissions; anything but dashes there
  # is a bit we would never have set.
  case "$(printf '%s\n' "$mode" | cut -c5-10)" in
    ------) ;;
    *) refuse "runtime directory $dir is accessible to other users" ;;
  esac
else
  mkdir -p "$dir" || exit 1
  # Only a directory we just created; tightening someone else's is worse than
  # refusing to use it.
  chmod 700 "$dir" 2>/dev/null || true
fi

# MANDATORY. `nvim --listen` refuses to start if *anything* exists at the path
# -- live socket, stale socket or plain file -- all with the same misleading
# `address already in use` message. In the script rather than in Rust because
# std::fs cannot unlink over ssh.
rm -f "$sock"

# Detach from the ssh session's process group, which the kernel SIGHUPs when the
# connection closes. `setsid` is cleanest but is util-linux and does not exist
# on macOS, so probe for it and fall back to `nohup`.
#
# All three file descriptors are redirected in both branches, and that is not
# hygiene: `ssh host 'cmd &'` with stdout still attached hangs the ssh client
# until its timeout. Measured at 12s versus 0.23s.
if command -v setsid >/dev/null 2>&1; then
  setsid nvim --headless --listen "$sock" </dev/null >>"$log" 2>&1 &
  guess=$!
else
  nohup nvim --headless --listen "$sock" </dev/null >>"$log" 2>&1 &
  guess=$!
  # Not a POSIX builtin, and absent in dash. `nohup` already did the work.
  disown 2>/dev/null || true
fi

# Wait for the socket to appear. Readiness is a *separate* question the caller
# settles with a deferred RPC call -- see src/rpc.rs.
sock_state=timeout
i=0
while [ "$i" -lt 100 ]; do
  if [ -S "$sock" ]; then
    # Alongside the umask above: never trust the caller's mask.
    chmod 600 "$sock" 2>/dev/null || true
    sock_state=ok
    break
  fi
  # `sleep 0.05` is not POSIX; fall back to a whole second rather than spin.
  sleep 0.05 2>/dev/null || sleep 1
  i=$((i + 1))
done

# Confirm the pid really is the nvim serving *this* socket before reporting it.
#
# `$!` is not necessarily nvim's pid: `setsid` execs directly only when the
# caller is not already a process group leader, and otherwise forks, leaving $!
# pointing at setsid. The pid is what a later `kill -KILL` targets, and pids get
# reused, so an unvalidated one could name an unrelated process.
#
# `-o args=` is the POSIX spelling, `-ww` stops macOS truncating to terminal
# width, and `grep -F` because the socket path is data, not a pattern.
pid=''
if [ -n "$guess" ] && kill -0 "$guess" 2>/dev/null; then
  if ps -ww -o args= -p "$guess" 2>/dev/null | grep -q -F -- "--listen $sock"; then
    pid=$guess
  fi
fi

# If the guess did not check out, find the process actually serving this socket
# rather than report something unsafe to kill. `-A` not `-e`: on macOS `-e`
# means "show the environment". `-u` restricts it to our own processes.
if [ -z "$pid" ]; then
  pid=$(ps -ww -A -u "$(id -u)" -o pid=,args= 2>/dev/null \
        | grep -F -- "--listen $sock" \
        | grep -v grep \
        | awk 'NR==1{print $1}')
fi

printf 'PID %s\n' "$pid"
printf 'SOCK %s\n' "$sock_state"
printf 'NVMUX_END\n'
