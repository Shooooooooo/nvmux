#!/usr/bin/env python3
"""Regenerate the README's demo recording.

    python3 tools/record-demo.py            # writes assets/demo.gif

nvmux is driven on a pty here, and every byte it writes is recorded with the
time it was written, which becomes an asciicast for `agg` to render.

Both halves of that matter, and the reason is the fade.

**The pty answers.** nvmux dissolves between screens by interpolating toward
the terminal's own background, so at startup it asks for it (OSC 10 and 11, see
`src/palette.rs`). Nothing answers in a headless container, and an unanswered
query is not a degraded fade but no fade at all -- `fade::enabled` is false and
every transition is a hard cut. So this script answers the colour query itself,
with THEME, and hands agg that same theme so the dissolve ends on exactly the
background it renders. It answers the rest of the startup handshake too (cursor
position, device attributes, the kitty keyboard query), without which nvmux does
not start.

**The recording is the byte stream, not screenshots.** A fade is about five
repaints inside `duration_ms`, 100ms by default. Sampling the screen even 20
times a second catches two of them and turns a dissolve into a step. Recording
what nvmux actually wrote loses nothing, and agg replays it at the speed it
happened.

Driving on a pty means there is no `capture-pane` to read back, so the checks
render the stream with pyte -- through `Screen` below, since pyte has no
alternate screen of its own.

Needs, all on PATH:
  * nvim >= 0.11
  * agg          https://github.com/asciinema/agg/releases
  * python3 -m pip install pyte
  * a release build of nvmux (cargo build --release)

Everything it touches is scratch: a config under a temporary directory, and
demo sessions it creates itself. Your own nvmux sessions and config are left
alone.
"""

import codecs
import fcntl
import json
import os
import pty
import re
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
NVMUX = os.path.join(ROOT, "target", "release", "nvmux")
OUT_GIF = os.path.join(ROOT, "assets", "demo.gif")

COLS, ROWS = 100, 28

# A fade is repaints about 20ms apart, so the cap has to clear 50fps or the
# dissolve is thinned back out to the steps this script exists to avoid.
# `check_clean` samples at the same rate, since this is what a frame can show.
FPS_CAP = 60

# The sessions the picker already has when the recording opens, made off camera
# so the demo starts on a populated list rather than an empty one.
NAMES = ["api-server", "dotfiles", "notes"]

# And the one made on camera, which the rest of the take then detaches from and
# comes back to.
NEW_NAME = "scratch"

# One theme, used twice: answered to nvmux over OSC so its fade knows what it is
# dissolving into, and written into the asciicast header so agg renders with the
# same colours. They have to agree, or the dissolve ends a shade off its own
# background. agg honours the header's theme; `--theme` is not passed for that
# reason.
THEME = {
    "fg": "#cdd0d4",
    "bg": "#12131a",
    "palette": ":".join([
        "#21242b", "#e06c75", "#98c379", "#e5c07b",
        "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
        "#3a3f4b", "#e88b93", "#b3d99d", "#efd8a1",
        "#8cc6f4", "#d7a3e8", "#84ccd4", "#cdd0d4",
    ]),
}

PROMPT = "\\[\\e[2K\\]\\[\\r\\]\\[\\e[38;5;71m\\]$\\[\\e[0m\\] "

# What the keys are as bytes, now that there is no `send-keys` to name them for
# us. Ctrl-Space is NUL, which is the whole reason it works as a prefix.
ENTER = b"\r"
ESCAPE = b"\x1b"
DOWN = b"\x1b[B"
PREFIX = b"\x00"


def hex_to_osc(colour):
    """`#rrggbb` as the `rgb:rrrr/gggg/bbbb` an OSC reply carries."""
    r, g, b = (colour[1:3], colour[3:5], colour[5:7])
    return "rgb:%s%s/%s%s/%s%s" % (r, r, g, g, b, b)


class Screen:
    """A terminal screen, only as far as the checks in here need one.

    Two pyte screens with the alternate-screen sequences switching between
    them: pyte has no `?1049` of its own, so without this the picker would go
    on showing the editor's buffer after a detach, and every check that reads
    the screen afterwards would be reading the wrong one.

    `?1049` also saves the cursor on the way in and restores it on the way out,
    and that half is not a detail here. It is what puts the cursor back on the
    line a hung-up client printed its last words on, which is the line nvmux
    then erases. Model the switch without the cursor and the erase lands one
    line low, and `check_clean` reports a message that a real terminal never
    shows.
    """

    ALT = re.compile(rb"\x1b\[\?1049([hl])")

    def __init__(self, cols=COLS, rows=ROWS):
        import pyte
        self.primary = pyte.Screen(cols, rows)
        self.alt = pyte.Screen(cols, rows)
        self.streams = {
            id(self.primary): pyte.Stream(self.primary),
            id(self.alt): pyte.Stream(self.alt),
        }
        self.active = self.primary
        self.saved = None          # one slot, as a terminal has
        self.decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")

    def feed(self, data):
        pos = 0
        for m in self.ALT.finditer(data):
            self._feed(data[pos:m.end()])
            if m.group(1) == b"h":
                self.saved = (self.primary.cursor.y, self.primary.cursor.x)
                # Entering clears, as it does on a real terminal. Without this
                # the alternate screen still holds the last session's picture,
                # so a check waiting for the picker matches the *previous*
                # picker before this one has painted a cell.
                self.alt.reset()
                self.active = self.alt
            else:
                self.active = self.primary
                if self.saved is not None:
                    self.primary.cursor_position(self.saved[0] + 1,
                                                 self.saved[1] + 1)
            pos = m.end()
        self._feed(data[pos:])

    def _feed(self, chunk):
        if chunk:
            self.streams[id(self.active)].feed(self.decoder.decode(chunk))

    def lines(self):
        return [line.rstrip() for line in self.active.display]

    def text(self):
        return "\n".join(self.lines())

    def cursor(self):
        return self.active.cursor.y + 1, self.active.cursor.x + 1


class Terminal:
    """nvmux on a pty: answers what it asks, records what it writes."""

    def __init__(self, argv, env, record=False):
        self.screen = Screen()
        self.lock = threading.Lock()
        self.events = []
        self.record = record
        self.t0 = None
        self.qbuf = b""
        self.closed = False
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execvpe(argv[0], argv, env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ,
                    struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.reader = threading.Thread(target=self._read_loop, daemon=True)
        self.reader.start()

    # --- the answering half ------------------------------------------------
    def _replies(self, data):
        """Reply to the startup handshake, and to the colour query the fade
        depends on. Matched queries are cut out of the buffer so nothing is
        answered twice; a short tail is kept so a query split across two reads
        is still seen whole."""
        self.qbuf += data
        out = []

        def take(pattern, answer):
            def sub(m):
                out.append(answer(m) if callable(answer) else answer)
                return b""
            self.qbuf = re.sub(pattern, sub, self.qbuf)

        row, col = self.screen.cursor()
        take(rb"\x1b\[6n", ("\x1b[%d;%dR" % (row, col)).encode())
        take(rb"\x1b\[5n", b"\x1b[0n")
        take(rb"\x1b\[>0?q", b"\x1bP>|answering-pty(1)\x1b\\")
        take(rb"\x1b\[>0?c", b"\x1b[>0;279;0c")
        take(rb"\x1b\[0?c", b"\x1b[?62;1;2;6;9;15;22;29c")
        take(rb"\x1b\[\?u", b"\x1b[?0u")
        take(rb"\x1bP\+q([0-9A-Fa-f;]*)\x1b\\",
             lambda m: b"\x1bP0+r" + m.group(1) + b"\x1b\\")
        # The colour query: without these three the fade never runs.
        take(rb"\x1b\]10;\?(?:\x1b\\|\x07)",
             ("\x1b]10;%s\x1b\\" % hex_to_osc(THEME["fg"])).encode())
        take(rb"\x1b\]11;\?(?:\x1b\\|\x07)",
             ("\x1b]11;%s\x1b\\" % hex_to_osc(THEME["bg"])).encode())
        pal = THEME["palette"].split(":")
        take(rb"\x1b\]4;(\d+);\?(?:\x1b\\|\x07)",
             lambda m: ("\x1b]4;%s;%s\x1b\\"
                        % (m.group(1).decode(),
                           hex_to_osc(pal[int(m.group(1)) % len(pal)]))).encode())
        self.qbuf = self.qbuf[-64:]
        return out

    def _read_loop(self):
        while not self.closed:
            try:
                r, _, _ = select.select([self.fd], [], [], 0.05)
            except (OSError, ValueError):
                return
            if not r:
                continue
            try:
                chunk = os.read(self.fd, 65536)
            except OSError:
                return
            if not chunk:
                return
            now = time.time()
            with self.lock:
                self.screen.feed(chunk)
                if self.record and self.t0 is not None:
                    self.events.append((now - self.t0, chunk))
                for reply in self._replies(chunk):
                    try:
                        os.write(self.fd, reply)
                    except OSError:
                        pass

    # --- driving -----------------------------------------------------------
    def start_recording(self):
        with self.lock:
            self.t0 = time.time()
            self.events = []

    def write(self, data, wait=0.0):
        try:
            os.write(self.fd, data)
        except OSError:
            pass
        if wait:
            time.sleep(wait)

    def type(self, text, per_char=0.0, wait=0.0):
        if per_char:
            for ch in text:
                self.write(ch.encode())
                time.sleep(per_char)
        else:
            self.write(text.encode())
        if wait:
            time.sleep(wait)

    def text(self):
        with self.lock:
            return self.screen.text()

    def expect(self, needle, what, timeout=15):
        end = time.time() + timeout
        while time.time() < end:
            if needle in self.text():
                return
            time.sleep(0.1)
        self.close()
        sys.exit("timed out waiting for %s\n%s" % (what, self.text()))

    def is_selected(self, name):
        return any("\u25b8" in line and name in line
                   for line in self.text().split("\n"))

    def select(self, name, limit=6):
        """Step down to a session, and make sure that is where we landed.

        Stepping rather than jumping: it shows the picker being navigated, and
        it does not care where the selection started or what number the session
        was given. Checked before returning, because attaching to the wrong one
        would look perfectly plausible on camera.
        """
        for _ in range(limit):
            if self.is_selected(name):
                return
            self.write(DOWN, wait=0.55)
        if not self.is_selected(name):
            self.close()
            sys.exit("the selection never reached %r:\n%s" % (name, self.text()))

    def close(self):
        self.closed = True
        try:
            os.kill(self.pid, 9)
            os.waitpid(self.pid, 0)
        except (ProcessLookupError, ChildProcessError):
            pass
        try:
            os.close(self.fd)
        except OSError:
            pass


def shell(env, record=False):
    """A bash on the pty, so the recording shows `nvmux` being typed at a
    prompt rather than starting from nowhere."""
    term = Terminal(["bash", "--norc", "--noprofile"], env, record=record)
    time.sleep(1.0)
    term.type("export PS1='%s'; clear\n" % PROMPT, wait=1.2)
    return term


def make_sessions(env):
    """Create the sessions the picker already has, off camera."""
    term = shell(env)
    term.type("nvmux\n", wait=0.3)
    term.expect("attach", "the picker")
    for name in NAMES:
        term.write(b"c", wait=1.0)
        term.expect("new session name", "the create prompt")
        term.type(name, wait=0.5)
        term.write(ENTER, wait=4.0)                  # creates, then attaches
        term.write(PREFIX + b" ", wait=2.0)          # one write: the chord has
        term.expect("attach", "the picker after %s" % name)   # 500ms to land
        print("  created %s" % name)
    term.write(b"q", wait=1.0)
    term.close()


def perform(env):
    """The recorded take."""
    term = shell(env, record=True)
    term.start_recording()
    time.sleep(1.0)

    # 1. the picker, with the sessions that already exist
    term.type("nvmux", per_char=0.09, wait=0.5)
    term.write(ENTER, wait=0.2)
    term.expect("attach", "the picker")
    time.sleep(1.8)

    # 2. make one, which is `c` and a name
    term.write(b"c", wait=0.6)
    term.expect("new session name", "the create prompt")
    time.sleep(0.8)
    term.type(NEW_NAME, per_char=0.1, wait=0.9)
    term.write(ENTER, wait=0.2)

    # 3. creating attaches to it
    term.expect("[No Name]", "the new session to paint")
    time.sleep(1.6)

    term.write(b"i", wait=0.5)
    term.type("a session that outlives the connection", per_char=0.05, wait=0.7)
    term.write(ESCAPE, wait=1.4)

    # 4. detach: the session keeps running, nvmux exits
    term.write(PREFIX + b"d")
    term.expect("$", "the shell prompt back")
    time.sleep(1.8)

    # 5. come back -- the session made a moment ago is in the list now
    term.type("nvmux", per_char=0.09, wait=0.4)
    term.write(ENTER, wait=0.2)
    term.expect("attach", "the picker again")
    time.sleep(1.4)
    term.select(NEW_NAME)
    time.sleep(0.7)
    term.write(ENTER, wait=0.2)
    term.expect("outlives", "the text to still be there")
    time.sleep(2.6)

    with term.lock:
        events = list(term.events)
    term.close()
    return events


def check_clean(events):
    """Refuse to ship a recording showing the departing client's last words.

    nvmux erases that line itself (`term::erase_hung_up_clients_line`), so the
    bytes are in the stream and the screen ends up clean. Hence replaying and
    reading the *rendered* screen rather than searching the bytes, which would
    find the line nvmux already wiped.

    Sampled at the rate agg renders with, because that is what decides what a
    frame can show. Checking after every write instead reports the gap between
    the client printing the line and nvmux erasing it -- two writes about a
    third of a millisecond apart, some fifty times shorter than one frame, and
    a state nothing ever draws.
    """
    screen = Screen()
    step, i, t = 1.0 / FPS_CAP, 0, 0.0
    end = events[-1][0] if events else 0.0
    while t <= end + step:
        while i < len(events) and events[i][0] <= t:
            screen.feed(events[i][1])
            i += 1
        if "deadly signal" in screen.text():
            sys.exit("the departing client's signal message is on screen at "
                     "%.2fs, for a whole frame; nvmux is meant to erase it" % t)
        t += step


def write_cast(events, path, tail=1.2):
    decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
    with open(path, "w") as f:
        f.write(json.dumps({
            "version": 2, "width": COLS, "height": ROWS,
            "theme": THEME,
            "env": {"TERM": "xterm-256color"},
        }) + "\n")
        for t, chunk in events:
            text = decoder.decode(chunk)
            if text:
                f.write(json.dumps([round(t, 3), "o", text]) + "\n")
        if events:
            f.write(json.dumps([round(events[-1][0] + tail, 3), "o", ""]) + "\n")


def main():
    for tool in ("nvim", "agg"):
        if not shutil.which(tool):
            sys.exit("%s is not on PATH; see the header of this script" % tool)
    try:
        import pyte                                   # noqa: F401
    except ImportError:
        sys.exit("pyte is needed for the screen checks: pip install pyte")
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
    env["COLORTERM"] = "truecolor"
    env.pop("NVMUX", None)
    env.pop("NO_COLOR", None)                         # it would turn the fade off

    try:
        print("creating demo sessions...")
        make_sessions(env)

        print("recording...")
        events = perform(env)
        check_clean(events)
        print("  %d writes, %.1fs" % (len(events), events[-1][0]))

        cast = os.path.join(tmp, "demo.cast")
        write_cast(events, cast)

        os.makedirs(os.path.dirname(OUT_GIF), exist_ok=True)
        print("rendering %s ..." % OUT_GIF)
        # No --theme: the cast header carries it.
        subprocess.run(["agg", "--font-size", "15", "--fps-cap", str(FPS_CAP),
                        "--idle-time-limit", "1.5", cast, OUT_GIF], check=True)
        print("wrote %s (%.0f KB)" % (OUT_GIF, os.path.getsize(OUT_GIF) / 1024))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
        print("note: the demo's nvmux sessions are still running; "
              "remove them with `x` in the picker if you do not want them.")


if __name__ == "__main__":
    main()
