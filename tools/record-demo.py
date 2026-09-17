#!/usr/bin/env python3
"""Regenerate the README's demo recording.

    python3 tools/record-demo.py            # writes assets/demo.gif

nvmux is a full-screen program that asks the terminal questions on startup
(cursor position, device attributes), so it cannot be driven from a bare pipe
-- something has to answer. This script uses tmux as that terminal: nvmux runs
inside a detached tmux session, `send-keys` types at it, and `capture-pane`
screenshots the pane on a fixed tick. The frames become an asciicast, which
`agg` renders to a GIF.

Needs, all on PATH:
  * tmux
  * nvim >= 0.11
  * agg          https://github.com/asciinema/agg/releases
  * a release build of nvmux (cargo build --release)

Everything it touches is scratch: a config under a temporary directory, a tmux
server on its own socket, and demo sessions it creates and kills itself. Your
own nvmux sessions and config are left alone.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
NVMUX = os.path.join(ROOT, "target", "release", "nvmux")
OUT_GIF = os.path.join(ROOT, "assets", "demo.gif")

COLS, ROWS = 100, 28
FPS = 20
SOCKET = "nvmux-demo"          # a tmux server of our own, not the user's
SESSION = "rec"
NAMES = ["api-server", "dotfiles", "scratch", "notes"]

# `keys.timeout_ms` defaults to 500ms: the two halves of a prefix chord have to
# reach nvmux inside that window, so they go in a single send-keys call.
PREFIX = "C-Space"

# A green `$`, preceded by an erase-line and a carriage return.
#
# The erase is not load-bearing any more: `term::erase_hung_up_clients_line`
# wipes the line a retired client printed its `Caught deadly signal 'SIGHUP'` on,
# so nvmux leaves a clean screen whatever prompt follows it. It stays because
# every prompt worth imitating does this -- zsh, starship, powerlevel10k and
# fish all erase their line before drawing -- and `bash --norc --noprofile` with
# a bare `PS1='$ '` is unusually bare for a recording meant to look like a
# terminal someone uses. `check_clean` below holds the result to it either way.
#
# `\[ \]` marks both sequences zero-width, so bash still counts columns right.
PROMPT = "\\[\\e[2K\\]\\[\\r\\]\\[\\e[38;5;71m\\]$\\[\\e[0m\\] "


def tmux(*args, capture=False):
    cmd = ["tmux", "-L", SOCKET, *args]
    if capture:
        return subprocess.run(cmd, capture_output=True, text=True).stdout
    subprocess.run(cmd, capture_output=True)


def keys(*ks, wait=0.0):
    tmux("send-keys", "-t", SESSION, *ks)
    if wait:
        time.sleep(wait)


def literal(text, wait=0.0, per_char=0.0):
    if per_char:
        for ch in text:
            tmux("send-keys", "-t", SESSION, "-l", ch)
            time.sleep(per_char)
    else:
        tmux("send-keys", "-t", SESSION, "-l", text)
    if wait:
        time.sleep(wait)


def pane():
    return tmux("capture-pane", "-t", SESSION, "-p", capture=True)


def expect(needle, what, timeout=10):
    end = time.time() + timeout
    while time.time() < end:
        if needle in pane():
            return
        time.sleep(0.2)
    sys.exit("timed out waiting for %s\n%s" % (what, pane()))


def grab():
    """One screenshot, as a full-screen repaint."""
    out = tmux("capture-pane", "-t", SESSION, "-e", "-p", capture=True)
    lines = out.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    lines = lines[:ROWS] + [""] * max(0, ROWS - len(lines))
    return "\x1b[H\x1b[2J" + "\r\n".join(l + "\x1b[0m" for l in lines)


class Recorder(threading.Thread):
    """Screenshot on a tick, keeping only the frames that changed."""

    def __init__(self):
        super().__init__(daemon=True)
        self.frames, self.stop_flag = [], False

    def run(self):
        t0, last = time.time(), None
        while not self.stop_flag:
            frame = grab()
            if frame != last:
                self.frames.append((time.time() - t0, frame))
                last = frame
            time.sleep(1.0 / FPS)

    def stop(self):
        self.stop_flag = True
        self.join(timeout=2)


def start_pane(env, command=None):
    """A fresh tmux server, because a running one hands new sessions its own
    environment rather than the one passed here."""
    tmux("kill-server")
    time.sleep(0.8)
    argv = ["tmux", "-L", SOCKET, "new-session", "-d", "-s", SESSION,
            "-x", str(COLS), "-y", str(ROWS)]
    argv += [command] if command else ["bash", "--norc", "--noprofile"]
    subprocess.run(argv, env=env, check=True)
    time.sleep(1.5)


def make_sessions(env):
    """Create the sessions the demo browses, off camera."""
    start_pane(env, command=NVMUX)
    expect("attach", "the picker")
    for name in NAMES:
        keys("c", wait=1.2)
        expect("new session name", "the create prompt")
        literal(name, wait=0.5)
        keys("Enter", wait=4.0)                 # creates, then attaches
        tmux("send-keys", "-t", SESSION, PREFIX, "Space")
        time.sleep(2.0)
        expect("attach", "the picker after creating %s" % name)
        print("  created %s" % name)
    keys("q", wait=1.0)


def perform():
    """The recorded take."""
    rec = Recorder()
    rec.start()
    time.sleep(1.0)

    literal("nvmux", per_char=0.09, wait=0.5)
    keys("Enter", wait=0.2)
    expect("attach", "the picker")
    time.sleep(1.6)

    keys("Down", wait=0.65)
    keys("Down", wait=1.1)                      # land on `scratch`

    keys("Enter", wait=0.2)
    expect("[No Name]", "nvim to paint")
    time.sleep(1.6)

    keys("i", wait=0.5)
    literal("a session that outlives the connection", per_char=0.055, wait=0.7)
    keys("Escape", wait=1.4)

    tmux("send-keys", "-t", SESSION, PREFIX, "d")   # detach
    expect("$", "the shell prompt back")
    time.sleep(1.8)

    literal("nvmux", per_char=0.09, wait=0.4)    # and come straight back to it
    keys("Enter", wait=0.2)
    expect("attach", "the picker again")
    time.sleep(1.5)
    keys("Down", wait=0.6)
    keys("Down", wait=1.0)
    keys("Enter", wait=0.2)
    expect("outlives", "the text to still be there")
    time.sleep(2.6)

    rec.stop()
    return rec.frames


def check_clean(frames):
    """Refuse to ship a recording carrying the departing client's last words.

    See PROMPT: the prompt is what covers that line, so a frame still holding it
    means the cover failed. Checked across every frame rather than once at the
    end, because at 20fps the message can be caught merely flashing between
    Neovim printing it and bash redrawing over it.
    """
    stray = [round(t, 1) for t, frame in frames if "deadly signal" in frame]
    if stray:
        sys.exit("the departing client's signal message reached the recording "
                 "at %ss; the prompt is meant to erase it" % stray)


def write_cast(frames, path, tail=1.2):
    with open(path, "w") as f:
        f.write(json.dumps({"version": 2, "width": COLS, "height": ROWS,
                            "env": {"TERM": "xterm-256color"}}) + "\n")
        for t, frame in frames:
            f.write(json.dumps([round(t, 3), "o", frame]) + "\n")
        if frames:
            f.write(json.dumps([round(frames[-1][0] + tail, 3), "o", ""]) + "\n")


def main():
    for tool in ("tmux", "nvim", "agg"):
        if not shutil.which(tool):
            sys.exit("%s is not on PATH; see the header of this script" % tool)
    if not os.path.exists(NVMUX):
        sys.exit("build nvmux first: cargo build --release")

    tmp = tempfile.mkdtemp(prefix="nvmux-demo-")
    cfg = os.path.join(tmp, "config.toml")
    with open(cfg, "w") as f:
        f.write('[keys]\nprefix = "Ctrl-Space"\n')

    env = os.environ.copy()
    env["PATH"] = os.path.join(ROOT, "target", "release") + os.pathsep + env["PATH"]
    env["NVMUX_CONFIG"] = cfg
    env["TERM"] = "xterm-256color"
    env["PS1"] = PROMPT

    try:
        print("creating demo sessions...")
        make_sessions(env)

        print("recording...")
        start_pane(env)
        tmux("send-keys", "-t", SESSION, "-l",
             "export PS1='%s'; clear" % PROMPT)
        keys("Enter", wait=1.0)
        frames = perform()
        check_clean(frames)

        cast = os.path.join(tmp, "demo.cast")
        write_cast(frames, cast)
        print("  %d frames, %.1fs" % (len(frames), frames[-1][0]))

        os.makedirs(os.path.dirname(OUT_GIF), exist_ok=True)
        print("rendering %s ..." % OUT_GIF)
        subprocess.run(["agg", "--font-size", "15", "--theme", "asciinema",
                        "--fps-cap", str(FPS), "--idle-time-limit", "1.5",
                        cast, OUT_GIF], check=True)
        print("wrote %s (%.0f KB)" % (OUT_GIF, os.path.getsize(OUT_GIF) / 1024))
    finally:
        # Only this script's tmux server goes; the user's is on another socket.
        tmux("kill-server")
        shutil.rmtree(tmp, ignore_errors=True)
        print("note: the demo's nvmux sessions are still running; "
              "remove them with `x` in the picker if you do not want them.")


if __name__ == "__main__":
    main()
