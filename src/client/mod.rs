//! nvmux's own Neovim client: what draws a session unless `[client] ui =
//! "nvim"` asks for Neovim's.
//!
//! Neovim's own client, `nvim --remote-ui`, negotiates with the terminal
//! itself, and everything the terminal can do reaches the editor as Neovim
//! negotiated it (see [`crate::pty`]). What that client will not do is move.
//! Neovide animates the cursor, the scroll and the windows of a Neovim it
//! draws in a window of its own; this client draws in the terminal, as that
//! one does, and animates as much of the same as a grid of character cells
//! allows:
//!
//! - the cursor travels between cells instead of jumping, its leading edge
//!   ahead of its trailing one, so a long jump leaves a smear behind it, drawn
//!   in block elements at a quarter of a cell or finer and fading towards its
//!   tail ([`anim::smear`]);
//! - sparks, rings and outlines off the cursor as it moves — Neovide's
//!   `railgun`, `torpedo`, `pixiedust`, `sonicboom`, `ripple` and `wireframe`
//!   ([`anim::vfx`]);
//! - a scroll slides the text through the window a row at a time rather than
//!   redrawing it in place ([`anim::scroll`]);
//! - a float, the message area or windows rearranged slide to where they are
//!   going ([`anim::motion`]);
//! - a blinking cursor fades in and out ([`anim::blink`]);
//! - and, from animate.nvim rather than Neovide, a split window opening flies
//!   in from its side as its text fades in, one closing flies back into its
//!   side as its text dims away, one changing size moves its edges there
//!   ([`anim::layout`]), and one showing another buffer fades from one to the
//!   other ([`anim::switch`]).
//!
//! What a terminal cannot do is not imitated: no blur behind floats, no
//! movement finer than a cell for text, no fonts, no transparency of its own.
//!
//! # Where it runs
//!
//! In the same place `nvim --remote-ui` would: a child of nvmux on a pty
//! (`nvmux --client <sock>`, see [`crate::cli`]), its output relayed to the
//! terminal and its input fed from it, every rule of the relay unchanged. So
//! it opens, closes and asks its questions the way Neovim's TUI does (see
//! [`term`]), and everything the relay builds on that — the held first paint,
//! the notice, the hint row, kept clients and the ledger that puts their modes
//! back — works for it as it does for that one.
//!
//! # What it asks of Neovim
//!
//! `nvim_ui_attach` with `ext_linegrid` and `ext_multigrid`: every window, the
//! message area and every float on a grid of its own, placed by the client
//! ([`compose`]). That is what makes a window something that can be seen to
//! move, and a scroll something that happened to one window rather than to a
//! rectangle of the screen.
//!
//! Neovim keeps windows on grids of their own only while every UI attached
//! asks for it, though. With one attached that does not — Neovim's own TUI in
//! another nvmux, say — it draws the windows on grid 1 for everybody, and
//! sends floats it does not say where to put. So the client asks who else is
//! attached before it attaches, and again whenever that changes (Neovim says
//! each of the editor-wide `ext_*` options again when it does), and while such
//! a UI is there it is attached without `ext_multigrid`: everything is on grid
//! 1, and what is lost is what needs windows of their own: their moving. An
//! nvmux client says in its client info that it wants
//! `ext_multigrid`, so that one only passing through grid 1 does not send the
//! rest there with it.
//!
//! A UI attaching with `ext_multigrid` to a session another UI has drawn is
//! sent no window at all, besides: no placement, and nothing drawn on its
//! grid. Neovim keeps a window's grid from one UI to the next and sends its
//! cells only when they change, and nothing has, or a few have. On a terminal
//! of another size than that UI's, a window Neovim resizes is placed again,
//! and drawn only where the resize added to it: under the rows it kept, or to
//! the right of each of them. So the client asks where every window it is told
//! of is ([`LAYOUT_LUA`]), and while Neovim has yet to draw every cell of one
//! for it ([`model::Model::lacking`]), has Neovim draw the windows afresh by
//! having it put them on grid 1 and back: a second connection attaches a UI
//! without `ext_multigrid` for as long as Neovim takes to draw for it, and
//! goes ([`refresh`]). The windows of a tab page the client has not seen are
//! the same, the first time it is shown. Drawing waits for both, for up to
//! [`HOLD_LIMIT`], so that what the terminal shows is the editor whole.
//!
//! Two things the windows' animations need are not in what Neovim draws for a
//! UI at all: which buffer a window shows — a window switching buffers fades
//! from one to the other — and the options that decide how windows are laid
//! out. So a multigrid attach leaves an agent in the editor ([`AGENT_LUA`]),
//! a handful of autocommands that tell this client, and it only, of both.
//!
//! # Cards
//!
//! What nvmux has to say while a session is in front — which session a switch
//! landed in, what the other sessions report — it tells this client to show,
//! in a sequence on its input no terminal sends (see [`crate::card`]); and the
//! client draws it in the margin, over every frame it composes ([`cards`]).
//! Nothing of it is the editor's, and nothing of it is asked of the editor.
//!
//! # One thread
//!
//! One `poll` over the terminal, the socket, and the two signal pipes, with a
//! timeout that is the next animation frame, the `'ttimeoutlen'` an escape is
//! held for, or the end of a wait for what a multigrid attach lacks — the
//! shape of the relay's own loop, for the relay's reasons. Nothing animates
//! while nothing moves: an idle client sleeps in `poll`. The one thing done
//! off it is a refresh, which blocks on a connection of its own for as long
//! as Neovim takes to draw for it, and has nothing to tell the loop: its work
//! arrives on the client's own socket ([`refresh`]).
//!
//! Nor, with `[client] throttle` on, while nobody sees it: a client nvmux has
//! put aside — parked, or started ahead of its session's first visit — is
//! told so ([`crate::card::Command::Away`]), and until it is back in front it
//! animates nothing and draws what changes at most once every
//! [`AWAY_FRAME_GAP`], however busy its session is.

pub mod anim;
pub mod cards;
pub mod compose;
pub mod grid;
pub mod input;
pub mod model;
pub mod predict;
pub mod redraw;
pub mod screen;
pub mod style;
pub mod term;
pub mod wide;
pub mod wire;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use rmpv::ValueRef;

use self::anim::{Animator, Effects};
use self::grid::Cell;
use self::input::{Input, Mouse, Parser, Reply};
use self::model::{Changes, Model, Place, Placement};
use self::predict::{Command, Mode, Predictor, Side};
use self::redraw::Event;
use self::screen::Screen;
use self::wire::{Arg, Inbox, Outbox};
use crate::palette::{Palette, Rgb};

/// The options that lay windows out, as the agent gives them: `'laststatus'`,
/// `'splitright'`, `'splitbelow'`.
fn options(v: &[ValueRef<'_>]) -> anim::layout::Options {
    let dflt = anim::layout::Options::default();
    let flag = |i: usize, d: bool| match v.get(i) {
        Some(ValueRef::Boolean(b)) => *b,
        _ => d,
    };
    anim::layout::Options {
        laststatus: v.first().and_then(redraw::int).unwrap_or(dflt.laststatus),
        splitright: flag(1, dflt.splitright),
        splitbelow: flag(2, dflt.splitbelow),
    }
}

/// What each window wants, as the measure says (see [`wide::MEASURE_LUA`]),
/// or `None` where it said nothing of them.
fn wants(v: Option<&ValueRef<'_>>) -> Option<BTreeMap<i64, usize>> {
    let v = v?.as_array()?;
    Some(
        v.iter()
            .filter_map(|pair| {
                let pair = pair.as_array()?;
                let win = redraw::int(pair.first()?)?;
                let wanted = usize::try_from(redraw::int(pair.get(1)?)?).ok()?;
                Some((win, wanted))
            })
            .collect(),
    )
}

/// The environment variable a parent nvmux hands the terminal's colours down
/// in (see [`crate::palette::Palette::to_env`]): the client is on a pty and
/// cannot ask the terminal itself without the answers racing the user's keys.
pub const PALETTE_ENV: &str = "NVMUX_PALETTE";

/// The variable the settings a client draws with come in, from the nvmux
/// that started it: those nvmux runs on, which the file the client would
/// otherwise read does not have where the settings screen has changed them
/// and not saved (see [`crate::config::Drawing`]).
pub const SETTINGS_ENV: &str = "NVMUX_CLIENT";

/// The least time between two frames drawn for Neovim's flushes: a burst of
/// them — a `:terminal` running something — is drawn at most this often, the
/// last of them always. All but the second frame after a quiet spell, which
/// is drawn at once: see [`App::dirty_due`].
const MIN_FRAME_GAP: Duration = Duration::from_millis(4);

/// The least time between two frames while the client is away: parked or
/// started ahead, its screen seen by nobody (see
/// [`crate::card::Command::Away`]). Drawn at all only for what nvmux keeps of
/// the screen meanwhile, which the client brings up to date the moment it is
/// back in front; and a session that draws all the while — a build, a log —
/// is then a frame a second, not one every few milliseconds.
const AWAY_FRAME_GAP: Duration = Duration::from_secs(1);

/// How long drawing waits for what a multigrid attach lacks — where its
/// windows are, what is on them, the other UIs to settle — before drawing
/// what there is. See the module docs.
const HOLD_LIMIT: Duration = Duration::from_millis(500);

/// How long a terminal's answer part way through is waited for, from the last
/// of it to come, before what has come of it is dropped.
const REPLY_WAIT: Duration = Duration::from_secs(1);

/// How soon to ask who is attached again, having found a UI only passing
/// through grid 1: another nvmux client's refresh, or one attaching again.
const RECHECK: Duration = Duration::from_millis(30);

/// How long a refresh is given to be answered before it is given up, and how
/// long before one that came to nothing is tried again.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(2);

/// How many times a window is refreshed for, at most, before it is drawn as
/// it is: Neovim does not draw for a UI attaching while it waits for Enter.
const REFRESH_TRIES: u8 = 2;

/// Who else is attached: `{ foreign, passing }` — a UI but this one without
/// `ext_multigrid` for good, and one without it only for now, which an nvmux
/// client says in its client info (see [`App::client_info`]).
const UIS_LUA: &str = "local mine = ...
local foreign, passing = false, false
for _, ui in ipairs(vim.api.nvim_list_uis()) do
  if ui.chan ~= mine and not ui.ext_multigrid then
    local client = vim.api.nvim_get_chan_info(ui.chan).client or {}
    if (client.attributes or {}).multigrid == 'wanted' then
      passing = true
    else
      foreign = true
    end
  end
end
return { foreign, passing }";

/// Where each of the windows given is, for windows Neovim has not placed for
/// this UI: `{ 0 }` for one not to be seen — on another tab page, hidden, or
/// gone — `{ 1, row, col, top, width, height }` for a window in the layout,
/// and `{ 2, row, col, top, bottom, left, right, zindex, mouse, anchor }` for
/// a float, with the margins `win_viewport_margins` would have said — a
/// winbar, a border — and the window it is anchored to, or -1.
const LAYOUT_LUA: &str = "local cur = vim.api.nvim_get_current_tabpage()
local out = {}
for i, w in ipairs({ ... }) do
  local ok, r = pcall(function()
    local cfg = vim.api.nvim_win_get_config(w)
    if vim.api.nvim_win_get_tabpage(w) ~= cur or cfg.hide then
      return { 0 }
    end
    local pos = vim.api.nvim_win_get_position(w)
    local bar = vim.wo[w].winbar ~= '' and 1 or 0
    if (cfg.relative or '') == '' then
      return { 1, pos[1], pos[2], bar, vim.api.nvim_win_get_width(w), vim.api.nvim_win_get_height(w) }
    end
    local b = type(cfg.border) == 'table' and cfg.border or {}
    local function side(k)
      if #b == 0 then
        return 0
      end
      local c = b[(k - 1) % #b + 1]
      if type(c) == 'table' then
        c = c[1]
      end
      return (c ~= nil and c ~= '') and 1 or 0
    end
    local mouse = cfg.focusable == false and 0 or 1
    local anchor = cfg.relative == 'win' and cfg.win or -1
    return { 2, pos[1], pos[2], bar + side(2), side(6), side(8), side(4), cfg.zindex or 50, mouse,
      anchor }
  end)
  out[i] = ok and r or { 0 }
end
return out";

/// The client's agent in the editor, given the client's channel and whether
/// the client predicts scrolls: it tells the client, in `nvmux_client`
/// notifications, of a buffer entering a window (`buf`, window, buffer), a
/// window made (`win`, window, buffer) or closed (`closed`, window), and the
/// options that lay windows out changing (`options`, `'laststatus'`,
/// `'splitright'`, `'splitbelow'`) — and answers with those options and the
/// buffer of every window there is. It goes when this client's UI does, or
/// the first time it finds the channel gone.
///
/// For a client that predicts, it also sends what the predictions are drawn
/// from (see [`predict`]): for each window of the tab page, as its view
/// moves, the lines either side of the view — as many windows as its fifth
/// argument says the client may scroll ahead, or a `User` autocommand
/// `nvmux_client_reach_<chan>` says since, with the number as its data; two
/// at first, the rest a moment later, no more than a few hundred lines at a
/// time; and none at all while that number is 0, as it is while the link is
/// too fast for predictions (see [`Predictor::agent_reach`]), what was sent
/// let go of — and the view's own (`lines`, window, buffer, `b:changedtick`,
/// first line, lines, whether they replace what the client has, the most
/// bytes of a line sent), only those the client does not have, by the same
/// rule as [`predict`] keeps them by, and each no longer than the window
/// could show; of those the client has, no more than a few thousand, the
/// end away from the lines being sent let go (`keep`, window, buffer,
/// `b:changedtick`, the first line kept, the line after the last); as its
/// buffer changes, each edit, once typing pauses
/// (`splice`, window, buffer, the `b:changedtick` the client's lines are at
/// and the one they are brought to, first line, the line after the last it
/// replaced, how many lines replace them, and those lines where they touch
/// what the client has, the most bytes of a line sent), or the lines afresh
/// where the edits are more than is worth sending that way. With the lines,
/// the colours the editor's highlighting gives them (a list a line of start
/// byte, end byte or -1 for the line's end, and highlight group), and again
/// where an edit or a new parse changes them (`spans`, window, buffer,
/// `b:changedtick`, first line, the lists); before them, the attributes of
/// each group the client has yet to be sent, in both of `hl_attr_define`'s
/// forms, and all of them again with a new colour scheme (`colours`, a list
/// of group, GUI attributes, cterm attributes). And what to draw them with
/// and how far its
/// scrolls go, when that changes (`shape`, window, see [`predict::Shape`]) —
/// among it, which of the keys the client follows, given it as the agent's
/// third argument and those in insert mode as its fourth, something maps in
/// the window's buffer; a view moved sideways, as Neovim moves it (`leftcol`,
/// window, its first column); and each time Neovim moves a view while it is
/// behind on the keys typed (`behind`), which it sends no frame for till it
/// has caught up — what the client scrolled ahead of it waits for it
/// meanwhile (see [`Predictor::deadline`]).
const AGENT_LUA: &str = "local chan, predict, keys, ikeys, reach = ...
local api = vim.api
local group = api.nvim_create_augroup('nvmux_client_' .. chan, { clear = true })
-- Whether this client has gone: what is left of the agent stops.
local gone = false
local function send(...)
  if not pcall(vim.rpcnotify, chan, 'nvmux_client', ...) then
    gone = true
    pcall(api.nvim_del_augroup_by_id, group)
  end
end
local function au(event, opts, f)
  opts.group, opts.callback = group, f
  api.nvim_create_autocmd(event, opts)
end
au('BufWinEnter', {}, function(ev)
  send('buf', api.nvim_get_current_win(), ev.buf)
end)
au('WinNew', {}, function()
  local win = api.nvim_get_current_win()
  send('win', win, api.nvim_win_get_buf(win))
end)
au('WinClosed', {}, function(ev)
  send('closed', tonumber(ev.match))
end)
au('OptionSet', { pattern = { 'laststatus', 'splitright', 'splitbelow' } }, function()
  send('options', vim.o.laststatus, vim.o.splitright, vim.o.splitbelow)
end)
au('UILeave', {}, function()
  if vim.v.event.chan == chan then
    gone = true
    pcall(api.nvim_del_augroup_by_id, group)
  end
end)
if predict then
  local have, shapes, maps = {}, {}, {}
  local soon
  -- The keys something maps, or starts a mapping with, in the window's
  -- buffer: looked up again a second after they last were.
  local function mapped(buf)
    local m = maps[buf]
    if m and vim.uv.now() - m.at < 1000 then
      return m.keys, m.ikeys
    end
    local function look(list, modes)
      local out = {}
      for _, key in ipairs(list) do
        local ms = key:find('ScrollWheel') and { 'n', 'x', 'i' } or modes
        for _, mode in ipairs(ms) do
          if vim.fn.mapcheck(key, mode) ~= '' then
            out[#out + 1] = key
            break
          end
        end
      end
      return out
    end
    maps[buf] = { at = vim.uv.now(), keys = look(keys, { 'n', 'x' }), ikeys = look(ikeys, { 'i' }) }
    return maps[buf].keys, maps[buf].ikeys
  end
  local function shape(buf, info)
    local ok = not vim.wo.diff and vim.bo[buf].buftype ~= 'terminal'
    local page = 0
    if #api.nvim_tabpage_list_wins(0) == 1 and vim.o.window > 0
        and vim.o.window < vim.o.lines - 1 then
      page = math.max(1, vim.o.window - 2)
    end
    local so, siso = vim.wo.scrolloff, vim.wo.sidescrolloff
    if so < 0 then
      so = vim.go.scrolloff
    end
    if siso < 0 then
      siso = vim.go.sidescrolloff
    end
    local nmaps, imaps = mapped(buf)
    return { buf, api.nvim_buf_line_count(buf), info.textoff, vim.bo[buf].tabstop,
      vim.wo.number, vim.wo.relativenumber, vim.wo.statuscolumn == '', vim.wo.wrap,
      vim.fn.winsaveview().leftcol, ok, so,
      tonumber(vim.o.mousescroll:match('ver:(%d+)')) or 3,
      vim.opt.fillchars:get().eob or '~', vim.wo.list,
      vim.wo.list and vim.opt.listchars:get().tab or '', vim.wo.scroll, page,
      vim.o.startofline, nmaps, imaps, siso, vim.o.sidescroll,
      tonumber(vim.o.mousescroll:match('hor:(%d+)')) or 6 }
  end
  -- No more of a line than could be shown, and a cell over for the client
  -- to see it would not fit: a minified file is a line a megabyte long.
  local function get(buf, from, to, cap)
    local lines = api.nvim_buf_get_lines(buf, from, to, false)
    for i, l in ipairs(lines) do
      if #l > cap then
        lines[i] = l:sub(1, cap)
      end
    end
    return lines
  end
  -- The highlight groups the client has been sent the attributes of, true;
  -- false for one with none, whose spans are left out.
  local groups = {}
  local function attrs(gid)
    local a = api.nvim_get_hl(0, { id = gid, link = false })
    local function flags(from, into)
      for _, k in ipairs({ 'bold', 'italic', 'underline', 'undercurl', 'underdouble',
          'underdotted', 'underdashed', 'strikethrough', 'reverse', 'altfont' }) do
        if from[k] then
          into[k] = true
        end
      end
      return into
    end
    local rgb = flags(a, vim.empty_dict())
    rgb.foreground, rgb.background, rgb.special = a.fg, a.bg, a.sp
    local cterm = flags(a.cterm or {}, vim.empty_dict())
    cterm.foreground, cterm.background = a.ctermfg, a.ctermbg
    return next(a) ~= nil, rgb, cterm
  end
  -- The colours the editor's own highlighting gives lines from to to of a
  -- buffer — its tree-sitter captures, and the highlights put on it, such
  -- as a language server's semantic tokens — a list a line of start byte,
  -- end byte (-1 to the line's end) and highlight group, later over earlier
  -- as priorities have it. The groups' attributes go to the client first.
  local function colours(buf, from, to)
    local found, fresh = {}, {}
    local function add(sr, sc, er, ec, gid, prio, sub)
      if not gid or gid <= 0 or er < from or sr >= to then
        return
      end
      if groups[gid] == nil then
        local some, rgb, cterm = attrs(gid)
        groups[gid] = some
        if some then
          fresh[#fresh + 1] = { gid, rgb, cterm }
        end
      end
      if groups[gid] then
        found[#found + 1] = { sr, sc, er, ec, gid, prio, sub, #found }
      end
    end
    local hl = vim.treesitter.highlighter.active[buf]
    if hl then
      pcall(function()
        hl.tree:parse({ from, to })
        local sub = 0
        hl.tree:for_each_tree(function(tstree, ltree)
          sub = sub + 1
          local q = hl:get_query(ltree:lang())
          local query = q and q:query()
          if not query then
            return
          end
          for id, node, meta in query:iter_captures(tstree:root(), buf, from, to) do
            local r = vim.treesitter.get_range(node, buf, meta and meta[id])
            local prio = tonumber(meta.priority or (meta[id] and meta[id].priority)) or 100
            add(r[1], r[2], r[4], r[5], q:get_hl_from_capture(id), prio, sub)
          end
        end)
      end)
    end
    local ok, marks = pcall(api.nvim_buf_get_extmarks, buf, -1, { from, 0 }, { to - 1, -1 },
      { details = true, type = 'highlight', overlap = true })
    for _, m in ipairs(ok and marks or {}) do
      local d = m[4]
      for _, g in ipairs(type(d.hl_group) == 'table' and d.hl_group or { d.hl_group }) do
        local gid = type(g) == 'number' and g or api.nvim_get_hl_id_by_name(g)
        add(m[2], m[3], d.end_row or m[2], d.end_col or m[3], gid, d.priority or 4096, 0)
      end
    end
    if #fresh > 0 then
      send('colours', fresh)
    end
    table.sort(found, function(x, y)
      if x[6] ~= y[6] then
        return x[6] < y[6]
      elseif x[7] ~= y[7] then
        return x[7] < y[7]
      end
      return x[8] < y[8]
    end)
    local out = {}
    for i = 1, to - from do
      out[i] = {}
    end
    for _, f in ipairs(found) do
      for r = math.max(f[1], from), math.min(f[3], to - 1) do
        if r < f[3] or f[4] > 0 or r == f[1] then
          local line = out[r - from + 1]
          line[#line + 1] = r == f[1] and f[2] or 0
          line[#line + 1] = r == f[3] and f[4] or -1
          line[#line + 1] = f[5]
        end
      end
    end
    return out
  end
  -- Rows of a buffer whose colours may have changed since they were sent,
  -- a few runs of them: its tree-sitter tree changed there, or the lines did.
  local stale, followed, pushed = {}, {}, {}
  local function spoil(buf, lo, hi)
    local runs, merged = stale[buf] or {}, { lo, hi }
    local kept = {}
    for _, r in ipairs(runs) do
      if r[1] <= merged[2] and r[2] >= merged[1] then
        merged = { math.min(r[1], merged[1]), math.max(r[2], merged[2]) }
      else
        kept[#kept + 1] = r
      end
    end
    kept[#kept + 1] = merged
    if #kept > 16 then
      local all = { math.huge, -1 }
      for _, r in ipairs(kept) do
        all = { math.min(all[1], r[1]), math.max(all[2], r[2]) }
      end
      kept = { all }
    end
    stale[buf] = kept
  end
  local function follow(buf)
    local hl = vim.treesitter.highlighter.active[buf]
    if not hl or followed[buf] == hl.tree then
      return
    end
    local tree = hl.tree
    followed[buf] = tree
    -- A parse reports as changed not only what an edit changed but every row
    -- whose injected trees it made or let go of; and a parse of other rows
    -- than the last lets go of the last one's. The agent's parse of the rows
    -- it colours and the highlighter's of the view would report each other's
    -- rows for good, each report a push and each push a parse: an editor
    -- busy for as long as the client is there, a C file's macros enough.
    -- Only an edit changes what colours a row has. So of the reports at one
    -- b:changedtick, those up to the end of the first push to end after the
    -- first of them are heard, and none after. Not only those of the parse
    -- that took the edit, which the highlighter makes a few milliseconds at
    -- a time: what an edit did to a tree injected into the buffer — a
    -- markdown code block's, where a comment opened colours rows well past
    -- the edit — that tree alone reports, and it is often parsed first by
    -- that push's own parse of the rows edited. Those the edit itself
    -- changed are sent again whatever is heard (see flush).
    local heard, hearing
    tree:register_cbs({
      on_changedtree = function(ranges)
        if gone or followed[buf] ~= tree then
          return
        end
        local tick = api.nvim_buf_get_changedtick(buf)
        if heard == tick then
          return
        end
        hearing = tick
        for _, r in ipairs(ranges) do
          spoil(buf, r[1], r[4] + 1)
        end
        soon()
      end,
    }, true)
    pushed[buf] = function()
      if followed[buf] == tree and hearing == api.nvim_buf_get_changedtick(buf) then
        heard = hearing
      end
    end
  end
  -- Edits to a buffer since the client was sent its lines, in order: each
  -- left lines a to b of the buffer as it was before it as lines a to c,
  -- whose text it keeps. One next to the last is taken into it. Sent as
  -- splices when the windows are next pushed, so that an edit costs the
  -- lines it changed, not every line the client keeps; past 2000 lines of
  -- it, the client is sent its lines afresh instead.
  local edits, ticks, attached = {}, {}, {}
  local function edited(buf, tick, first, last, new)
    local shown = false
    for _, h in pairs(have) do
      shown = shown or h.buf == buf
    end
    if gone or not shown then
      attached[buf], edits[buf] = nil, nil
      return true
    end
    local e = edits[buf] or { base = ticks[buf], list = {}, size = 0 }
    edits[buf], e.tick = e, tick
    if not first or e.over then
      return
    end
    if new - first > 2000 then
      e.over = true
      return
    end
    local text = api.nvim_buf_get_lines(buf, first, new, false)
    local p = e.list[#e.list]
    if p and first <= p.c and last >= p.a then
      local t = {}
      for i = 1, first - p.a do
        t[#t + 1] = p.text[i]
      end
      for _, l in ipairs(text) do
        t[#t + 1] = l
      end
      for i = last - p.a + 1, #p.text do
        t[#t + 1] = p.text[i]
      end
      e.size = e.size - #p.text + #t
      p.a, p.b, p.c = math.min(p.a, first), last > p.c and p.b + last - p.c or p.b,
        math.max(p.c, last) + new - last
      p.text, p.tick = t, tick
    else
      e.list[#e.list + 1] = { a = first, b = last, c = new, text = text, tick = tick }
      e.size = e.size + #text
    end
    e.over = e.size > 2000 or #e.list > 100
  end
  local function attach(buf)
    if attached[buf] then
      return
    end
    attached[buf] = api.nvim_buf_attach(buf, false, {
      on_lines = function(_, b, tick, first, last, new)
        return edited(b, tick, first, last, new)
      end,
      on_changedtick = function(_, b, tick)
        return edited(b, tick)
      end,
      on_reload = function(_, b)
        edits[b] = nil
        for win, h in pairs(have) do
          if h.buf == b then
            have[win] = nil
          end
        end
        soon()
      end,
      on_detach = function(_, b)
        attached[b], edits[b] = nil, nil
      end,
    })
  end
  -- Each window's lines brought up to date with the edits, one splice an
  -- edit, as the client keeps them: with the edit's text where it touches
  -- what the client has, the lines after it moved where it does not.
  local function flush()
    for buf, e in pairs(edits) do
      edits[buf], ticks[buf] = nil, e.tick
      -- The lines an edit brings have no colours with them: theirs come
      -- after, where the editor gives any — found where the edits after it
      -- moved them to.
      if vim.treesitter.highlighter.active[buf] then
        local runs = {}
        for _, p in ipairs(e.list) do
          local delta = (p.c - p.a) - (p.b - p.a)
          for _, r in ipairs(runs) do
            if r[1] >= p.b then
              r[1], r[2] = r[1] + delta, r[2] + delta
            elseif r[2] > p.a then
              r[2] = math.max(r[2] + delta, p.c)
            end
          end
          runs[#runs + 1] = { p.a, p.c + 1 }
        end
        for _, r in ipairs(runs) do
          spoil(buf, math.max(r[1], 0), r[2])
        end
      end
      for win, h in pairs(have) do
        if h.buf == buf and (h.tick ~= e.base or e.over) then
          have[win] = nil
        elseif h.buf == buf then
          local base, n = e.base, #e.list
          for i = 1, math.max(n, 1) do
            local p = e.list[i] or { a = 0, b = 0, c = 0, text = {} }
            local tick = i >= n and e.tick or p.tick
            local delta = (p.c - p.a) - (p.b - p.a)
            local lines = {}
            if p.a <= h.hi and p.b >= h.lo then
              for j, l in ipairs(p.text) do
                lines[j] = #l > h.cap and l:sub(1, h.cap) or l
              end
              h.lo, h.hi = math.min(h.lo, p.a), math.max(h.hi + delta, p.c)
            elseif p.b < h.lo then
              h.lo, h.hi = h.lo + delta, h.hi + delta
            end
            send('splice', win, buf, base, tick, p.a, p.b, p.c - p.a, lines, h.cap)
            base = tick
          end
          h.tick = e.tick
          if h.hi - h.lo > 4000 then
            have[win] = nil
          end
        end
      end
    end
  end
  local function push(win, spoilt)
    if api.nvim_win_get_config(win).relative ~= '' then
      return
    end
    local buf = api.nvim_win_get_buf(win)
    local info = vim.fn.getwininfo(win)[1]
    local n = api.nvim_buf_line_count(buf)
    local cap = 4 * (vim.fn.winsaveview().leftcol + info.width) + 64
    local tick = api.nvim_buf_get_changedtick(buf)
    -- As many windows either side as the client may scroll ahead over the
    -- link as it is now; only two when they are sent afresh, the rest a
    -- moment later and a few hundred lines a push, so that neither a buffer
    -- coming into a window nor a reach grown holds up Neovim's own redraw,
    -- or the keys typed meanwhile, for long.
    local function around(k)
      local m = math.min(k * info.height + 10, 1000)
      return math.max(0, info.topline - 1 - m), math.min(n, info.botline + m)
    end
    local lo, hi = around(reach)
    local h = have[win]
    local joins = h and h.buf == buf and h.tick == tick and h.cap >= cap
      and lo <= h.hi and hi >= h.lo
    if not joins then
      lo, hi = around(2)
    end
    local s = shape(buf, info)
    if s[10] and vim.wo.foldenable then
      for l = lo + 1, hi do
        if vim.fn.foldclosed(l) ~= -1 then
          s[10] = false
          break
        end
      end
    end
    if s[10] and #api.nvim_buf_get_extmarks(buf, -1, { lo, 0 }, { math.max(hi - 1, 0), -1 },
        { type = 'virt_lines', limit = 1 }) > 0 then
      s[10] = false
    end
    local key = table.concat(vim.tbl_map(tostring, { unpack(s, 1, 18) }), '\\0')
      .. '\\0' .. table.concat(s[19], ' ') .. '\\0' .. table.concat(s[20], ' ')
      .. '\\0' .. table.concat({ s[21], s[22], s[23] }, ',')
    if shapes[win] ~= key then
      shapes[win] = key
      send('shape', win, s)
    end
    if joins then
      -- No more than 300 lines a push, shared between the two sides where
      -- both want them, the rest 20 ms later; and no more than 4000 kept,
      -- those furthest from the lines sent let go, so that a long run of
      -- pages never has the client start again from two windows.
      local want_lo, want_hi = math.max(h.lo - lo, 0), math.max(hi - h.hi, 0)
      local give_lo = math.min(want_lo, math.max(150, 300 - want_hi))
      local give_hi = math.min(want_hi, 300 - give_lo)
      local klo, khi = h.lo - give_lo, h.hi + give_hi
      if khi - klo > 4000 and give_hi > 0 then
        klo = khi - 4000
      elseif khi - klo > 4000 then
        khi = klo + 4000
      end
      if klo > h.lo or khi < h.hi then
        h.lo, h.hi = math.max(klo, h.lo), math.min(khi, h.hi)
        send('keep', win, buf, tick, h.lo, h.hi)
      end
      for _, r in ipairs(spoilt[buf] or {}) do
        local a, b = math.max(r[1], h.lo), math.min(r[2], h.hi)
        if a < b then
          send('spans', win, buf, tick, a, colours(buf, a, b))
        end
      end
      if klo < h.lo then
        send('lines', win, buf, tick, klo, get(buf, klo, h.lo, cap), false, cap,
          colours(buf, klo, h.lo))
        h.lo = klo
      end
      if khi > h.hi then
        send('lines', win, buf, tick, h.hi, get(buf, h.hi, khi, cap), false, cap,
          colours(buf, h.hi, khi))
        h.hi = khi
      end
      if give_lo < want_lo or give_hi < want_hi then
        vim.defer_fn(soon, 20)
      end
    else
      send('lines', win, buf, tick, lo, get(buf, lo, hi, cap), true, cap, colours(buf, lo, hi))
      have[win] = { buf = buf, tick = tick, lo = lo, hi = hi, cap = cap }
      ticks[buf] = tick
      attach(buf)
      vim.defer_fn(soon, 100)
    end
    -- Changes to the colours from here on: not the parse the lines just sent
    -- took, which they have.
    follow(buf)
  end
  local queued, scrolled = false, false
  local function push_all()
    -- Nothing at all while the client keeps no lines, its link too fast for
    -- predictions: what was sent is let go of, to be sent afresh should the
    -- link slow. Before anything else, as a scroll pushes.
    if reach <= 0 then
      have, shapes, edits, stale = {}, {}, {}, {}
      return
    end
    if not pcall(api.nvim_get_autocmds, { group = group }) then
      return
    end
    flush()
    -- What was spoilt is sent again now; what pushing spoils, as a parse of
    -- lines not parsed before may, next time.
    local spoilt = stale
    stale = {}
    for _, win in ipairs(api.nvim_tabpage_list_wins(0)) do
      if api.nvim_win_is_valid(win) then
        pcall(api.nvim_win_call, win, function()
          push(win, spoilt)
        end)
      end
    end
    -- What was heard at a b:changedtick is all there is to hear at it: see
    -- follow.
    for _, done in pairs(pushed) do
      done()
    end
  end
  soon = function()
    if not queued then
      queued = true
      vim.schedule(function()
        queued, scrolled = false, false
        push_all()
      end)
    end
  end
  local timer = vim.uv.new_timer()
  au({ 'WinResized', 'BufWinEnter', 'WinNew', 'TabEnter', 'VimResized', 'CursorHold',
    'CursorHoldI' }, {}, function()
    soon()
  end)
  -- A view moved while Neovim is behind on the keys typed: it takes them
  -- before anything scheduled, so a push asked for at one scroll and not yet
  -- made at the next would wait for the last of them, and the pages typed
  -- ahead for their lines. Pushed with each scroll instead, till it has
  -- caught up. Nor does it send a frame till then, and a frame is what
  -- lands the scrolls the client made ahead of it: so the client is told,
  -- with each scroll, that Neovim is still at work on them.
  au('WinScrolled', {}, function()
    if scrolled then
      if reach > 0 then
        send('behind')
      end
      push_all()
    end
    scrolled = true
    soon()
  end)
  -- A view moved sideways, said at once: Neovim draws it next, and nothing
  -- else it sends says so.
  au('WinScrolled', {}, function()
    for id, d in pairs(vim.v.event) do
      local win = tonumber(id)
      if win and d.leftcol ~= 0 and api.nvim_win_is_valid(win) then
        send('leftcol', win, vim.fn.getwininfo(win)[1].leftcol)
      end
    end
  end)
  -- New colours for every group the client knows.
  au('ColorScheme', {}, function()
    local again = {}
    for gid in pairs(groups) do
      local some, rgb, cterm = attrs(gid)
      groups[gid] = some
      again[#again + 1] = { gid, rgb, cterm }
    end
    if #again > 0 then
      send('colours', again)
    end
  end)
  -- An edit is sent once typing pauses: see flush.
  au({ 'TextChanged', 'TextChangedI', 'TextChangedP' }, {}, function()
    timer:stop()
    timer:start(150, 0, vim.schedule_wrap(soon))
  end)
  au('OptionSet', { pattern = { 'number', 'relativenumber', 'statuscolumn', 'wrap',
    'tabstop', 'list', 'listchars', 'fillchars', 'scrolloff', 'mousescroll', 'foldenable',
    'foldcolumn', 'signcolumn', 'numberwidth', 'diff', 'buftype', 'scroll', 'window',
    'startofline', 'sidescrolloff', 'sidescroll' } }, function()
    soon()
  end)
  au('WinClosed', {}, function(ev)
    local win = tonumber(ev.match)
    have[win], shapes[win] = nil, nil
  end)
  -- How far the client may scroll ahead, again, as the round trip moves.
  au('User', { pattern = 'nvmux_client_reach_' .. chan }, function(ev)
    reach = tonumber(ev.data) or reach
    soon()
  end)
  au('UILeave', {}, function()
    if vim.v.event.chan == chan then
      timer:close()
    end
  end)
  soon()
end
local wins = {}
for _, w in ipairs(api.nvim_list_wins()) do
  wins[#wins + 1] = { w, api.nvim_win_get_buf(w) }
end
return { vim.o.laststatus, vim.o.splitright, vim.o.splitbelow, wins }";

/// Where a scroll ahead lands the cursor in window `win`, `width` columns
/// wide, as [`anim::Lander`] has it: from display column `want`, on a line
/// the client has been sent.
fn landed(
    p: &Predictor,
    win: i64,
    shape: &predict::Shape,
    l: anim::Land,
    want: usize,
    width: usize,
) -> Option<Option<(usize, usize)>> {
    let text = p.text(win, l.line)?;
    let (vcol, want) = if l.first {
        let v = predict::first_nonblank(text, shape);
        (v, v)
    } else {
        (predict::landing(text, shape, want, l.insert), want)
    };
    Some(
        (vcol + shape.textoff)
            .checked_sub(shape.leftcol)
            .filter(|&col| col < width)
            .map(|col| (col, want)),
    )
}

/// What the client has asked and not been answered.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ask {
    ApiInfo,
    /// [`UIS_LUA`].
    Uis,
    /// [`LAYOUT_LUA`], for these grids and their windows.
    Layout(Vec<(u64, i64)>),
    /// The detach of attaching again the other way.
    Detach,
    /// [`AGENT_LUA`].
    Agent,
    /// [`wide::MEASURE_LUA`].
    Measure,
    /// `nvim_get_mode`, for the round trip it takes: see [`predict`].
    Ping,
    /// A request asked after keys, whose answer says Neovim has taken them
    /// and drawn what they did: see [`predict::Typed`]. With the count of
    /// inputs it was asked after.
    Fence(u64),
}

/// What a scroll the client predicts was made by: keys, which are predicted
/// in normal and visual mode — or in insert mode, those that scroll there —
/// once Neovim has caught up with every key before them; a turn of the wheel
/// by a page, in normal and visual mode; or by lines, in insert mode as
/// well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum By {
    Keys,
    InsertKeys,
    PageWheel,
    Wheel,
}

/// How the client is attached: see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attach {
    /// Not yet: who else is attached decides how.
    Asking,
    Attached {
        multigrid: bool,
    },
    /// Detached, to attach again as `multigrid` says, and waiting for the
    /// detach to be answered: what comes before that is the old attach's.
    Leaving {
        multigrid: bool,
    },
}

/// The kitty keyboard question, from asking to settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keyboard {
    Asking,
    /// The terminal answered it — its protocol is on — and the device
    /// attributes that close the question are still to come.
    Closing,
    Settled,
}

/// A refresh asked for a grid: when, and how many times.
#[derive(Debug, Clone, Copy)]
struct Refreshed {
    at: Instant,
    tries: u8,
}

/// The client.
pub struct App {
    model: Model,
    screen: Screen,
    anim: Animator,
    parser: Parser,
    outbox: Outbox,
    /// What `redraw` has said since its last `flush`.
    batch: Vec<Event>,
    /// The terminal's size: columns, rows.
    size: (usize, usize),
    attach: Attach,
    /// Whether to attach with `ext_multigrid` where that can be had: an
    /// effect that needs windows of their own is on.
    multigrid_wanted: bool,
    /// Whether the client is on a terminal, as it tells Neovim.
    tty: term::Tty,
    keyboard: Keyboard,
    /// This client's channel, from `nvim_get_api_info`: how it tells itself
    /// apart among the UIs attached.
    chan: Option<u64>,
    asked: HashMap<u32, Ask>,
    /// The UIs were asked after and have not answered; and they changed
    /// again since they were asked.
    checking: bool,
    recheck: bool,
    /// When to ask after them again, having found one passing through.
    recheck_at: Option<Instant>,
    /// The UIs changed, and are not known to have settled.
    unsettled: bool,
    /// Layouts asked for and not answered.
    placing: usize,
    /// The window grids to be seen that Neovim has yet to draw every cell of,
    /// as of the last flush, and the refreshes asked for them.
    lacking: Vec<u64>,
    refreshed: HashMap<u64, Refreshed>,
    /// A refresh is to be started: see [`App::take_refresh`].
    refresh: bool,
    /// The agent has been asked for, this attach.
    agent: bool,
    /// How far either side of a view the agent was last told to keep lines:
    /// see [`Predictor::reach`].
    reach: i64,
    /// The buffer each window shows, by window, as the agent says.
    bufs: HashMap<i64, i64>,
    /// Windows that took another buffer since the last flush.
    switched: Vec<i64>,
    /// Since when drawing has waited for what a multigrid attach lacks.
    hold_since: Option<Instant>,
    /// Since when the parser has held an escape, or a sequence cut short.
    escape_since: Option<Instant>,
    /// The grid a mouse button went down on: its drag and its release go to
    /// the same one, wherever they are.
    pressed: Option<u64>,
    /// Mouse reporting as last set: on, and with motion.
    mouse: (bool, bool),
    /// A frame is wanted: something changed.
    dirty: bool,
    last_frame: Option<Instant>,
    /// When the frame before the last was drawn.
    frame_before: Option<Instant>,
    /// This attach has had its first flush: before it, nothing animates or
    /// is drawn.
    flushed: bool,
    /// Put aside, and not back in front yet: see [`AWAY_FRAME_GAP`].
    away: bool,
    /// What to write to the terminal before the next frame — the bell, the
    /// title, the server's own bytes — in order.
    say: Vec<u8>,
    /// The server is gone, or said to go.
    done: bool,
    /// Scrolling ahead of a slow link, if the config has it: see
    /// [`predict`].
    predict: Option<Predictor>,
    /// When each question asked to measure the round trip by was asked.
    asked_at: HashMap<u32, Instant>,
    /// When the user last typed, turned the wheel or pasted: the round trip
    /// is measured while they are at work.
    typed: Option<Instant>,
    /// The first columns windows' views have moved to, as the agent says,
    /// for the next frame: see [`anim::side`].
    lefts: Vec<(i64, usize)>,
    /// The cards nvmux has said to show, drawn over every frame: see
    /// [`cards`].
    cards: cards::Cards,
    /// `[client] centre`: each window's text in the middle of the window, on
    /// a terminal wider than it needs (see [`wide`]).
    centre: bool,
    /// `[client] window_width`: how many columns a window's text gets with
    /// it centred; 0 for no limit.
    window_width: usize,
    /// `[client] smart_width`: each window's width judged by what it shows,
    /// rather than `window_width` for all of them (see [`wide::MEASURE_LUA`]).
    smart: bool,
    /// The measure has been left in the editor, this attach.
    measuring: bool,
    /// The window grids asked to be narrower than their windows, and how
    /// wide: see [`wide`].
    narrowed: HashMap<u64, usize>,
    /// The window grids the client has cut to the width it asked for, which
    /// Neovim has not resized since, and how wide Neovim last had each: see
    /// [`App::narrow_windows`].
    cut: HashMap<u64, usize>,
    /// What each window of the tab page in front wants, by window, as the
    /// measure last said (see [`wide::MEASURE_LUA`]).
    wants: BTreeMap<i64, usize>,
    /// The window grids this attach has looked at for a width another
    /// client asked for, each once: see [`App::narrow_windows`].
    looked: HashSet<u64>,
}

impl App {
    /// A client drawing with `effects`, predicting scrolls if `predict`
    /// (see [`predict`]), on a terminal of `size`.
    pub fn new(term: Palette, effects: Effects, predict: bool, size: (usize, usize)) -> Self {
        Self {
            model: Model::new(term),
            screen: Screen::new(),
            // A prediction is a window's: windows of their own are wanted.
            multigrid_wanted: effects.want_multigrid() || predict,
            tty: term::Tty::detect(),
            anim: Animator::new(effects),
            parser: Parser::default(),
            outbox: Outbox::default(),
            batch: Vec::new(),
            size,
            attach: Attach::Asking,
            keyboard: Keyboard::Asking,
            chan: None,
            asked: HashMap::new(),
            checking: false,
            recheck: false,
            recheck_at: None,
            unsettled: false,
            placing: 0,
            lacking: Vec::new(),
            refreshed: HashMap::new(),
            refresh: false,
            agent: false,
            reach: 0,
            bufs: HashMap::new(),
            switched: Vec::new(),
            hold_since: None,
            escape_since: None,
            pressed: None,
            mouse: (false, false),
            dirty: false,
            last_frame: None,
            frame_before: None,
            flushed: false,
            away: false,
            say: Vec::new(),
            done: false,
            predict: predict.then(Predictor::default),
            asked_at: HashMap::new(),
            typed: None,
            lefts: Vec::new(),
            cards: cards::Cards::default(),
            centre: false,
            window_width: 0,
            smart: false,
            measuring: false,
            narrowed: HashMap::new(),
            cut: HashMap::new(),
            wants: BTreeMap::new(),
            looked: HashSet::new(),
        }
    }

    /// This client, with each window's text in the middle of the window if
    /// `centre`, `window_width` wide — or, `smart`, as wide as what it shows
    /// wants (see [`wide`]).
    pub fn with_centre(self, centre: bool, window_width: usize, smart: bool) -> Self {
        Self {
            centre,
            window_width,
            smart,
            ..self
        }
    }

    /// This client, writing each frame in as few bytes as it takes if `lean`
    /// (see [`screen`]).
    pub fn with_lean_output(mut self, lean: bool) -> Self {
        self.screen.set_lean(lean);
        self
    }

    /// Whether each window's text is centred, as it stands: not with no
    /// limit on a window's width.
    fn centring(&self) -> bool {
        self.centre && self.window_width != 0
    }

    /// Say who this client is, and attach — once it is known who else is
    /// attached, where that decides how.
    pub fn start(&mut self) {
        let id = self.outbox.request("nvim_get_api_info", &[]);
        self.asked.insert(id, Ask::ApiInfo);
        // Answered at once, busy or not, so a measure of the link alone.
        self.asked_at.insert(id, Instant::now());
        if let Some(p) = self.predict.as_mut() {
            p.typed.attached();
        }
        self.client_info();
        // Asked before anything an attach waits for, so its answer comes
        // first: what each window wants is known by the time the windows are
        // drawn. Nothing is, with nothing centred.
        if self.centring() {
            self.measure(None);
        }
        if self.multigrid_wanted {
            self.ask_uis();
        } else {
            self.attach_as(false);
        }
    }

    /// Ask the editor what each of its windows wants (see
    /// [`wide::MEASURE_LUA`]); given this client's channel, the measure stays
    /// and says so again whenever that changes.
    fn measure(&mut self, chan: Option<u64>) {
        let mut args = vec![Arg::Int(self.window_width as i64), Arg::Bool(self.smart)];
        if let Some(chan) = chan {
            args.push(Arg::Int(chan as i64));
        }
        let id = self.outbox.request(
            "nvim_exec_lua",
            &[Arg::str(wide::MEASURE_LUA), Arg::Array(args)],
        );
        self.asked.insert(id, Ask::Measure);
    }

    /// Leave the measure in the editor for this attach, once the client knows
    /// its own channel.
    fn leave_measure(&mut self) {
        if !self.centring() || self.measuring || self.attach == Attach::Asking {
            return;
        }
        if let Some(chan) = self.chan {
            self.measuring = true;
            self.measure(Some(chan));
        }
    }

    /// What the measure says: `'laststatus'`, and what each window wants.
    fn measured(
        &mut self,
        laststatus: Option<i64>,
        wants: Option<BTreeMap<i64, usize>>,
        now: Instant,
    ) {
        if let Some(w) = wants.filter(|w| *w != self.wants) {
            self.wants = w;
            self.narrow_windows(now);
            self.dirty = true;
        }
        if let Some(l) = laststatus {
            if self.model.laststatus != Some(l) {
                self.model.laststatus = Some(l);
                self.dirty = true;
            }
        }
    }

    /// With each window's text centred in it, ask for every window wider than
    /// it wants to have a grid as wide as that, and for the rest to have
    /// their own width again (see [`wide`]): a window on a tab page not on
    /// show keeps what it has. With the text not centred, every grid asked
    /// to be narrower is let go of. A window this attach finds on a grid
    /// another client asked for, and that this one would not narrow, is
    /// asked for whole. A grid asked to be narrower is cut to that width in
    /// the client's own copy at once, as Neovim will cut its own. Says
    /// whether it asked Neovim anything.
    fn narrow_windows(&mut self, now: Instant) -> bool {
        let on = self.centring() && self.multigrid();
        let mut want: HashMap<u64, usize> = HashMap::new();
        if on {
            for (&grid, p) in &self.model.layout {
                let Place::Window { width, .. } = p.place else {
                    continue;
                };
                if grid == 1 {
                    continue;
                }
                if p.hidden {
                    if let Some(&w) = self.narrowed.get(&grid) {
                        want.insert(grid, w);
                    }
                } else {
                    let w = self.want_of(grid);
                    if w < width {
                        want.insert(grid, w);
                    }
                }
            }
        }
        let mut asks: Vec<(u64, usize)> = want
            .iter()
            .filter(|(g, w)| self.narrowed.get(g) != Some(w))
            .map(|(g, w)| (*g, *w))
            .collect();
        asks.extend(
            self.narrowed
                .keys()
                .filter(|g| !want.contains_key(g) && self.model.grids.contains_key(g))
                .map(|g| (*g, 0)),
        );
        // A window on a grid not its own width is on one another client
        // asked for: one attached now, the windows' grids being every UI's,
        // or one killed before it could let go of it, whose ask Neovim keeps
        // for every UI until one asks for the window whole — keeps as it
        // was, too, so that a window made narrower since is on a grid wider
        // than itself. Asked for whole unless this client narrows it itself,
        // so that the client to attach last has the windows as it wants
        // them. Looked at once an attach, the first time both widths are
        // known: what another client asks for after that is left to it.
        if self.multigrid() {
            for (&grid, p) in &self.model.layout {
                let Place::Window { width, .. } = p.place else {
                    continue;
                };
                let Some(g) = self.model.grids.get(&grid) else {
                    continue;
                };
                if grid == 1 || p.hidden || width == 0 || !self.looked.insert(grid) {
                    continue;
                }
                if g.width() != width && !want.contains_key(&grid) {
                    asks.push((grid, 0));
                }
            }
        }
        asks.sort_unstable();
        asks.dedup();
        // An ask that would put a grid the client cut itself (see below)
        // back as wide as Neovim last had it waits for Neovim to have resized
        // it: taking both asks between one redraw and the next, Neovim would
        // find its grid as wide as it was and say nothing, leaving the
        // client's copy cut and what is past the cut never sent again. The
        // pass after that resize asks it.
        let mut narrowed = want;
        asks.retain(|&(grid, width)| {
            let Some(&before) = self.cut.get(&grid) else {
                return true;
            };
            let to = match self.model.layout.get(&grid).map(|p| &p.place) {
                Some(Place::Window { width: whole, .. }) if width == 0 => *whole,
                _ => width,
            };
            if to != before {
                return true;
            }
            match self.narrowed.get(&grid) {
                Some(&w) => narrowed.insert(grid, w),
                None => narrowed.remove(&grid),
            };
            false
        });
        self.narrowed = narrowed;
        let asked = !asks.is_empty();
        for (grid, width) in asks {
            // What Neovim does to its own copy of the grid once it has the
            // ask, done to the client's at once: as wide as asked, what it
            // shows kept from the left, as a resize keeps it. So the window's
            // text is drawn where it goes from the next frame — a split's
            // first, as it flies in — not a round trip later, and Neovim's
            // grid of that width, once it comes, changes no more than where
            // its lines wrap. A grid asked for wider has what it gains from
            // Neovim alone.
            if let Some(g) = self
                .model
                .grids
                .get_mut(&grid)
                .filter(|g| width > 0 && width < g.width())
            {
                self.cut.entry(grid).or_insert(g.width());
                g.resize(width, g.height());
                self.dirty = true;
            }
            // A height of nought asks for none: the window's own.
            self.outbox.notify(
                "nvim_ui_try_resize_grid",
                &[Arg::Int(grid as i64), Arg::Int(width as i64), Arg::Int(0)],
            );
        }
        // Text that moved in or out of its window slides there.
        self.anim.insets(&self.model, now);
        asked
    }

    /// What the window on `grid` wants, as the measure last said; or
    /// `window_width`, of a window it has not said anything of yet.
    fn want_of(&self, grid: u64) -> usize {
        self.model
            .wins
            .get(&grid)
            .and_then(|win| self.wants.get(win))
            .copied()
            .unwrap_or(self.window_width)
    }

    /// Let go of every grid asked to be narrower than its window, as the
    /// client goes: another UI of the session is to see the windows whole.
    /// Says whether there were any.
    pub fn let_windows_go(&mut self, now: Instant) -> bool {
        // Nothing waits on Neovim's resizing a grid the client cut: its own
        // copy no longer matters, and the windows are to be whole.
        self.cut.clear();
        let window_width = std::mem::take(&mut self.window_width);
        let asked = self.narrow_windows(now);
        self.window_width = window_width;
        asked
    }

    /// `nvim_set_client_info`: an nvmux UI, and whether it wants
    /// `ext_multigrid` — which tells the other nvmux clients that this one
    /// without it is only passing through (see [`UIS_LUA`]).
    fn client_info(&mut self) {
        let version = ["major", "minor", "patch"]
            .into_iter()
            .zip(env!("CARGO_PKG_VERSION").split('.'))
            .filter_map(|(key, part)| Some((key, Arg::Int(part.parse().ok()?))))
            .collect();
        let attributes = if self.multigrid_wanted {
            vec![("multigrid", Arg::str("wanted"))]
        } else {
            Vec::new()
        };
        self.outbox.notify(
            "nvim_set_client_info",
            &[
                Arg::str("nvmux"),
                Arg::Map(version),
                Arg::str("ui"),
                Arg::Map(Vec::new()),
                Arg::Map(attributes),
            ],
        );
    }

    fn multigrid(&self) -> bool {
        self.attach == Attach::Attached { multigrid: true }
    }

    /// Attach, with `ext_multigrid` or without.
    fn attach_as(&mut self, multigrid: bool) {
        self.attach = Attach::Attached { multigrid };
        self.flushed = false;
        let term = std::env::var("TERM").unwrap_or_default();
        let (w, h) = self.size;
        // Asked and looked at afresh by this attach, which may not have the
        // windows the last one had grids for.
        self.narrowed.clear();
        self.cut.clear();
        self.looked.clear();
        let mut opts = vec![
            ("rgb", Arg::Bool(true)),
            ("ext_linegrid", Arg::Bool(true)),
            ("ext_termcolors", Arg::Bool(true)),
            ("term_name", Arg::str(&term)),
            ("term_colors", Arg::Int(256)),
            // On a tty, as Neovim's own TUI says it is: see `term::Tty`.
            ("stdin_tty", Arg::Bool(self.tty.stdin)),
            ("stdout_tty", Arg::Bool(self.tty.stdout)),
        ];
        if multigrid {
            opts.push(("ext_multigrid", Arg::Bool(true)));
        }
        self.outbox.notify(
            "nvim_ui_attach",
            &[Arg::Int(w as i64), Arg::Int(h as i64), Arg::Map(opts)],
        );
        self.agent = false;
        self.leave_agent();
        self.measuring = false;
        self.leave_measure();
    }

    /// Leave the agent in the editor, once an attach that animates windows
    /// has the channel to give it. See [`AGENT_LUA`].
    fn leave_agent(&mut self) {
        let Some(chan) = self.chan else { return };
        let predict = self.predict.is_some();
        if self.agent || !self.multigrid() || !(self.anim.wants_agent() || predict) {
            return;
        }
        self.agent = true;
        self.reach = self.predict.as_ref().map_or(2, Predictor::agent_reach);
        let (keys, ikeys) = (predict::keys(), predict::insert_keys());
        fn list(keys: &[String]) -> Arg<'_> {
            Arg::Array(keys.iter().map(|k| Arg::str(k)).collect())
        }
        let id = self.outbox.request(
            "nvim_exec_lua",
            &[
                Arg::str(AGENT_LUA),
                Arg::Array(vec![
                    Arg::Int(chan as i64),
                    Arg::Bool(predict),
                    list(&keys),
                    list(&ikeys),
                    Arg::Int(self.reach),
                ]),
            ],
        );
        self.asked.insert(id, Ask::Agent);
    }

    /// Tell the agent how far either side of a view to keep lines, should
    /// the round trip have moved it: see [`Predictor::agent_reach`].
    fn tell_reach(&mut self) {
        let (Some(chan), Some(p)) = (self.chan, self.predict.as_mut()) else {
            return;
        };
        let reach = p.agent_reach();
        if !self.agent || reach == self.reach {
            return;
        }
        // None: the link too fast for predictions now, the agent sends
        // nothing more and lets go of what it sent, and what the client
        // keeps of it goes too — no edit reaches it from here on — till the
        // agent sends it all afresh, with a slow link back.
        if reach == 0 {
            p.reset();
        }
        self.reach = reach;
        let pattern = format!("nvmux_client_reach_{chan}");
        self.outbox.notify(
            "nvim_exec_autocmds",
            &[
                Arg::str("User"),
                Arg::Map(vec![
                    ("pattern", Arg::str(&pattern)),
                    ("data", Arg::Int(reach)),
                    ("modeline", Arg::Bool(false)),
                ]),
            ],
        );
    }

    /// What the agent says: see [`AGENT_LUA`].
    fn agent_said(&mut self, params: &[ValueRef<'_>], now: Instant) {
        let int = |i: usize| params.get(i).and_then(redraw::int);
        match params.first().and_then(redraw::as_str) {
            Some("buf") => {
                if let (Some(win), Some(buf)) = (int(1), int(2)) {
                    if self.bufs.insert(win, buf).is_some_and(|was| was != buf) {
                        self.switched.push(win);
                    }
                }
            }
            Some("win") => {
                if let (Some(win), Some(buf)) = (int(1), int(2)) {
                    self.bufs.insert(win, buf);
                }
            }
            Some("closed") => {
                if let Some(win) = int(1) {
                    self.bufs.remove(&win);
                    if let Some(p) = self.predict.as_mut() {
                        p.closed(win);
                    }
                }
            }
            Some("options") => self.anim.set_options(options(&params[1..])),
            Some("measure") => self.measured(int(1), wants(params.get(2)), now),
            // The attributes of the editor's highlight groups the colours
            // of the lines it sends name.
            Some("colours") => {
                let groups = params.get(1).and_then(ValueRef::as_array);
                for g in groups.into_iter().flatten().filter_map(ValueRef::as_array) {
                    let (Some(id), Some(rgb), Some(cterm)) = (
                        g.first()
                            .and_then(redraw::int)
                            .and_then(|n| u32::try_from(n).ok()),
                        g.get(1),
                        g.get(2),
                    ) else {
                        continue;
                    };
                    let attrs = |v| redraw::attrs(v).unwrap_or_default();
                    self.model.set_group(id, attrs(rgb), attrs(cterm));
                }
                self.dirty = true;
            }
            // Said just before Neovim draws it: taken with that frame.
            Some("leftcol") => {
                if let (Some(win), Some(left)) = (int(1), int(2)) {
                    self.lefts.push((win, usize::try_from(left).unwrap_or(0)));
                }
            }
            // Neovim is still at work on keys typed ahead of it, and will
            // send no frame till it has caught up: what it has yet to land
            // waits for it as long as a scroll made now would.
            Some("behind") => {
                if let Some(p) = self.predict.as_ref() {
                    self.anim.defer(p.deadline(now));
                }
            }
            Some(what) => {
                if let Some(p) = self.predict.as_mut() {
                    p.said(what, &params[1..]);
                }
                // What a scroll ahead was waiting for, perhaps.
                if matches!(what, "lines" | "splice") && self.flushed {
                    self.fill_in(now);
                }
            }
            None => {}
        }
    }

    /// Gone to the back (see [`crate::card::Command::Away`]): what moves
    /// stops where it is going, and nothing starts to until the client is
    /// back in front. What is left to draw, or changes from here on, is drawn
    /// as [`AWAY_FRAME_GAP`] allows.
    fn go_away(&mut self) {
        self.away = true;
        self.anim.reset();
        self.dirty = true;
    }

    /// Draw with `drawing` from here on, as nvmux has changed its settings
    /// (see [`crate::card::Command::Settings`]).
    ///
    /// The effects change at once: what is moving moves on to its end, and
    /// nothing turned off starts again. So does how a frame is written, from
    /// the next on (see [`screen`]). The rest are things this attach has
    /// told Neovim, and are told it again: whether the windows have grids of
    /// their own, and what the agent left in the editor predicts, by
    /// attaching again — as the client does when the UIs attached change —
    /// and how wide each window's text is to be, by measuring again.
    pub fn reconfigure(&mut self, drawing: &crate::config::Drawing, now: Instant) {
        let effects = Effects::from_settings(&drawing.settings());
        let client = drawing.client;
        let wanted = effects.want_multigrid() || client.predict;
        let predicting = self.predict.is_some();
        self.anim.set_effects(effects);
        self.screen.set_lean(client.lean_output);
        self.dirty = true;
        if client.predict != predicting {
            self.predict = client.predict.then(|| {
                let mut p = Predictor::default();
                p.typed.attached();
                p
            });
        }
        let flipped = wanted != self.multigrid_wanted;
        self.multigrid_wanted = wanted;
        if flipped {
            self.client_info();
        }
        let centred = (self.centre, self.window_width, self.smart);
        self.centre = client.centre;
        self.window_width = usize::from(client.window_width);
        self.smart = client.smart_width;

        let again = match self.attach {
            // Windows of their own wanted now: whether they can be had is
            // who else is attached, which the answer decides as it does at
            // the start.
            Attach::Attached { multigrid: false } if flipped && wanted => {
                self.ask_uis();
                false
            }
            // Not wanted any more; or the agent to be told it predicts, or
            // that it does not.
            Attach::Attached { multigrid: true } if !wanted || client.predict != predicting => {
                self.leave(wanted, now);
                true
            }
            Attach::Leaving { multigrid } => {
                self.attach = Attach::Leaving {
                    multigrid: multigrid && wanted,
                };
                true
            }
            Attach::Asking | Attach::Attached { .. } => false,
        };
        // An attach to come leaves the agent and the measure as they now
        // are; this one is told here.
        if again {
            return;
        }
        self.leave_agent();
        if centred != (self.centre, self.window_width, self.smart) {
            if self.centring() {
                // A measure left in the editor is replaced by the next.
                self.measuring = false;
                self.leave_measure();
            } else if let Some(chan) = self.chan.filter(|_| self.measuring) {
                self.measuring = false;
                self.outbox.notify(
                    "nvim_exec_lua",
                    &[
                        Arg::str(wide::UNMEASURE_LUA),
                        Arg::Array(vec![Arg::Int(chan as i64)]),
                    ],
                );
            }
            self.narrow_windows(now);
        }
    }

    /// Detach, to attach again as `multigrid` says once Neovim has done it:
    /// the windows let go of first, while Neovim still takes it of this UI.
    fn leave(&mut self, multigrid: bool, now: Instant) {
        tracing::info!(multigrid, "client: attaching again for the UIs there are");
        self.let_windows_go(now);
        let id = self.outbox.request("nvim_ui_detach", &[]);
        self.asked.insert(id, Ask::Detach);
        self.attach = Attach::Leaving { multigrid };
    }

    fn ask_uis(&mut self) {
        let mine = self.chan.map_or(-1, |c| c as i64);
        let id = self.outbox.request(
            "nvim_exec_lua",
            &[Arg::str(UIS_LUA), Arg::Array(vec![Arg::Int(mine)])],
        );
        self.asked.insert(id, Ask::Uis);
        self.checking = true;
        self.recheck = false;
        self.recheck_at = None;
    }

    /// Ask where these windows are; drawing waits for the answer.
    fn ask_layout(&mut self, wins: Vec<(u64, i64)>, now: Instant) {
        if wins.is_empty() {
            return;
        }
        let args = wins.iter().map(|(_, win)| Arg::Int(*win)).collect();
        let id = self
            .outbox
            .request("nvim_exec_lua", &[Arg::str(LAYOUT_LUA), Arg::Array(args)]);
        self.asked.insert(id, Ask::Layout(wins));
        self.placing += 1;
        self.hold_since.get_or_insert(now);
    }

    /// Take a whole msgpack-RPC frame from the server.
    pub fn message(&mut self, frame: &[u8], now: Instant) {
        let mut rd = frame;
        let Ok(v) = rmpv::decode::read_value_ref(&mut rd) else {
            tracing::debug!("client: a frame that would not decode");
            return;
        };
        let Some(msg) = v.as_array() else { return };
        match msg.first().and_then(ValueRef::as_u64) {
            Some(wire::NOTIFICATION) => {
                let method = msg.get(1).and_then(redraw::as_str);
                let params = msg.get(2);
                match (method, params) {
                    // Between a detach and its answer, what is drawn is the
                    // old attach's, and the next one starts from nothing.
                    (Some("redraw"), Some(_)) if matches!(self.attach, Attach::Leaving { .. }) => {}
                    (Some("redraw"), Some(params)) => {
                        redraw::decode(params, &mut self.batch);
                        self.take_flushes(now);
                    }
                    (Some("nvim_error_event"), Some(params)) => {
                        tracing::warn!(error = %params.to_owned(), "client: the server reported an error");
                    }
                    (Some("nvmux_client"), Some(params)) => {
                        if let Some(params) = params.as_array() {
                            self.agent_said(params, now);
                        }
                    }
                    _ => {}
                }
            }
            Some(wire::RESPONSE) => {
                let id = msg.get(1).and_then(ValueRef::as_u64).unwrap_or(0) as u32;
                let error = msg.get(2).filter(|e| !matches!(e, ValueRef::Nil));
                let result = msg.get(3);
                if let Some(e) = error {
                    tracing::debug!(error = %e.to_owned(), "client: a request failed");
                }
                if let (Some(at), Some(p)) = (self.asked_at.remove(&id), self.predict.as_mut()) {
                    p.sample(now.saturating_duration_since(at));
                    self.tell_reach();
                }
                match self.asked.remove(&id) {
                    Some(Ask::ApiInfo) => {
                        self.chan = result
                            .and_then(ValueRef::as_array)
                            .and_then(|a| a.first())
                            .and_then(ValueRef::as_u64);
                        self.leave_agent();
                        self.leave_measure();
                    }
                    Some(Ask::Agent) => {
                        let answer = result.and_then(ValueRef::as_array);
                        let answer = answer.map_or(&[][..], Vec::as_slice);
                        self.anim.set_options(options(answer));
                        let wins = answer.get(3).and_then(ValueRef::as_array);
                        self.bufs = wins
                            .map_or(&[][..], Vec::as_slice)
                            .iter()
                            .filter_map(|pair| {
                                let pair = pair.as_array()?;
                                Some((redraw::int(pair.first()?)?, redraw::int(pair.get(1)?)?))
                            })
                            .collect();
                    }
                    Some(Ask::Measure) => {
                        let answer = result.and_then(ValueRef::as_array);
                        let laststatus = answer.and_then(|a| a.first()).and_then(redraw::int);
                        self.measured(laststatus, wants(answer.and_then(|a| a.get(1))), now);
                    }
                    Some(Ask::Uis) => {
                        self.checking = false;
                        let flag = |i: usize| {
                            matches!(
                                result.and_then(ValueRef::as_array).and_then(|a| a.get(i)),
                                Some(ValueRef::Boolean(true))
                            )
                        };
                        self.consider(flag(0), flag(1), now);
                    }
                    Some(Ask::Layout(wins)) => {
                        self.placing = self.placing.saturating_sub(1);
                        if let (true, Some(answer)) =
                            (self.multigrid(), result.and_then(ValueRef::as_array))
                        {
                            self.placed(&wins, answer, now);
                            // How wide the windows are is known now, where
                            // Neovim did not say: see `narrow_windows`.
                            self.narrow_windows(now);
                        }
                        self.settle(now);
                    }
                    Some(Ask::Detach) => {
                        if let Attach::Leaving { multigrid } = self.attach {
                            self.reset();
                            self.attach_as(multigrid);
                        }
                    }
                    Some(Ask::Fence(at)) => {
                        let mode = Mode::of(&self.model.mode_name);
                        if let Some(p) = self.predict.as_mut() {
                            p.typed.answered(at, mode);
                        }
                    }
                    Some(Ask::Ping) | None => {}
                }
            }
            Some(wire::REQUEST) => {
                let id = msg.get(1).and_then(ValueRef::as_u64).unwrap_or(0);
                self.outbox.refuse(id, "nvmux's client serves no requests");
            }
            _ => {}
        }
    }

    /// What to make of who else is attached: whether there is a UI that keeps
    /// windows on grid 1 for good (`foreign`), and whether there is one that
    /// is only passing through. See the module docs.
    fn consider(&mut self, foreign: bool, passing: bool, now: Instant) {
        match self.attach {
            Attach::Asking => self.attach_as(!foreign),
            Attach::Attached { multigrid: true } if foreign => self.leave(false, now),
            Attach::Attached { multigrid: true } => {
                if passing {
                    self.recheck_at = Some(now + RECHECK);
                } else {
                    // Settled. Where the windows are may have changed while
                    // they were on grid 1, and Neovim does not say again.
                    self.unsettled = false;
                    let wins = self.model.wins.iter().map(|(g, w)| (*g, *w)).collect();
                    self.ask_layout(wins, now);
                }
            }
            Attach::Attached { multigrid: false } if !foreign && self.multigrid_wanted => {
                self.leave(true, now);
            }
            Attach::Attached { .. } | Attach::Leaving { .. } => {}
        }
        if self.recheck {
            self.ask_uis();
        }
        self.settle(now);
    }

    /// The UIs attached may have changed: ask, and draw nothing of a
    /// multigrid attach until they are known to have settled.
    fn uis_changed(&mut self, now: Instant) {
        if !self.multigrid_wanted {
            return;
        }
        if self.multigrid() {
            self.unsettled = true;
            self.hold_since.get_or_insert(now);
        }
        if self.checking {
            self.recheck = true;
        } else {
            self.ask_uis();
        }
    }

    /// Place the windows a layout was asked for, as it answered: quietly,
    /// for where they are is not something that just happened.
    fn placed(&mut self, wins: &[(u64, i64)], answer: &[ValueRef<'_>], now: Instant) {
        let mut events = Vec::new();
        for (&(grid, win), a) in wins.iter().zip(answer) {
            // Gone since, or another window's now.
            if self.model.wins.get(&grid) != Some(&win) {
                continue;
            }
            let n: Vec<i64> = a
                .as_array()
                .map(|a| a.iter().map(|v| redraw::int(v).unwrap_or(0)).collect())
                .unwrap_or_default();
            let at = |i: usize| n.get(i).copied().unwrap_or(0);
            let cells = |i: usize| usize::try_from(at(i)).unwrap_or(0);
            let (width, height) = self
                .model
                .grids
                .get(&grid)
                .map_or((0, 0), |g| (g.width(), g.height()));
            // The window's own size where the answer has it, which is its
            // grid's unless its text is centred in it (see `wide`).
            let own = |i: usize, grid: usize| Some(cells(i)).filter(|n| *n > 0).unwrap_or(grid);
            match at(0) {
                1 => {
                    events.push(Event::WinPos {
                        grid,
                        win: Some(win),
                        row: cells(1),
                        col: cells(2),
                        width: own(4, width),
                        height: own(5, height),
                    });
                    events.push(Event::WinViewportMargins {
                        grid,
                        top: cells(3),
                        bottom: 0,
                        left: 0,
                        right: 0,
                    });
                }
                2 => {
                    let compindex = match self.model.layout.get(&grid).map(|p| &p.place) {
                        Some(Place::Float { compindex, .. }) => *compindex,
                        _ => 0,
                    };
                    let anchor = self
                        .model
                        .wins
                        .iter()
                        .find(|(_, w)| **w == at(9))
                        .map(|(g, _)| *g);
                    events.push(Event::WinFloatPos {
                        grid,
                        win: Some(win),
                        mouse: at(8) != 0,
                        zindex: at(7),
                        compindex,
                        row: at(1),
                        col: at(2),
                        anchor,
                    });
                    events.push(Event::WinViewportMargins {
                        grid,
                        top: cells(3),
                        bottom: cells(4),
                        left: cells(5),
                        right: cells(6),
                    });
                }
                _ => events.push(Event::WinHide { grid }),
            }
        }
        if !events.is_empty() {
            self.apply(events, false, &[], now);
        }
    }

    /// Forget the editor: a fresh attach is about to describe it again. The
    /// screen is kept as the terminal has it, for the next frame to be drawn
    /// over.
    fn reset(&mut self) {
        let term = self.model.colors().term;
        self.model = Model::new(term);
        self.batch.clear();
        self.anim.reset();
        self.flushed = false;
        self.unsettled = false;
        self.placing = 0;
        self.lacking.clear();
        self.refreshed.clear();
        self.hold_since = None;
        self.pressed = None;
        if let Some(p) = self.predict.as_mut() {
            p.reset();
            p.typed.attached();
        }
    }

    /// Apply every whole batch the events so far hold.
    fn take_flushes(&mut self, now: Instant) {
        while let Some(end) = self.batch.iter().position(|e| *e == Event::Flush) {
            let rest = self.batch.split_off(end + 1);
            let batch = std::mem::replace(&mut self.batch, rest);
            self.flush(batch, now);
        }
    }

    /// One batch, from one `flush` to the next.
    fn flush(&mut self, batch: Vec<Event>, now: Instant) {
        // The set of UIs changing is the editor-wide ext_* options being sent
        // again, after the ones the attach itself brought.
        if self.flushed
            && batch
                .iter()
                .any(|e| matches!(e, Event::OptionSet { name, .. } if name.starts_with("ext_")))
        {
            self.uis_changed(now);
        }
        let animate = self.flushed && !self.holding(now) && !self.away;
        // The windows that took another buffer since the last flush, by grid:
        // this batch is the one that draws them.
        let switched: Vec<u64> = std::mem::take(&mut self.switched)
            .into_iter()
            .filter_map(|win| {
                let (grid, _) = self.model.wins.iter().find(|(_, w)| **w == win)?;
                Some(*grid)
            })
            .collect();
        self.apply(batch, animate, &switched, now);
        self.flushed = true;
        self.landed_sideways();
        self.fill_in(now);
        if let Some(p) = self.predict.as_mut() {
            p.typed.settle(Mode::of(&self.model.mode_name));
        }
        if self.multigrid() {
            self.inspect(now);
        }
        self.narrow_windows(now);
        self.settle(now);
    }

    /// Take a batch into the model, and set off what it starts — if
    /// `animate`, or simply put everything where it now is. `switched` are
    /// the window grids the batch draws another buffer in.
    fn apply(&mut self, batch: Vec<Event>, animate: bool, switched: &[u64], now: Instant) {
        let multigrid = self.multigrid();
        let before = self.anim.before(&self.model, &batch, multigrid, switched);
        // A grid Neovim resizes is as wide as Neovim has it again, whatever
        // the client cut it to (see `narrow_windows`).
        for event in &batch {
            if let Event::GridResize { grid, .. } | Event::GridDestroy { grid } = event {
                self.cut.remove(grid);
            }
        }
        let mut changes = Changes::default();
        for event in batch {
            self.model.apply(event, &mut changes);
        }
        self.model.refresh_styles();
        if changes.repaint {
            self.screen.invalidate();
        }
        for bytes in &changes.sent {
            self.say.extend_from_slice(bytes);
        }
        for _ in 0..changes.bells.min(1) {
            self.say.push(0x07);
        }
        if changes.title {
            self.say.extend_from_slice(&term::title(&self.model.title));
        }
        let want = (self.model.mouse, self.model.options.mousemoveevent);
        if want != self.mouse {
            self.say
                .extend_from_slice(term::mouse(want.0, want.1 && want.0));
            self.mouse = want;
        }
        if let Some(status) = changes.exit {
            tracing::info!(status, "client: the server says it is exiting");
            self.done = true;
        }
        self.anim
            .after(before, &self.model, &changes, multigrid, animate, now);
        // After the animations have taken what they keep of the batch: what
        // was kept from before it is theirs now, or gone.
        if self.model.reclaiming() {
            let mut held = self.model.held();
            self.anim.hold(&mut held);
            self.model.reclaim(&held);
        }
        self.dirty = true;
    }

    /// What a multigrid attach still lacks after a flush: windows not drawn
    /// whole, which a refresh is for, and windows nowhere, which a layout is
    /// asked for. See the module docs.
    fn inspect(&mut self, now: Instant) {
        self.lacking = self.model.lacking();
        let lacking = &self.lacking;
        self.refreshed.retain(|grid, _| lacking.contains(grid));
        let due = self
            .lacking
            .iter()
            .any(|grid| match self.refreshed.get(grid) {
                None => true,
                Some(r) => r.tries < REFRESH_TRIES && now >= r.at + REFRESH_TIMEOUT,
            });
        if due {
            tracing::debug!(grids = ?self.lacking, "client: windows not drawn whole; refreshing");
            for grid in &self.lacking {
                let r = self
                    .refreshed
                    .entry(*grid)
                    .or_insert(Refreshed { at: now, tries: 0 });
                r.at = now;
                r.tries += 1;
            }
            self.refresh = true;
            self.hold_since.get_or_insert(now);
        }
        let unplaced = self.model.unplaced();
        if !unplaced.is_empty() && self.placing == 0 {
            self.ask_layout(unplaced, now);
        }
    }

    /// Whether drawing still waits, and so whether a hold begins or ends.
    fn settle(&mut self, now: Instant) {
        let refreshing = self.lacking.iter().any(|grid| {
            self.refreshed
                .get(grid)
                .is_some_and(|r| now < r.at + HOLD_LIMIT)
        });
        if self.multigrid() && (self.unsettled || self.placing > 0 || refreshing) {
            self.hold_since.get_or_insert(now);
        } else if self.hold_since.take().is_some() {
            self.dirty = true;
        }
    }

    /// Whether drawing waits: for an attach to be made, or for what a
    /// multigrid one lacks, up to [`HOLD_LIMIT`].
    fn holding(&self, now: Instant) -> bool {
        match self.attach {
            Attach::Asking | Attach::Leaving { .. } => true,
            Attach::Attached { multigrid: false } => false,
            Attach::Attached { multigrid: true } => self
                .hold_since
                .is_some_and(|since| now < since + HOLD_LIMIT),
        }
    }

    /// The terminal said something.
    pub fn keys(&mut self, bytes: &[u8], now: Instant) {
        let mut out = Vec::new();
        self.parser.feed(bytes, &mut out);
        self.escape_since = self.parser.waiting().then_some(now);
        self.inputs(out, now);
    }

    /// When what the parser holds is to be taken as it is: `'ttimeoutlen'`
    /// after the last of it came — or [`REPLY_WAIT`] for a terminal's answer
    /// already part way through (see [`input`]).
    fn escape_deadline(&self) -> Option<Instant> {
        let since = self.escape_since?;
        if self.parser.in_reply() {
            return Some(since + REPLY_WAIT);
        }
        let o = &self.model.options;
        let wait = if o.ttimeout { o.ttimeoutlen } else { 0 };
        Some(since + Duration::from_millis(wait.max(1)))
    }

    pub fn escape_due(&self, now: Instant) -> bool {
        self.escape_deadline().is_some_and(|at| at <= now)
    }

    /// `'ttimeoutlen'` has passed with an escape held.
    pub fn escape_timeout(&mut self, now: Instant) {
        let mut out = Vec::new();
        self.parser.timeout(&mut out);
        self.escape_since = None;
        self.inputs(out, now);
    }

    fn inputs(&mut self, inputs: Vec<Input>, now: Instant) {
        for input in inputs {
            if matches!(
                input,
                Input::Keys(_) | Input::Mouse(_) | Input::Paste { .. }
            ) {
                self.typed = Some(now);
            }
            match input {
                Input::Keys(keys) => {
                    self.keys_typed(&keys, now);
                    self.outbox.notify("nvim_input", &[Arg::str(&keys)]);
                    self.anim.typed(now);
                }
                Input::Mouse(m) => self.mouse_input(m, now),
                Input::Paste { phase, data } => {
                    if let Some(p) = self.predict.as_mut() {
                        p.typed.other();
                    }
                    self.outbox.notify(
                        "nvim_paste",
                        &[Arg::Str(&data), Arg::Bool(true), Arg::Int(phase)],
                    );
                }
                Input::Focus(on) => self.outbox.notify("nvim_ui_set_focus", &[Arg::Bool(on)]),
                Input::Reply(reply) => self.reply(reply),
                Input::Card(body) => match crate::card::decode(&body) {
                    Some(crate::card::Command::Settings(drawing)) => {
                        self.reconfigure(&drawing, now);
                    }
                    Some(crate::card::Command::Away) => self.go_away(),
                    Some(cmd) => {
                        if matches!(cmd, crate::card::Command::Clear) {
                            self.away = false;
                        }
                        if self.cards.apply(cmd, now) {
                            self.dirty = true;
                        }
                    }
                    None => tracing::debug!("client: a card that would not decode"),
                },
            }
        }
        self.fence();
    }

    /// Keys typed, as the predictions follow them: a scroll among them is
    /// made at once where it can be. See [`predict::Typed`].
    fn keys_typed(&mut self, keys: &str, now: Instant) {
        let grid = self.model.cursor.grid;
        let win = self.model.wins.get(&grid).copied();
        for key in predict::tokens(keys) {
            let Some(p) = self.predict.as_mut() else {
                return;
            };
            if let Some(command) = p.key(win, key) {
                let by = if p.typed.inserting() {
                    By::InsertKeys
                } else {
                    By::Keys
                };
                self.predict_command(grid, command, by, now);
            }
        }
    }

    /// Ask after a fence, if the predictions want one: see
    /// [`predict::Typed`]. Queued after every key there is to queue, so
    /// Neovim answers it once it has taken them all.
    fn fence(&mut self) {
        if let Some(p) = self.predict.as_mut().filter(|p| p.active()) {
            if p.typed.wants_fence() {
                let at = p.typed.fenced();
                let id = self.outbox.request("nvim_eval", &[Arg::str("0")]);
                self.asked.insert(id, Ask::Fence(at));
            }
        }
    }

    /// An answer from the terminal: to the client's own keyboard question,
    /// or for the server.
    fn reply(&mut self, reply: Reply) {
        match (self.keyboard, reply) {
            (Keyboard::Asking, Reply::KittyFlags) => {
                self.keyboard = Keyboard::Closing;
                self.say.extend_from_slice(term::KITTY_ON);
            }
            (Keyboard::Asking, Reply::DeviceAttributes(_)) => {
                self.keyboard = Keyboard::Settled;
                self.say.extend_from_slice(term::OTHER_KEYS_ON);
            }
            (Keyboard::Closing, Reply::DeviceAttributes(_)) => {
                self.keyboard = Keyboard::Settled;
            }
            (_, Reply::KittyFlags) => {}
            (_, Reply::DeviceAttributes(seq) | Reply::Other(seq)) => {
                self.outbox.notify(
                    "nvim_ui_term_event",
                    &[Arg::str("termresponse"), Arg::Str(&seq)],
                );
            }
        }
    }

    /// A mouse report, to the grid it landed on.
    ///
    /// Beside a window's text centred in it (see [`wide`]), nothing is there
    /// to land on: a press or a move goes nowhere — but for a turn of the
    /// wheel, which scrolls that window — and a drag or a release carries on
    /// at the edge of the text, so that a selection is not cut short.
    fn mouse_input(&mut self, m: Mouse, now: Instant) {
        let carried = matches!(m.action, "drag" | "release");
        let (grid, row, col) = if !self.multigrid() {
            (0, m.row as i64, m.col as i64)
        } else {
            let pressed = self.pressed.filter(|_| carried);
            let mut grid = pressed.unwrap_or_else(|| self.grid_at(m.row as i64, m.col as i64));
            if grid == 1 && !carried {
                if let Some(beside) = self.window_beside_text(m.row, m.col) {
                    if m.button != "wheel" {
                        return;
                    }
                    grid = beside;
                }
            }
            let (r0, c0) = self.origin_of(grid);
            let width = self.model.grids.get(&grid).map_or(1, |g| g.width().max(1));
            let col = if grid == 1 {
                m.col as i64 - c0
            } else {
                (m.col as i64 - c0).clamp(0, width as i64 - 1)
            };
            (grid, m.row as i64 - r0, col)
        };
        match m.action {
            "press" => self.pressed = Some(grid),
            "release" => self.pressed = None,
            _ => {}
        }
        match (m.button, m.action) {
            ("wheel", way @ ("down" | "up" | "right" | "left")) => {
                self.predict_wheel(grid, way, &m.mods, now);
            }
            _ => {
                // A click or a drag: the cursor goes somewhere, a mode may
                // start.
                if let Some(p) = self.predict.as_mut() {
                    p.typed.other();
                }
            }
        }
        self.outbox.notify(
            "nvim_input_mouse",
            &[
                Arg::str(m.button),
                Arg::str(m.action),
                Arg::str(&m.mods),
                Arg::Int(grid as i64),
                Arg::Int(row.max(0)),
                Arg::Int(col.max(0)),
            ],
        );
    }

    /// A turn of the wheel over grid `grid`, `way` — `down`, `up`, `right` or
    /// `left` — with modifiers `mods`: plain, `'mousescroll'` lines or
    /// columns; with Shift or Ctrl, a page, or the window's width — or
    /// whatever something mapping it says, which is not predicted.
    fn predict_wheel(&mut self, grid: u64, way: &str, mods: &str, now: Instant) {
        let Some(p) = self.predict.as_mut() else {
            return;
        };
        let shape = self.model.wins.get(&grid).and_then(|w| p.shape(*w));
        let insert = Mode::of(&self.model.mode_name) == Mode::Insert;
        let (dir, sideways) = match way {
            "down" => (1, false),
            "up" => (-1, false),
            "right" => (1, true),
            _ => (-1, true),
        };
        let name = {
            let mut w = way.to_string();
            w[..1].make_ascii_uppercase();
            predict::wheel(&w, mods)
        };
        let width = self.model.grids.get(&grid).map_or(0, |g| g.width() as i64);
        let command = match (shape, mods) {
            (Some(s), _) if s.mapped.contains(&name) => None,
            (Some(s), "") if sideways => Some((Command::Side(Side::Wheel(dir * s.hor)), By::Wheel)),
            (Some(_), "S" | "C") if sideways => {
                Some((Command::Side(Side::Wheel(dir * width)), By::Wheel))
            }
            (Some(s), "") => Some((Command::Lines(dir * s.step), By::Wheel)),
            // In insert mode as many lines as the window shows, not a page.
            (Some(_), "S" | "C") if insert => Some((Command::Screen(dir), By::Wheel)),
            (Some(_), "S" | "C") => Some((
                Command::Page {
                    down: dir > 0,
                    count: 1,
                },
                By::PageWheel,
            )),
            _ => None,
        };
        match command {
            Some((command, by)) => {
                self.predict_command(grid, command, by, now);
            }
            None => p.typed.missed(),
        }
    }

    /// A scroll Neovim will make of window grid `grid`, typed or turned
    /// `by`: made at once, where the client can say what Neovim will make of
    /// it. Says whether the screen is as Neovim will have it — made, or
    /// nothing to make; one that is not is one the predictions after it wait
    /// for Neovim to have drawn. See [`predict`].
    fn predict_command(&mut self, grid: u64, command: Command, by: By, now: Instant) -> bool {
        let made = match command {
            Command::Side(side) => self.predict_side(grid, side, by, now),
            _ => self.predict_scroll(grid, command, by, now),
        };
        if let (false, Some(p)) = (made, self.predict.as_mut()) {
            p.typed.missed();
        }
        made
    }

    fn predict_scroll(&mut self, grid: u64, command: Command, by: By, now: Instant) -> bool {
        let (Some(&win), Some(rect)) = (
            self.model.wins.get(&grid),
            anim::text_rect(&self.model, grid),
        ) else {
            return false;
        };
        let (top, bot, left, right) = rect;
        let h = (bot - top) as i64;
        // Neovim takes it as 'scroll' from now on, whatever is predicted.
        if let (Command::Half { count: Some(n), .. }, Some(p)) = (command, self.predict.as_mut()) {
            p.set_scroll(win, n.min(h));
        }
        let Some(p) = self.predict.as_ref().filter(|p| p.active()) else {
            return false;
        };
        if !self.multigrid() || !self.flushed || self.holding(now) {
            return false;
        }
        // The modes a key or the wheel scrolls a window in as predicted:
        // keys and pages in normal and visual mode, lines in insert mode as
        // well.
        let mode = self.model.mode_name.as_str();
        let normal = matches!(mode, "normal" | "visual");
        let insert = matches!(mode, "insert" | "replace");
        let ok = match by {
            By::Keys => normal && p.typed.sure(),
            By::InsertKeys => insert && p.typed.sure(),
            By::PageWheel => normal && self.model.mouse,
            By::Wheel => {
                self.model.mouse
                    && matches!(
                        mode,
                        "normal" | "visual" | "visual_select" | "insert" | "replace"
                    )
            }
        };
        if !ok
            || !matches!(
                self.model.layout.get(&grid),
                Some(Placement {
                    place: Place::Window { .. },
                    hidden: false
                })
            )
        {
            return false;
        }
        let Some(&view) = self.model.viewports.get(&grid) else {
            return false;
        };
        let Some(shape) = p
            .shape(win)
            .filter(|s| s.predictable && s.line_count == view.line_count)
        else {
            return false;
        };
        if !view.one_to_one(h as usize) || self.anim.side_left(grid).is_some() {
            return false;
        }
        // From where the view and the cursor are, the scrolls ahead taken.
        let (rows, cursor) = self.anim.ahead(grid);
        let at = predict::At {
            top: view.topline + rows,
            cursor: view.curline + cursor,
        };
        let Some(to) = predict::outcome(command, at, h, view.line_count, shape) else {
            return false;
        };
        // Nothing to make, and nothing missed.
        if to == at {
            return true;
        }
        // The cursor alone moves: Neovim's to show.
        if to.top == at.top {
            return false;
        }
        let width = right - left;
        // Where the cursor goes, where it moves to another line or to the
        // first non-blank of its own: its column is worked out from the
        // line's text once the scrolls before it are drawn.
        let first = match command {
            Command::Line { first, .. } => first,
            Command::Half { .. } | Command::Page { .. } => shape.startofline,
            Command::Lines(_) | Command::Screen(_) | Command::Side(_) => false,
        };
        let land = (self.model.cursor.grid == grid
            && (normal || insert)
            && (first || to.cursor != at.cursor))
            .then_some(anim::Land {
                line: to.cursor,
                first,
                insert,
            });
        let Some(look) = self.look(grid, rect, &view, shape) else {
            return false;
        };
        let go = anim::Ahead {
            rows: to.top - at.top,
            cursor: to.cursor - at.cursor,
            col: None,
            land,
            until: p.deadline(now),
            scrolloff: shape.scrolloff,
        };
        let mut fill = |i: i64| p.row(win, view.topline + i, width, &look, to.cursor);
        let wants = self.wants(shape);
        let mut land =
            |l, want: Option<usize>| landed(p, win, shape, l, want.unwrap_or(wants), width);
        let made = self
            .anim
            .predict(&self.model, grid, go, (&mut fill, &mut land), now);
        if made {
            self.dirty = true;
        }
        made
    }

    /// The colours window grid `grid`'s rows are drawn in, read off those
    /// in its text rectangle `rect` (see [`predict::Look`]).
    fn look(
        &self,
        grid: u64,
        rect: (usize, usize, usize, usize),
        view: &model::Viewport,
        shape: &predict::Shape,
    ) -> Option<predict::Look> {
        let (top, bot, left, right) = rect;
        let rows = self.model.grids.get(&grid)?.lines(top, bot, left, right);
        let cursor_row = (self.model.cursor.grid == grid)
            .then(|| self.model.cursor.row.checked_sub(top))
            .flatten();
        let eob = self.model.groups.get("EndOfBuffer").copied().unwrap_or(0);
        Some(predict::Look::of(
            &rows,
            view.lines_shown(bot - top),
            shape.textoff,
            cursor_row,
            eob,
        ))
    }

    /// The display column the cursor wants, as far as Neovim's cursor says:
    /// the one it is in.
    fn wants(&self, shape: &predict::Shape) -> usize {
        (self.model.cursor.col + shape.leftcol).saturating_sub(shape.textoff)
    }

    /// Draw what windows scrolled ahead of Neovim could not show before, now
    /// that more of their lines have come or Neovim has drawn again; or, in
    /// a window no longer one to predict a scroll in, give them up, and
    /// predict nothing more till Neovim has caught up.
    fn fill_in(&mut self, now: Instant) {
        for grid in self.anim.held() {
            if !self.fill_in_grid(grid, now) {
                self.anim.give_up(grid);
                if let Some(p) = self.predict.as_mut() {
                    p.typed.missed();
                }
            }
            self.dirty = true;
        }
    }

    /// [`App::fill_in`] for window grid `grid`: says whether it is still
    /// one to predict a scroll in.
    fn fill_in_grid(&mut self, grid: u64, now: Instant) -> bool {
        let Some(p) = self.predict.as_ref() else {
            return false;
        };
        let (Some(&win), Some(&view), Some(rect)) = (
            self.model.wins.get(&grid),
            self.model.viewports.get(&grid),
            anim::text_rect(&self.model, grid),
        ) else {
            return false;
        };
        let (top, bot, left, right) = rect;
        let Some(shape) = p
            .shape(win)
            .filter(|s| s.predictable && s.line_count == view.line_count)
        else {
            return false;
        };
        if !view.one_to_one(bot - top) {
            return false;
        }
        let Some(look) = self.look(grid, rect, &view, shape) else {
            return false;
        };
        let width = right - left;
        let cursor = view.curline + self.anim.ahead(grid).1;
        let mut fill = |i: i64| p.row(win, view.topline + i, width, &look, cursor);
        let wants = self.wants(shape);
        let mut land =
            |l, want: Option<usize>| landed(p, win, shape, l, want.unwrap_or(wants), width);
        self.anim
            .fill_in(&self.model, grid, (&mut fill, &mut land), now);
        true
    }

    /// A scroll sideways Neovim will make of window grid `grid`, typed or
    /// turned `by`: made at once, where the client can say what Neovim will
    /// make of it. See [`predict_command`](Self::predict_command).
    fn predict_side(&mut self, grid: u64, side: Side, by: By, now: Instant) -> bool {
        let Some(p) = self.predict.as_ref().filter(|p| p.active()) else {
            return false;
        };
        if !self.multigrid() || !self.flushed || self.holding(now) {
            return false;
        }
        let mode = Mode::of(&self.model.mode_name);
        let ok = match by {
            By::Keys => mode == Mode::Normal && p.typed.sure(),
            By::Wheel => self.model.mouse && mode != Mode::Other,
            By::InsertKeys | By::PageWheel => false,
        };
        if !ok
            || !matches!(
                self.model.layout.get(&grid),
                Some(Placement {
                    place: Place::Window { .. },
                    hidden: false
                })
            )
        {
            return false;
        }
        let (Some(&win), Some(&view), Some(rect)) = (
            self.model.wins.get(&grid),
            self.model.viewports.get(&grid),
            anim::text_rect(&self.model, grid),
        ) else {
            return false;
        };
        let (top, bot, left, right) = rect;
        let h = bot - top;
        let Some(shape) = p
            .shape(win)
            .filter(|s| s.predictable && s.line_count == view.line_count)
        else {
            return false;
        };
        if !view.one_to_one(h) || self.anim.ahead(grid) != (0, 0) {
            return false;
        }
        // From where the view and the cursor are, a scroll sideways ahead
        // taken: the cursor where it is drawn, in its window; elsewhere, its
        // window's own, from the column Neovim gives it in bytes.
        let from = self.anim.side_left(grid).unwrap_or(shape.leftcol);
        let here = self.model.cursor.grid == grid;
        let at = if here {
            let (row, col) = self.anim.cursor_at(&self.model);
            let (r0, c0) = self.origin_of(grid);
            let row = (row - r0) as usize;
            let col = (col - c0) as usize;
            predict::Across {
                left: from,
                line: view.topline + row.saturating_sub(top) as i64,
                vcol: (col + from).saturating_sub(shape.textoff),
            }
        } else {
            let vcol = p
                .text(win, view.curline)
                .map_or(0, |t| predict::vcol_at(t, shape, view.curcol));
            predict::Across {
                left: from,
                line: view.curline,
                vcol,
            }
        };
        let shown = view.topline..view.topline + view.lines_shown(h) as i64;
        let Some(to) = predict::across(
            side,
            at,
            right - left,
            shape,
            |l| p.text(win, l),
            shown.clone(),
        ) else {
            return false;
        };
        if to == at {
            return true;
        }
        let text = |l| p.text(win, l);
        if !predict::known(at, to, right - left, shape, text, |l| p.cut(win, l), shown) {
            return false;
        }
        let Some(rows) = self.side_rows(grid, to.left) else {
            return false;
        };
        let row = to.line - view.topline;
        let cursor = (here && (0..h as i64).contains(&row) && to.vcol >= to.left)
            .then(|| (top + row as usize, shape.textoff + to.vcol - to.left));
        let until = p.deadline(now);
        self.anim
            .side(&self.model, grid, rows, (to.left, until), cursor, now);
        self.dirty = true;
        true
    }

    /// The rows of window grid `grid` as Neovim drew them, moved to show its
    /// text from display column `to`. See [`predict::shifted`].
    fn side_rows(&self, grid: u64, to: usize) -> Option<Vec<Vec<Cell>>> {
        let p = self.predict.as_ref()?;
        let win = *self.model.wins.get(&grid)?;
        let view = self.model.viewports.get(&grid)?;
        let shape = p.shape(win)?;
        let (top, bot, left, right) = anim::text_rect(&self.model, grid)?;
        let rows = self.model.grids.get(&grid)?.lines(top, bot, left, right);
        let lines = view.lines_shown(bot - top);
        let cursor = (self.model.cursor.grid == grid)
            .then(|| self.model.cursor.row.checked_sub(top))
            .flatten();
        let eob = self.model.groups.get("EndOfBuffer").copied().unwrap_or(0);
        let look = predict::Look::of(&rows, lines, shape.textoff, cursor, eob);
        predict::shifted(&rows, shape.leftcol, to, shape, &look, |r| {
            if r < lines {
                Some(Some(p.coloured(win, view.topline + r as i64)?))
            } else {
                Some(None)
            }
        })
    }

    /// Neovim has drawn: a window it moved sideways lands the scrolls ahead
    /// up to it, and one still ahead is drawn again from Neovim's rows.
    fn landed_sideways(&mut self) {
        for (win, left) in std::mem::take(&mut self.lefts) {
            if let Some(p) = self.predict.as_mut() {
                p.set_leftcol(win, left);
            }
            if let Some((&grid, _)) = self.model.wins.iter().find(|(_, w)| **w == win) {
                self.anim.landed(grid, left);
            }
        }
        for grid in self.anim.sided() {
            let rows = self
                .anim
                .side_left(grid)
                .and_then(|to| self.side_rows(grid, to));
            self.anim.refresh_side(grid, rows);
        }
    }

    /// Where a grid is drawn, as the editor laid it out: in from there,
    /// where its text is centred in its window (see [`wide`]).
    fn origin_of(&self, grid: u64) -> (i64, i64) {
        self.model.drawn_origin(grid)
    }

    /// The window whose text is centred in it (see [`wide`]) that has the
    /// cell at `row`, `col` beside its text.
    fn window_beside_text(&self, row: usize, col: usize) -> Option<u64> {
        self.model
            .layout
            .iter()
            .filter(|(g, p)| **g != 1 && !p.hidden && self.model.inset(**g) > 0)
            .find(|(g, _)| {
                self.model.window_cells(**g).is_some_and(|(r, c, w, h)| {
                    (r..r + h).contains(&row) && (c..c + w).contains(&col)
                })
            })
            .map(|(g, _)| *g)
    }

    /// The grid on top at a cell of the screen: a float that takes the mouse,
    /// the message area, a window, or grid 1 under them all.
    fn grid_at(&self, row: i64, col: i64) -> u64 {
        let inside = |grid: u64| {
            let (r0, c0) = self.origin_of(grid);
            self.model.grids.get(&grid).is_some_and(|g| {
                (r0..r0 + g.height() as i64).contains(&row)
                    && (c0..c0 + g.width() as i64).contains(&col)
            })
        };
        let mut layers: Vec<_> = self
            .model
            .layout
            .iter()
            .filter(|(g, p)| !p.hidden && **g != 1)
            .filter(|(_, p)| !matches!(p.place, Place::Float { mouse: false, .. }))
            .filter_map(|(g, p)| p.layer().map(|z| (z, *g)))
            .collect();
        layers.sort();
        if let Some((_, g)) = layers.iter().rev().find(|(_, g)| inside(*g)) {
            return *g;
        }
        self.model
            .layout
            .iter()
            .find(|(g, p)| !p.hidden && matches!(p.place, Place::Window { .. }) && inside(**g))
            .map_or(1, |(g, _)| *g)
    }

    /// The terminal changed size.
    pub fn resized(&mut self, size: (usize, usize)) {
        self.size = size;
        // Every time, even at the same size: nvmux shrinks the pty a row and
        // puts it back to have a session repaint (`pty::nudge`), and a
        // server answers this with a full redraw. Not attached, the attach
        // to come takes the size.
        if let Attach::Attached { .. } = self.attach {
            self.outbox.notify(
                "nvim_ui_try_resize",
                &[Arg::Int(size.0 as i64), Arg::Int(size.1 as i64)],
            );
        }
        self.screen.invalidate();
        self.anim.reset();
        self.dirty = true;
    }

    /// The terminal's size, as last told.
    pub fn size(&self) -> (usize, usize) {
        self.size
    }

    /// Anything the client is to do on the clock: ask after the UIs again,
    /// and after the round trip.
    pub fn tick(&mut self, now: Instant) {
        if let Some(p) = self.predict.as_mut() {
            if p.wants_ping(now, self.typed) {
                p.pinged(now);
                let id = self.outbox.request("nvim_get_mode", &[]);
                self.asked.insert(id, Ask::Ping);
                self.asked_at.insert(id, now);
            }
        }
        self.fence();
        if self.recheck_at.is_some_and(|at| at <= now) {
            self.recheck_at = None;
            if self.checking {
                self.recheck = true;
            } else {
                self.ask_uis();
            }
        }
    }

    /// Whether a refresh is to be started now (see [`refresh`]). Asking says
    /// it has been.
    pub fn take_refresh(&mut self) -> bool {
        std::mem::take(&mut self.refresh)
    }

    /// When the loop must wake next on the client's own account.
    pub fn wake_at(&self, now: Instant) -> Option<Instant> {
        let frame = if !self.flushed {
            None
        } else if self.holding(now) {
            // Not a time gone by: an attach asked for, or a detach, waits
            // for its answer however long the hold has been, and a wait
            // already over would have the loop poll without waiting, round
            // and round until the answer came.
            self.hold_since
                .map(|since| since + HOLD_LIMIT)
                .filter(|&at| at > now)
        } else if self.dirty {
            Some(self.dirty_due(now))
        } else if self.away {
            None
        } else {
            [
                self.anim.next_frame(&self.model, now),
                self.cards_change(now),
            ]
            .into_iter()
            .flatten()
            .min()
        };
        [self.escape_deadline(), frame, self.recheck_at]
            .into_iter()
            .flatten()
            .min()
    }

    /// Whether a frame is to be drawn now.
    pub fn due(&self, now: Instant) -> bool {
        if !self.flushed || self.holding(now) {
            return false;
        }
        if self.dirty {
            return now >= self.dirty_due(now);
        }
        if self.away {
            return false;
        }
        [
            self.anim.next_frame(&self.model, now),
            self.cards_change(now),
        ]
        .into_iter()
        .flatten()
        .any(|at| at <= now)
    }

    /// When a frame for something that changed may be drawn, `now` or later:
    /// [`MIN_FRAME_GAP`] after the last — but at once where the last came
    /// after a quiet spell, a frame's time or more without one. A key in
    /// normal mode has Neovim flush twice, a fraction of a millisecond apart
    /// with `'showcmd'` on, as it is unless turned off: the key shown, and
    /// what it did. Held apart by the gap, what it did would come those
    /// milliseconds late, as most keys' would. A burst still comes no more
    /// often than the gap allows, all but its first two frames. Away, no
    /// sooner than [`AWAY_FRAME_GAP`] after the last, whatever came before.
    fn dirty_due(&self, now: Instant) -> Instant {
        let Some(last) = self.last_frame else {
            return now;
        };
        if self.away {
            return (last + AWAY_FRAME_GAP).max(now);
        }
        let quiet = self
            .frame_before
            .is_none_or(|before| last.saturating_duration_since(before) >= anim::FRAME);
        if quiet {
            return now;
        }
        (last + MIN_FRAME_GAP).max(now)
    }

    /// When the cards next change from how the last frame showed them.
    fn cards_change(&self, now: Instant) -> Option<Instant> {
        self.cards.next_frame(self.last_frame.unwrap_or(now))
    }

    /// The next frame's bytes, with whatever was waiting to be said ahead of
    /// it.
    pub fn frame(&mut self, now: Instant) -> Vec<u8> {
        self.anim.advance(now);
        let (w, h) = self.size;
        // Put together in the buffer of a frame done with; and mended once,
        // last — and before what is painted over it too, where that reads
        // the cells under it: a card, or the cursor's smear, sparks or blink.
        self.cards.expire(now);
        let over = self.anim.paints_over() || !self.cards.is_empty();
        let mut frame = self.screen.take_spare();
        compose::compose_into(&mut frame, &self.model, &self.anim, w, h, over);
        let cursor = self.anim.paint(&mut frame, &self.model, now);
        // Last, over everything the editor has: see [`cards`].
        if !self.cards.is_empty() {
            let signs = cards::Signs::of(&self.model);
            self.cards.paint(
                &mut frame,
                self.model.style(0),
                &self.model.colors(),
                &signs,
                now,
            );
        }
        frame.mend();
        let sync = self.model.options.termsync;
        let mut out = std::mem::take(&mut self.say);
        out.reserve(4096);
        self.screen
            .draw_into(&mut out, frame, &cursor, sync, self.model.links());
        self.dirty = false;
        self.frame_before = self.last_frame.replace(now);
        out
    }

    /// Bytes for the terminal that cannot wait for a frame — the keyboard
    /// mode, before the first one.
    pub fn take_said(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.say)
    }

    /// What has been queued for the server, for the tests.
    #[cfg(test)]
    fn sent(&mut self) -> Vec<rmpv::Value> {
        let mut out = Vec::new();
        let mut buf = Vec::new();
        self.outbox.send(&mut buf).expect("a Vec takes everything");
        let mut rd = &buf[..];
        while !rd.is_empty() {
            out.push(rmpv::decode::read_value(&mut rd).expect("msgpack"));
        }
        out
    }
}

/// Run the client on its pty, for the session at `sock`, until the session or
/// the client is done. See the module docs.
pub fn run(sock: &Path) -> anyhow::Result<()> {
    let settings = std::env::var(SETTINGS_ENV)
        .ok()
        .and_then(|text| crate::config::Drawing::from_env(&text))
        .map(|drawing| drawing.settings())
        .unwrap_or_else(|| crate::config::load().unwrap_or_default());
    let effects = Effects::from_settings(&settings);
    let term = palette_from_env().unwrap_or(FALLBACK);

    let mut stream =
        UnixStream::connect(sock).with_context(|| format!("connecting to {}", sock.display()))?;
    stream.set_nonblocking(true)?;

    let _raw = term::Raw::enter()?;
    let hangup = term::Hangup::install()?;
    let winch = crate::winch::Winch::install()?;
    crate::term::write_stdout(term::OPENING)?;

    let mut app = App::new(term, effects, settings.client.predict, term::size())
        .with_centre(
            settings.client.centre,
            usize::from(settings.client.window_width),
            settings.client.smart_width,
        )
        .with_lean_output(settings.client.lean_output);
    app.start();
    let mut inbox = Inbox::default();
    let stdin = std::io::stdin();
    let mut buf = vec![0u8; 64 * 1024];

    let outcome = 'session: loop {
        app.tick(Instant::now());
        if app.take_refresh() {
            refresh(sock, app.size());
        }
        if let Err(e) = app.outbox.send(&mut stream) {
            break Err(anyhow::Error::from(e).context("writing to the session"));
        }
        let said = app.take_said();
        if !said.is_empty() {
            if let Err(e) = crate::term::write_stdout(&said) {
                break Err(e.into());
            }
        }
        let now = Instant::now();
        let timeout = app.wake_at(now).map_or(-1, |at| {
            at.saturating_duration_since(now).as_micros().div_ceil(1000) as libc::c_int
        });
        let sock_events = if app.outbox.is_empty() {
            libc::POLLIN
        } else {
            libc::POLLIN | libc::POLLOUT
        };
        let mut fds = [
            crate::pty::pollfd(stdin.as_raw_fd()),
            libc::pollfd {
                fd: stream.as_raw_fd(),
                events: sock_events,
                revents: 0,
            },
            crate::pty::pollfd(winch.fd()),
            crate::pty::pollfd(hangup.fd()),
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break Err(e.into());
        }
        if n > 0 && crate::pty::ready(&fds[3]) {
            tracing::debug!("client: hung up");
            break Ok(());
        }
        let now = Instant::now();
        if n > 0 && crate::pty::ready(&fds[2]) {
            winch.drain();
            app.resized(term::size());
        }
        if n > 0 && fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match inbox.fill(&mut stream) {
                Ok(true) => {}
                Ok(false) => break Ok(()),
                Err(e) => break Err(anyhow::Error::from(e).context("reading from the session")),
            }
            loop {
                match inbox.take_frame() {
                    Ok(Some(frame)) => app.message(&frame, now),
                    Ok(None) => break,
                    Err(e) => {
                        break 'session Err(
                            anyhow::Error::from(e).context("reading from the session")
                        );
                    }
                }
            }
        }
        if n > 0 && crate::pty::ready(&fds[0]) {
            match (&stdin).read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(len) => app.keys(&buf[..len], now),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                // EIO: nvmux has closed the pty.
                Err(_) => break Ok(()),
            }
        } else if app.escape_due(now) {
            app.escape_timeout(now);
        }
        if app.due(now) {
            let bytes = app.frame(now);
            if let Err(e) = crate::term::write_stdout(&bytes) {
                break Err(e.into());
            }
        }
        if app.done {
            break Ok(());
        }
    };
    // Whatever ended it, the terminal is handed back as it was — and the
    // windows their own width, for whoever else draws them.
    if app.let_windows_go(Instant::now()) {
        leave(stream, inbox, std::mem::take(&mut app.outbox));
    } else {
        let _ = app.outbox.send(&mut stream);
    }
    let _ = crate::term::write_stdout(term::CLOSING);
    outcome
}

/// How long a client on its way out waits for Neovim to have done what it
/// last asked (see [`leave`]): well inside the two seconds nvmux gives a
/// client it hangs up before killing it.
const LEAVE_WAIT: Duration = Duration::from_millis(500);

/// Send what is queued on the way out, and wait for Neovim to have done it,
/// for up to [`LEAVE_WAIT`]. Neovim queues what a channel asks of it, and
/// throws away what is still queued once it has read the channel's end
/// (`request_event` in its `msgpack_rpc/channel.c`): a client that goes the
/// moment it has asked can have its last asks dropped — under load, one time
/// in four. So a request goes after them, queued as they are, and its answer
/// says they are done.
fn leave(stream: UnixStream, inbox: Inbox, mut out: Outbox) {
    let fence = out.request("nvim_eval", &[Arg::str("0")]);
    // Blocking last, so that it never blocks without the timeouts.
    let blocking = stream.set_read_timeout(Some(LEAVE_WAIT)).is_ok()
        && stream.set_write_timeout(Some(LEAVE_WAIT)).is_ok()
        && stream.set_nonblocking(false).is_ok();
    let mut line = Line {
        stream,
        inbox,
        out,
        deadline: Instant::now() + LEAVE_WAIT,
    };
    if !blocking {
        let _ = line.out.send(&mut line.stream);
        return;
    }
    if let Err(e) = line.answer(fence, |_| ()) {
        tracing::debug!(error = %e, "client: Neovim did not answer on the way out");
    }
}

/// Have Neovim draw the windows of the current tab page afresh for the UIs
/// attached with `ext_multigrid`: a UI without it attaches on a connection of
/// its own, which has Neovim put every window on grid 1 for it, and goes once
/// Neovim has drawn for it, which has Neovim give the windows their own grids
/// again — and send everything on them. See the module docs.
///
/// On a thread of its own, so that the client goes on reading and drawing
/// while it is done, and with nothing to say back: the client sees it work by
/// the windows it is sent.
fn refresh(sock: &Path, size: (usize, usize)) {
    let sock = sock.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("nvmux-refresh".into())
        .spawn(move || {
            if let Err(e) = refresh_now(&sock, size) {
                tracing::debug!(error = %e, "client: a refresh did not finish");
            }
        });
    if let Err(e) = spawned {
        tracing::debug!(error = %e, "client: no thread for a refresh");
    }
}

fn refresh_now(sock: &Path, (w, h): (usize, usize)) -> std::io::Result<()> {
    let deadline = Instant::now() + REFRESH_TIMEOUT;
    let stream = UnixStream::connect(sock)?;
    stream.set_read_timeout(Some(REFRESH_TIMEOUT))?;
    stream.set_write_timeout(Some(REFRESH_TIMEOUT))?;
    let mut line = Line {
        stream,
        inbox: Inbox::default(),
        out: Outbox::default(),
        deadline,
    };
    // Neovim aborts on its seventeenth UI (see `crate::rpc::Client::list_uis`),
    // and this is one more for as long as it is attached: with more than a
    // few there already, the windows are left as they are rather than risk
    // it, whichever clients are refreshing at once.
    let uis = line.out.request("nvim_list_uis", &[]);
    let count = line.answer(uis, |v| v.as_array().map_or(usize::MAX, Vec::len))?;
    if count > REFRESH_MAX_UIS {
        tracing::debug!(count, "client: too many UIs attached to refresh");
        return Ok(());
    }
    // An nvmux UI only passing through grid 1 (see `UIS_LUA`).
    line.out.notify(
        "nvim_set_client_info",
        &[
            Arg::str("nvmux"),
            Arg::Map(Vec::new()),
            Arg::str("ui"),
            Arg::Map(Vec::new()),
            Arg::Map(vec![("multigrid", Arg::str("wanted"))]),
        ],
    );
    // The size of the client it is for, which is never less than the size
    // the editor already is, and so changes nothing of it. `ext_termcolors`
    // as the client has it, or the default colours would change for a
    // moment for every UI.
    let attach = line.out.request(
        "nvim_ui_attach",
        &[
            Arg::Int(w as i64),
            Arg::Int(h as i64),
            Arg::Map(vec![
                ("rgb", Arg::Bool(true)),
                ("ext_linegrid", Arg::Bool(true)),
                ("ext_termcolors", Arg::Bool(true)),
            ]),
        ],
    );
    // Answered once Neovim has drawn for it — with the windows on grid 1,
    // which is the half of the refresh that matters.
    line.answer(attach, |_| ())?;
    // Detached, the windows go back to grids of their own. The connection
    // closing would do it as well; this does not wait for that.
    line.out.notify("nvim_ui_detach", &[]);
    let _ = line.out.send(&mut line.stream);
    Ok(())
}

/// How many UIs may be attached already for a refresh to attach one more:
/// see [`refresh_now`].
const REFRESH_MAX_UIS: usize = 4;

/// A connection used blocking, and given until `deadline`: a refresh's own,
/// or the client's on its way out.
struct Line {
    stream: UnixStream,
    inbox: Inbox,
    out: Outbox,
    deadline: Instant,
}

impl Line {
    /// Send what is queued, and wait for the answer to request `id`, made of
    /// what `read` makes of its result. Anything else that comes — what
    /// Neovim draws for an attached UI — is read and let go.
    fn answer<T>(&mut self, id: u32, read: impl Fn(&ValueRef<'_>) -> T) -> std::io::Result<T> {
        while !self.out.is_empty() {
            let left = self.left()?;
            self.stream.set_write_timeout(Some(left))?;
            self.out.send(&mut self.stream)?;
        }
        loop {
            while let Some(frame) = self
                .inbox
                .take_frame()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
            {
                let mut rd = &frame[..];
                let Ok(v) = rmpv::decode::read_value_ref(&mut rd) else {
                    continue;
                };
                let msg = v.as_array().map(Vec::as_slice).unwrap_or_default();
                if msg.first().and_then(ValueRef::as_u64) == Some(wire::RESPONSE)
                    && msg.get(1).and_then(ValueRef::as_u64) == Some(u64::from(id))
                {
                    return Ok(read(msg.get(3).unwrap_or(&ValueRef::Nil)));
                }
            }
            let left = self.left()?;
            self.stream.set_read_timeout(Some(left))?;
            if !self.inbox.fill(&mut self.stream)? {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
        }
    }

    /// What is left of the time given, for a read or a write to wait no
    /// longer than; none left, an error.
    fn left(&self) -> std::io::Result<Duration> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        Ok(left)
    }
}

/// The terminal's colours as the parent nvmux found them, if it did.
fn palette_from_env() -> Option<Palette> {
    Palette::from_env(&std::env::var(PALETTE_ENV).ok()?)
}

/// What stands in for a terminal that never said what its colours are:
/// Neovim's own default dark scheme, for a client that would otherwise have
/// to guess at a colour to fade into.
const FALLBACK: Palette = Palette {
    fg: Rgb(0xe0, 0xe2, 0xea),
    bg: Rgb(0x14, 0x16, 0x1b),
    ansi: crate::palette::XTERM_ANSI,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::predict::PING_EVERY;
    use rmpv::Value;

    type Call = (Option<u64>, String, Vec<Value>);

    fn palette() -> Palette {
        Palette {
            fg: Rgb(255, 255, 255),
            bg: Rgb(0, 0, 0),
            ansi: [Rgb(0, 0, 0); 16],
        }
    }

    /// A client with an effect that wants windows of their own, or none.
    fn app(multigrid: bool) -> App {
        let effects = Effects {
            windows: multigrid.then_some(anim::Windows {
                slide: 0.15,
                resize: 0.15,
                open: 0.2,
                close: 0.18,
                switch: 0.2,
            }),
            ..Effects::none()
        };
        App::new(palette(), effects, false, (20, 6))
    }

    fn encoded(v: Value) -> Vec<u8> {
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &v).expect("encode");
        buf
    }

    fn answer(app: &mut App, id: u64, result: Value, now: Instant) {
        let v = Value::Array(vec![1.into(), id.into(), Value::Nil, result]);
        app.message(&encoded(v), now);
    }

    /// One `redraw` notification of the events given, a `flush` at its end.
    fn redraw(app: &mut App, events: Vec<(&str, Vec<Value>)>, now: Instant) {
        let mut batches: Vec<Value> = events
            .into_iter()
            .map(|(name, args)| Value::Array(vec![name.into(), Value::Array(args)]))
            .collect();
        batches.push(Value::Array(vec!["flush".into(), Value::Array(vec![])]));
        let v = Value::Array(vec![2.into(), "redraw".into(), Value::Array(batches)]);
        app.message(&encoded(v), now);
    }

    /// What the client sent since last asked, as `(id, method, args)`: no id
    /// for a notification.
    fn calls(app: &mut App) -> Vec<Call> {
        app.sent()
            .into_iter()
            .map(|v| {
                let a = v.as_array().expect("an array").clone();
                let (id, method, args) = match a[0].as_u64() {
                    Some(0) => (a[1].as_u64(), &a[2], &a[3]),
                    _ => (None, &a[1], &a[2]),
                };
                let args = args.as_array().cloned().unwrap_or_default();
                (id, method.as_str().unwrap_or("").to_string(), args)
            })
            .collect()
    }

    /// The id of the request for `method` — and for `nvim_exec_lua`, of the
    /// Lua that starts with `lua`.
    fn request(calls: &[Call], method: &str, lua: &str) -> u64 {
        calls
            .iter()
            .find(|(id, m, args)| {
                id.is_some()
                    && m == method
                    && (lua.is_empty()
                        || args
                            .first()
                            .and_then(Value::as_str)
                            .is_some_and(|s| s.starts_with(lua)))
            })
            .and_then(|(id, ..)| *id)
            .unwrap_or_else(|| panic!("no {method} {lua} in {calls:?}"))
    }

    fn asks_uis(calls: &[Call]) -> u64 {
        request(calls, "nvim_exec_lua", "local mine")
    }

    /// Whether the attach among `calls` asked for `ext_multigrid`, if there
    /// is one.
    fn attached_with_multigrid(calls: &[Call]) -> Option<bool> {
        let (_, _, args) = calls.iter().find(|(_, m, _)| m == "nvim_ui_attach")?;
        let opts = args.get(2)?.as_map()?;
        Some(
            opts.iter()
                .any(|(k, v)| k.as_str() == Some("ext_multigrid") && *v == Value::from(true)),
        )
    }

    fn uis(foreign: bool, passing: bool) -> Value {
        Value::Array(vec![foreign.into(), passing.into()])
    }

    /// A `grid_line` of `text` from the start of a row: a cell a character,
    /// the first naming the highlight and the rest carrying it on.
    fn line(grid: u64, row: u64, text: &str) -> (&'static str, Vec<Value>) {
        line_at(grid, row, 0, text)
    }

    /// [`line`], from column `col`.
    fn line_at(grid: u64, row: u64, col: u64, text: &str) -> (&'static str, Vec<Value>) {
        let cells = text
            .chars()
            .enumerate()
            .map(|(i, ch)| {
                let mut cell = vec![Value::from(ch.to_string())];
                if i == 0 {
                    cell.push(0.into());
                }
                Value::Array(cell)
            })
            .collect();
        (
            "grid_line",
            vec![
                grid.into(),
                row.into(),
                col.into(),
                Value::Array(cells),
                false.into(),
            ],
        )
    }

    /// What Neovim draws of a grid `width` × `height` it has just made: every
    /// cell of it, `rows` from the top and blanks after them.
    fn whole(
        grid: u64,
        width: usize,
        height: usize,
        rows: &[impl AsRef<str>],
    ) -> Vec<(&'static str, Vec<Value>)> {
        (0..height)
            .map(|r| {
                let text = rows.get(r).map_or("", AsRef::as_ref);
                line(grid, r as u64, &format!("{text:width$}"))
            })
            .collect()
    }

    fn resize(grid: u64, width: u64, height: u64) -> (&'static str, Vec<Value>) {
        (
            "grid_resize",
            vec![grid.into(), width.into(), height.into()],
        )
    }

    /// A `win_pos`: window `win`'s grid `grid`, at `row`, `col` and `width`
    /// by `height`.
    fn place(
        grid: u64,
        win: i64,
        row: u64,
        col: u64,
        width: u64,
        height: u64,
    ) -> (&'static str, Vec<Value>) {
        (
            "win_pos",
            vec![
                grid.into(),
                win.into(),
                row.into(),
                col.into(),
                width.into(),
                height.into(),
            ],
        )
    }

    fn viewport(grid: u64, win: i64) -> (&'static str, Vec<Value>) {
        let n = Value::from;
        (
            "win_viewport",
            vec![grid.into(), win.into(), n(0), n(1), n(0), n(0), n(1), n(0)],
        )
    }

    fn ui_change() -> (&'static str, Vec<Value>) {
        ("option_set", vec!["ext_cmdline".into(), false.into()])
    }

    /// Started and attached as `uis` says, with grid 1 drawn.
    fn attached(foreign: bool, t0: Instant) -> App {
        let mut a = app(true);
        a.start();
        let sent = calls(&mut a);
        answer(
            &mut a,
            request(&sent, "nvim_get_api_info", ""),
            Value::Array(vec![7.into()]),
            t0,
        );
        answer(&mut a, asks_uis(&sent), uis(foreign, false), t0);
        redraw(&mut a, vec![resize(1, 20, 6)], t0);
        calls(&mut a);
        a
    }

    /// The settings the test clients start with, as nvmux would tell them:
    /// windows animated, and nothing predicted or centred.
    fn drawing() -> crate::config::Drawing {
        let mut d = crate::config::Drawing::default();
        d.client.predict = false;
        d.effects.scroll.enabled = false;
        d
    }

    /// Whether `calls` has the Lua that starts with `lua`, asked or told.
    fn runs_lua(calls: &[Call], lua: &str) -> bool {
        calls.iter().any(|(_, m, args)| {
            m == "nvim_exec_lua"
                && args
                    .first()
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.starts_with(lua))
        })
    }

    /// Told settings whose effects want no windows of their own, a client
    /// attached with them attaches again without.
    #[test]
    fn a_client_told_it_needs_no_windows_of_their_own_attaches_again_without() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);
        let mut d = drawing();
        d.effects.windows.enabled = false;
        a.reconfigure(&d, t0);
        let sent = calls(&mut a);
        let detach = request(&sent, "nvim_ui_detach", "");
        answer(&mut a, detach, Value::Nil, t0);
        assert_eq!(attached_with_multigrid(&calls(&mut a)), Some(false));
    }

    /// Told settings that want windows of their own, a client attached
    /// without asks who else is attached, and attaches again with them where
    /// nothing stops it — as it would have from the start.
    #[test]
    fn a_client_told_it_wants_windows_of_their_own_asks_after_the_uis() {
        let t0 = Instant::now();
        let mut a = app(false);
        a.start();
        let sent = calls(&mut a);
        answer(
            &mut a,
            request(&sent, "nvim_get_api_info", ""),
            Value::Array(vec![7.into()]),
            t0,
        );
        assert_eq!(attached_with_multigrid(&sent), Some(false));
        a.reconfigure(&drawing(), t0);
        let sent = calls(&mut a);
        answer(&mut a, asks_uis(&sent), uis(false, false), t0);
        let sent = calls(&mut a);
        let detach = request(&sent, "nvim_ui_detach", "");
        answer(&mut a, detach, Value::Nil, t0);
        assert_eq!(attached_with_multigrid(&calls(&mut a)), Some(true));
    }

    /// Told to predict, or not to, a client attaches again, so the agent it
    /// leaves in the editor is left as the prediction now needs.
    #[test]
    fn a_client_told_to_predict_attaches_again_for_the_agent() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);
        let mut d = drawing();
        d.client.predict = true;
        a.reconfigure(&d, t0);
        assert!(a.predict.is_some());
        let sent = calls(&mut a);
        request(&sent, "nvim_ui_detach", "");
        assert!(
            !runs_lua(&sent, AGENT_LUA),
            "the agent waits for the attach"
        );
    }

    /// Told to centre each window's text, a client measures the windows; told
    /// not to any more, it takes the measure back out of the editor. Neither
    /// attaches again.
    #[test]
    fn a_client_told_to_centre_measures_and_told_not_to_stops() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);
        let mut d = drawing();
        d.client.centre = true;
        a.reconfigure(&d, t0);
        let sent = calls(&mut a);
        assert!(runs_lua(&sent, "local most"), "measured: {sent:?}");
        assert!(!sent.iter().any(|(_, m, _)| m == "nvim_ui_detach"));

        d.client.centre = false;
        a.reconfigure(&d, t0);
        let sent = calls(&mut a);
        assert!(runs_lua(&sent, wide::UNMEASURE_LUA), "{sent:?}");
        assert!(!sent.iter().any(|(_, m, _)| m == "nvim_ui_detach"));
    }

    /// A client writes each frame lean until told not to, then each whole
    /// from the next frame on, and lean again once told to: the next of two
    /// rows changed one under the other reached by CR LF, then by a CUP, and
    /// then by CR LF.
    #[test]
    fn a_client_writes_lean_frames_until_told_not_to() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);
        a.frame(t0);
        let drawn = |a: &mut App, top: &str, under: &str| {
            redraw(a, vec![line(1, 1, top), line(1, 2, under)], t0);
            String::from_utf8_lossy(&a.frame(t0)).into_owned()
        };
        let text = drawn(&mut a, "ab", "cd");
        assert!(text.contains("ab\r\ncd"), "{text:?}");

        let mut d = drawing();
        d.client.lean_output = false;
        a.reconfigure(&d, t0);
        let text = drawn(&mut a, "ef", "gh");
        assert!(text.contains("ef\x1b[3;1Hgh"), "{text:?}");

        d.client.lean_output = true;
        a.reconfigure(&d, t0);
        let text = drawn(&mut a, "ij", "kl");
        assert!(text.contains("ij\r\nkl"), "{text:?}");
    }

    /// Settings come in the card's way, in the client's input, and go to the
    /// client rather than to the cards.
    #[test]
    fn settings_in_the_input_reconfigure_the_client() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);
        let mut d = drawing();
        d.client.centre = true;
        a.keys(&crate::card::encode(&crate::card::Command::Settings(d)), t0);
        assert!(a.centre);
        assert!(runs_lua(&calls(&mut a), "local most"));
    }

    /// A key in normal mode has Neovim flush twice a fraction of a
    /// millisecond apart, the key shown with `'showcmd'` and then what it
    /// did: after a quiet spell both are drawn at once, and a burst after
    /// them no more often than [`MIN_FRAME_GAP`].
    #[test]
    fn what_a_key_did_is_drawn_at_once_after_a_quiet_spell() {
        let t0 = Instant::now();
        let mut a = attached(true, t0);
        a.frame(t0);
        // Quiet, and then the key shown...
        let shown = t0 + Duration::from_secs(1);
        redraw(&mut a, vec![line(1, 0, "k")], shown);
        assert!(a.due(shown), "the key shown, at once");
        a.frame(shown);
        // ...and what it did, half a millisecond after: at once too.
        let did = shown + Duration::from_micros(500);
        redraw(&mut a, vec![line(1, 1, "did")], did);
        assert_eq!(a.wake_at(did), Some(did));
        assert!(a.due(did), "what the key did, at once");
        a.frame(did);
        // A burst from there: a frame no sooner than the gap allows.
        let more = did + Duration::from_micros(500);
        redraw(&mut a, vec![line(1, 2, "more")], more);
        assert!(!a.due(more), "the third, held");
        assert_eq!(a.wake_at(more), Some(did + MIN_FRAME_GAP));
        let third = did + MIN_FRAME_GAP;
        assert!(a.due(third));
        a.frame(third);
        let again = third + Duration::from_micros(500);
        redraw(&mut a, vec![line(1, 3, "again")], again);
        assert!(!a.due(again), "and the fourth");
        assert_eq!(a.wake_at(again), Some(third + MIN_FRAME_GAP));
    }

    /// A card is a frame when it comes, and another each time it looks
    /// different — its age ticking, its words going — and none in between:
    /// none for the mark it leaves, which stays as it is.
    #[test]
    fn a_card_is_drawn_when_it_changes_and_only_then() {
        let t0 = Instant::now();
        let mut a = attached(true, t0);
        a.frame(t0);
        assert!(!a.due(t0), "drawn");
        let card = |id, mark| {
            crate::card::Card {
                life_ms: 2500,
                fade_ms: 0,
                ..crate::card::Card::news(id, crate::card::Kind::Finished, "s", "done", 3, "")
            }
            .leaving(mark)
        };
        let came = t0 + MIN_FRAME_GAP;
        a.keys(
            &crate::card::encode(&crate::card::Command::Show(card(1, None))),
            t0,
        );
        assert!(a.due(came), "a card that comes is a frame");
        a.frame(came);
        let tick = t0 + Duration::from_secs(1);
        assert!(a.wake_at(came).is_some_and(|at| at <= tick));
        assert!(!a.due(tick - Duration::from_millis(1)));
        assert!(a.due(tick), "its age ticks");
        a.frame(tick);
        assert!(!a.due(tick));
        // Timed from the frame it was first drawn on.
        let gone = came + Duration::from_millis(2500);
        assert!(a.due(gone), "its words go, and it with them");
        a.frame(gone);
        assert!(a.cards.is_empty());
        assert_eq!(a.cards_change(gone), None, "nothing more to wake for");

        let mark = Some(crate::card::Kind::Finished);
        a.keys(
            &crate::card::encode(&crate::card::Command::Show(card(2, mark))),
            gone,
        );
        let came = gone + MIN_FRAME_GAP;
        a.frame(came);
        let marked = came + Duration::from_millis(2500);
        assert!(a.due(marked), "its words go");
        a.frame(marked);
        assert!(!a.cards.is_empty(), "its mark stays");
        assert_eq!(a.cards_change(marked), None, "and is still");
    }

    /// Tell the client what nvmux would, in its input.
    fn told(a: &mut App, cmd: &crate::card::Command, now: Instant) {
        a.keys(&crate::card::encode(cmd), now);
    }

    fn goto(row: u64, col: u64) -> (&'static str, Vec<Value>) {
        ("grid_cursor_goto", vec![1.into(), row.into(), col.into()])
    }

    /// Away, a client draws no sooner than [`AWAY_FRAME_GAP`] after its last
    /// frame: what it had still to draw as it went, and then a session that
    /// draws all the while, a frame a gap, the last of it always. Back in
    /// front, what is left is drawn at once, and from then on as in front.
    #[test]
    fn a_client_away_draws_at_a_crawl_and_back_in_front_at_once() {
        use crate::card::Command::{Away, Clear};
        let t0 = Instant::now();
        let mut a = attached(true, t0);
        a.frame(t0);
        let gone = t0 + Duration::from_millis(10);
        told(&mut a, &Away, gone);
        assert!(!a.due(gone), "drawn a moment ago");
        assert_eq!(a.wake_at(gone), Some(t0 + AWAY_FRAME_GAP));
        let first = t0 + AWAY_FRAME_GAP;
        assert!(a.due(first));
        a.frame(first);
        assert_eq!(a.wake_at(first), None, "nothing more to wake for");

        let busy = first + Duration::from_millis(1);
        redraw(&mut a, vec![line(1, 0, "a")], busy);
        assert!(!a.due(busy), "held");
        let more = busy + Duration::from_millis(300);
        redraw(&mut a, vec![line(1, 1, "b")], more);
        assert!(!a.due(more), "held with it");
        let second = first + AWAY_FRAME_GAP;
        assert_eq!(a.wake_at(more), Some(second));
        assert!(!a.due(second - Duration::from_millis(1)));
        assert!(a.due(second), "both, a gap on");
        a.frame(second);
        // A change after as long as the gap, quiet, is drawn when it comes.
        let late = second + 3 * AWAY_FRAME_GAP;
        redraw(&mut a, vec![line(1, 2, "c")], late);
        assert!(a.due(late));
        a.frame(late);

        let left = late + Duration::from_millis(100);
        redraw(&mut a, vec![line(1, 3, "d")], left);
        assert!(!a.due(left));
        let front = left + Duration::from_millis(5);
        told(&mut a, &Clear, front);
        assert_eq!(a.wake_at(front), Some(front));
        assert!(a.due(front), "back in front: at once");
        a.frame(front);
        // A key's two flushes, after a quiet spell: both at once.
        let shown = front + Duration::from_millis(100);
        redraw(&mut a, vec![line(1, 4, "k")], shown);
        assert!(a.due(shown));
        a.frame(shown);
        let did = shown + Duration::from_micros(500);
        redraw(&mut a, vec![line(1, 5, "did")], did);
        assert!(a.due(did), "drawn as in front");
    }

    /// A blinking cursor wakes the client on a clock of its own, a frame at
    /// a time while it fades: not away, where nobody would see it fade.
    #[test]
    fn a_client_away_blinks_no_more() {
        use crate::card::Command::{Away, Clear};
        let t0 = Instant::now();
        let mut a = attached(true, t0);
        a.anim.set_effects(Effects {
            blink: true,
            ..Effects::none()
        });
        let block = Value::Map(vec![
            ("name".into(), "normal".into()),
            ("cursor_shape".into(), "block".into()),
            ("blinkwait".into(), 700.into()),
            ("blinkon".into(), 400.into()),
            ("blinkoff".into(), 250.into()),
        ]);
        redraw(
            &mut a,
            vec![
                (
                    "mode_info_set",
                    vec![true.into(), Value::Array(vec![block])],
                ),
                ("mode_change", vec!["normal".into(), 0.into()]),
            ],
            t0,
        );
        a.frame(t0);
        assert!(a.wake_at(t0).is_some(), "it blinks");

        let gone = t0 + Duration::from_secs(5);
        told(&mut a, &Away, gone);
        assert!(a.due(gone));
        a.frame(gone);
        assert_eq!(a.wake_at(gone), None, "away, it blinks no more");
        for later in [
            gone + Duration::from_secs(1),
            gone + Duration::from_secs(10),
        ] {
            assert!(!a.due(later), "nor is it drawn");
        }

        let front = gone + Duration::from_secs(20);
        told(&mut a, &Clear, front);
        assert!(a.wake_at(front).is_some(), "back in front, it blinks again");
    }

    /// A card up as the client goes — the one a switch lands with, drawn on
    /// the screen it leaves — has a clock of its own, its age ticking and its
    /// words going: away, nobody is there to see it, and it is let go of with
    /// the rest as the client comes back.
    #[test]
    fn a_client_away_draws_no_card_on_its_clock() {
        use crate::card::Command::{Away, Clear, Show};
        let t0 = Instant::now();
        let mut a = attached(true, t0);
        a.frame(t0);
        let card = crate::card::Card {
            life_ms: 2500,
            fade_ms: 0,
            ..crate::card::Card::news(1, crate::card::Kind::Finished, "s", "done", 3, "")
        };
        told(&mut a, &Show(card), t0);
        let came = t0 + MIN_FRAME_GAP;
        a.frame(came);
        assert!(a.cards_change(came).is_some(), "its age ticks");

        let gone = came + Duration::from_millis(100);
        told(&mut a, &Away, gone);
        let drawn = came + AWAY_FRAME_GAP;
        assert!(a.due(drawn));
        a.frame(drawn);
        assert_eq!(a.wake_at(drawn), None, "nothing on its clock");
        for later in [
            drawn + Duration::from_millis(500),
            came + Duration::from_secs(2),
        ] {
            assert!(!a.due(later), "nor is it drawn");
        }

        let front = came + Duration::from_secs(10);
        told(&mut a, &Clear, front);
        assert!(a.due(front), "taken down at once");
        a.frame(front);
        assert!(a.cards.is_empty());
    }

    /// The cursor travels where it moves in front. Away, whatever was on its
    /// way stops where it was going, and nothing sets off.
    #[test]
    fn a_client_away_moves_nothing() {
        use crate::card::Command::{Away, Clear};
        let t0 = Instant::now();
        let mut a = attached(true, t0);
        a.anim.set_effects(Effects {
            smear: Some(anim::smear::Settings {
                duration: 0.1,
                short: 0.04,
                trail: 0.4,
                gradient: 1.0,
            }),
            ..Effects::none()
        });
        redraw(&mut a, vec![goto(0, 0)], t0);
        a.frame(t0);
        let moved = t0 + Duration::from_secs(1);
        redraw(&mut a, vec![goto(4, 10)], moved);
        a.frame(moved);
        assert!(a.anim.next_frame(&a.model, moved).is_some(), "it travels");

        let gone = moved + Duration::from_millis(5);
        told(&mut a, &Away, gone);
        assert!(a.anim.next_frame(&a.model, gone).is_none(), "stopped");
        let drawn = moved + AWAY_FRAME_GAP;
        assert_eq!(
            a.wake_at(gone),
            Some(drawn),
            "where it was going, at a crawl"
        );
        a.frame(drawn);
        // The first move after the stop only places it, as the first ever
        // does; it is the next that would travel.
        let again = drawn + Duration::from_secs(5);
        redraw(&mut a, vec![goto(0, 0)], again);
        a.frame(again);
        let further = again + Duration::from_secs(5);
        redraw(&mut a, vec![goto(4, 10)], further);
        assert!(
            a.anim.next_frame(&a.model, further).is_none(),
            "a move away sets nothing off"
        );
        a.frame(further);
        assert_eq!(a.wake_at(further), None);

        let front = further + Duration::from_secs(1);
        told(&mut a, &Clear, front);
        redraw(&mut a, vec![goto(0, 0)], front);
        a.frame(front);
        assert!(
            a.anim.next_frame(&a.model, front).is_some(),
            "back in front, it travels again"
        );
    }

    /// A multigrid attach leaves the agent in the editor, given the client's
    /// channel. A window it says took a buffer other than the one it showed
    /// is marked for the next flush, which draws it; a window it has not
    /// told of is a new one, not a switch.
    #[test]
    fn the_agent_says_which_window_took_another_buffer() {
        let t0 = Instant::now();
        let mut a = app(true);
        a.start();
        let sent = calls(&mut a);
        answer(
            &mut a,
            request(&sent, "nvim_get_api_info", ""),
            Value::Array(vec![7.into()]),
            t0,
        );
        answer(&mut a, asks_uis(&sent), uis(false, false), t0);
        let sent = calls(&mut a);
        let agent = request(&sent, "nvim_exec_lua", "local chan");
        let (_, _, args) = sent
            .iter()
            .find(|(id, ..)| *id == Some(agent))
            .expect("the agent");
        assert_eq!(
            args[1].as_array().map(|a| a[..2].to_vec()),
            Some(vec![7.into(), false.into()]),
            "given the channel, and that the client does not predict"
        );
        let wins = Value::Array(vec![Value::Array(vec![1000.into(), 1.into()])]);
        let said = vec![3.into(), true.into(), false.into(), wins];
        answer(&mut a, agent, Value::Array(said), t0);
        assert_eq!(a.bufs.get(&1000), Some(&1));
        let notify = |a: &mut App, params: Vec<Value>| {
            let v = Value::Array(vec![2.into(), "nvmux_client".into(), Value::Array(params)]);
            a.message(&encoded(v), t0);
        };
        notify(&mut a, vec!["buf".into(), 1000.into(), 1.into()]);
        assert!(a.switched.is_empty(), "the buffer it showed");
        notify(&mut a, vec!["buf".into(), 1000.into(), 2.into()]);
        notify(&mut a, vec!["buf".into(), 1001.into(), 3.into()]);
        assert_eq!(a.switched, vec![1000]);
        redraw(&mut a, vec![resize(1, 20, 6)], t0);
        assert!(a.switched.is_empty(), "taken by the flush");
        notify(&mut a, vec!["closed".into(), 1001.into()]);
        assert!(!a.bufs.contains_key(&1001));
        // Without the effect, no agent.
        let mut a = app(false);
        a.start();
        let sent = calls(&mut a);
        answer(
            &mut a,
            request(&sent, "nvim_get_api_info", ""),
            Value::Array(vec![7.into()]),
            t0,
        );
        assert!(!calls(&mut a).iter().any(|(_, m, _)| m == "nvim_exec_lua"));
    }

    /// The agent's options, and Neovim's defaults for what it leaves out.
    #[test]
    fn the_agents_options_are_read() {
        let said = [Value::from(3), Value::from(true), Value::from(false)];
        let refs: Vec<ValueRef<'_>> = said.iter().map(Value::as_ref).collect();
        let o = options(&refs);
        assert_eq!((o.laststatus, o.splitright, o.splitbelow), (3, true, false));
        assert_eq!(options(&[]), anim::layout::Options::default());
    }

    /// Who else is attached is asked before attaching, and decides how.
    #[test]
    fn the_attach_waits_for_who_else_is_attached() {
        let t0 = Instant::now();
        for (foreign, multigrid) in [(false, true), (true, false)] {
            let mut a = app(true);
            a.start();
            let sent = calls(&mut a);
            assert!(
                attached_with_multigrid(&sent).is_none(),
                "not before the answer"
            );
            let (_, _, info) = sent
                .iter()
                .find(|(_, m, _)| m == "nvim_set_client_info")
                .expect("client info");
            assert_eq!(info[0], Value::from("nvmux"));
            answer(&mut a, asks_uis(&sent), uis(foreign, false), t0);
            assert_eq!(attached_with_multigrid(&calls(&mut a)), Some(multigrid));
        }
        // With nothing that needs windows of their own: at once, without.
        let mut a = app(false);
        a.start();
        assert_eq!(attached_with_multigrid(&calls(&mut a)), Some(false));
    }

    /// Attached to a session another UI drew, a window placed nowhere with a
    /// few cells on it — those that changed since that UI saw it — is as
    /// good as one with nothing: it is refreshed, and the screen waits for
    /// it, none of those cells drawn until the rest are.
    #[test]
    fn a_few_cells_of_a_window_drawn_before_wait_for_the_rest() {
        let t0 = Instant::now();
        let mut a = app(true);
        a.start();
        let uis_id = asks_uis(&calls(&mut a));
        answer(&mut a, uis_id, uis(false, false), t0);
        calls(&mut a);
        redraw(
            &mut a,
            vec![
                resize(1, 20, 6),
                resize(2, 20, 5),
                viewport(2, 1000),
                line(2, 3, "let mut"),
            ],
            t0,
        );
        assert!(a.take_refresh(), "refreshed, a few cells or none");
        assert!(!a.due(t0), "waited for");
    }

    /// Attached to a session another UI drew, the windows come placed
    /// nowhere and with nothing on them: their layout is asked for and a
    /// refresh started, nothing is drawn until both are in, and then all of
    /// it is.
    #[test]
    fn a_session_drawn_before_is_put_together_before_it_is_drawn() {
        let t0 = Instant::now();
        let mut a = app(true);
        a.start();
        let uis_id = asks_uis(&calls(&mut a));
        answer(&mut a, uis_id, uis(false, false), t0);
        calls(&mut a);
        redraw(
            &mut a,
            vec![
                resize(1, 20, 6),
                resize(2, 9, 5),
                resize(4, 10, 5),
                viewport(2, 1000),
                viewport(4, 1001),
                ("grid_cursor_goto", vec![4.into(), 1.into(), 0.into()]),
            ],
            t0,
        );
        assert!(
            a.take_refresh(),
            "a refresh, for windows with nothing on them"
        );
        assert!(!a.take_refresh(), "one");
        let layout = request(&calls(&mut a), "nvim_exec_lua", "local cur");
        assert!(!a.due(t0), "nothing drawn yet");

        let n = Value::from;
        answer(
            &mut a,
            layout,
            Value::Array(vec![
                Value::Array(vec![n(1), n(0), n(11), n(0)]),
                Value::Array(vec![n(1), n(0), n(0), n(1)]),
            ]),
            t0,
        );
        assert!(!a.due(t0), "the windows have nothing on them yet");
        redraw(
            &mut a,
            [
                whole(2, 9, 5, &["right"]),
                whole(4, 10, 5, &["bar", "left"]),
            ]
            .concat(),
            t0,
        );
        assert!(a.due(t0));
        assert!(!a.take_refresh(), "nothing lacking now");
        let f = compose::compose(&a.model, &a.anim, 20, 6);
        let text = f.text();
        let rows: Vec<&str> = text.lines().take(2).collect();
        assert_eq!(rows, ["bar        right    ", "left                "]);
        assert_eq!(a.model.margins(4).top, 1, "the winbar, as the layout said");
        assert_eq!(a.model.cursor_on_screen(), (1, 0));
    }

    /// Attached on a terminal taller than the last UI's, or wider, a window
    /// Neovim kept from that UI comes resized and placed, and drawn on only
    /// where the resize added to it: its last rows, or the last columns of
    /// every row. The rest Neovim takes the client to have as that UI did.
    /// It is refreshed, nothing is drawn until it is in whole, and then all
    /// of it is.
    #[test]
    fn a_window_drawn_only_where_it_grew_is_waited_for() {
        let t0 = Instant::now();
        let taller: Vec<_> = (3..5).map(|r| line(2, r, &" ".repeat(20))).collect();
        let wider: Vec<_> = (0..5).map(|r| line_at(2, r, 12, &" ".repeat(8))).collect();
        for (grew, drawn) in [("taller", taller), ("wider", wider)] {
            let mut a = app(true);
            a.start();
            let uis_id = asks_uis(&calls(&mut a));
            answer(&mut a, uis_id, uis(false, false), t0);
            calls(&mut a);
            let mut events = vec![
                resize(1, 20, 6),
                resize(2, 20, 5),
                place(2, 1000, 0, 0, 20, 5),
            ];
            events.extend(drawn);
            redraw(&mut a, events, t0);
            assert!(a.take_refresh(), "{grew}: a refresh, for what it kept");
            assert!(!a.due(t0), "{grew}: and the frame held for it");
            assert!(
                calls(&mut a).iter().all(|(_, m, _)| m != "nvim_exec_lua"),
                "{grew}: placed, so no layout asked"
            );

            // What the refresh has Neovim draw: every cell of it.
            redraw(&mut a, whole(2, 20, 5, &["kept from before"]), t0);
            assert!(a.due(t0), "{grew}: drawn once it is whole");
            assert!(!a.take_refresh(), "{grew}: nothing lacking now");
            assert_eq!(top_row(&a), "kept from before", "{grew}");
        }
    }

    /// A layout that never answers keeps the screen from being drawn for a
    /// while, not for good.
    #[test]
    fn the_wait_is_given_up() {
        let t0 = Instant::now();
        let mut a = app(true);
        a.start();
        let uis_id = asks_uis(&calls(&mut a));
        answer(&mut a, uis_id, uis(false, false), t0);
        redraw(
            &mut a,
            vec![resize(1, 20, 6), resize(2, 10, 5), viewport(2, 1000)],
            t0,
        );
        assert!(!a.due(t0));
        assert_eq!(a.wake_at(t0), Some(t0 + HOLD_LIMIT));
        assert!(a.due(t0 + HOLD_LIMIT));
    }

    /// A client attaching again, asked to while a hold was still waiting,
    /// waits for Neovim's answer without waking for a hold long over: a
    /// wake time already gone would have its loop poll without waiting,
    /// round and round, for as long as Neovim takes to answer.
    #[test]
    fn a_detach_answered_late_is_waited_for_without_spinning() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);
        // Who else is attached changed, and the answer has yet to come: the
        // hold begins.
        redraw(&mut a, vec![ui_change()], t0);
        calls(&mut a);
        // Told to predict meanwhile, the client detaches to attach again.
        let mut d = drawing();
        d.client.predict = true;
        a.reconfigure(&d, t0 + Duration::from_millis(100));
        let sent = calls(&mut a);
        let detach = request(&sent, "nvim_ui_detach", "");
        // The hold is there, its end still to come.
        assert_eq!(
            a.wake_at(t0 + Duration::from_millis(100)),
            Some(t0 + HOLD_LIMIT)
        );
        // Long past the hold, with no answer yet.
        let late = t0 + 2 * HOLD_LIMIT;
        assert!(!a.due(late));
        assert!(
            a.wake_at(late).is_none_or(|at| at > late),
            "woken at once, for a hold long over: {:?}",
            a.wake_at(late).map(|at| late.saturating_duration_since(at))
        );
        // The answer still ends the wait.
        answer(&mut a, detach, Value::Nil, late);
        assert_eq!(attached_with_multigrid(&calls(&mut a)), Some(true));
    }

    /// A UI that keeps windows on grid 1 attaching has the client detach
    /// and attach again without `ext_multigrid`, and its going has it attach
    /// with it again; what is drawn for an attach after its detach is asked
    /// is not taken.
    #[test]
    fn a_foreign_ui_sends_the_client_to_grid_1_and_back() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);

        redraw(&mut a, vec![ui_change()], t0);
        let sent = calls(&mut a);
        let uis_id = asks_uis(&sent);
        let mine = sent
            .iter()
            .find(|(id, ..)| *id == Some(uis_id))
            .map(|(_, _, args)| args[1].clone());
        assert_eq!(mine, Some(Value::Array(vec![7.into()])), "itself left out");
        assert!(!a.due(t0), "unsettled");
        answer(&mut a, uis_id, uis(true, false), t0);
        let detach = request(&calls(&mut a), "nvim_ui_detach", "");
        redraw(&mut a, vec![resize(9, 3, 3)], t0);
        assert!(!a.model.grids.contains_key(&9), "the old attach's");
        answer(&mut a, detach, Value::Nil, t0);
        assert_eq!(attached_with_multigrid(&calls(&mut a)), Some(false));
        assert!(!a.due(t0), "not drawn before the new attach's first flush");
        redraw(&mut a, vec![resize(1, 20, 6)], t0);
        assert!(a.due(t0), "on grid 1, nothing to wait for");

        redraw(&mut a, vec![ui_change()], t0);
        let uis_id = asks_uis(&calls(&mut a));
        answer(&mut a, uis_id, uis(false, false), t0);
        let detach = request(&calls(&mut a), "nvim_ui_detach", "");
        answer(&mut a, detach, Value::Nil, t0);
        assert_eq!(attached_with_multigrid(&calls(&mut a)), Some(true));
    }

    /// A UI only passing through grid 1 — another nvmux client's refresh —
    /// is waited out rather than followed there; settled, the windows'
    /// places are asked again.
    #[test]
    fn a_ui_passing_through_is_waited_out() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);
        let mut events = vec![resize(2, 10, 5)];
        events.extend(whole(2, 10, 5, &["x"]));
        events.extend([viewport(2, 1000), place(2, 1000, 0, 0, 10, 5)]);
        redraw(&mut a, events, t0);
        assert!(
            a.due(t0),
            "a session drawn for this client: nothing lacking"
        );
        calls(&mut a);

        redraw(&mut a, vec![ui_change()], t0);
        let uis_id = asks_uis(&calls(&mut a));
        answer(&mut a, uis_id, uis(false, true), t0);
        assert!(calls(&mut a).iter().all(|(_, m, _)| m != "nvim_ui_detach"));
        assert!(!a.due(t0));
        assert_eq!(a.wake_at(t0), Some(t0 + RECHECK));
        let later = t0 + RECHECK;
        a.tick(later);
        let uis_id = asks_uis(&calls(&mut a));
        answer(&mut a, uis_id, uis(false, false), later);
        let layout = request(&calls(&mut a), "nvim_exec_lua", "local cur");
        assert!(!a.due(later), "where the windows are is asked again");
        let n = Value::from;
        answer(
            &mut a,
            layout,
            Value::Array(vec![Value::Array(vec![n(1), n(1), n(2), n(0)])]),
            later,
        );
        assert!(a.due(later));
        assert_eq!(a.origin_of(2), (1, 2), "and put where the answer says");
    }

    /// A tab page the client has never seen: its windows are placed with
    /// grids the client has never been sent, and a refresh is had for them.
    #[test]
    fn a_tab_page_never_seen_is_refreshed_for() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);
        redraw(
            &mut a,
            vec![(
                "win_pos",
                vec![
                    6.into(),
                    1003.into(),
                    1.into(),
                    0.into(),
                    20.into(),
                    4.into(),
                ],
            )],
            t0,
        );
        assert!(a.take_refresh());
        assert!(!a.due(t0));
        let mut events = vec![resize(6, 20, 4)];
        events.extend(whole(6, 20, 4, &["tab two"]));
        redraw(&mut a, events, t0);
        assert!(a.due(t0));
    }

    /// A client that predicts, with no effects at all, attached with
    /// windows of its own over a link whose round trip is `rtt`, its agent
    /// left: one window of 20 × 5 over a buffer of 50 lines, `line 0` at the
    /// top, the cursor on it, in normal mode with the mouse on.
    fn predicting(rtt: Duration) -> App {
        let t0 = Instant::now();
        let mut a = App::new(palette(), Effects::none(), true, (20, 6));
        a.start();
        let sent = calls(&mut a);
        answer(
            &mut a,
            request(&sent, "nvim_get_api_info", ""),
            Value::Array(vec![7.into()]),
            t0 + rtt,
        );
        answer(&mut a, asks_uis(&sent), uis(false, false), t0 + rtt);
        let sent = calls(&mut a);
        assert_eq!(
            attached_with_multigrid(&sent),
            Some(true),
            "windows of its own"
        );
        let agent = request(&sent, "nvim_exec_lua", "local chan");
        let (_, _, args) = sent
            .iter()
            .find(|(id, ..)| *id == Some(agent))
            .expect("agent");
        let args = args[1].as_array().expect("its arguments");
        assert_eq!(args[..2], [7.into(), true.into()], "told to predict");
        let keys = args[2].as_array().expect("the keys to look up");
        assert!(keys.contains(&Value::from("<C-d>")));
        let mut events = vec![
            resize(1, 20, 6),
            resize(2, 20, 5),
            (
                "win_pos",
                vec![
                    2.into(),
                    1000.into(),
                    0.into(),
                    0.into(),
                    20.into(),
                    5.into(),
                ],
            ),
            ("mode_change", vec!["normal".into(), 0.into()]),
            ("mouse_on", vec![]),
            ("grid_cursor_goto", vec![2.into(), 0.into(), 0.into()]),
            at(0, 0, 0),
        ];
        let rows: Vec<String> = (0..5).map(|r| format!("line {r}")).collect();
        events.extend(whole(2, 20, 5, &rows));
        redraw(&mut a, events, t0 + rtt);
        let notify = |a: &mut App, params: Vec<Value>| {
            let v = Value::Array(vec![2.into(), "nvmux_client".into(), Value::Array(params)]);
            a.message(&encoded(v), t0 + rtt);
        };
        let shape: Vec<Value> = vec![
            1.into(),
            50.into(),
            0.into(),
            8.into(),
            false.into(),
            false.into(),
            true.into(),
            false.into(),
            0.into(),
            true.into(),
            0.into(),
            3.into(),
            "~".into(),
            false.into(),
            "".into(),
            2.into(),
            0.into(),
            false.into(),
            Value::Array(vec![]),
        ];
        notify(
            &mut a,
            vec!["shape".into(), 1000.into(), Value::Array(shape)],
        );
        let text: Vec<Value> = (0..20).map(|n| Value::from(format!("line {n}"))).collect();
        notify(
            &mut a,
            vec![
                "lines".into(),
                1000.into(),
                1.into(),
                1.into(),
                0.into(),
                Value::Array(text),
                true.into(),
            ],
        );
        calls(&mut a);
        a
    }

    /// `win_viewport` for window grid 2 of [`predicting`]'s: a view of five
    /// rows from `topline`, scrolled `delta`, the cursor on `curline`.
    fn at(topline: i64, curline: i64, delta: i64) -> (&'static str, Vec<Value>) {
        let n = Value::from;
        (
            "win_viewport",
            vec![
                2.into(),
                1000.into(),
                n(topline),
                n(topline + 6),
                n(curline),
                n(0),
                n(50),
                n(delta),
            ],
        )
    }

    fn top_row(a: &App) -> String {
        let f = compose::compose(&a.model, &a.anim, 20, 6);
        f.text()
            .lines()
            .next()
            .unwrap_or_default()
            .trim_end()
            .to_string()
    }

    /// Over a slow link a turn of the wheel scrolls the window at once, and
    /// still goes to Neovim; Neovim's scroll, when it comes, lands where the
    /// client already is.
    #[test]
    fn a_turn_of_the_wheel_over_a_slow_link_scrolls_at_once() {
        let mut a = predicting(Duration::from_millis(200));
        let t1 = Instant::now() + Duration::from_secs(1);
        a.keys(b"\x1b[<65;3;2M", t1);
        assert_eq!(top_row(&a), "line 3", "before Neovim has heard of it");
        assert!(a.due(t1), "and drawn");
        assert_eq!(
            a.anim.cursor_at(&a.model),
            (0, 0),
            "the cursor on line 3 now"
        );
        let sent = calls(&mut a);
        assert!(
            sent.iter().any(|(_, m, args)| m == "nvim_input_mouse"
                && args[..2] == [Value::from("wheel"), Value::from("down")]
                && args[3] == Value::from(2)),
            "{sent:?}"
        );

        let mut events = vec![
            (
                "grid_scroll",
                vec![
                    2.into(),
                    0.into(),
                    5.into(),
                    0.into(),
                    20.into(),
                    3.into(),
                    0.into(),
                ],
            ),
            at(3, 3, 3),
        ];
        events.extend((2..5).map(|r| line(2, r, &format!("line {}", r + 3))));
        redraw(&mut a, events, t1 + Duration::from_millis(200));
        assert_eq!(a.anim.ahead(2), (0, 0), "confirmed");
        assert_eq!(top_row(&a), "line 3");

        // Up past the top goes no further than it.
        let t2 = t1 + Duration::from_millis(300);
        a.keys(b"\x1b[<64;3;2M\x1b[<64;3;2M", t2);
        assert_eq!(top_row(&a), "line 0");
        assert_eq!(a.anim.ahead(2), (-3, 0));

        // With Ctrl, a page: three rows of five, the cursor to the top.
        a.keys(b"\x1b[<81;3;2M", t2);
        assert_eq!(top_row(&a), "line 3");
        assert_eq!(a.anim.cursor_at(&a.model), (0, 0));
    }

    /// Keys that scroll, over a slow link, once Neovim has caught up with
    /// every key before them: made at once, the cursor taken where Neovim
    /// takes it — and not while a key before them has yet to be drawn.
    #[test]
    fn keys_that_scroll_scroll_at_once() {
        let mut a = predicting(Duration::from_millis(200));
        let t1 = Instant::now() + Duration::from_secs(1);
        a.keys(b"\x04", t1);
        assert_eq!(top_row(&a), "line 0", "not before the attach's fence");
        a.tick(t1);
        let fence = request(&calls(&mut a), "nvim_eval", "");
        answer(&mut a, fence, Value::from(0), t1);
        // <C-d>: 'scroll' is 2.
        a.keys(b"\x04", t1);
        assert_eq!(top_row(&a), "line 2");
        assert_eq!(a.anim.cursor_at(&a.model), (0, 0), "on line 2, at the top");
        assert!(calls(&mut a)
            .iter()
            .any(|(_, m, args)| m == "nvim_input" && args[0] == Value::from("<C-d>")));
        // A key that moves the cursor: the scroll after it waits for Neovim,
        // and a fence is asked after them both.
        a.keys(b"j\x05", t1);
        assert_eq!(top_row(&a), "line 2");
        let sent = calls(&mut a);
        let fence = request(&sent, "nvim_eval", "");
        assert_eq!(
            sent.last().and_then(|(id, ..)| *id),
            Some(fence),
            "after the keys"
        );
        answer(&mut a, fence, Value::from(0), t1);
        a.keys(b"zt", t1);
        assert_eq!(
            top_row(&a),
            "line 2",
            "the cursor is on it, as far as is known"
        );
        a.keys(b"2\x05", t1);
        assert_eq!(top_row(&a), "line 4", "a count");
    }

    /// Over a fast link, or with a window the agent says is not to be
    /// predicted, a turn of the wheel waits for Neovim.
    #[test]
    fn a_turn_of_the_wheel_waits_where_it_cannot_be_predicted() {
        let mut a = predicting(Duration::ZERO);
        let t1 = Instant::now() + Duration::from_secs(1);
        a.keys(b"\x1b[<65;3;2M", t1);
        assert_eq!(top_row(&a), "line 0", "a fast link");
        assert!(calls(&mut a)
            .iter()
            .any(|(_, m, _)| m == "nvim_input_mouse"));

        let mut a = predicting(Duration::from_millis(200));
        let mut a2 = predicting(Duration::from_millis(200));
        // A view that is not a line to a row: a line wraps, say.
        let n = Value::from;
        let short = (
            "win_viewport",
            vec![2.into(), 1000.into(), n(0), n(5), n(0), n(0), n(50), n(0)],
        );
        redraw(&mut a2, vec![short], t1);
        a2.keys(b"\x1b[<65;3;2M", t1);
        assert_eq!(top_row(&a2), "line 0", "a line taking two rows");
        // In the command line.
        redraw(
            &mut a,
            vec![("mode_change", vec!["cmdline_normal".into(), 0.into()])],
            t1,
        );
        a.keys(b"\x1b[<65;3;2M", t1);
        assert_eq!(top_row(&a), "line 0", "the command line");
    }

    /// Neovim behind on the keys typed sends no frame till it has taken the
    /// last of them, and nothing lands what the client scrolled ahead of it
    /// meanwhile: its agent says it is still at work as it moves each view,
    /// and each time, what is ahead waits as long again as a scroll made then
    /// would — the loop woken for it then. Not heard from, a scroll is given
    /// up on as ever, two round trips and a little more after it was made.
    #[test]
    fn scrolls_ahead_wait_while_neovim_is_at_work_on_the_keys() {
        let ms = Duration::from_millis;
        let t1 = Instant::now() + Duration::from_secs(1);
        let behind = |a: &mut App, now| notify(a, "nvmux_client", vec!["behind".into()], now);
        let mut a = predicting(ms(200));
        a.keys(b"\x1b[<65;3;2M\x1b[<65;3;2M", t1);
        assert_eq!(top_row(&a), "line 6");
        let p = a.predict.as_ref().expect("predicting");
        let (made, heard) = (p.deadline(t1), p.deadline(t1 + ms(500)));
        behind(&mut a, t1 + ms(500));
        a.frame(made);
        assert_eq!(a.anim.ahead(2), (6, 6), "still waited for");
        assert_eq!(a.wake_at(made), Some(heard), "and woken for");
        a.frame(heard);
        assert_eq!(a.anim.ahead(2), (0, 0), "given up on");
        assert_eq!(top_row(&a), "line 0");

        let mut a = predicting(ms(200));
        a.keys(b"\x1b[<65;3;2M", t1);
        let made = a.predict.as_ref().expect("predicting").deadline(t1);
        a.frame(made - ms(1));
        assert_eq!(a.anim.ahead(2), (3, 3));
        a.frame(made);
        assert_eq!(a.anim.ahead(2), (0, 0), "not heard from");
    }

    /// The round trip is measured again while the user is at work, and not
    /// while they are not.
    #[test]
    fn the_round_trip_is_measured_while_at_work() {
        let mut a = predicting(Duration::from_millis(200));
        let t1 = Instant::now() + Duration::from_secs(10);
        a.tick(t1);
        assert!(
            calls(&mut a).iter().all(|(_, m, _)| m != "nvim_get_mode"),
            "idle"
        );
        a.keys(b"j", t1);
        a.tick(t1);
        let ping = request(&calls(&mut a), "nvim_get_mode", "");
        answer(&mut a, ping, Value::Nil, t1 + Duration::from_millis(1));
        a.tick(t1 + PING_EVERY);
        assert!(
            calls(&mut a).iter().all(|(_, m, _)| m != "nvim_get_mode"),
            "nothing typed since"
        );
        // The link gets faster: a millisecond, every time it is asked.
        let mut t = t1;
        for _ in 0..40 {
            t += PING_EVERY;
            a.keys(b"j", t);
            a.tick(t);
            let ping = request(&calls(&mut a), "nvim_get_mode", "");
            answer(&mut a, ping, Value::Nil, t + Duration::from_millis(1));
        }
        let p = a.predict.as_ref().expect("predicting");
        assert!(!p.active(), "a fast link now");
    }

    /// The agent keeps lines as far either side of a view as a key held down
    /// pages in a round trip: it is told how far when it is left, and again
    /// each time the round trip moves that, and only then.
    #[test]
    fn the_agent_is_told_how_far_a_round_trip_pages() {
        let t0 = Instant::now();
        let mut a = App::new(palette(), Effects::none(), true, (20, 6));
        a.start();
        let sent = calls(&mut a);
        let rtt = Duration::from_millis(200);
        answer(
            &mut a,
            request(&sent, "nvim_get_api_info", ""),
            Value::Array(vec![7.into()]),
            t0 + rtt,
        );
        answer(&mut a, asks_uis(&sent), uis(false, false), t0 + rtt);
        let sent = calls(&mut a);
        let agent = request(&sent, "nvim_exec_lua", "local chan");
        let (_, _, args) = sent
            .iter()
            .find(|(id, ..)| *id == Some(agent))
            .expect("agent");
        let args = args[1].as_array().expect("its arguments");
        assert_eq!(args[4], 12.into(), "ten pages a round trip, and two");

        // Each round trip measured, what the agent is told.
        let mut t = t0 + Duration::from_secs(1);
        let mut measure = |a: &mut App, rtt: Duration| {
            t += PING_EVERY;
            a.keys(b"j", t);
            a.tick(t);
            let ping = request(&calls(a), "nvim_get_mode", "");
            answer(a, ping, Value::Nil, t + rtt);
            calls(a)
                .into_iter()
                .filter(|(_, m, _)| m == "nvim_exec_autocmds")
                .map(|(_, _, args)| {
                    assert_eq!(args[0], "User".into());
                    let opts = args[1].as_map().expect("its options");
                    let get = |k: &str| {
                        opts.iter()
                            .find(|(key, _)| key.as_str() == Some(k))
                            .map(|(_, v)| v.clone())
                    };
                    assert_eq!(get("pattern"), Some("nvmux_client_reach_7".into()));
                    get("data").and_then(|v| v.as_i64()).expect("how far")
                })
                .collect::<Vec<i64>>()
        };
        assert!(measure(&mut a, rtt).is_empty(), "the same round trip");
        // A second, smoothed to 300 ms: fifteen pages.
        assert_eq!(measure(&mut a, Duration::from_secs(1)), [17]);
        // Faster again, down to a link no page outruns, and on to one too
        // fast to predict over: no lines at all.
        let mut told = Vec::new();
        for _ in 0..60 {
            told.extend(measure(&mut a, Duration::ZERO));
        }
        assert_eq!(told.last(), Some(&0));
        assert!(
            told.windows(2).all(|w| w[0] > w[1]),
            "only as it moved: {told:?}"
        );
    }

    /// Over a link too fast to predict over, the agent keeps no lines: it
    /// is left told to keep none, and told how far once the link slows.
    #[test]
    fn the_agent_keeps_no_lines_for_a_link_too_fast_to_predict_over() {
        let t0 = Instant::now();
        let mut a = App::new(palette(), Effects::none(), true, (20, 6));
        a.start();
        let sent = calls(&mut a);
        let rtt = Duration::from_millis(1);
        answer(
            &mut a,
            request(&sent, "nvim_get_api_info", ""),
            Value::Array(vec![7.into()]),
            t0 + rtt,
        );
        answer(&mut a, asks_uis(&sent), uis(false, false), t0 + rtt);
        let sent = calls(&mut a);
        let agent = request(&sent, "nvim_exec_lua", "local chan");
        let (_, _, args) = sent
            .iter()
            .find(|(id, ..)| *id == Some(agent))
            .expect("agent");
        let args = args[1].as_array().expect("its arguments");
        assert_eq!(args[1], true.into(), "left to predict");
        assert_eq!(args[4], 0.into(), "but keeping no lines yet");

        // The link slows: the agent is told how far, and sends them.
        let t1 = t0 + Duration::from_secs(1);
        a.keys(b"j", t1);
        a.tick(t1 + PING_EVERY);
        let ping = request(&calls(&mut a), "nvim_get_mode", "");
        answer(
            &mut a,
            ping,
            Value::Nil,
            t1 + PING_EVERY + Duration::from_secs(1),
        );
        let told = calls(&mut a);
        assert!(
            told.iter().any(|(_, m, _)| m == "nvim_exec_autocmds"),
            "told how far: {told:?}"
        );
    }

    /// A link fast again stops the predictions, and the agent sending what
    /// they are made from: what the client kept of it goes too, since no
    /// edit reaches it from then on.
    #[test]
    fn a_link_fast_again_lets_go_of_the_lines_kept() {
        let mut a = predicting(Duration::from_millis(200));
        assert!(a.predict.as_ref().is_some_and(|p| p.shape(1000).is_some()));
        let mut t = Instant::now() + Duration::from_secs(1);
        let mut told = Vec::new();
        for _ in 0..60 {
            t += PING_EVERY;
            a.keys(b"j", t);
            a.tick(t);
            let ping = request(&calls(&mut a), "nvim_get_mode", "");
            answer(&mut a, ping, Value::Nil, t);
            told.extend(
                calls(&mut a)
                    .into_iter()
                    .filter(|(_, m, _)| m == "nvim_exec_autocmds"),
            );
        }
        assert!(!told.is_empty(), "the agent told");
        let p = a.predict.as_ref().expect("predicting");
        assert!(!p.active(), "a fast link");
        assert!(p.shape(1000).is_none(), "the window's shape let go of");
        // Nothing to predict from: a turn of the wheel goes to Neovim.
        a.keys(b"\x1b[<65;3;2M", t);
        assert_eq!(top_row(&a), "line 0");
    }

    /// The device attributes that close the kitty question are the client's
    /// own; any after are the server's.
    #[test]
    fn the_keyboard_question_is_closed_once() {
        let t0 = Instant::now();
        let mut a = app(false);
        a.keys(b"\x1b[?0u\x1b[?62;22c", t0);
        assert_eq!(a.take_said(), term::KITTY_ON);
        assert!(calls(&mut a)
            .iter()
            .all(|(_, m, _)| m != "nvim_ui_term_event"));
        a.keys(b"\x1b[?62;22c", t0);
        assert!(calls(&mut a)
            .iter()
            .any(|(_, m, _)| m == "nvim_ui_term_event"));
    }

    /// An escape held is let go `'ttimeoutlen'` after it came, whatever else
    /// wakes the loop in between.
    #[test]
    fn an_escape_is_let_go_on_time() {
        let t0 = Instant::now();
        let mut a = app(false);
        a.keys(b"\x1b", t0);
        let at = t0 + Duration::from_millis(50);
        assert_eq!(a.wake_at(t0 + Duration::from_millis(20)), Some(at));
        assert!(!a.escape_due(at - Duration::from_millis(1)));
        assert!(a.escape_due(at));
        a.escape_timeout(at);
        let sent = calls(&mut a);
        assert!(sent
            .iter()
            .any(|(_, m, args)| m == "nvim_input" && args[0] == Value::from("<Esc>")));
        assert_eq!(a.wake_at(at), None);
    }

    /// A terminal's answer part way through is waited for longer than an
    /// escape, from the last of it to come.
    #[test]
    fn a_reply_under_way_is_waited_for() {
        let t0 = Instant::now();
        let mut a = app(false);
        a.keys(b"\x1b]52;c;aGVs", t0);
        assert_eq!(a.wake_at(t0), Some(t0 + REPLY_WAIT));
        let later = t0 + Duration::from_millis(300);
        a.keys(b"bG8", later);
        assert!(!a.escape_due(t0 + REPLY_WAIT));
        a.keys(b"\x1b\\", later);
        assert_eq!(a.wake_at(later), None, "answered");
        let sent = calls(&mut a);
        assert!(sent.iter().any(|(_, m, args)| m == "nvim_ui_term_event"
            && args[1].as_slice() == Some(&b"\x1b]52;c;aGVsbG8"[..])));
    }

    /// A notification from the server: `method` with `params`.
    fn notify(app: &mut App, method: &str, params: Vec<Value>, now: Instant) {
        let v = Value::Array(vec![2.into(), method.into(), Value::Array(params)]);
        app.message(&encoded(v), now);
    }

    /// What `calls` asked of the windows' grids: each grid, and the width
    /// and height asked for it.
    fn grids_asked(calls: &[Call]) -> Vec<Vec<Value>> {
        calls
            .iter()
            .filter(|(_, m, _)| m == "nvim_ui_try_resize_grid")
            .map(|(_, _, args)| args.clone())
            .collect()
    }

    /// Started with each window's text centred, up to 8 columns of it;
    /// attached, and what it sent let go of.
    fn wide_app(t0: Instant) -> (App, Vec<Call>) {
        let mut a = app(true).with_centre(true, 8, false);
        a.start();
        let sent = calls(&mut a);
        let measure = request(&sent, "nvim_exec_lua", "local most");
        answer(
            &mut a,
            request(&sent, "nvim_get_api_info", ""),
            Value::Array(vec![7.into()]),
            t0,
        );
        answer(&mut a, measure, Value::Array(vec![2.into()]), t0);
        answer(&mut a, asks_uis(&sent), uis(false, false), t0);
        let sent = calls(&mut a);
        (a, sent)
    }

    /// What a frame drawn at `now` shows, a row to a line.
    fn shown(a: &mut App, now: Instant) -> Vec<String> {
        let (cols, rows) = a.size();
        let mut p = vt100::Parser::new(rows as u16, cols as u16, 0);
        p.process(&a.frame(now));
        p.screen()
            .contents()
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect()
    }

    /// With each window's text centred, a window wider than the text gets is
    /// asked for a grid that wide and drawn in its middle — and let go of as
    /// the client goes, so that another UI sees the window whole.
    #[test]
    fn a_window_wider_than_its_text_has_it_in_its_middle() {
        let t0 = Instant::now();
        let (mut a, sent) = wide_app(t0);
        let (_, _, args) = sent
            .iter()
            .find(|(_, m, _)| m == "nvim_ui_attach")
            .expect("attached");
        assert_eq!(
            args[..2],
            [Value::from(20), Value::from(6)],
            "the whole terminal"
        );
        let n = Value::from;
        redraw(
            &mut a,
            vec![
                resize(1, 20, 6),
                resize(2, 20, 5),
                ("win_pos", vec![n(2), n(1000), n(0), n(0), n(20), n(5)]),
                viewport(2, 1000),
            ],
            t0,
        );
        let narrowed = |calls: &[Call]| -> Vec<Vec<Value>> {
            calls
                .iter()
                .filter(|(_, m, _)| m == "nvim_ui_try_resize_grid")
                .map(|(_, _, args)| args.clone())
                .collect()
        };
        assert_eq!(narrowed(&calls(&mut a)), [vec![n(2), n(8), n(0)]]);
        redraw(&mut a, vec![resize(2, 8, 5), line(2, 0, "text")], t0);
        assert!(narrowed(&calls(&mut a)).is_empty(), "asked once");
        assert_eq!(shown(&mut a, t0)[0], "      text", "6 in, of 20");

        assert!(a.let_windows_go(t0), "a grid to let go of");
        assert_eq!(narrowed(&calls(&mut a)), [vec![n(2), n(0), n(0)]]);
        assert!(!a.let_windows_go(t0), "none left");
    }

    /// A window's grid is cut to the width the client asks for at once, as
    /// Neovim will cut its own: its text is in the middle of the window in
    /// the next frame, not a round trip later, and Neovim's grid of that
    /// width, when it comes, changes nothing of where.
    #[test]
    fn a_window_narrowed_is_drawn_narrowed_at_once() {
        let t0 = Instant::now();
        let (mut a, _) = wide_app(t0);
        let n = Value::from;
        redraw(
            &mut a,
            vec![
                resize(1, 20, 6),
                resize(2, 20, 5),
                ("win_pos", vec![n(2), n(1000), n(0), n(0), n(20), n(5)]),
                viewport(2, 1000),
                line(2, 0, "text"),
            ],
            t0,
        );
        assert_eq!(a.model.grids[&2].width(), 8, "cut as asked");
        assert_eq!(
            shown(&mut a, t0)[0],
            "      text",
            "6 in, of 20, unanswered"
        );
        redraw(&mut a, vec![resize(2, 8, 5)], t0);
        a.screen.invalidate();
        assert_eq!(shown(&mut a, t0)[0], "      text", "answered");
    }

    /// Attaching again without `ext_multigrid` for a UI that keeps windows on
    /// grid 1, the client lets go of the windows it narrowed before it
    /// detaches, while Neovim still takes it of it: that UI is to see them
    /// whole.
    #[test]
    fn a_ui_without_windows_of_its_own_has_them_let_go_of() {
        let t0 = Instant::now();
        let (mut a, _) = wide_app(t0);
        let n = Value::from;
        redraw(
            &mut a,
            vec![
                resize(1, 20, 6),
                resize(2, 20, 5),
                ("win_pos", vec![n(2), n(1000), n(0), n(0), n(20), n(5)]),
                viewport(2, 1000),
            ],
            t0,
        );
        let resized = |calls: &[Call], width: i64| {
            calls.iter().position(|(_, m, args)| {
                m == "nvim_ui_try_resize_grid" && args[..] == [n(2), n(width), n(0)]
            })
        };
        assert!(resized(&calls(&mut a), 8).is_some(), "narrowed");

        redraw(&mut a, vec![ui_change()], t0);
        let uis_id = asks_uis(&calls(&mut a));
        answer(&mut a, uis_id, uis(true, false), t0);
        let sent = calls(&mut a);
        let detach = sent.iter().position(|(_, m, _)| m == "nvim_ui_detach");
        let let_go = resized(&sent, 0);
        assert!(
            let_go.is_some() && detach.is_some() && let_go < detach,
            "not let go of before the detach: {sent:?}"
        );
    }

    /// Each window's text is as wide as the measure says the window wants:
    /// narrower than the window, a grid that wide; as wide or wider — a
    /// window showing a terminal wants the whole terminal — none narrower,
    /// and one that was let go of; said nothing of, `window_width`.
    #[test]
    fn each_window_is_given_what_it_wants() {
        let t0 = Instant::now();
        let (mut a, sent) = wide_app(t0);
        let stays = request(&sent, "nvim_exec_lua", "local most");
        let (_, _, args) = sent
            .iter()
            .find(|(id, ..)| *id == Some(stays))
            .expect("left");
        assert_eq!(
            args.get(1).and_then(Value::as_array).map(|a| a.len()),
            Some(3),
            "given the client's channel, to stay: {sent:?}"
        );
        let n = Value::from;
        redraw(
            &mut a,
            vec![
                resize(1, 20, 6),
                resize(2, 20, 5),
                ("win_pos", vec![n(2), n(1000), n(0), n(0), n(20), n(5)]),
                viewport(2, 1000),
            ],
            t0,
        );
        let narrowed = |calls: &[Call]| -> Vec<Vec<Value>> {
            calls
                .iter()
                .filter(|(_, m, _)| m == "nvim_ui_try_resize_grid")
                .map(|(_, _, args)| args.clone())
                .collect()
        };
        assert_eq!(
            narrowed(&calls(&mut a)),
            [vec![n(2), n(8), n(0)]],
            "said nothing of"
        );

        let measure = |wants: Vec<(i64, i64)>| {
            let each = wants
                .into_iter()
                .map(|(win, w)| Value::Array(vec![n(win), n(w)]))
                .collect();
            vec!["measure".into(), 2.into(), Value::Array(each)]
        };
        notify(&mut a, "nvmux_client", measure(vec![(1000, 12)]), t0);
        assert_eq!(
            narrowed(&calls(&mut a)),
            [vec![n(2), n(12), n(0)]],
            "what it wants"
        );
        redraw(&mut a, vec![resize(2, 12, 5)], t0);
        assert!(narrowed(&calls(&mut a)).is_empty(), "asked once");

        notify(&mut a, "nvmux_client", measure(vec![(1000, 1 << 30)]), t0);
        assert_eq!(
            narrowed(&calls(&mut a)),
            [vec![n(2), n(0), n(0)]],
            "the whole terminal: a terminal"
        );
        redraw(&mut a, vec![resize(2, 20, 5)], t0);
        assert!(narrowed(&calls(&mut a)).is_empty(), "and left be");
    }

    /// A window wanting its whole width again before Neovim has resized the
    /// grid the client cut for it is asked for it once Neovim has: asked at
    /// once, Neovim could take both asks before it redraws, find its grid
    /// as wide as it was and say nothing, and the client's copy would stay
    /// cut, with what is past the cut never sent again.
    #[test]
    fn a_grid_cut_is_asked_back_whole_once_neovim_has_cut_it() {
        let t0 = Instant::now();
        let (mut a, _) = wide_app(t0);
        let n = Value::from;
        redraw(
            &mut a,
            vec![
                resize(1, 20, 6),
                resize(2, 20, 5),
                ("win_pos", vec![n(2), n(1000), n(0), n(0), n(20), n(5)]),
                viewport(2, 1000),
            ],
            t0,
        );
        let resized = |calls: &[Call], width: i64| {
            calls.iter().any(|(_, m, args)| {
                m == "nvim_ui_try_resize_grid" && args[..] == [n(2), n(width), n(0)]
            })
        };
        assert!(resized(&calls(&mut a), 8), "narrowed");
        assert_eq!(a.model.grids[&2].width(), 8, "cut as asked");

        let whole = Value::Array(vec![Value::Array(vec![n(1000), n(1 << 30)])]);
        notify(
            &mut a,
            "nvmux_client",
            vec!["measure".into(), 2.into(), whole],
            t0,
        );
        assert!(
            !resized(&calls(&mut a), 0),
            "asked back before Neovim has cut it"
        );
        redraw(&mut a, vec![resize(2, 8, 5)], t0);
        assert!(resized(&calls(&mut a), 0), "not asked back once it has");
        redraw(&mut a, vec![resize(2, 20, 5)], t0);
        assert_eq!(
            (a.model.grids[&2].width(), a.model.inset(2)),
            (20, 0),
            "whole"
        );
    }

    /// With nothing centred, nothing is measured, and nothing left in the
    /// editor to measure it.
    #[test]
    fn nothing_centred_measures_nothing() {
        let t0 = Instant::now();
        let mut a = app(true).with_centre(false, 8, true);
        a.start();
        let sent = calls(&mut a);
        answer(
            &mut a,
            request(&sent, "nvim_get_api_info", ""),
            Value::Array(vec![7.into()]),
            t0,
        );
        answer(&mut a, asks_uis(&sent), uis(false, false), t0);
        let sent = [sent, calls(&mut a)].concat();
        assert!(
            sent.iter().any(|(_, m, _)| m == "nvim_ui_attach"),
            "attached: {sent:?}"
        );
        assert!(
            !sent.iter().any(|(_, m, args)| m == "nvim_exec_lua"
                && args
                    .first()
                    .and_then(Value::as_str)
                    .is_some_and(|lua| lua.starts_with("local most"))),
            "no measure: {sent:?}"
        );
    }

    /// A window on a grid narrower than itself — as a client killed before
    /// it could let go of one leaves it, for every UI — is asked for whole
    /// by the next client to attach with windows of their own, as soon as
    /// it knows how wide the window is: from Neovim placing it, or from the
    /// layout asked for where Neovim does not. Once: a grid narrower after
    /// that is one a client attached now asked for.
    #[test]
    fn a_window_left_narrow_is_asked_for_whole() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);
        let n = Value::from;
        redraw(
            &mut a,
            vec![
                resize(2, 8, 5),
                place(2, 1000, 0, 0, 20, 5),
                viewport(2, 1000),
            ],
            t0,
        );
        assert_eq!(grids_asked(&calls(&mut a)), [vec![n(2), n(0), n(0)]]);
        redraw(&mut a, vec![resize(2, 20, 5)], t0);
        redraw(&mut a, vec![resize(2, 8, 5)], t0);
        assert!(grids_asked(&calls(&mut a)).is_empty(), "looked at once");

        // Not placed, as Neovim places no window of a session another UI
        // drew: not till the layout says how wide it is.
        redraw(&mut a, vec![resize(3, 8, 5), viewport(3, 1001)], t0);
        let sent = calls(&mut a);
        assert!(grids_asked(&sent).is_empty(), "{sent:?}");
        let laid = Value::Array(vec![n(1), n(0), n(0), n(0), n(20), n(5)]);
        answer(
            &mut a,
            request(&sent, "nvim_exec_lua", "local cur"),
            Value::Array(vec![laid]),
            t0,
        );
        assert_eq!(grids_asked(&calls(&mut a)), [vec![n(3), n(0), n(0)]]);
    }

    /// A client centring each window's text has the windows it finds on
    /// grids another client asked for as it wants them: one it narrows as
    /// wide as it narrows windows, and one it does not whole — on a grid
    /// narrower than the window, or wider, the window made narrower since.
    #[test]
    fn a_centring_client_has_the_windows_it_finds_as_it_wants_them() {
        let t0 = Instant::now();
        let (mut a, _) = wide_app(t0);
        let n = Value::from;
        redraw(
            &mut a,
            vec![
                resize(1, 20, 6),
                // Wider than the 8 its text gets, on a grid of 6.
                resize(2, 6, 2),
                place(2, 1000, 0, 0, 13, 2),
                viewport(2, 1000),
                // Narrower than that, on a grid of 4.
                resize(3, 4, 2),
                place(3, 1001, 0, 14, 6, 2),
                viewport(3, 1001),
                // And on a grid of 7, wider than it.
                resize(4, 7, 2),
                place(4, 1002, 3, 0, 5, 2),
                viewport(4, 1002),
            ],
            t0,
        );
        assert_eq!(
            grids_asked(&calls(&mut a)),
            [
                vec![n(2), n(8), n(0)],
                vec![n(3), n(0), n(0)],
                vec![n(4), n(0), n(0)],
            ]
        );
    }
}
