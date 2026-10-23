#!/usr/bin/env python3
"""Regenerate the README's demo recording.

    python3 tools/record-demo.py            # writes assets/demo.gif

Drives nvmux on a pty and records every byte it writes, with its time, as an
asciicast. agg draws it a frame at a time, each frame is set in a window with
the keys just pressed beneath it, and Pillow encodes the GIF (see `encode`).

The take is `nvmux devbox` over real ssh (ControlMaster, socket forwards,
login shell). devbox is this machine: the script runs its own sshd on a free
127.0.0.1 port with generated keys, and an `ssh` on PATH adds `-F` with a
config naming it devbox (the `selftest` alias of tests/ssh_lifecycle.rs).
Staging can therefore reach session sockets directly, but the picker lists
every session in /tmp/nvmux-<uid>, so the script refuses to run while any
exists (`preflight`).

The pty answers nvmux's startup queries: colours (OSC 10/11, see
`src/palette.rs`) with THEME, since unanswered they disable fades entirely;
and cursor position, device attributes and kitty keyboard, without which
nvmux does not start. agg renders the same THEME so dissolves end on the
background it draws. The byte stream is recorded, not screenshots: a fade is
about seven repaints in 100 ms, which sampling would turn into steps. Checks
render the stream with pyte through `Screen` (pyte has no alternate screen).

Sessions run EDITOR as init.lua: a bare remote-UI Neovim without
'termguicolors' draws inactive statuslines bold and underlined. devbox's ssh
config sends XDG directories under the run's temp dir, so none of your
editor state is read or written.

Session notes (`src/notes.rs`) are staged off camera through each session's
socket (`rpc`): api-server gets a progress message left running, dotfiles a
finished install script in a hidden terminal. The build typed on camera runs
the demo's `cargo` (CARGO), which finishes when the take says so.

The GIF is drawn larger than shown (see SCALE): the README shows it ~880 px
wide, 1760 device px on a high-density screen, where agg's 15px text came out
soft. `schedule` times the frames.

Needs, all on PATH:
  * nvim >= 0.11
  * agg >= 1.9   https://github.com/asciinema/agg/releases (for `--renderer`,
                 which the notice box needs, and `--select`; see `draw_screens`)
  * OpenSSH: ssh, ssh-keygen and sshd (sshd is also looked for in sbin; it is
    the openssh-server package on Debian and Ubuntu)
  * python3 -m pip install pyte pillow
  * a release build of nvmux (cargo build --release)

The font, JetBrainsMono Nerd Font Mono, is fetched once into the gitignored
`tools/.fonts` if not installed; NVMUX_DEMO_FONT_DIR overrides where to look.

Everything the run makes lives in a temp dir removed on exit, and the demo's
sessions are then killed with the picker's `x` script. They live in your
real /tmp/nvmux-<uid> (nvmux has no override). /tmp/nvmux-<uid>/nvmux.log is
deleted beforehand and rewritten by the demo's NVMUX_LOG=nvmux=debug run,
so `check_fade_ran` never reads a previous run's log.
"""

import codecs
import fcntl
import json
import os
import pty
import pwd
import re
import select
import shlex
import shutil
import signal
import socket
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
RUNTIME = "/tmp/nvmux-%d" % os.geteuid()

COLS, ROWS = 100, 28

# The remote host the take types at the prompt.
HOST = "devbox"

# Frame grid. Fades draw every 16 ms (`fade::FRAME`), so much coarser turns
# dissolves into steps; GIFs keep time in hundredths, so 1/60 s would round to
# 2/100 and play a fifth slow. 50 is close to one and exact in the other.
FPS = 50
STEP = 1.0 / FPS

# Silent pauses are cut to this. The take's own holds are shorter, so this
# only trims unintended waits.
IDLE_LIMIT = 2.0

# Hold on the last screen before looping.
TAIL = 1.6

# Sessions made off camera, each in a small project with its files open. They
# `:cd` first so the statusline shows project-relative paths. The last file is
# on screen; the one before is the alternate buffer Ctrl-^ returns to.
SESSIONS = [
    ("api-server", ["src/main.rs"]),
    ("dotfiles", ["zsh/.zshrc", "nvim/init.lua"]),
    ("notes", ["todo.md"]),
]

# Made on camera, then moved to the end of the list.
NEW_NAME = "blog"

# The sessions' files, written under the run's temp dir.
PROJECTS = {
    "api-server/src/main.rs": """\
use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tracing::info;

mod config;
mod db;
mod routes;

/// What every handler can reach.
#[derive(Clone)]
pub struct AppState {
    pub db: db::Pool,
    pub config: Arc<config::Config>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("api_server=debug,tower_http=info")
        .init();

    let config = config::Config::from_env()?;
    let db = db::connect(&config.database_url).await?;
    db::migrate(&db).await?;

    let state = AppState {
        db,
        config: Arc::new(config),
    };

    let app = Router::new()
        .route("/health", get(routes::health))
        .route("/users", get(routes::list_users))
        .route("/users/:id", get(routes::get_user))
        .route("/sessions", get(routes::list_sessions))
        .with_state(state.clone());

    let port = state.config.port;
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Ctrl-C or SIGTERM, whichever comes first, so a deploy
/// drains the requests in flight instead of dropping them.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install the Ctrl-C handler");
    };

    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        signal(SignalKind::terminate())
            .expect("install the SIGTERM handler")
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutting down");
}
""",
    "api-server/src/routes.rs": """\
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;

use crate::db::{Session, User};
use crate::AppState;

pub async fn health() -> &'static str {
    "ok"
}

pub async fn list_users(
    State(state): State<AppState>,
) -> Result<Json<Vec<User>>, StatusCode> {
    let users = state
        .db
        .users()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(users))
}

pub async fn get_user(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<User>, StatusCode> {
    match state.db.user(id).await {
        Ok(Some(user)) => Ok(Json(user)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

pub async fn list_sessions(
    State(state): State<AppState>,
) -> Result<Json<Vec<Session>>, StatusCode> {
    let sessions = state
        .db
        .sessions()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(sessions))
}
""",
    "dotfiles/nvim/init.lua": """\
-- Options
vim.g.mapleader = " "
vim.opt.number = true
vim.opt.relativenumber = true
vim.opt.splitright = true
vim.opt.splitbelow = true
vim.opt.scrolloff = 8
vim.opt.ignorecase = true
vim.opt.smartcase = true
vim.opt.undofile = true

-- Keys
local map = vim.keymap.set
map("n", "<leader>w", "<cmd>write<cr>", { desc = "Write" })
map("n", "<leader>q", "<cmd>quit<cr>", { desc = "Quit" })
map("n", "<C-h>", "<C-w>h", { desc = "Window left" })
map("n", "<C-l>", "<C-w>l", { desc = "Window right" })
map("n", "<Esc>", "<cmd>nohlsearch<cr>")

-- Flash what was yanked
vim.api.nvim_create_autocmd("TextYankPost", {
  callback = function()
    vim.hl.on_yank({ timeout = 150 })
  end,
})

-- Back to where the cursor was when a file was last closed
vim.api.nvim_create_autocmd("BufReadPost", {
  callback = function(args)
    local mark = vim.api.nvim_buf_get_mark(args.buf, '"')
    local lines = vim.api.nvim_buf_line_count(args.buf)
    if mark[1] > 0 and mark[1] <= lines then
      pcall(vim.api.nvim_win_set_cursor, 0, mark)
    end
  end,
})
""",
    "dotfiles/zsh/.zshrc": """\
# History
HISTFILE=~/.zsh_history
HISTSIZE=50000
SAVEHIST=50000
setopt share_history hist_ignore_dups hist_ignore_space

# Completion
autoload -Uz compinit && compinit
zstyle ':completion:*' matcher-list 'm:{a-z}={A-Z}'
zstyle ':completion:*' menu select

# Editor
export EDITOR=nvim
export VISUAL=nvim
alias v=nvim
alias dev='nvmux devbox'

# Prompt
autoload -Uz vcs_info
precmd() { vcs_info }
zstyle ':vcs_info:git:*' formats ' %b'
setopt prompt_subst
PROMPT='%F{blue}%~%f%F{green}${vcs_info_msg_0_}%f %# '
""",
    # Run off camera in a hidden terminal, for the picker's `✓`. Links nothing.
    "dotfiles/install.sh": """\
#!/bin/sh
cd "$(dirname "$0")" || exit 1
echo "would link ~/.zshrc -> $PWD/zsh/.zshrc"
echo "would link ~/.config/nvim/init.lua -> $PWD/nvim/init.lua"
""",
    "notes/todo.md": """\
# This week

- [x] Move the API to the new database
- [x] Drain requests on shutdown
- [ ] Rotate the deploy keys
- [ ] Write up Tuesday's outage
- [ ] Review the session cleanup PR

## Ideas

- Cache the user lookups for a minute
- Rate limit `/sessions` per token
""",
}

# Answered to nvmux over OSC and written into the asciicast header for agg
# (so `--theme` is not passed). They must agree, or dissolves end a shade off
# the background.
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

# The sessions' init.lua. Normal sets no background, so the editor sits on
# the terminal's, as the picker does, and fades between them go to one
# colour. `editor` adds the terminal colours and the demo's cargo.
#
# 'winwidth' gives the current window room for cargo's last line, wider than
# half the screen. A terminal's width is re-taken after its line numbers are
# removed; otherwise the "Finished" line wraps.
EDITOR = r"""
local o = vim.o
o.termguicolors = true
o.number = true
o.cursorline = true
o.wrap = false
o.splitright = true
o.winwidth = 62
o.fillchars = 'eob: '

-- A file by its path from the project, a terminal by its command.
function _G.demo_name()
  if vim.bo.buftype == 'terminal' then
    return vim.api.nvim_buf_get_name(0):match('//%d+:(.*)$') or 'terminal'
  end
  local name = vim.fn.expand('%:.')
  return name ~= '' and name or '[No Name]'
end
o.statusline = " %{v:lua.demo_name()}%{&modified ? ' [+]' : ''}%=%{&filetype}   %l:%c  "

vim.api.nvim_create_autocmd('TermOpen', {
  callback = function()
    vim.wo.number = false
    vim.wo.cursorline = false
    vim.cmd('vertical resize -1 | vertical resize +1')
  end,
})

local c = {
  fg = '#cdd0d4', dim = '#7f8695', faint = '#3d4250',
  red = '#e06c75', orange = '#d19a66', yellow = '#e5c07b',
  green = '#98c379', cyan = '#56b6c2', blue = '#61afef', purple = '#c678dd',
}
local function hl(group, spec)
  vim.api.nvim_set_hl(0, group, spec)
end

hl('Normal', { fg = c.fg })
hl('CursorLine', { bg = '#1a1c24' })
hl('LineNr', { fg = c.faint })
hl('CursorLineNr', { fg = c.dim, bold = true })
hl('StatusLine', { fg = '#dfe2e7', bg = '#272a35' })
hl('StatusLineNC', { fg = '#6b7280', bg = '#191b22' })
hl('WinSeparator', { fg = '#2a2d38' })
hl('NonText', { fg = c.faint })
hl('Visual', { bg = '#2c313c' })
hl('MatchParen', { bg = '#2c313c', bold = true })

hl('Comment', { fg = '#646b79', italic = true })
hl('SpecialComment', { fg = '#7a8291', italic = true })
hl('Constant', { fg = c.orange })
hl('String', { fg = c.green })
hl('Character', { fg = c.green })
hl('Identifier', { fg = c.yellow })
hl('Function', { fg = c.blue })
hl('Statement', { fg = c.purple })
hl('Keyword', { fg = c.purple })
hl('Operator', { fg = c.cyan })
hl('PreProc', { fg = c.yellow })
hl('Include', { fg = c.fg })
hl('Macro', { fg = c.cyan })
hl('Type', { fg = c.yellow })
hl('Special', { fg = c.cyan })
hl('Delimiter', { fg = c.dim })
hl('Title', { fg = c.blue, bold = true })
hl('Todo', { fg = c.yellow, bold = true })
-- What nvmux's notes colour their signs by: a finished command, a
-- notification, a failure.
hl('OkMsg', { fg = c.green })
hl('WarningMsg', { fg = c.yellow })
hl('ErrorMsg', { fg = c.red })
hl('@variable', { fg = c.fg })
"""

# Draw size over display size; sizes below are display pixels, through `px`.
# 1.5 looks like 2 on a high-density screen, and 2 cost a third more bytes.
SCALE = 1.5
FONT_SIZE = 15                 # agg's, as shown


def px(size):
    """A size as shown, in the pixels the GIF is drawn at."""
    return int(size * SCALE + 0.5)


# The window frames are set in. GIF transparency is one-bit, so the rounded
# corners are softened only by the browser scaling the image down.
WINDOW = {
    "bar": 34,                 # the title bar
    "pad": 16,                 # either side of the terminal
    "gap": 6,                  # between the bar and the terminal
    "foot": 40,                # under the terminal, where the keys are shown
    "radius": 12,
    "bar_bg": (0x1a, 0x1c, 0x25),
    "bar_line": (0x26, 0x29, 0x35),
    "border": (0x2c, 0x2f, 0x3c),
    "lights": [(0xff, 0x5f, 0x57), (0xfe, 0xbc, 0x2e), (0x28, 0xc8, 0x40)],
}

# The key strip under the window: each a cap, then what it did. Drawn outside
# the terminal: reversed text inside it looked like nvmux's own selection.
KEYS = {
    "px": 13,
    "height": 24,
    "face": (0x26, 0x29, 0x36),
    "edge": (0x3a, 0x3e, 0x50),
    "lip": (0x0b, 0x0c, 0x11),
    "text": (0xe8, 0xea, 0xf0),
    "label": (0x87, 0x8d, 0x9e),
}

KEY_HOLD = 1.0

# The Nerd Font's `Mono` cut fits icons in one cell; a double-width glyph would
# push a row out of line. agg silently falls back through the family list, so
# `ensure_font` makes sure the font is present.
FONT_NAME = "JetBrainsMono Nerd Font Mono"
FONT_FAMILY = FONT_NAME + ",JetBrains Mono,DejaVu Sans Mono"
FONT_CACHE = os.path.join(ROOT, "tools", ".fonts")
FONT_FILES = ["JetBrainsMonoNerdFontMono-%s.ttf" % cut
              for cut in ("Regular", "Bold", "Italic", "BoldItalic")]
FONT_URL = ("https://github.com/ryanoasis/nerd-fonts/releases/latest/download"
            "/JetBrainsMono.tar.xz")

PROMPT = "\\[\\e[2K\\]\\[\\r\\]\\[\\e[38;5;71m\\]$\\[\\e[0m\\] "

# Keys as bytes. Ctrl-Space is NUL.
ENTER = b"\r"
ESCAPE = b"\x1b"
DOWN = b"\x1b[B"
UP = b"\x1b[A"
PREFIX = b"\x00"
# Ctrl-L: readline reprints PS1 and the typed line, echoing nothing else.
# `shell` uses it to get the prompt into the recording.
REDRAW = b"\x0c"
# Neovim's half-page scrolls, and its alternate buffer.
CTRL_D = b"\x04"
CTRL_U = b"\x15"
CTRL_CARET = b"\x1e"


def font_installed():
    """Whether fontconfig already knows the family, so nothing need be fetched."""
    if not shutil.which("fc-list"):
        return False
    listed = subprocess.run(["fc-list", ":", "family"],
                            capture_output=True, text=True).stdout
    return FONT_NAME in listed


def ensure_font():
    """The directory to hand agg with `--font-dir`, or None if installed.

    The four cuts (10MB) are cached under `tools/.fonts`, not committed. With
    NVMUX_DEMO_FONT_DIR set, nothing is downloaded.
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
    # Only the four cuts agg uses, of the ~90 in the archive.
    subprocess.run(["tar", "-C", FONT_CACHE, "-xJf", tar] + FONT_FILES,
                   check=True)
    os.remove(tar)
    return FONT_CACHE


def font_file(font_dir, cut):
    """The file of one cut of the font, for the key strip, which Pillow draws
    rather than agg."""
    name = "JetBrainsMonoNerdFontMono-%s.ttf" % cut
    if font_dir and os.path.exists(os.path.join(font_dir, name)):
        return os.path.join(font_dir, name)
    if shutil.which("fc-match"):
        path = subprocess.run(["fc-match", "-f", "%{file}",
                               "%s:style=%s" % (FONT_NAME, cut)],
                              capture_output=True, text=True).stdout.strip()
        if FONT_NAME.replace(" ", "") in path.replace(" ", ""):
            return path
    sys.exit("could not find the %s cut of %s for the keys" % (cut, FONT_NAME))


def hex_to_osc(colour):
    """`#rrggbb` as the `rgb:rrrr/gggg/bbbb` an OSC reply carries."""
    r, g, b = (colour[1:3], colour[3:5], colour[5:7])
    return "rgb:%s%s/%s%s/%s%s" % (r, r, g, g, b, b)


def hex_to_rgb(colour):
    return tuple(int(colour[i:i + 2], 16) for i in (1, 3, 5))


class Screen:
    """A terminal screen, as far as the checks here need one.

    Two pyte screens switched by `?1049`, which pyte lacks, so checks after a
    detach read the right one. The switch also saves and restores the cursor,
    putting it back on the line a hung-up client printed its last words on,
    which nvmux then erases; without that the erase lands a line low and
    `check_clean` reports a message a real terminal never shows.
    """

    ALT = re.compile(rb"\x1b\[\?1049([hl])")

    # DCS, SOS, PM and APC strings draw nothing, but pyte prints their bodies,
    # moving the cursor the answering half reports to nvmux. So they are cut
    # out first, and one still open at the end of a read is held for the next.
    STRING = re.compile(rb"\x1b[P_^X].*?(?:\x1b\\|\x07)", re.S)
    OPEN = re.compile(rb"\x1b(?:[P_^X].*)?\Z", re.S)

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
        self.held = b""
        self.decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")

    def feed(self, data):
        data = self.STRING.sub(b"", self.held + data)
        m = self.OPEN.search(data)
        self.held = data[m.start():] if m else b""
        if m:
            data = data[:m.start()]
        pos = 0
        for m in self.ALT.finditer(data):
            self._feed(data[pos:m.end()])
            if m.group(1) == b"h":
                self.saved = (self.primary.cursor.y, self.primary.cursor.x)
                # Entering clears, as on a real terminal; otherwise a check
                # waiting for the picker matches the previous one.
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
    """A program on a pty: answers what it asks, records what it writes."""

    def __init__(self, argv, env, record=False, size=(COLS, ROWS)):
        self.screen = Screen(*size)
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
                    struct.pack("HHHH", size[1], size[0], 0, 0))
        self.reader = threading.Thread(target=self._read_loop, daemon=True)
        self.reader.start()

    # --- the answering half ------------------------------------------------
    def _replies(self, data):
        """Answer the queries in this chunk in the order they were asked.

        nvmux ends its colour query with a DSR (`ESC [ 5 n`), whose reply tells
        it the colour replies have all arrived (see `src/palette.rs`). Answered
        first, it leaves the OSC replies to be read as keystrokes: the picker
        took the `/` in `rgb:cdcd/d0d0/d4d4` as its filter key and the script
        hung. Matched spans are cut out so nothing is answered twice, and a
        short tail is kept so a query split across reads is still seen.
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
            # The colour query; unanswered, there is no fade at all.
            (rb"\x1b\]10;\?(?:\x1b\\|\x07)",
             lambda m: ("\x1b]10;%s\x1b\\" % hex_to_osc(THEME["fg"])).encode()),
            (rb"\x1b\]11;\?(?:\x1b\\|\x07)",
             lambda m: ("\x1b]11;%s\x1b\\" % hex_to_osc(THEME["bg"])).encode()),
            (rb"\x1b\]4;(\d+);\?(?:\x1b\\|\x07)",
             lambda m: ("\x1b]4;%s;%s\x1b\\"
                        % (m.group(1).decode(),
                           hex_to_osc(pal[int(m.group(1)) % len(pal)]))).encode()),
            # Last only as a tie-break: position orders the replies, and this
            # one ends the colour query.
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
            # Answer before parsing, outside the lock: the colour query has a
            # 1.5s deadline (QUERY_CAP in `src/palette.rs`) and feeding pyte a
            # full repaint can use most of it. Missing it means no palette, so
            # no fade, and the recording silently shows the wrong thing.
            #
            # `_replies` reports the cursor at the chunk's start, where the
            # query was sent from; only this thread writes it.
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
        """`show` is `(keys, label)` for the key strip (the caps to draw and
        what they did), or None. Only keys worth copying carry one; `type`
        never does, keeping typed names and commands out of the strip.
        """
        if show is not None and self.t0 is not None:
            with self.lock:
                self.presses.append((time.time() - self.t0, tuple(show[0]),
                                     show[1]))
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
        self.expect_that(lambda text: needle in text, what, timeout)

    def expect_that(self, test, what, timeout=15):
        """Wait until `test` holds of the screen's text."""
        end = time.time() + timeout
        while time.time() < end:
            if test(self.text()):
                return
            time.sleep(0.05)
        self.close()
        sys.exit("timed out waiting for %s\n%s" % (what, self.text()))

    def expect_gone(self, needle, what, timeout=15):
        """Wait for `needle` to leave the screen: after a detach there is no
        new text to wait for, as the shell's screen returns exactly as it was.
        """
        end = time.time() + timeout
        while time.time() < end:
            if needle not in self.text():
                return
            time.sleep(0.05)
        self.close()
        sys.exit("timed out waiting for %s to go\n%s" % (what, self.text()))

    def listed(self):
        """The demo's sessions, in the order the picker lists them."""
        names = [n for n, _ in SESSIONS] + [NEW_NAME]
        order = []
        for line in self.text().split("\n"):
            words = line.split()
            for name in names:
                if name in words and name not in order:
                    order.append(name)
        return order

    def is_selected(self, name):
        return any("\u25b8" in line and name in line
                   for line in self.text().split("\n"))

    def select(self, name, key=None, limit=6):
        """Step to a session with the arrows (down unless `key` is UP) and
        check we landed there: attaching to the wrong one would look plausible
        on camera. The list wraps, so any start position works.
        """
        key = key or DOWN
        cap = "\u2191" if key == UP else "\u2193"
        for _ in range(limit):
            if self.is_selected(name):
                return
            self.write(key, wait=0.45, show=((cap,), "move"))
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


def shell(env, record=False, typed=""):
    """A bash on the pty, so the recording shows nvmux run at a prompt.

    bash prints PS1 before `start_recording`, so the stream would lack it and
    the first command would be drawn with no `$`. Ctrl-L after recording
    starts reprints PS1 and the typed line, matching the real screen
    (`check_prompt` checks this). `typed` is that line, so the first frame,
    all a viewer without autoplay sees, shows the command about to run.
    """
    term = Terminal(["bash", "--norc", "--noprofile"], env, record=record)
    time.sleep(1.0)
    term.type("export PS1='%s'; clear\n" % PROMPT, wait=1.2)
    if typed:
        term.type(typed, wait=0.3)
    if record:
        term.start_recording()
        term.write(REDRAW, wait=0.5)
    return term


def write_projects(root):
    """The files the demo sessions open, under `root`."""
    for path, text in PROJECTS.items():
        full = os.path.join(root, path)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "w") as f:
            f.write(text)
        if text.startswith("#!"):
            os.chmod(full, 0o755)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def host_token(host):
    """`ids::host_token`, which names a host's ControlPath and forwards:
    FNV-1a of the host as typed, 40 bits in base32."""
    h = 0xcbf29ce484222325
    for b in host.encode():
        h = ((h ^ b) * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
    n = h & 0xFFFFFFFFFF
    alphabet = "abcdefghijklmnopqrstuvwxyz234567"
    return "".join(alphabet[(n >> (35 - 5 * i)) & 0x1f] for i in range(8))


def answers(path):
    """Whether something is listening on the unix socket at `path`."""
    with socket.socket(socket.AF_UNIX) as s:
        try:
            s.connect(path)
            return True
        except OSError:
            return False


def processes_naming(marker):
    """Every process but this one whose command line holds `marker`."""
    out = subprocess.run(["ps", "-axo", "pid=,args="],
                         capture_output=True, text=True).stdout
    pids = []
    for line in out.splitlines():
        pid, _, args = line.strip().partition(" ")
        if marker in args and pid.isdigit() and int(pid) != os.getpid():
            pids.append(int(pid))
    return pids


class Devbox:
    """The take's host: the script's own sshd on this machine, and an `ssh`
    that knows it as HOST.

    Everything is under `root`, whose path is on the command line of every
    process this starts (sshd's `-f`; via the shim, every ssh's `-F`, masters
    included), which is how `stop` finds them without touching others.
    """

    def __init__(self, root, xdg):
        self.root = root
        self.xdg = xdg
        self.sshd = None
        self.bin = os.path.join(root, "bin")

    def start(self):
        sshd = shutil.which("sshd", path=os.pathsep.join(
            [os.environ.get("PATH", ""), "/usr/sbin", "/usr/local/sbin", "/sbin"]))
        if not sshd:
            sys.exit("sshd is not installed; the demo connects to a host of its "
                     "own (see the header of this script)")
        os.makedirs(self.bin)
        key = os.path.join(self.root, "id_ed25519")
        host_key = os.path.join(self.root, "host_ed25519")
        for path, comment in ((key, "nvmux-demo"), (host_key, HOST)):
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "",
                            "-C", comment, "-f", path], check=True)
        shutil.copy(key + ".pub", os.path.join(self.root, "authorized_keys"))
        port = free_port()
        with open(host_key + ".pub") as f:
            host_pub = f.read().split()
        with open(os.path.join(self.root, "known_hosts"), "w") as f:
            f.write("[127.0.0.1]:%d %s %s\n" % (port, host_pub[0], host_pub[1]))

        # The editors' XDG directories, sent with every connection and
        # accepted by sshd.
        xdg = {"XDG_%s_HOME" % k.upper(): os.path.join(self.xdg, k)
               for k in ("config", "data", "state", "cache")}
        with open(os.path.join(self.root, "sshd_config"), "w") as f:
            f.write("\n".join([
                "Port %d" % port,
                "ListenAddress 127.0.0.1",
                "HostKey %s" % host_key,
                "PidFile %s" % os.path.join(self.root, "sshd.pid"),
                "AuthorizedKeysFile %s" % os.path.join(self.root, "authorized_keys"),
                "PubkeyAuthentication yes",
                "PasswordAuthentication no",
                "KbdInteractiveAuthentication no",
                "PermitRootLogin prohibit-password",
                # PAM needs root, which the script may lack. StrictModes would
                # reject key files under the world-writable /tmp.
                "UsePAM no",
                "StrictModes no",
                "AcceptEnv %s" % " ".join(sorted(xdg)),
                "PrintMotd no",
                "PrintLastLog no",
                "",
            ]))
        with open(os.path.join(self.root, "config"), "w") as f:
            f.write("\n".join([
                "Host %s" % HOST,
                "  HostName 127.0.0.1",
                "  Port %d" % port,
                "  User %s" % pwd.getpwuid(os.geteuid()).pw_name,
                "  IdentityFile %s" % key,
                "  IdentitiesOnly yes",
                "  IdentityAgent none",
                "  StrictHostKeyChecking yes",
                "  UserKnownHostsFile %s" % os.path.join(self.root, "known_hosts"),
                "  GlobalKnownHostsFile /dev/null",
                "  SetEnv %s" % " ".join("%s=%s" % kv for kv in sorted(xdg.items())),
                "  LogLevel ERROR",
                "",
            ]))
        ssh = shutil.which("ssh")
        shim = os.path.join(self.bin, "ssh")
        with open(shim, "w") as f:
            f.write("#!/bin/sh\nexec %s -F %s \"$@\"\n"
                    % (shlex.quote(ssh), shlex.quote(os.path.join(self.root, "config"))))
        os.chmod(shim, 0o755)

        # As root, sshd needs its privilege separation directory, which may
        # not exist yet where sshd was installed but never started.
        if os.geteuid() == 0 and sys.platform.startswith("linux"):
            os.makedirs("/run/sshd", mode=0o755, exist_ok=True)
        log = open(os.path.join(self.root, "sshd.log"), "w")
        self.sshd = subprocess.Popen(
            [sshd, "-D", "-e", "-f", os.path.join(self.root, "sshd_config")],
            stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        end = time.time() + 10
        while time.time() < end:
            if subprocess.run([shim, "-o", "BatchMode=yes", "-o", "ConnectTimeout=2",
                               HOST, "true"], stdin=subprocess.DEVNULL,
                              capture_output=True).returncode == 0:
                return
            if self.sshd.poll() is not None:
                break
            time.sleep(0.2)
        with open(os.path.join(self.root, "sshd.log")) as f:
            sys.exit("could not reach %s over ssh:\n%s" % (HOST, f.read()))

    def stop(self):
        if self.sshd is not None and self.sshd.poll() is None:
            self.sshd.terminate()
            try:
                self.sshd.wait(5)
            except subprocess.TimeoutExpired:
                self.sshd.kill()
        for pid in processes_naming(self.root):
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        # The masters' ControlPath and forwards in the runtime directory, named
        # by HOST's token. Otherwise nvmux would clear them on its next
        # connection to a devbox, which would be someone else's.
        token = host_token(HOST)
        time.sleep(0.5)
        for entry in os.listdir(RUNTIME) if os.path.isdir(RUNTIME) else []:
            path = os.path.join(RUNTIME, entry)
            if (entry.startswith("cm-" + token) or entry.startswith(token + "-")) \
                    and not answers(path):
                os.remove(path)


def live_sessions():
    """The sessions in the runtime directory whose editor is still there."""
    found = []
    try:
        entries = sorted(os.listdir(RUNTIME))
    except FileNotFoundError:
        return found
    for entry in entries:
        if not entry.endswith(".json"):
            continue
        try:
            with open(os.path.join(RUNTIME, entry)) as f:
                meta = json.load(f)
        except (OSError, ValueError):
            continue
        if os.path.exists(os.path.join(RUNTIME, entry[:-len(".json")] + ".sock")):
            found.append(meta)
    return found


def preflight():
    """Refuse to record over sessions that are not the demo's: devbox is this
    machine, so the picker on camera would list them all.
    """
    others = [m.get("name", "?") for m in live_sessions()]
    if others:
        sys.exit("%s has sessions running (%s), and the picker the demo records "
                 "would list them. Kill them first, or record as another user."
                 % (RUNTIME, ", ".join(others)))
    # nvmux reuses a live master named by the host as typed, so with a master
    # up to a real host called devbox, the take would go there.
    master = os.path.join(RUNTIME, "cm-" + host_token(HOST))
    if os.path.exists(master) and answers(master):
        sys.exit("an ssh master to a host called %s is up (%s), and the demo's "
                 "nvmux would use it rather than the demo's own %s. Leave "
                 "nvmux there and give the master a minute to go."
                 % (HOST, master, HOST))


def kill_sessions(since):
    """Kill the demo's sessions with the picker's `x` script (devbox is this
    machine). Retried, since a leftover would block the next run, and
    reported if one survives."""
    names = {n for n, _ in SESSIONS} | {NEW_NAME}

    def ours():
        return [m for m in live_sessions()
                if m.get("name") in names and m.get("created", 0) >= since]

    for attempt in range(2):
        for meta in ours():
            subprocess.run(["sh", os.path.join(ROOT, "scripts", "kill.sh"), RUNTIME,
                            meta["id"], str(meta.get("pid", ""))],
                           capture_output=True)
        if not ours():
            return
        time.sleep(1.0)
    print("note: could not kill the demo's %s; `x` in nvmux's picker will"
          % ", ".join(m["name"] for m in ours()))


def make_sessions(env, projects):
    """Create the sessions the picker already has, off camera, each with its
    project's files open."""
    term = shell(env)
    term.type("nvmux %s\n" % HOST, wait=0.3)
    term.expect("attach", "the picker")
    for name, files in SESSIONS:
        term.write(b"c", wait=1.0)
        term.expect("new session name", "the create prompt")
        term.type(name, wait=0.5)
        term.write(ENTER, wait=4.0)                  # creates, then attaches
        term.expect("NVIM v", "%s to paint" % name)
        opens = " | ".join("edit %s" % f for f in files)
        term.type(":cd %s | %s\r" % (os.path.join(projects, name), opens), wait=1.0)
        term.expect(files[-1], "%s to open %s" % (name, files[-1]))
        term.write(PREFIX + b" ", wait=2.0)          # one write, so the chord
        term.expect("attach", "the picker after %s" % name)   # fits keys.timeout_ms
        print("  created %s" % name)
    term.write(b"q", wait=1.0)
    term.close()


# Run by an `nvim` acting only as a client of the session (see `rpc`). Socket
# and code come through the environment, so nothing is quoted twice.
RPC = ("local ch = vim.fn.sockconnect('pipe', os.getenv('DEMO_SOCK'), { rpc = true }) "
       "local ok, err = pcall(vim.rpcrequest, ch, 'nvim_exec_lua', os.getenv('DEMO_LUA'), {}) "
       "vim.fn.chanclose(ch) "
       "if not ok then io.stderr:write(tostring(err) .. '\\n') vim.cmd('cquit 1') end "
       "vim.cmd('qall!')")

# Off camera: an agent at work in api-server, as a plugin reports one (a
# progress message left running); then the message line is cleared so it is
# not shown on attach.
AGENT_AT_WORK = r"""
vim.api.nvim_echo({ { 'editing src/routes.rs' } }, false,
  { kind = 'progress', source = 'agent', title = 'claude', status = 'running' })
vim.cmd.echo('""')
"""

# Off camera: dotfiles' install script, run in a hidden terminal and waited
# for until it closes, so the watcher sees it finish.
INSTALL = r"""
local buf = vim.api.nvim_create_buf(true, false)
local closed = false
vim.api.nvim_create_autocmd('TermClose', {
  buffer = buf,
  once = true,
  callback = function()
    closed = true
  end,
})
vim.api.nvim_buf_call(buf, function()
  vim.fn.jobstart({ './install.sh' }, { term = true, cwd = vim.fn.getcwd() })
end)
vim.wait(5000, function()
  return closed
end)
"""

# The `cargo` on the sessions' PATH for the take's build. `main` prepends
# BUILT, the file the take creates when the build should finish. It compiles,
# then waits for BUILT, so it is still running when the take leaves it. Output
# mimics cargo (bold green verb right-aligned in 12 columns) and gives the
# real elapsed time.
CARGO = r"""
import os
import sys
import time

CRATES = [
    ("proc-macro2", "1.0.92"), ("unicode-ident", "1.0.14"), ("libc", "0.2.169"),
    ("cfg-if", "1.0.0"), ("pin-project-lite", "0.2.16"), ("bytes", "1.9.0"),
    ("itoa", "1.0.14"), ("memchr", "2.7.4"), ("quote", "1.0.38"),
    ("syn", "2.0.96"), ("serde", "1.0.217"), ("tracing-core", "0.1.33"),
    ("mio", "1.0.3"), ("socket2", "0.5.8"), ("tokio-macros", "2.5.0"),
    ("tokio", "1.43.0"), ("http", "1.2.0"), ("hyper", "1.5.2"),
    ("tower", "0.5.2"), ("axum-core", "0.5.0"), ("axum", "0.8.1"),
]


def say(verb, what):
    sys.stdout.write("\033[1;32m%12s\033[0m %s\n" % (verb, what))
    sys.stdout.flush()


start = time.time()
for name, version in CRATES:
    say("Compiling", "%s v%s" % (name, version))
    time.sleep(0.1)
while not os.path.exists(BUILT):
    time.sleep(0.05)
say("Compiling", "api-server v0.1.0")
time.sleep(0.4)
say("Finished", "`release` profile [optimized] target(s) in %.2fs"
    % (time.time() - start))
"""


def editor(tools):
    """The sessions' init.lua: EDITOR, then THEME's terminal colours and the
    demo cargo's directory ahead of PATH."""
    colours = "".join("vim.g.terminal_color_%d = %s\n" % (i, json.dumps(c))
                      for i, c in enumerate(THEME["palette"].split(":")))
    return (EDITOR.lstrip() + colours
            + "vim.env.PATH = %s .. ':' .. vim.env.PATH\n" % json.dumps(tools))


def socket_of(name):
    """The socket of the session called `name`, from its metadata."""
    for meta in live_sessions():
        if meta.get("name") == name:
            return os.path.join(RUNTIME, meta["id"] + ".sock")
    sys.exit("no session called %r in %s" % (name, RUNTIME))


def rpc(env, name, code):
    """Run `code`, Lua, in the session called `name`, over its own socket."""
    run = dict(env, DEMO_SOCK=socket_of(name), DEMO_LUA=code)
    done = subprocess.run(["nvim", "--clean", "--headless",
                           "-c", "lua " + RPC, "-c", "cquit 1"],
                          env=run, capture_output=True, text=True, timeout=30)
    if done.returncode != 0:
        sys.exit("could not run Lua in %s: %s" % (name, done.stderr.strip()))


def stage_news(env):
    """Stage, off camera, the agent at work in api-server and dotfiles'
    finished install."""
    rpc(env, "api-server", AGENT_AT_WORK)
    rpc(env, "dotfiles", INSTALL)


def perform(env, built):
    """The recorded take. `built` is the file that lets the demo's cargo
    finish (see CARGO)."""
    want = [n for n, _ in SESSIONS] + [NEW_NAME]
    term = shell(env, record=True, typed="nvmux %s" % HOST)
    time.sleep(1.1)

    # 1. connect: the picker, with api-server's agent (spinner, note under the
    # list) and dotfiles' finished install
    term.write(ENTER, show=(("\u23ce",), "connect"))
    term.expect("attach", "the picker")
    term.expect("editing src/routes.rs", "the agent's note under the list")
    # Signs arrive as the picker reads each session, possibly after the note.
    term.expect_that(lambda text: any("dotfiles" in line and "\u2713" in line
                                      for line in text.split("\n")),
                     "a \u2713 beside dotfiles", timeout=5)
    time.sleep(1.9)

    # 2. make one, which is `c` and a name; creating attaches to it
    term.write(b"c", show=(("c",), "new session"))
    term.expect("new session name", "the create prompt")
    time.sleep(0.5)
    term.type(NEW_NAME, per_char=0.09, wait=0.4)
    term.write(ENTER, show=(("\u23ce",), "create"))
    term.expect("NVIM v", "the new session to paint")
    time.sleep(1.0)

    # 3. back to the picker, still attached, and the new one moved to the end.
    # It was made below the selection, so moves are counted from where it is:
    # the list wraps, and one move too many would take it round.
    term.write(PREFIX + b" ", show=(("Ctrl-Space", "Space"), "picker"))
    term.expect("attach", "the picker over the session")
    time.sleep(0.7)
    if not term.is_selected(NEW_NAME):
        term.close()
        sys.exit("the picker did not open on %r:\n%s" % (NEW_NAME, term.text()))
    moves = len(want) - 1 - term.listed().index(NEW_NAME)
    term.write(b" ", wait=0.5, show=(("Space",), "pick up"))
    for _ in range(moves):
        term.write(DOWN, wait=0.4, show=(("\u2193",), "move"))
    term.write(b" ", show=(("Space",), "put down"))
    end = time.time() + 5
    while term.listed() != want and time.time() < end:
        time.sleep(0.05)
    if term.listed() != want:
        term.close()
        sys.exit("the reorder did not land: %r\n%s" % (term.listed(), term.text()))
    time.sleep(1.0)

    # 4. into another (one step down from the end, as the list wraps), the
    # editor as it was left, drawn by nvmux's own client
    term.select("api-server")
    time.sleep(0.2)
    term.write(ENTER, show=(("\u23ce",), "attach"))
    term.expect("AppState", "api-server to paint")
    time.sleep(1.0)

    # 5. the client animates: a scroll slides
    term.write(CTRL_D, wait=0.9, show=(("Ctrl-d",), "scroll"))
    term.write(CTRL_U, wait=0.9, show=(("Ctrl-u",), "scroll"))

    # 6. a release build, in a terminal whose split flies in from the side
    term.type(":vsplit | term cargo build --release", per_char=0.04, wait=0.3)
    term.write(ENTER, show=(("\u23ce",), "build"))
    term.expect("Compiling", "the build to start")
    time.sleep(1.6)

    # 7. the next session while it builds, without the picker; a buffer
    # switch fades over
    term.write(PREFIX + b"n", show=(("Ctrl-Space", "n"), "next session"))
    term.expect("init.lua", "dotfiles to paint")
    time.sleep(1.3)
    term.write(CTRL_CARET, show=(("Ctrl-^",), "other buffer"))
    term.expect("HISTFILE", "the other buffer")
    time.sleep(0.8)

    # 8. the build finishes and a margin note says so; the prefix shows the
    # note's key, and Enter goes to the finished build
    open(built, "w").close()
    term.expect("exited with code 0", "the note in the margin")
    time.sleep(2.0)
    term.write(PREFIX, show=(("Ctrl-Space",), "prefix"))
    term.expect("go there", "the key under the note")
    time.sleep(0.5)
    term.write(ENTER, show=(("\u23ce",), "go there"))
    term.expect("Finished", "api-server, its build done")
    time.sleep(1.6)

    # 9. detach: the sessions keep running on devbox, nvmux exits
    term.write(PREFIX + b"d", show=(("Ctrl-Space", "d"), "detach"))
    # Wait for the editor to go, not just the prompt: leaving the alternate
    # screen restores the old `$` before the detach finishes, and the next
    # `nvmux` would land in the editor.
    term.expect_gone("Finished", "the editor")
    term.expect("$", "the shell prompt back")
    time.sleep(0.8)

    # 10. come back: the order is kept, and the build's split is where it
    # was left
    term.type("nvmux %s" % HOST, per_char=0.07, wait=0.3)
    term.write(ENTER, show=(("\u23ce",), "connect"))
    term.expect("attach", "the picker again")
    time.sleep(0.9)
    if term.listed() != want:
        term.close()
        sys.exit("the order did not survive: %r" % term.listed())
    term.select("api-server", key=UP)
    time.sleep(0.3)
    term.write(ENTER, show=(("\u23ce",), "attach"))
    term.expect("Finished", "the build's split to still be there")
    time.sleep(2.6)

    with term.lock:
        events, presses = list(term.events), list(term.presses)
    term.close()
    return events, presses


def check_fade_ran():
    """Refuse to ship a recording with no dissolves.

    An unanswered colour query (given up after QUERY_CAP) leaves
    `fade::enabled` false, and the recording still looks fine. nvmux logs
    which happened, so read that rather than trust the timing.
    """
    log = os.path.join(RUNTIME, "nvmux.log")
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
    """Refuse to ship a recording whose first command has no prompt.

    The GIF is the byte stream from `start_recording`; a PS1 written earlier
    shows on the pty but not in the GIF. So replay the stream and check the
    line the command lands on.
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
    bytes are in the stream: check the rendered screen, not the bytes. Sampled
    at the GIF's frame rate, since print and erase are ~0.3 ms apart, a state
    no frame shows.
    """
    screen = Screen()
    i, t = 0, 0.0
    end = events[-1][0] if events else 0.0
    while t <= end + STEP:
        while i < len(events) and events[i][0] <= t:
            screen.feed(events[i][1])
            i += 1
        if "deadly signal" in screen.text():
            sys.exit("the departing client's signal message is on screen at "
                     "%.2fs, for a whole frame; nvmux is meant to erase it" % t)
        t += STEP


SYNC_BEGIN = b"\x1b[?2026h"
SYNC_END = b"\x1b[?2026l"


def synchronized(events, cap=0.15):
    """The writes, re-cut so that a synchronized update is one write.

    nvmux's client draws each frame between `?2026h` and `?2026l`, but agg
    ignores the mode and draws a frame split across pty reads half done. So
    output after `?2026h` is held until its `?2026l` and emitted at that
    time. An update open longer than `cap` is released, as terminals time
    one out.
    """
    out, carry, since = [], b"", None
    for t, chunk in events:
        data = carry + chunk
        begin = data.rfind(SYNC_BEGIN)
        held = begin != -1 and data.find(SYNC_END, begin) == -1
        if held and since is not None and t - since > cap:
            held = False
        if held:
            if data[:begin]:
                out.append((t, data[:begin]))
            carry = data[begin:]
            since = t if since is None else since
        else:
            out.append((t, data))
            carry, since = b"", None
    if carry:
        out.append((events[-1][0], carry))
    return out


def retime(events, presses, limit=IDLE_LIMIT):
    """The take on the GIF's clock: from the first write, pauses over `limit`
    cut to it, and presses moved with the screen.

    Done here rather than with agg's `--idle-time-limit` so cast times are GIF
    times: the key strip is drawn by this script and must land on the frames
    the screen reacts in. A press keeps its lead on the write after it.
    """
    out, shift, last = [], events[0][0], None
    for t, chunk in events:
        if last is not None and t - last > limit:
            shift += (t - last) - limit
        out.append((t - shift, chunk))
        last = t
    moved = []
    for t, keys, label in presses:
        after = next((i for i, (u, _) in enumerate(events) if u >= t), None)
        if after is None:
            at = out[-1][0] + min(t - events[-1][0], limit)
        else:
            lead = min(events[after][0] - t, limit)
            floor = out[after - 1][0] if after else 0.0
            at = max(floor, out[after][0] - lead)
        moved.append((at, keys, label))
    return out, moved


def write_cast(events, path, end, size=(COLS, ROWS)):
    """The cast agg draws from, running to `end`: agg draws nothing past the
    last event, so an empty one is added there."""
    decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
    with open(path, "w") as f:
        f.write(json.dumps({
            "version": 2, "width": size[0], "height": size[1],
            "theme": THEME,
            "env": {"TERM": "xterm-256color"},
        }) + "\n")
        for t, chunk in events:
            text = decoder.decode(chunk)
            if text:
                f.write(json.dumps([round(t, 4), "o", text]) + "\n")
        f.write(json.dumps([round(end, 4), "o", ""]) + "\n")


def schedule(events, presses, end):
    """The GIF's frames as `(seconds, screen, keys)`: duration, the grid slot
    whose screen it shows, and the keys under it.

    The grid is FPS slots a second. A slot with anything new (a write, or a
    key shown or hidden) starts a frame showing the state at its end, so each
    frame lasts whole slots, which GIF hundredths hold exactly, and each
    screen is drawn once.
    """
    shown = []
    for i, (t, keys, label) in enumerate(presses):
        stop = t + KEY_HOLD
        if i + 1 < len(presses):
            stop = min(stop, presses[i + 1][0])
        shown.append((t, stop, (keys, label)))

    def keys_at(t):
        return next((k for start, stop, k in shown if start <= t < stop), None)

    def slot(t):
        return int(t / STEP + 1e-9)

    screens = sorted({slot(t) for t, _ in events})
    marks = sorted(set(screens) | {slot(t) for s, e, _ in shown for t in (s, e)})
    frames, at, screen = [], 0, None
    for k in marks:
        while at < len(screens) and screens[at] <= k:
            screen = screens[at]
            at += 1
        if screen is None:
            continue
        keys = keys_at((k + 1) * STEP - 1e-6)
        if frames and frames[-1][1:] == (screen, keys):
            continue
        frames.append((k, screen, keys))
    last = max(frames[-1][0] + 1, slot(end))
    return [((nxt - k) * STEP, screen, keys)
            for (k, screen, keys), nxt in zip(frames, [f[0] for f in frames[1:]] + [last])]


def draw_screens(cast, slots, font_dir, work):
    """Each slot's screen, drawn by its own agg run; returns paths by slot.

    One run per screen with `--select` at the slot's end, not agg on the whole
    cast: agg hands frames to gifski at default quality, which drops small
    per-pixel changes, so dissolves lagged and stopped short.

    `--renderer resvg` is required for the notice box: the default `swash`
    draws box-drawing lines on the cell grid but takes the rounded corners
    (`╭ ╮ ╰ ╯`, U+256D..U+2570, see `src/announce.rs`) from the font, a pixel
    higher and thicker, so the box does not close. resvg draws all of them
    from the font.
    """
    from concurrent.futures import ThreadPoolExecutor

    # No --theme: the cast header carries it. No idle limit: `retime` has
    # already put the cast on the GIF's clock.
    agg = ["agg", "-q", "--renderer", "resvg",
           "--font-family", FONT_FAMILY, "--font-size", str(px(FONT_SIZE)),
           "--idle-time-limit", "1000000"]
    if font_dir:
        agg += ["--font-dir", font_dir]
    os.makedirs(work)
    paths = {k: os.path.join(work, "%06d.gif" % k) for k in slots}

    def draw(k):
        subprocess.run(agg + ["--select", "%.4f" % ((k + 1) * STEP - 0.0005),
                              cast, paths[k]], check=True)

    with ThreadPoolExecutor(os.cpu_count() or 4) as pool:
        list(pool.map(draw, sorted(slots)))
    return paths


class Window:
    """The window a screen is shown in, with the key strip. Drawn by Pillow,
    not into the cast, so the keys are not taken for nvmux's cells (see KEYS).
    """

    def __init__(self, inner, fonts):
        from PIL import ImageFont
        self.inner = inner
        self.at = (px(WINDOW["pad"]), px(WINDOW["bar"] + WINDOW["gap"]))
        self.size = (inner[0] + 2 * self.at[0],
                     self.at[1] + inner[1] + px(WINDOW["foot"]))
        self.foot = (self.at[0], self.at[1] + inner[1])
        self.fonts = {cut: ImageFont.truetype(path, KEYS["px"] * SCALE * 3)
                      for cut, path in fonts.items()}
        base, self.outside = self._chrome()
        self.base = fixed(base, 24, keep=hex_to_rgb(THEME["bg"]))
        self.colours = {rgb for _, rgb in self.base.getcolors(256)}
        self.strips = {}

    def _chrome(self):
        """The window without contents, and a mask of what is outside it.

        Drawn at 4x and scaled down for smooth corners, border and lights; the
        outside is cut at half coverage, as GIF transparency is all or nothing.
        """
        from PIL import Image, ImageDraw
        k = 4
        w, h = self.size

        def u(size):                       # a size as shown, drawn here
            return size * SCALE * k

        big = Image.new("RGBA", (w * k, h * k), (0, 0, 0, 0))
        d = ImageDraw.Draw(big)
        right, bottom = w * k - 1, h * k - 1
        r, bar, edge = u(WINDOW["radius"]), u(WINDOW["bar"]), max(1, round(u(1)))
        bg = hex_to_rgb(THEME["bg"]) + (255,)
        d.rounded_rectangle([0, 0, right, bottom], r, fill=bg)
        d.rounded_rectangle([0, 0, right, bar + r], r, fill=WINDOW["bar_bg"] + (255,))
        d.rectangle([0, bar, right, bar + r + 1], fill=bg)
        d.rectangle([0, bar, right, bar + edge - 1], fill=WINDOW["bar_line"] + (255,))
        d.rounded_rectangle([0, 0, right, bottom], r,
                            outline=WINDOW["border"] + (255,), width=edge)
        for i, colour in enumerate(WINDOW["lights"]):
            cx, cy, rr = u(18 + 20 * i), bar / 2, u(6)
            d.ellipse([cx - rr, cy - rr, cx + rr, cy + rr], fill=colour + (255,))
        # Scale colour and coverage apart, colour laid over the border colour,
        # so a kept edge pixel is the border, not darkened by the clear black.
        coverage = big.getchannel("A")
        flat = Image.new("RGB", big.size, WINDOW["border"])
        flat.paste(big.convert("RGB"), (0, 0), coverage)
        base = flat.resize((w, h), Image.BOX)
        outside = coverage.resize((w, h), Image.BOX).point(
            lambda a: 255 if a < 128 else 0)
        return base, outside

    def strip(self, keys, label):
        """The foot of the window with `keys` as caps and `label` after them."""
        from PIL import Image, ImageDraw
        if (keys, label) in self.strips:
            return self.strips[(keys, label)]
        k = 3
        # Stops short of the bottom border; caps stay centred on the whole foot.
        w, h = self.inner[0], px(WINDOW["foot"]) - px(2)

        def u(size):
            return size * SCALE * k

        big = Image.new("RGB", (w * k, h * k), hex_to_rgb(THEME["bg"]))
        d = ImageDraw.Draw(big)
        bold, plain = self.fonts["Bold"], self.fonts["Regular"]
        cap, pad, gap, r, lip, after = (u(KEYS["height"]), u(9), u(6), u(6), u(2), u(12))
        widths = [d.textlength(key, font=bold) + 2 * pad for key in keys]
        total = sum(widths) + gap * (len(keys) - 1) + after + d.textlength(label, font=plain)
        x, mid = (w * k - total) / 2, px(WINDOW["foot"]) * k / 2
        top = mid - cap / 2
        for key, width in zip(keys, widths):
            d.rounded_rectangle([x, top + lip, x + width, top + cap + lip], r, fill=KEYS["lip"])
            d.rounded_rectangle([x, top, x + width, top + cap], r, fill=KEYS["face"],
                                outline=KEYS["edge"], width=max(1, round(u(1))))
            d.text((x + width / 2, mid), key, font=bold, fill=KEYS["text"], anchor="mm")
            x += width + gap
        d.text((x - gap + after, mid), label, font=plain, fill=KEYS["label"], anchor="lm")
        strip = fixed(big.resize((w, h), Image.BOX), 16, keep=hex_to_rgb(THEME["bg"]))
        self.strips[(keys, label)] = (strip, {rgb for _, rgb in strip.getcolors(256)})
        return self.strips[(keys, label)]

    def frame(self, screen, keys):
        """One GIF frame: a paletted image whose last entry is transparent.

        Its colours are exactly those drawn, so unchanged pixels match the
        previous frame and need not be stored again. Quantizing from more
        colours shifted the window's antialiased pixels each frame, so every
        frame was stored whole.
        """
        from PIL import Image
        strip, extra = self.strip(*keys) if keys is not None else (None, set())
        img = self.compose(screen, strip)
        if img.getcolors(255) is None:
            # Over 255: reduce the screen to what the window leaves, the same
            # way each time, so a still screen stays still.
            img = self.compose(fixed(screen, 255 - len(self.colours | extra)), strip)
        # Within the limit, median cut gives each colour its own box, so every
        # pixel keeps its colour exactly. A hand-built palette does not: Pillow
        # matches via coarsened colours, landing a few levels off, differently
        # each frame.
        out = img.quantize(255, method=Image.Quantize.MEDIANCUT,
                           dither=Image.Dither.NONE)
        palette = out.getpalette()[:255 * 3]
        out.putpalette(palette + [0] * (255 * 3 - len(palette)) + [255, 0, 255])
        out.paste(255, (0, 0), self.outside)
        out.info["transparency"] = 255
        return out

    def compose(self, screen, strip):
        img = self.base.copy()
        img.paste(screen, self.at)
        if strip is not None:
            img.paste(strip, self.foot)
        return img


def fixed(img, colours, keep=None):
    """`img` as RGB in at most `colours` colours, the same ones each time, so
    unchanged pixels stay unchanged. `keep` must come out exactly: the window
    background, which has to meet the terminal's without a seam."""
    from PIL import Image, ImageChops
    out = img.quantize(colours, method=Image.Quantize.MEDIANCUT,
                       dither=Image.Dither.NONE).convert("RGB")
    if keep is not None:
        # Where `img` was exactly `keep`: no channel off by anything.
        off = ImageChops.difference(img, Image.new("RGB", img.size, keep)).split()
        exact = ImageChops.lighter(ImageChops.lighter(off[0], off[1]), off[2])
        out.paste(keep, (0, 0), exact.point(lambda v: 255 if v == 0 else 0))
    return out


def encode(frames, screens, window, out):
    """Write the GIF with Pillow, frame for frame as drawn.

    Not gifski: even at top quality it skips small per-pixel changes, so
    dissolves left a faint residue of the previous screen. Pillow gives each
    frame its own palette and makes only unchanged pixels transparent, for
    about 10% more bytes.
    """
    from PIL import Image

    cache = {}

    def images():
        for _, screen, keys in frames:
            if screen not in cache:
                cache.clear()
                with Image.open(screens[screen]) as im:
                    cache[screen] = im.convert("RGB")
            yield window.frame(cache[screen], keys)

    it = images()
    first = next(it)
    first.save(out, save_all=True, append_images=it, loop=0,
               duration=[round(seconds * 1000) for seconds, _, _ in frames],
               optimize=True, disposal=1)


def render(events, presses, font_dir, work):
    """The take, from the bytes on the pty to the GIF."""
    from PIL import Image
    events, presses = retime(synchronized(events), presses)
    end = events[-1][0] + TAIL
    os.makedirs(work)
    cast = os.path.join(work, "demo.cast")
    write_cast(events, cast, end)
    frames = schedule(events, presses, end)
    slots = {screen for _, screen, _ in frames}
    print("  drawing %d screens for %d frames, %.1fs" % (len(slots), len(frames), end))
    screens = draw_screens(cast, slots, font_dir, os.path.join(work, "screens"))
    with Image.open(screens[min(slots)]) as im:
        inner = im.size
    window = Window(inner, {cut: font_file(font_dir, cut) for cut in ("Regular", "Bold")})
    os.makedirs(os.path.dirname(OUT_GIF), exist_ok=True)
    encode(frames, screens, window, OUT_GIF)


def main():
    for tool in ("nvim", "agg", "ssh", "ssh-keygen"):
        if not shutil.which(tool):
            sys.exit("%s is not on PATH; see the header of this script" % tool)
    try:
        import pyte                                   # noqa: F401
    except ImportError:
        sys.exit("pyte is needed for the screen checks: pip install pyte")
    try:
        import PIL                                    # noqa: F401
    except ImportError:
        sys.exit("pillow is needed to render the GIF: pip install pillow")
    if not os.path.exists(NVMUX):
        sys.exit("build nvmux first: cargo build --release")
    font_dir = ensure_font()
    preflight()

    # A stale log would let `check_fade_ran` pass on a previous run's evidence.
    if os.path.exists(os.path.join(RUNTIME, "nvmux.log")):
        os.remove(os.path.join(RUNTIME, "nvmux.log"))

    tmp = tempfile.mkdtemp(prefix="nvmux-demo-")
    cfg = os.path.join(tmp, "config.toml")
    with open(cfg, "w") as f:
        f.write('[keys]\nprefix = "Ctrl-Space"\n')
    tools = os.path.join(tmp, "tools")
    built = os.path.join(tmp, "built")
    os.makedirs(tools)
    with open(os.path.join(tools, "cargo"), "w") as f:
        f.write("#!/usr/bin/env python3\nBUILT = %r\n" % built + CARGO)
    os.chmod(os.path.join(tools, "cargo"), 0o755)
    xdg = os.path.join(tmp, "xdg")
    os.makedirs(os.path.join(xdg, "config", "nvim"))
    with open(os.path.join(xdg, "config", "nvim", "init.lua"), "w") as f:
        f.write(editor(tools))
    devbox = Devbox(os.path.join(tmp, "ssh"), xdg)

    env = os.environ.copy()
    env["PATH"] = os.pathsep.join([devbox.bin, os.path.join(ROOT, "target", "release"),
                                   env["PATH"]])
    env["NVMUX_CONFIG"] = cfg
    # nvmux stores each host's launch command under XDG_STATE_HOME; keep the
    # demo's out of yours.
    env["XDG_STATE_HOME"] = os.path.join(xdg, "state")
    env["TERM"] = "xterm-256color"
    env["COLORTERM"] = "truecolor"
    # So `check_fade_ran` can read whether the colour query was answered.
    env["NVMUX_LOG"] = "nvmux=debug"
    env.pop("NVMUX", None)
    env.pop("NO_COLOR", None)                         # it would turn the fade off

    since = int(time.time())
    try:
        print("starting %s..." % HOST)
        devbox.start()
        print("creating demo sessions...")
        projects = os.path.join(tmp, "projects")
        write_projects(projects)
        make_sessions(env, projects)
        stage_news(env)

        print("recording...")
        events, presses = perform(env, built)
        check_fade_ran()
        check_prompt(events)
        check_clean(events)
        print("  %d writes, %d keys shown, %.1fs"
              % (len(events), len(presses), events[-1][0]))

        print("rendering %s ..." % OUT_GIF)
        render(events, presses, font_dir, os.path.join(tmp, "render"))
        print("wrote %s (%.0f KB)" % (OUT_GIF, os.path.getsize(OUT_GIF) / 1024))
    finally:
        kill_sessions(since)
        devbox.stop()
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
