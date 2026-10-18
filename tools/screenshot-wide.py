#!/usr/bin/env python3
"""Regenerate the README's pictures of a wide terminal.

    python3 tools/screenshot-wide.py        # writes assets/wide-*.png

One session, on a terminal wider than its two windows need, shot with
`[client] centre` off and smart: nvmux is run twice at a prompt, each time
with a config of its own, and attaches to the session each time -- off, the
default, then smart. Each shot is drawn by agg from what nvmux had written up
to that moment, and set in the demo's window. The README says which is
which: a label in the picture would be too small to read at the width a
page shows it.

**The terminal is a whole ultra-wide monitor's:** 3440 by 1440 pixels, the
commonest, filled by a terminal in the demo's font at the demo's size --
COLS by ROWS below -- with each window's text as wide as what it shows
wants, as `"smart"` has it: the code its `'textwidth'`, and the prose
`window_width`. It is drawn at that size, and not at the half as big again
the demo is drawn at (see SCALE there): a page shows the picture a quarter
as wide, where its text is not for reading but where it stands is.

**The left window is code and the right one prose,** wrapped at the window's
width: what a wide window does to a paragraph is most of why a user would
want its text centred instead. Each fits its window with its text
centred: a line longer than WIDTH would be cut off, and a paragraph wrapped
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

# The terminal: as much of an ultra-wide monitor's 3440 by 1440 pixels as
# the window it is set in leaves, in the cells agg draws the demo's font in
# at the demo's size -- 9 pixels by 21, with half a cell more all round.
# And `[client] window_width`, as it is by default, which the prose gets;
# the code gets its `'textwidth'`, as Rust's is set, and the column of its
# line numbers beside it, three digits and a space. Half of COLS is wider
# than either, so that centred, each window's text stands well in from the
# window's left edge, where it starts with centring off.
COLS, ROWS = 376, 64
WIDTH = 120
TEXTWIDTH = 100
CODE_WIDTH = TEXTWIDTH + 4

NAME = "api-server"

# The shots, in the order they are taken (see the header): each one's name,
# `[client] centre`, and what the editor then says of itself (see `widths`):
# `'columns'`, what each window's text is given, and each window's own
# width. Sharing the whole terminal, the left window is half of it and the
# right one short of that, for the separator between them.
LEFT, RIGHT = COLS // 2, COLS - 1 - COLS // 2
SHOTS = [
    ("off", "off", [COLS, [LEFT, RIGHT], [LEFT, RIGHT]]),
    ("on", "smart", [COLS, [CODE_WIDTH, WIDTH], [LEFT, RIGHT]]),
]

# The project's, in the demo's api-server: some code, and the page of prose
# about it beside it, enough of each to fill a window as tall as the
# monitor. Every line fits in WIDTH, line numbers and all, and every
# paragraph, wrapped there, in the rows above the status line (see the
# header).
FILES = {
    "src/sessions.rs": """\
//! Sessions: who is signed in, and until when.
//!
//! A session is a row in `sessions`, found by the hash of its token. The token itself is only ever in the
//! user's cookie, so a copy of the database is not a copy of anyone's sign-in.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::{debug, info};

/// How long a session lasts unused.
pub const IDLE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How many rows the cleanup deletes in one statement, so that a backlog never holds the table for long.
const BATCH: i64 = 1000;

/// A signed-in user, as a request sees them.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: i64,
    pub user_id: i64,
    pub created: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

/// A new session for `user_id`: its row, and the token to put in the cookie.
pub async fn create(db: &PgPool, user_id: i64) -> Result<(Session, String)> {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    let session = sqlx::query_as!(
        Session,
        "INSERT INTO sessions (user_id, token_hash) VALUES ($1, $2)
         RETURNING id, user_id, created, last_seen",
        user_id,
        hash(&token),
    )
    .fetch_one(db)
    .await
    .context("creating a session")?;
    info!(user_id, session = session.id, "signed in");
    Ok((session, token))
}

/// The session `token` names, unless it has expired: seen now, so that it lasts another `IDLE`.
pub async fn find(db: &PgPool, token: &str) -> Result<Option<Session>> {
    sqlx::query_as!(
        Session,
        "UPDATE sessions SET last_seen = now()
         WHERE token_hash = $1 AND last_seen > now() - $2::interval
         RETURNING id, user_id, created, last_seen",
        hash(token),
        IDLE,
    )
    .fetch_optional(db)
    .await
    .context("finding a session")
}

/// Sign out: the session is gone, and its token with it.
pub async fn end(db: &PgPool, id: i64) -> Result<()> {
    sqlx::query!("DELETE FROM sessions WHERE id = $1", id).execute(db).await?;
    info!(session = id, "signed out");
    Ok(())
}

/// Sign `user_id` out everywhere, as a new password does.
pub async fn end_all(db: &PgPool, user_id: i64) -> Result<u64> {
    let n = sqlx::query!("DELETE FROM sessions WHERE user_id = $1", user_id)
        .execute(db)
        .await?
        .rows_affected();
    info!(user_id, sessions = n, "signed out everywhere");
    Ok(n)
}

/// Delete expired sessions, `BATCH` at a time, until none are left. Run every hour.
pub async fn clean(db: &PgPool) -> Result<u64> {
    let mut deleted = 0;
    loop {
        let n = sqlx::query!(
            "DELETE FROM sessions WHERE id IN (
                 SELECT id FROM sessions WHERE last_seen < now() - $1::interval LIMIT $2
             )",
            IDLE,
            BATCH,
        )
        .execute(db)
        .await?
        .rows_affected();
        if n == 0 {
            debug!(deleted, "expired sessions deleted");
            return Ok(deleted);
        }
        deleted += n;
    }
}

/// What the table keeps of a token.
fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}
""",
    "docs/sessions.md": """\
# Sessions

A session starts when a user signs in, and lasts until they sign out or leave it unused for thirty days. Every \
request carries its token in a cookie, and the server looks the token up before it does anything else: a token it \
cannot find is treated as signed out, not as an error, so a request with an old cookie gets the sign-in page rather \
than a 500.

Sessions live in Postgres, in the `sessions` table, one row each. Nothing about a session is kept in memory between \
requests, so any instance of the server can answer any request, and a deploy signs nobody out.

## Tokens

A token is 32 random bytes, hex-encoded into the cookie. The table never holds a token, only its SHA-256: a copy of \
the database, or a backup of it, is not a copy of anyone's sign-in. Looking a session up by its token is a lookup by \
that hash, which is indexed, so it costs the same however many sessions there are.

The cookie is `HttpOnly`, `Secure` and `SameSite=Lax`, and has no `Expires`: the server decides when a session ends, \
not the browser, which only forgets the cookie when it is closed.

## Expiry

Every request a session answers moves its `last_seen` to now, in the same statement that finds it, so a session in \
use never expires. One that has not been seen for thirty days is gone from that moment on, whether or not the \
cleanup has deleted its row yet: `find` only matches rows seen within the last thirty days.

## Cleanup

Expired sessions are deleted by a job that runs every hour. It deletes at most a thousand rows in one statement and \
repeats until there are none left, so a backlog — after an outage, say, or the first run on a database that has \
never been cleaned — drains over a few statements instead of locking the table for one long one.

The job logs how many sessions it deleted at `debug`. It is safe to run twice at once: both delete the same expired \
rows, and whichever gets there second finds fewer.

## Signing out everywhere

Signing out deletes the session's row, so its token stops working on the next request. To sign a user out \
everywhere — after a password change, say — `end_all` deletes every row with their `user_id`; there is no list of \
devices to keep in step, because each session is its own row.
""",
}

# The code from where its last line is at the bottom of its window, the
# cursor in `clean`, at the start of its query.
CODE = FILES["src/sessions.rs"].splitlines()
CURSOR = max(i for i, line in enumerate(CODE, 1) if line.lstrip().startswith("let n = "))
TOP = max(1, len(CODE) - (ROWS - 2) + 1)

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
# and scrolled to its end, and the message line cleared of what opening them
# said. Made the same width from the code's window: `wincmd =` gives the odd
# column to the window in front, which a split leaves on the right, and the
# shots have it on the left (see LEFT).
SETUP = r"""
vim.cmd.cd(%s)
vim.cmd('silent edit src/sessions.rs')
vim.bo.textwidth = %d
vim.cmd('silent vsplit docs/sessions.md')
vim.wo.cursorline = false
vim.cmd.wincmd('h')
vim.cmd.wincmd('=')
vim.fn.winrestview({ topline = %d, lnum = %d, col = 8 })
vim.cmd.echo('""')
"""

WIDTHS = ("json_encode([&columns, map(range(1, winnr('$')), 'winwidth(v:val)'),"
          " map(nvim_tabpage_list_wins(0), 'nvim_win_get_width(v:val)')])")


def toml(way):
    """A way of `[client] centre` as the config file spells it."""
    return '"%s"' % way


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
    for name, centre, want in SHOTS:
        term.type("NVMUX_CONFIG=%s nvmux\n" % shlex.quote(configs[name]))
        if sock is None:
            term.expect("press c", "the picker")
            term.write(b"c", wait=1.0)
            term.expect("new session name", "the create prompt")
            term.type(NAME, wait=0.3)
            term.write(demo.ENTER)
            term.expect("[No Name]", "the session to paint", timeout=20)
            demo.rpc(env, NAME, SETUP % (json.dumps(project), TEXTWIDTH, TOP, CURSOR))
            sock = demo.socket_of(NAME)
        else:
            term.expect(NAME, "the picker")
            term.write(demo.ENTER)
            term.expect("sessions.rs", "the session to paint", timeout=20)
        moments.append(settle(term, want, sock, "centre = %s" % toml(centre)))
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
    at the size the demo is shown at rather than drawn at (see the header),
    and with no keys under it, so with no more room under the terminal than
    either side of it."""
    from PIL import Image
    demo.SCALE = 1.0
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
    for (name, _, _), moment in zip(SHOTS, moments):
        gif = os.path.join(work, "%s.gif" % name)
        subprocess.run(agg + ["--select", "%.4f" % moment, cast, gif], check=True)
        with Image.open(gif) as im:
            screen = im.convert("RGB")
        if window is None:
            window = demo.Window(screen.size, fonts)
        shot = window.frame(screen, None)
        out = OUT % name
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
    for name, centre, _ in SHOTS:
        configs[name] = os.path.join(tmp, "%s.toml" % name)
        with open(configs[name], "w") as f:
            f.write('[keys]\nprefix = "Ctrl-Space"\n\n'
                    '[session]\ncommand = "nvim --headless -u %s -i NONE -n '
                    '--listen {sock}"\n\n'
                    '[client]\ncentre = %s\nwindow_width = %d\n'
                    % (init, toml(centre), WIDTH))

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
