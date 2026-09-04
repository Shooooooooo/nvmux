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

# Neovim creates its listen socket with 0777 & ~umask -- it is not 0755 by
# default. Under a permissive umask the socket would be world-writable, and any
# local user could connect and run nvim_command("!sh") as us. The 0700 directory
# is the primary control; this closes the second hole.
umask 077
if [ -d "$dir" ]; then
  # An existing directory must be OURS and private before we put a socket in
  # it. Locally nvmux has already checked this, but over ssh this script is the
  # only check there is, and `mkdir -p` plus a best-effort chmod would happily
  # accept a directory another user created first. /tmp is world-writable, so
  # that is not a theoretical concern.
  #
  # `find` is used for the ownership test because POSIX `test` has no
  # "-owned-by-me" operator and `stat` is spelled differently on macOS and Linux.
  if [ -L "$dir" ] || [ -n "$(find "$dir" -maxdepth 0 ! -user "$(id -u)" 2>/dev/null)" ]; then
    printf 'ERROR %s\n' "runtime directory $dir is not owned by us"
    printf 'NVMUX_END\n'
    exit 1
  fi
  if [ -n "$(find "$dir" -maxdepth 0 -perm /077 2>/dev/null)" ]; then
    printf 'ERROR %s\n' "runtime directory $dir is accessible to other users"
    printf 'NVMUX_END\n'
    exit 1
  fi
else
  mkdir -p "$dir" || exit 1
  # Only ever chmod a directory we just created; silently tightening someone
  # else's is worse than refusing to use it.
  chmod 700 "$dir" 2>/dev/null || true
fi

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
  guess=$!
else
  nohup nvim --headless --listen "$sock" </dev/null >>"$log" 2>&1 &
  guess=$!
  # Not a POSIX builtin; present in bash and zsh, absent in dash. `nohup` has
  # already done the work, so failure here is fine.
  disown 2>/dev/null || true
fi

# Wait for the socket to appear. Readiness is a *separate* question -- nvim
# answers nvim_get_api_info within milliseconds while the user's init.lua is
# still sourcing -- so the caller still probes with a deferred RPC call before
# declaring the session ready.
sock_state=timeout
i=0
while [ "$i" -lt 100 ]; do
  if [ -S "$sock" ]; then
    # Belt and braces alongside the umask above: never trust the caller's mask.
    chmod 600 "$sock" 2>/dev/null || true
    sock_state=ok
    break
  fi
  # `sleep 0.05` is not POSIX, but every sh we target accepts it. If it does
  # not, fall back to a whole second rather than spinning.
  sleep 0.05 2>/dev/null || sleep 1
  i=$((i + 1))
done

# Confirm the pid really is the nvim serving *this* socket before reporting it.
#
# `$!` is the pid the shell backgrounded, which is not necessarily nvim's:
# `setsid` execs directly only when the caller is not already a process group
# leader, and otherwise forks, leaving $! pointing at setsid. That is the safe
# direction to be wrong in only if we check, because the pid is what a later
# `kill -KILL` would target -- and pids get reused. An unvalidated pid could
# name an entirely unrelated process by then.
#
# `-o args=` is the POSIX spelling (`-o command=` is a BSD/GNU alias), and `-ww`
# stops macOS truncating the output to terminal width, which would silently
# break the match for exactly the long socket paths where it matters.
# `grep -F` because the socket path is data, not a pattern: a runtime directory
# containing `.` or `[` would otherwise make this match the wrong process, or
# nothing at all.
pid=''
if [ -n "$guess" ] && kill -0 "$guess" 2>/dev/null; then
  if ps -ww -o args= -p "$guess" 2>/dev/null | grep -q -F -- "--listen $sock"; then
    pid=$guess
  fi
fi

# If the guess did not check out, look for the process actually serving this
# socket rather than reporting something unsafe to kill.
#
# `-A` for "every process", not `-e`: on macOS `-e` means "show the
# environment", so the scan would silently cover only this terminal's processes
# and usually find nothing. `-u` restricts it to our own processes, so another
# user's command line can never be matched.
if [ -z "$pid" ]; then
  pid=$(ps -ww -A -u "$(id -u)" -o pid=,args= 2>/dev/null \
        | grep -F -- "--listen $sock" \
        | grep -v grep \
        | awk 'NR==1{print $1}')
fi

printf 'PID %s\n' "$pid"
printf 'SOCK %s\n' "$sock_state"
printf 'NVMUX_END\n'
