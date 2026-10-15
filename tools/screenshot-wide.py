#!/usr/bin/env python3
"""Regenerate the README's pictures of a wide terminal.

    python3 tools/screenshot-wide.py        # writes assets/wide-*.png

One session, on a terminal wider than its two windows need, shot once for
each `[client] centre`: nvmux is run three times at a prompt, each with a
config of its own, and attaches to the session each time -- "off", the
default, then "windows", then "editor". In that order because the last is
the one that lays the windows out again for the width it attaches at: an
editor narrower than the terminal when it was last attached to would leave
the other two its windows as they were. Each shot is drawn by agg from what
nvmux had written up to that moment, and set in the demo's window. The
README says which is which: a label in the picture would be too small to
read at the width a page shows it.

**The sizes are smaller than a monitor that wants this.** At 120 columns a
window, the default `window_width`, two windows side by side and a margin
either side of them make a picture whose text is too small to read on a
page; COLS and WIDTH below are that picture brought down to one that can
be, the same in every way but the numbers.

**The left window is code and the right one prose,** wrapped at the window's
width: what a wide window does to a paragraph is most of why a user would
want its text centred instead. Each fits its window as the centred ways
have it: a line longer than WIDTH would be cut off, and a paragraph wrapped
past the last row ends in Neovim's `@@@`.

It borrows from tools/record-demo.py -- the theme the pty answers with and
agg draws in, the sessions' editor, the font, the answering pty and the
window -- so that the pictures look like the demo, and needs what that does
but ssh: the session is on this machine. It runs in your runtime directory
(/tmp/nvmux-<uid>) while the script does, which is why the script will not
start beside a session that is already there, and is killed on the way out.
"""

import importlib.util
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location(
    "record_demo", os.path.join(HERE, "record-demo.py"))
demo = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(demo)

OUT = os.path.join(demo.ROOT, "assets", "wide-%s.png")

# The terminal, and `[client] window_width`. Two windows of WIDTH side by
# side leave a margin either side in COLS, and half of COLS is wider than
# WIDTH, so that each of the three ways looks unlike the others.
COLS, ROWS = 150, 13
WIDTH = 50

NAME = "api-server"

# The shots, in the order they are taken (see the header), each with what
# the editor then says of itself (see `widths`): `'columns'`, what each
# window's text is given, and each window's own width. Sharing the whole
# terminal, the left window is half of it and the right one short of that,
# for the separator between them.
LEFT, RIGHT = COLS // 2, COLS - 1 - COLS // 2
SHOTS = [
    ("off", [COLS, [LEFT, RIGHT], [LEFT, RIGHT]]),
    ("windows", [COLS, [WIDTH, WIDTH], [LEFT, RIGHT]]),
    ("editor", [2 * WIDTH + 1, [WIDTH, WIDTH], [WIDTH, WIDTH]]),
]

# The project's, in the demo's api-server: some code, and the page of prose
# about it beside it. Every line fits in WIDTH, line numbers and all, and
# every paragraph, wrapped there, in the rows above the status line (see the
# header).
FILES = {
    "src/sessions.rs": """\
use anyhow::Result;

use crate::db::Pool;

/// Sessions unused this long are expired.
const IDLE_DAYS: i64 = 30;

/// Delete expired sessions, a thousand at a
/// time, so that a backlog after an outage
/// never locks the table for long.
pub async fn clean(db: &Pool) -> Result<u64> {
    let mut deleted = 0;
    loop {
        let n = db.expire(IDLE_DAYS).await?;
        if n == 0 {
            return Ok(deleted);
        }
        deleted += n;
    }
}
""",
    "docs/sessions.md": """\
# Sessions

A session starts when a user signs in, and lasts until they sign out or \
leave it unused for thirty days. Every request carries its token in a \
cookie.

## Cleanup

A job deletes expired sessions every hour, a thousand rows at a time.
""",
}

# The shots' own, after the demo's editor: no room kept for the window in
# front ('winwidth'), which would make the two unequal, and prose wrapped at
# a word.
EDITOR = r"""
vim.o.winwidth = 20
vim.api.nvim_create_autocmd('FileType', {
  pattern = 'markdown',
  callback = function()
    vim.wo.wrap = true
    vim.wo.linebreak = true
  end,
})
"""

# Off camera, in the session: the two files side by side, the code in front
# and scrolled to `clean`, and the message line cleared of what opening them
# said. Made the same width from the code's window, as nvmux's client makes
# them when it centres the editor: `wincmd =` gives the odd column to the
# window in front, which a split leaves on the right, so the windows are laid
# out alike whichever way is shot first.
SETUP = r"""
vim.cmd.cd(%s)
vim.cmd('silent edit src/sessions.rs')
vim.cmd('silent vsplit docs/sessions.md')
vim.wo.cursorline = false
vim.cmd.wincmd('h')
vim.cmd.wincmd('=')
vim.fn.winrestview({ topline = 8, lnum = 14, col = 8 })
vim.cmd.echo('""')
"""

WIDTHS = ("json_encode([&columns, map(range(1, winnr('$')), 'winwidth(v:val)'),"
          " map(nvim_tabpage_list_wins(0), 'nvim_win_get_width(v:val)')])")


def widths(sock):
    """What the editor says of itself, as SHOTS has it, or None."""
    done = subprocess.run(["nvim", "--server", sock, "--remote-expr", WIDTHS],
                          capture_output=True, text=True, timeout=10)
    try:
        return json.loads(done.stdout)
    except ValueError:
        return None


def settle(term, want, sock, what, quiet=0.8, limit=10.0):
    """Wait for the editor to say `want`, and then for nvmux to have drawn
    nothing for `quiet` seconds with the session's name nowhere on the screen
    -- whatever moved on the way stopped, and the row of names an attach
    lands with, in the top right corner over whatever is under it, gone. The
    moment of the shot, on the recording's clock."""
    end = time.time() + limit
    while widths(sock) != want:
        if time.time() > end:
            term.close()
            sys.exit("%s never came: the editor says %s, not %s"
                     % (what, widths(sock), want))
        time.sleep(0.1)
    while True:
        with term.lock:
            last = term.events[-1][0] if term.events else 0.0
        now = time.time() - term.t0
        if now - last >= quiet and NAME not in term.text():
            return now
        if time.time() > end:
            term.close()
            sys.exit("%s never stopped moving" % what)
        time.sleep(0.05)


def kill(since):
    """Kill the session, the way `x` in the picker does -- and only ours."""
    def ours():
        return [m for m in demo.live_sessions()
                if m.get("name") == NAME and m.get("created", 0) >= since]

    for _ in range(2):
        for meta in ours():
            subprocess.run(["sh", os.path.join(demo.ROOT, "scripts", "kill.sh"),
                            demo.RUNTIME, meta["id"], str(meta.get("pid", ""))],
                           capture_output=True)
        if not ours():
            return
        time.sleep(1.0)
    print("note: could not kill %s; `x` in nvmux's picker will" % NAME)


def shoot(env, project, configs):
    """The take: nvmux run at a prompt once a shot, with that shot's config
    (see the header) -- the first making the session, the others attaching
    to it. Returns the recording and the moment of each shot."""
    term = demo.Terminal(["bash", "--norc", "--noprofile"], env,
                         record=True, size=(COLS, ROWS))
    time.sleep(1.0)
    term.type("export PS1='$ '; clear\n", wait=1.0)
    term.start_recording()

    moments, sock = [], None
    for centre, want in SHOTS:
        term.type("NVMUX_CONFIG=%s nvmux\n" % shlex.quote(configs[centre]))
        if sock is None:
            term.expect("press c", "the picker")
            term.write(b"c", wait=1.0)
            term.expect("new session name", "the create prompt")
            term.type(NAME, wait=0.3)
            term.write(demo.ENTER)
            term.expect("[No Name]", "the session to paint", timeout=20)
            demo.rpc(env, NAME, SETUP % json.dumps(project))
            sock = demo.socket_of(NAME)
        else:
            term.expect(NAME, "the picker")
            term.write(demo.ENTER)
            term.expect("sessions.rs", "the session to paint", timeout=20)
        moments.append(settle(term, want, sock, 'centre = "%s"' % centre))
        # One write, so the chord fits keys.timeout_ms; and back at the
        # prompt, the editor gone with the screen nvmux drew it on, before
        # the next run is typed.
        term.write(demo.PREFIX + b"d")
        term.expect_gone("sessions.rs", "the detach")
        time.sleep(0.5)
    with term.lock:
        term.record = False
        events = list(term.events)
    term.close()
    return events, moments


def render(events, moments, font_dir, work):
    """Each shot, drawn by agg at its moment and set in the demo's window --
    with no keys under it, so with no more room under the terminal than
    either side of it."""
    from PIL import Image
    demo.WINDOW = dict(demo.WINDOW, foot=demo.WINDOW["pad"])
    os.makedirs(work)
    cast = os.path.join(work, "wide.cast")
    end = max(events[-1][0], moments[-1]) + 0.1
    demo.write_cast(events, cast, end, size=(COLS, ROWS))
    agg = ["agg", "-q", "--renderer", "resvg",
           "--font-family", demo.FONT_FAMILY,
           "--font-size", str(demo.px(demo.FONT_SIZE)),
           "--idle-time-limit", "1000000"]
    if font_dir:
        agg += ["--font-dir", font_dir]
    fonts = {cut: demo.font_file(font_dir, cut) for cut in ("Regular", "Bold")}
    window = None
    for (centre, _), moment in zip(SHOTS, moments):
        gif = os.path.join(work, "%s.gif" % centre)
        subprocess.run(agg + ["--select", "%.4f" % moment, cast, gif], check=True)
        with Image.open(gif) as im:
            screen = im.convert("RGB")
        if window is None:
            window = demo.Window(screen.size, fonts)
        shot = window.frame(screen, None)
        out = OUT % centre
        shot.save(out, optimize=True, transparency=255)
        print("wrote %s (%.0f KB)" % (out, os.path.getsize(out) / 1024))


def main():
    for tool in ("nvim", "agg"):
        if not shutil.which(tool):
            sys.exit("%s is not on PATH; see the header of tools/record-demo.py"
                     % tool)
    try:
        import pyte                                   # noqa: F401
        import PIL                                    # noqa: F401
    except ImportError:
        sys.exit("pyte and pillow are needed: pip install pyte pillow")
    if not os.path.exists(demo.NVMUX):
        sys.exit("build nvmux first: cargo build --release")
    font_dir = demo.ensure_font()
    others = [m.get("name", "?") for m in demo.live_sessions()]
    if others:
        sys.exit("%s has sessions running (%s); their notes would come up in "
                 "the shots. Kill them first, or run this as another user."
                 % (demo.RUNTIME, ", ".join(others)))

    tmp = tempfile.mkdtemp(prefix="nvmux-wide-")
    project = os.path.join(tmp, NAME)
    for path, text in FILES.items():
        full = os.path.join(project, path)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "w") as f:
            f.write(text)
    init = os.path.join(tmp, "init.lua")
    with open(init, "w") as f:
        f.write(demo.editor(tmp) + EDITOR)
    configs = {}
    for centre, _ in SHOTS:
        configs[centre] = os.path.join(tmp, "%s.toml" % centre)
        with open(configs[centre], "w") as f:
            f.write('[keys]\nprefix = "Ctrl-Space"\n\n'
                    '[session]\ncommand = "nvim --headless -u %s -i NONE -n '
                    '--listen {sock}"\n\n'
                    '[client]\ncentre = "%s"\nwindow_width = %d\n'
                    % (init, centre, WIDTH))

    env = os.environ.copy()
    env["PATH"] = os.pathsep.join([os.path.dirname(demo.NVMUX), env["PATH"]])
    for kind in ("config", "data", "state", "cache"):
        env["XDG_%s_HOME" % kind.upper()] = os.path.join(tmp, "xdg", kind)
    env["TERM"] = "xterm-256color"
    env["COLORTERM"] = "truecolor"
    for name in ("NVMUX", "NO_COLOR", "NVMUX_CONFIG"):
        env.pop(name, None)

    since = int(time.time())
    try:
        print("shooting...")
        events, moments = shoot(env, project, configs)
        print("rendering...")
        render(events, moments, font_dir, os.path.join(tmp, "render"))
    finally:
        kill(since)
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
