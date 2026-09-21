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

**The recording is the byte stream, not screenshots.** A fade is about seven
repaints inside one direction -- half of `duration_ms`, so 100ms by default.
Sampling the screen even 20 times a second catches two of them and turns a
dissolve into a step. Recording what nvmux actually wrote loses nothing, and
agg replays it at the speed it happened.

Driving on a pty means there is no `capture-pane` to read back, so the checks
render the stream with pyte -- through `Screen` below, since pyte has no
alternate screen of its own.

Needs, all on PATH:
  * nvim >= 0.11
  * agg >= 1.9   https://github.com/asciinema/agg/releases -- for `--renderer`,
                 which the notice box needs; see the note where agg is run
  * python3 -m pip install pyte
  * a release build of nvmux (cargo build --release)

The GIF is drawn in JetBrainsMono Nerd Font Mono. If it is not installed it is
fetched once into `tools/.fonts`, which is gitignored; NVMUX_DEMO_FONT_DIR
overrides where to look.

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

# The font the GIF is drawn in. The `Mono` cut of the Nerd Font, not the plain
# one: its icons are squeezed into a single cell, and a double-width glyph in a
# character grid would push a whole row out of line.
#
# agg falls back through a family list silently, so a missing font does not fail
# -- it quietly renders a different-looking GIF. `ensure_font` is what stops
# that being discovered later.
FONT_NAME = "JetBrainsMono Nerd Font Mono"
FONT_FAMILY = FONT_NAME + ",JetBrains Mono,DejaVu Sans Mono"
FONT_CACHE = os.path.join(ROOT, "tools", ".fonts")
FONT_FILES = ["JetBrainsMonoNerdFontMono-%s.ttf" % cut
              for cut in ("Regular", "Bold", "Italic", "BoldItalic")]
FONT_URL = ("https://github.com/ryanoasis/nerd-fonts/releases/latest/download"
            "/JetBrainsMono.tar.xz")

PROMPT = "\\[\\e[2K\\]\\[\\r\\]\\[\\e[38;5;71m\\]$\\[\\e[0m\\] "

# What the keys are as bytes, now that there is no `send-keys` to name them for
# us. Ctrl-Space is NUL, which is the whole reason it works as a prefix.
ENTER = b"\r"
ESCAPE = b"\x1b"
DOWN = b"\x1b[B"
PREFIX = b"\x00"
# Ctrl-L, which is readline's clear-screen: it reprints PS1 and echoes nothing
# of its own. `shell` uses it to put the prompt into the recording.
REDRAW = b"\x0c"


def font_installed():
    """Whether fontconfig already knows the family, so nothing need be fetched."""
    if not shutil.which("fc-list"):
        return False
    listed = subprocess.run(["fc-list", ":", "family"],
                            capture_output=True, text=True).stdout
    return FONT_NAME in listed


def ensure_font():
    """The directory to hand agg with `--font-dir`, or None if it is installed.

    The four cuts are cached under `tools/.fonts` rather than committed: they
    are 10MB, which is not what a repository this size should carry for a file
    regenerated a few times a year. Set NVMUX_DEMO_FONT_DIR to point somewhere
    else and nothing is downloaded.
    """
    override = os.environ.get("NVMUX_DEMO_FONT_DIR")
    if override:
        return override
    if all(os.path.exists(os.path.join(FONT_CACHE, f)) for f in FONT_FILES):
        return FONT_CACHE
    if font_installed():
        return None
    if not shutil.which("curl"):
        sys.exit("%s is not installed and curl is missing to fetch it.\n"
                 "Install the font, or put these in %s:\n  %s\nfrom %s"
                 % (FONT_NAME, FONT_CACHE, "\n  ".join(FONT_FILES), FONT_URL))

    print("fetching %s ..." % FONT_NAME)
    os.makedirs(FONT_CACHE, exist_ok=True)
    tar = os.path.join(FONT_CACHE, "JetBrainsMono.tar.xz")
    if subprocess.run(["curl", "-fsSL", "-o", tar, FONT_URL]).returncode != 0:
        sys.exit("could not download %s; fetch it by hand into %s"
                 % (FONT_URL, FONT_CACHE))
    # Only the four cuts agg asks for, out of the ninety-odd in the archive.
    subprocess.run(["tar", "-C", FONT_CACHE, "-xJf", tar] + FONT_FILES,
                   check=True)
    os.remove(tar)
    return FONT_CACHE


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
        self.presses = []
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
        """Answer the queries in this chunk, **in the order they were asked**.

        The order is the whole thing. nvmux ends its colour query with a DSR
        (`ESC [ 5 n`) precisely because every terminal answers it, so the reply
        to that is its signal that the colour replies before it have all
        arrived -- see the note in `src/palette.rs`. Answer the DSR first and
        nvmux stops reading right there, and the OSC replies still in flight are
        left in its input to be read as keystrokes by whatever comes next. The
        picker takes the `/` inside `rgb:cdcd/d0d0/d4d4` as its filter key and
        types the rest of the reply into it, which is what wiped the hint row
        and hung this script for a whole afternoon.

        So the queries are found by position and answered in that order, the way
        a terminal would. Matched spans are cut out so nothing is answered
        twice, and a short tail is kept so a query split across two reads is
        still seen whole.
        """
        self.qbuf += data
        row, col = self.screen.cursor()
        pal = THEME["palette"].split(":")
        answers = (
            (rb"\x1b\[6n", lambda m: ("\x1b[%d;%dR" % (row, col)).encode()),
            (rb"\x1b\[>0?q", lambda m: b"\x1bP>|answering-pty(1)\x1b\\"),
            (rb"\x1b\[>0?c", lambda m: b"\x1b[>0;279;0c"),
            (rb"\x1b\[0?c", lambda m: b"\x1b[?62;1;2;6;9;15;22;29c"),
            (rb"\x1b\[\?u", lambda m: b"\x1b[?0u"),
            (rb"\x1bP\+q([0-9A-Fa-f;]*)\x1b\\",
             lambda m: b"\x1bP0+r" + m.group(1) + b"\x1b\\"),
            # The colour query. Without these the fade never runs at all.
            (rb"\x1b\]10;\?(?:\x1b\\|\x07)",
             lambda m: ("\x1b]10;%s\x1b\\" % hex_to_osc(THEME["fg"])).encode()),
            (rb"\x1b\]11;\?(?:\x1b\\|\x07)",
             lambda m: ("\x1b]11;%s\x1b\\" % hex_to_osc(THEME["bg"])).encode()),
            (rb"\x1b\]4;(\d+);\?(?:\x1b\\|\x07)",
             lambda m: ("\x1b]4;%s;%s\x1b\\"
                        % (m.group(1).decode(),
                           hex_to_osc(pal[int(m.group(1)) % len(pal)]))).encode()),
            # Last in this list only as a tie-break; position is what orders the
            # replies, and this one terminates the colour query.
            (rb"\x1b\[5n", lambda m: b"\x1b[0n"),
        )

        hits = []
        for pattern, answer in answers:
            for m in re.finditer(pattern, self.qbuf):
                hits.append((m.start(), m.end(), answer(m)))
        hits.sort(key=lambda h: h[0])

        kept, end = [], 0
        for start, stop, _ in hits:
            if start < end:          # overlapping match, already consumed
                continue
            kept.append(self.qbuf[end:start])
            end = stop
        kept.append(self.qbuf[end:])
        self.qbuf = b"".join(kept)[-64:]
        return [reply for start, _, reply in hits
                if not any(s < start < e for s, e, _ in hits)]

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
            # Answer before parsing, and outside the lock. The colour query has
            # a deadline -- QUERY_CAP in `src/palette.rs`, 1.5s -- and feeding a
            # full-screen repaint to pyte can eat most of it, so replying second
            # loses the race on a busy frame. nvmux then starts with no palette,
            # which is not a rougher fade but no fade at all, and the recording
            # comes out looking fine while showing the wrong thing.
            #
            # The cursor `_replies` reports is this chunk's starting one rather
            # than where the chunk leaves it, which is the position the query was
            # sent from, and the reader is the only thread that writes it.
            for reply in self._replies(chunk):
                try:
                    os.write(self.fd, reply)
                except OSError:
                    pass
            with self.lock:
                self.screen.feed(chunk)
                if self.record and self.t0 is not None:
                    self.events.append((now - self.t0, chunk))

    # --- driving -----------------------------------------------------------
    def start_recording(self):
        with self.lock:
            self.t0 = time.time()
            self.events = []
            self.presses = []

    def write(self, data, wait=0.0, show=None):
        """`show` is `(cap, label)` for the key strip, or None to say nothing.

        Only the keys worth copying carry one -- `c`, Enter, the arrows, the
        prefix chord. `type` never passes it, which is what keeps the literal
        typing of a name or a sentence out of the strip.
        """
        if show is not None and self.t0 is not None:
            with self.lock:
                self.presses.append((time.time() - self.t0, show[0], show[1]))
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

    def expect_gone(self, needle, what, timeout=15):
        """Wait for something to leave the screen.

        The counterpart to `expect`, and needed because some waits can only be
        stated negatively. After a detach there is no new text to wait for: the
        shell's screen comes back exactly as it was, prompt included.
        """
        end = time.time() + timeout
        while time.time() < end:
            if needle not in self.text():
                return
            time.sleep(0.1)
        self.close()
        sys.exit("timed out waiting for %s to go\n%s" % (what, self.text()))

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
            self.write(DOWN, wait=0.55, show=("\u2193", "move"))
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
    prompt rather than starting from nowhere.

    The prompt is set off camera and then asked for a second time, because the
    GIF is rendered from the byte stream and the stream begins where
    `start_recording` says it does. bash writes PS1 once, when it is ready for
    the line after `clear`, which is before that point: the screen carried the
    prompt the whole time while the stream did not, so the first `nvmux` was
    drawn against a bare column 1 with no `$` in front of it.

    Ctrl-L is readline redrawing -- it clears and reprints PS1, echoes nothing
    of its own, and leaves the real screen exactly as `clear` did, so the
    stream and the screen agree from the first frame. `check_prompt` is what
    keeps them agreeing.
    """
    term = Terminal(["bash", "--norc", "--noprofile"], env, record=record)
    time.sleep(1.0)
    term.type("export PS1='%s'; clear\n" % PROMPT, wait=1.2)
    if record:
        term.start_recording()
        term.write(REDRAW, wait=0.5)
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
    term = shell(env, record=True)      # recording is already running
    time.sleep(1.0)

    # 1. the picker, with the sessions that already exist
    term.type("nvmux", per_char=0.09, wait=0.5)
    term.write(ENTER, wait=0.2, show=("⏎", "open the picker"))
    term.expect("attach", "the picker")
    time.sleep(1.8)

    # 2. make one, which is `c` and a name
    term.write(b"c", wait=0.6, show=("c", "new session"))
    term.expect("new session name", "the create prompt")
    time.sleep(0.8)
    term.type(NEW_NAME, per_char=0.1, wait=0.9)
    term.write(ENTER, wait=0.2, show=("⏎", "create"))

    # 3. creating attaches to it
    term.expect("[No Name]", "the new session to paint")
    time.sleep(1.6)

    term.write(b"i", wait=0.5, show=("i", "insert"))
    term.type("a session that outlives the connection", per_char=0.05, wait=0.7)
    term.write(ESCAPE, wait=1.4, show=("Esc", "normal"))

    # 4. detach: the session keeps running, nvmux exits
    term.write(PREFIX + b"d", show=("Ctrl-Space  d", "detach"))
    # The editor has to be gone, and waiting for the prompt does not say that:
    # leaving the alternate screen restores the shell's screen, which already
    # carries the `$` from before nvmux was started. That check passes the
    # instant the alternate screen is left -- before the detach has finished --
    # and the `nvmux` typed next then lands in the editor instead of the shell.
    term.expect_gone("[No Name]", "the editor")
    term.expect("$", "the shell prompt back")
    time.sleep(1.8)

    # 5. come back -- the session made a moment ago is in the list now
    term.type("nvmux", per_char=0.09, wait=0.4)
    term.write(ENTER, wait=0.2, show=("⏎", "open the picker"))
    term.expect("attach", "the picker again")
    time.sleep(1.4)
    term.select(NEW_NAME)
    time.sleep(0.7)
    term.write(ENTER, wait=0.2, show=("⏎", "attach"))
    term.expect("outlives", "the text to still be there")
    time.sleep(2.6)

    with term.lock:
        events, presses = list(term.events), list(term.presses)
    term.close()
    return events, presses


def check_fade_ran():
    """Refuse to ship a recording with no dissolves in it.

    This is the failure the answering pty exists to prevent, and it is silent:
    nvmux asks the terminal for its colours, gives up after QUERY_CAP, and an
    unanswered query leaves `fade::enabled` false and every transition a hard
    cut. The recording still comes out looking perfectly good -- it is just of
    the wrong thing, which is how the first version of this script shipped.

    nvmux says which happened, so read it rather than trust the timing.
    """
    log = os.path.join("/tmp/nvmux-%d" % os.getuid(), "nvmux.log")
    try:
        with open(log) as f:
            lines = [l for l in f if "palette:" in l]
    except OSError:
        sys.exit("no nvmux log at %s; NVMUX_LOG has to be set for the fade "
                 "check" % log)
    if not lines:
        sys.exit("nvmux logged no palette query; cannot tell whether the fade "
                 "ran")
    if not any("answered=true" in l for l in lines):
        sys.exit("the terminal's colour query went unanswered, so nvmux ran "
                 "with no fade and every transition in this take is a hard "
                 "cut:\n  %s" % lines[-1].strip())


def check_prompt(events):
    """Refuse to ship a recording whose first command is typed against nothing.

    The pty this script drives is not what the GIF is made of: the GIF is the
    byte stream, and the stream holds nothing written before `start_recording`.
    bash writes PS1 once per line it reads, so the prompt the screen showed all
    along was a write from before the recording -- and the first `nvmux` was
    rendered at a bare column 1, with every check in here reading the pty and
    seeing a prompt that the GIF did not have.

    So this replays the stream and looks at the line the command lands on,
    which is the only view of it that sees what the GIF will.
    """
    screen = Screen()
    for t, chunk in events:
        screen.feed(chunk)
        for line in screen.lines():
            if "nvmux" not in line:
                continue
            if not line.lstrip().startswith("$"):
                sys.exit("the first `nvmux` of the recording is typed with no "
                         "prompt in front of it, at %.2fs:\n  %r" % (t, line))
            return
    sys.exit("no `nvmux` was ever typed in the recording")


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


def sgr_for(attrs):
    """The SGR that reproduces pyte's idea of the current text attributes.

    Drawing the strip has to reset colour before it erases a line, so it has to
    put back whatever was set afterwards: a program that sets an attribute at
    the end of one write and prints at the start of the next would otherwise
    lose it.
    """
    named = {"black": 30, "red": 31, "green": 32, "brown": 33, "blue": 34,
             "magenta": 35, "cyan": 36, "white": 37}
    codes = ["0"]
    for flag, code in (("bold", "1"), ("italics", "3"), ("underscore", "4"),
                       ("reverse", "7")):
        if getattr(attrs, flag, False):
            codes.append(code)
    for colour, base, ground in ((attrs.fg, 30, "38"), (attrs.bg, 40, "48")):
        if colour in (None, "default"):
            continue
        if colour in named:
            codes.append(str(named[colour] - 30 + base))
        elif len(colour) == 6:
            try:
                codes.append("%s;2;%d;%d;%d" % (ground, int(colour[0:2], 16),
                                                int(colour[2:4], 16),
                                                int(colour[4:6], 16)))
            except ValueError:
                pass
    return ("\x1b[" + ";".join(codes) + "m").encode()


def strip_bytes(screen, press, row):
    """Paint the key strip, and leave the terminal exactly as it was found.

    The cursor goes back by absolute position, taken from the replayed screen,
    rather than through DECSC/DECRC -- there is one save slot and nvim and the
    alternate screen both use it.
    """
    if press is None:
        body = ""
    else:
        cap, label = press
        body = "\x1b[7m %s \x1b[0m  \x1b[2m%s\x1b[0m" % (cap, label)
        # Centred on the visible width, which the escapes are not part of.
        pad = max(0, (COLS - (len(cap) + 2 + 2 + len(label))) // 2)
        body = " " * pad + body
    cy, cx = screen.cursor()
    return (("\x1b[%d;1H\x1b[0m\x1b[2K" % row).encode()
            + body.encode()
            + ("\x1b[%d;%dH" % (cy, cx)).encode()
            + sgr_for(screen.active.cursor.attrs))


def with_key_strip(events, presses, hold=0.9):
    """Draw the pressed key on rows nvmux does not know exist.

    The cast declares two rows more than the pty nvmux ran on, so the strip sits
    below everything it draws and can never cover the picker or the editor.

    Re-emitted after *every* write rather than once per press: entering the
    alternate screen clears the screen, and so does every full repaint, either
    of which would wipe a strip drawn once and left alone.

    Drawn into the stream rather than composited onto the rendered frames
    because agg's idle-time limit compresses gaps, so a frame's time is not a
    cast time -- every badge would drift away from the action it belongs to.
    In the stream, agg compresses the badge and the content together.
    """
    row = ROWS + 2
    screen = Screen()
    # A change of what the strip shows, at the moment it changes.
    changes = [(t, (cap, label)) for t, cap, label in presses]
    for i, (t, cap, label) in enumerate(presses):
        nxt = presses[i + 1][0] if i + 1 < len(presses) else None
        if nxt is None or nxt > t + hold:
            changes.append((t + hold, None))
    changes.sort(key=lambda c: c[0])

    out, ci, shown = [], 0, None
    for t, chunk in events:
        while ci < len(changes) and changes[ci][0] <= t:
            shown = changes[ci][1]
            if out:                       # before this write, at its own moment
                out.append((changes[ci][0], strip_bytes(screen, shown, row)))
            ci += 1
        screen.feed(chunk)
        out.append((t, chunk + strip_bytes(screen, shown, row)))
    for t, press in changes[ci:]:
        out.append((t, strip_bytes(screen, press, row)))
    return out


def write_cast(events, path, tail=1.2, rows=ROWS):
    decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
    with open(path, "w") as f:
        f.write(json.dumps({
            "version": 2, "width": COLS, "height": rows,
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
    font_dir = ensure_font()

    # A stale log would let `check_fade_ran` pass on a previous run's evidence.
    runtime = "/tmp/nvmux-%d" % os.getuid()
    if os.path.exists(os.path.join(runtime, "nvmux.log")):
        os.remove(os.path.join(runtime, "nvmux.log"))

    tmp = tempfile.mkdtemp(prefix="nvmux-demo-")
    cfg = os.path.join(tmp, "config.toml")
    with open(cfg, "w") as f:
        f.write('[keys]\nprefix = "Ctrl-Space"\n')

    env = os.environ.copy()
    env["PATH"] = os.path.join(ROOT, "target", "release") + os.pathsep + env["PATH"]
    env["NVMUX_CONFIG"] = cfg
    env["TERM"] = "xterm-256color"
    env["COLORTERM"] = "truecolor"
    # So `check_fade_ran` can read whether the colour query was answered.
    env["NVMUX_LOG"] = "nvmux=debug"
    env.pop("NVMUX", None)
    env.pop("NO_COLOR", None)                         # it would turn the fade off

    try:
        print("creating demo sessions...")
        make_sessions(env)

        print("recording...")
        events, presses = perform(env)
        check_fade_ran()
        check_prompt(events)
        check_clean(events)
        print("  %d writes, %d keys shown, %.1fs"
              % (len(events), len(presses), events[-1][0]))

        cast = os.path.join(tmp, "demo.cast")
        write_cast(with_key_strip(events, presses), cast, rows=ROWS + 2)

        os.makedirs(os.path.dirname(OUT_GIF), exist_ok=True)
        print("rendering %s ..." % OUT_GIF)
        # No --theme: the cast header carries it.
        #
        # `--renderer resvg` is for the notice box, and it is not a preference.
        # agg's default renderer, `swash`, draws the box-drawing block itself
        # rather than from the font -- on the cell grid, a pixel thick, which is
        # what makes a run of `─` a straight line at any size. It does not do
        # that for the rounded corners the box is built from (`src/announce.rs`:
        # `╭ ╮ ╰ ╯`, U+256D..U+2570), which come from the font instead and land
        # a pixel higher and thicker than the bar they are meant to meet. The
        # corners visibly float off the dashes at both ends of both rules.
        # `resvg` draws every one of them from the font, where they were
        # designed to line up, and the box closes. Nothing else in the frame
        # moves by more than antialiasing.
        render = ["agg", "--renderer", "resvg",
                  "--font-family", FONT_FAMILY, "--font-size", "15",
                  "--fps-cap", str(FPS_CAP), "--idle-time-limit", "1.5"]
        if font_dir:
            render += ["--font-dir", font_dir]
        subprocess.run(render + [cast, OUT_GIF], check=True)
        print("wrote %s (%.0f KB)" % (OUT_GIF, os.path.getsize(OUT_GIF) / 1024))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
        print("note: the demo's nvmux sessions are still running; "
              "remove them with `x` in the picker if you do not want them.")


if __name__ == "__main__":
    main()
