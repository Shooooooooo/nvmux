//! nvmux's own Neovim client, used unless `[client] ui = "nvim"`.
//!
//! It runs where `nvim --remote-ui` would (`nvmux --client <sock>` on a pty,
//! see [`crate::cli`]) and talks to the terminal as Neovim's TUI does
//! ([`term`]), so every relay feature works for it unchanged. Unlike that
//! TUI it animates, Neovide-style, as far as a grid of cells allows: cursor
//! smear in sub-cell blocks ([`anim::smear`]) and VFX ([`anim::vfx`]),
//! row-by-row smooth scrolling ([`anim::scroll`]), sliding floats, message
//! area and windows ([`anim::motion`]), blink fades ([`anim::blink`]), and
//! animate.nvim-style split open, close and resize ([`anim::layout`]) and
//! buffer-switch fades ([`anim::switch`]). No blur, sub-cell text movement,
//! fonts or transparency.
//!
//! # Neovim protocol
//!
//! Attaches with `ext_linegrid` and `ext_multigrid`: each window, the message
//! area and each float gets its own grid, composed by the client
//! ([`compose`]).
//!
//! Neovim uses per-window grids only while every attached UI asks for them;
//! otherwise windows are drawn on grid 1 and floats are sent unplaced. So the
//! client lists the other UIs before attaching and whenever Neovim re-sends
//! the editor-wide `ext_*` options, and attaches without `ext_multigrid` while
//! such a UI exists (losing window animations). nvmux clients mark
//! `ext_multigrid` as wanted in their client info, so one only passing
//! through grid 1 does not trigger this.
//!
//! A multigrid UI attaching to a session another UI has drawn gets no window
//! placements and only changed cells, since Neovim keeps grids across UIs; a
//! resize redraws only the added area. So the client asks where windows are
//! ([`LAYOUT_LUA`]) and, while a window lacks cells
//! ([`model::Model::lacking`]), forces a full redraw by briefly attaching a
//! second UI without `ext_multigrid` ([`refresh`]). Tab pages shown for the
//! first time get the same. Drawing waits up to [`HOLD_LIMIT`] for both.
//!
//! The UI protocol does not say which buffer a window shows or the options
//! that lay windows out, so a multigrid attach installs autocommands that
//! report both to this client ([`AGENT_LUA`]).
//!
//! # Cards
//!
//! nvmux sends cards (switch target, session reports) on the client's input
//! in a sequence no terminal sends ([`crate::card`]); they are drawn in the
//! margin over each frame ([`cards`]) and never involve the editor.
//!
//! # One thread
//!
//! One `poll` over the terminal, the socket and two signal pipes, timing out
//! at the next animation frame, the held-`ESC` deadline, or the multigrid
//! hold. An idle client sleeps in `poll`. Only a refresh runs off-thread; its
//! results arrive on the client's own socket ([`refresh`]).
//!
//! A client nvmux has put aside (parked or started ahead) is told so
//! ([`crate::card::Command::Away`]); it then animates nothing and draws at
//! most once every [`AWAY_FRAME_GAP`].

pub mod anim;
pub mod cards;
pub mod compose;
pub mod grid;
pub mod input;
pub mod model;
pub mod moves;
pub mod predict;
pub mod redraw;
pub mod region;
pub mod screen;
pub mod style;
pub mod term;
pub mod typing;
pub mod wide;
pub mod wire;
pub mod words;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use rmpv::ValueRef;

use self::anim::{chain, Animator, Effects};
use self::grid::{Cell, Text};
use self::input::{Input, Mouse, Parser, Reply};
use self::model::{Changes, Model, Place, Placement};
use self::predict::{Command, Mode, Pos, Predictor, Proof, Rest, Side, Unseen, Want};
use self::redraw::Event;
use self::screen::Screen;
use self::wire::{Arg, Inbox, Outbox};
use crate::config::{Centre, Predictions};
use crate::palette::{Palette, Rgb};

/// Parses the agent's `'laststatus'`, `'splitright'`, `'splitbelow'`.
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

/// Parses [`wide::MEASURE_LUA`]'s `{ window, wanted }` pairs; `None` if absent.
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

/// Carries the terminal's colours from the parent nvmux
/// ([`crate::palette::Palette::to_env`]): on a pty, querying the terminal
/// would race the user's keys.
pub const PALETTE_ENV: &str = "NVMUX_PALETTE";

/// Carries the drawing settings from the parent nvmux, which may include
/// unsaved settings-screen changes and whether the session is remote, neither
/// of which the config file has ([`crate::config::Drawing`]).
pub const SETTINGS_ENV: &str = "NVMUX_CLIENT";

/// Minimum gap between frames drawn for flushes; the last of a burst is always
/// drawn. The second frame after a quiet spell is exempt ([`App::dirty_due`]).
const MIN_FRAME_GAP: Duration = Duration::from_millis(4);

/// Minimum gap between frames while away ([`crate::card::Command::Away`]).
/// Frames then only keep nvmux's copy of the screen current; the client
/// catches up as soon as it is back in front.
const AWAY_FRAME_GAP: Duration = Duration::from_secs(1);

/// How long drawing waits for what a multigrid attach lacks (window places,
/// contents, other UIs settling) before drawing what it has.
const HOLD_LIMIT: Duration = Duration::from_millis(500);

/// How long after its last byte a partial terminal reply is kept.
const REPLY_WAIT: Duration = Duration::from_secs(1);

/// Max ms a lone `ESC` is held for the rest of a sequence, whatever
/// `'ttimeoutlen'` says ([`input`]). The relay already held it as long
/// (`crate::pty`'s `SEQUENCE_TIMEOUT`); a key's bytes split only at buffer
/// boundaries. Waiting `'ttimeoutlen'` (50 ms default) on top would delay
/// every `<Esc>` leaving Insert mode.
const RELAYED_ESCAPE: u64 = 10;

/// Delay before listing UIs again after finding one only passing through
/// grid 1 (another nvmux client's refresh or re-attach).
const RECHECK: Duration = Duration::from_millis(30);

/// How long text past the grids' edges ([`wide::TAILS_LUA`]) waits for its
/// `flush` before being taken anyway: a redraw that changed nothing sends none.
const TAILS_WAIT: Duration = Duration::from_millis(25);

/// Timeout for a refresh, and delay before retrying one that came to nothing.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(2);

/// Round-trip probe sent while the user is active ([`predict::PING_EVERY`]).
/// Neovim answers it on read, even while busy with typed-ahead keys, so it
/// times the link alone; parsing changes nothing. Not `nvim_get_mode`, which
/// Neovim holds until all typeahead is taken (neovim#6247): it would also
/// time that backlog, which `behind` and the ack lag already count
/// ([`predict::Predictor::deadline`]). Not `nvim_input` with no keys, which
/// rescans the typeahead for `<C-c>`.
const PING: &str = "nvim_parse_expression";

/// Max refreshes per window before drawing it as is: Neovim does not draw for
/// an attaching UI while it waits for Enter.
const REFRESH_TRIES: u8 = 2;

/// Returns `{ foreign, passing }`: whether another UI lacks `ext_multigrid`
/// for good, or only for now (an nvmux client, per [`App::client_info`]).
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

/// Places of the given windows, for those Neovim has not placed for this UI:
/// `{ 0 }` if not visible (other tab page, hidden, gone),
/// `{ 1, row, col, top, width, height }` for a layout window, and
/// `{ 2, row, col, top, bottom, left, right, zindex, mouse, anchor }` for a
/// float. Margins are what `win_viewport_margins` would give (winbar,
/// border); `anchor` is the anchoring window or -1.
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
/// it predicts. It sends `nvmux_client` notifications `buf` (window, buffer)
/// on `BufWinEnter`, `win` (window, buffer), `closed` (window) and `options`
/// (`'laststatus'`, `'splitright'`, `'splitbelow'`), and returns those
/// options and every window's buffer. It removes itself when this client's UI
/// detaches or its channel is found gone.
///
/// For a predicting client it also sends what [`predict`] draws from:
///
/// - `lines` (window, buffer, `b:changedtick`, first line, lines, whether
///   they replace the client's, per-line byte cap): the view and lines
///   either side of it as it moves, as many window heights as the fifth
///   argument (or a later `User` `nvmux_client_reach_<chan>` autocommand)
///   allows: two at first, the rest a moment later, a few hundred lines at
///   a time. Only lines the client lacks, by [`predict`]'s rule, each cut to
///   what the window can show.
/// - `keep` (window, buffer, `b:changedtick`, first kept, past last): the
///   few thousand lines the client keeps, trimmed away from those being sent.
/// - `splice` (window, buffer, old and new `b:changedtick`, first line, past
///   last replaced, replacement count, the replacing lines that touch the
///   client's, per-line byte cap): each edit once typing pauses, or fresh
///   `lines` when the edits are too many.
/// - `spans` (window, buffer, `b:changedtick`, first line, per line a list of
///   start byte, end byte or -1 for the line's end, group): highlights, with
///   the lines and on edits or reparses. `colours` (group, GUI attributes,
///   cterm attributes) precedes a group's first use and is resent on a colour
///   scheme change.
/// - `shape` (window, [`predict::Shape`]): drawing and scroll limits, among
///   them which followed keys (third argument; Insert mode's fourth) the
///   buffer maps; with acks, rechecked a second after the first wait for a
///   key since the last check.
/// - `leftcol` (window, first column).
/// - `want` (window, `curswant`), or with acks (sixth argument) `ack` when
///   Neovim waits for a key with all typed keys taken ([`predict::Ack`]),
///   its edits sent first. In Normal, Visual and Insert mode the agent
///   flushes the ack's frame as the next key is taken: Neovim skips that
///   flush when it takes a key without waiting.
/// - `insert` ([`typing::Facts`]): what would make an Insert-mode key in the
///   current buffer differ from plain typing, when it changes.
/// - `behind`: Neovim moved a view while behind on typed keys; it may send no
///   frame until caught up ([`Predictor::deadline`]).
///
/// Taken keys are counted per channel in `_G.nvmux_taken`, for a fence to
/// read ([`predict::Sync`]).
const AGENT_LUA: &str = "local chan, predict, keys, ikeys, reach, acks, typing = ...
local api = vim.api
local group = api.nvim_create_augroup('nvmux_client_' .. chan, { clear = true })
-- `gone` stops the agent once the client is gone; `unkey` drops its key count.
local gone, unkey = false, nil
local function leave()
  gone = true
  if unkey then
    unkey()
  end
  pcall(api.nvim_del_augroup_by_id, group)
end
local function send(...)
  if not pcall(vim.rpcnotify, chan, 'nvmux_client', ...) then
    leave()
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
    leave()
  end
end)
if predict then
  local have, shapes, maps = {}, {}, {}
  local soon
  -- Typed keys Neovim has taken, counted as the client counts what it sends
  -- (predict::Sync): one per key in Neovim's notation as it leaves the
  -- typeahead, before mappings; a click or wheel turn one; a paste none.
  -- Starts at 0 so a fence reading it early does not see no agent. `tail`
  -- keeps the last few keys for the client to check its count.
  local taken, tail = 0, {}
  -- `unflushed`: an ack went out whose frame Neovim may not have flushed;
  -- on_key flushes it as the next key is taken. Only in the `judged` modes,
  -- those the client judges by acks (predict::Rest).
  local unflushed = false
  local judged = { n = true, v = true, V = true, ['\\22'] = true, i = true }
  -- Where <C-w> and <C-u> stop in Insert mode, mirroring Neovim. `orig` is
  -- Insstart_orig, reset before each key to `began` (Insstart: the cursor
  -- on entry, or the line start for o and O), except that after the <C-g>u
  -- Neovim's default <C-w>/<C-u> mappings type it `sticks` there, moving
  -- back only as keys delete before it. Typed characters, <CR> and <Tab>
  -- move neither; other keys (arrows, clicks, completion), a joined line or
  -- a re-indent may, so both are unknown (began false) until Insert mode is
  -- entered again. `entering` is the Normal-mode key that entered it. The
  -- last Insert key's effect is known only once Neovim has taken it: it
  -- deleted (rubbed), was typed past the indent (typed_at: line, indent
  -- end), or was the <C-g> of a pair (ctrl_g).
  local began, orig, sticks, entering = false, false, false, nil
  local rubbed, typed_at, ctrl_g = false, nil, false
  local function indent_of()
    return #api.nvim_get_current_line():match('^[ \\t]*')
  end
  -- Applies the previous key's effect, now that Neovim has taken it.
  local function settle()
    if not began then
      return
    end
    local cur = api.nvim_win_get_cursor(0)
    if typed_at and typed_at[1] == cur[1] - 1 and typed_at[2] ~= indent_of() then
      began = false
    elseif rubbed and cur[1] - 1 == orig[2] and cur[2] < orig[3] then
      orig = { orig[1], orig[2], cur[2] }
    end
    rubbed, typed_at = false, nil
  end
  if acks then
    local keyns = api.nvim_create_namespace('nvmux_client_keys_' .. chan)
    _G.nvmux_taken = _G.nvmux_taken or {}
    _G.nvmux_taken[chan] = 0
    unkey = function()
      vim.on_key(nil, keyns)
      _G.nvmux_taken[chan] = nil
    end
    local still = {}
    for k, rubs in pairs({ ['<BS>'] = true, ['<C-h>'] = true, ['<C-w>'] = true,
        ['<C-u>'] = true, ['<CR>'] = false, ['<Tab>'] = false }) do
      still[vim.keycode(k)] = rubs
    end
    vim.on_key(function(key, typed)
      if gone then
        return
      end
      local mode = api.nvim_get_mode().mode
      if mode == 'n' then
        entering = key
      elseif mode == 'i' and began then
        settle()
        local cur = api.nvim_win_get_cursor(0)
        local b, rubs = key:byte(1) or 0, still[key]
        if ctrl_g then
          ctrl_g = false
          if key == 'u' then
            began, sticks = { began[1], cur[1] - 1, cur[2] }, true
          else
            began = false
          end
        elseif began then
          if not sticks then
            orig = began
          end
          if key == '\\7' then
            ctrl_g = true
          elseif rubs then
            rubbed = cur[2] > 0
            began = rubbed and began
          elseif rubs == nil and (b < 0x20 or b == 0x7f or b == 0x80) then
            began = false
          elseif rubs == nil and b ~= 0x20 and cur[2] >= indent_of() then
            typed_at = { cur[1] - 1, indent_of() }
          end
        end
      end
      if typed == nil or typed == '' then
        return
      end
      -- Flush the last ack's frame before this key changes anything: Neovim
      -- flushed it itself only if it waited for this key. `cursor` puts the
      -- cursor back where Neovim draws it, as drawing since (such as Insert
      -- mode's mode message) may have moved it.
      if unflushed then
        unflushed = false
        if judged[mode] then
          pcall(api.nvim__redraw, { flush = true, cursor = true })
        end
      end
      local t, i = vim.fn.keytrans(typed), 1
      while i <= #t do
        local j = t:sub(i, i) == '<' and t:find('>', i, true)
        local key = j and t:sub(i, j) or t:sub(i, i + vim.str_utf_end(t, i))
        i, taken = i + #key, taken + 1
        tail[#tail + 1] = key
        if #tail > 4 then
          table.remove(tail, 1)
        end
      end
      _G.nvmux_taken[chan] = taken
      -- Nothing returned: a string, even an empty one, would change the key.
    end, keyns)
  end
  -- Followed keys mapped (or starting a mapping) in the buffer, looked up
  -- again a second after the last lookup. With acks, also how the line
  -- up/down keys are mapped (predict::Idiom): rhs, expr, noremap, and
  -- whether no longer mapping starts with it (else Neovim waits).
  local function mapped(buf)
    local m = maps[buf]
    if m and vim.uv.now() - m.at < 1000 then
      return m.keys, m.ikeys, m.idioms
    end
    local function look(list, modes)
      local out = {}
      for _, key in ipairs(list) do
        -- Insert-entering keys are followed in Normal mode only: in Visual
        -- mode i and a start text objects, which plugins map.
        local ms = key:find('ScrollWheel') and { 'n', 'x', 'i' }
          or (key:find('^[iaAI]$') and { 'n' }) or modes
        for _, mode in ipairs(ms) do
          if vim.fn.mapcheck(key, mode) ~= '' then
            out[#out + 1] = key
            break
          end
        end
      end
      return out
    end
    local idioms = {}
    for _, key in ipairs(acks and { 'j', 'k', '<Down>', '<Up>' } or {}) do
      local map = vim.fn.maparg(key, 'n', false, true)
      if next(map) ~= nil then
        local longer = false
        for _, list in ipairs({ api.nvim_get_keymap('n'), api.nvim_buf_get_keymap(buf, 'n') }) do
          for _, x in ipairs(list) do
            longer = longer or (#x.lhs > #map.lhs and x.lhs:sub(1, #map.lhs) == map.lhs)
          end
        end
        idioms[#idioms + 1] = { key, map.rhs or '', map.expr == 1, map.noremap == 1, not longer }
      end
    end
    maps[buf] = { at = vim.uv.now(), keys = look(keys, { 'n', 'x' }), ikeys = look(ikeys, { 'i' }),
      idioms = idioms }
    return maps[buf].keys, maps[buf].ikeys, maps[buf].idioms
  end
  -- 0-based display columns 'colorcolumn' colours; relative entries count
  -- only when 'textwidth' is set.
  local function colorcolumn(win)
    local cc, tw = {}, vim.bo[api.nvim_win_get_buf(win)].textwidth
    for _, c in ipairs(vim.split(vim.wo[win].colorcolumn, ',', { trimempty = true })) do
      local n = tonumber(c)
      if n and c:find('^[+-]') then
        n = tw > 0 and tw + n or nil
      end
      if n and n > 0 then
        cc[#cc + 1] = n - 1
      end
    end
    return cc
  end
  -- charclass() of each character below 256 under the buffer's 'iskeyword',
  -- a digit each: 0 blank, 1 punctuation, 2 word (predict::Stepping::classes).
  local classesof = {}
  local function classes(buf)
    local isk = vim.bo[buf].iskeyword
    if not classesof[isk] then
      local t = {}
      api.nvim_buf_call(buf, function()
        for b = 0, 255 do
          t[b + 1] = math.min(vim.fn.charclass(vim.fn.nr2char(b)), 2)
        end
      end)
      classesof[isk] = table.concat(t)
    end
    return classesof[isk]
  end
  local function shape(buf, info)
    -- With acks, 'vartabstop' windows are not followed: tabs are not
    -- 'tabstop' wide there.
    local ok = not vim.wo.diff and vim.bo[buf].buftype ~= 'terminal'
      and not (acks and vim.bo[buf].vartabstop ~= '')
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
    local nmaps, imaps, idioms = mapped(buf)
    -- 'listchars' except tab, as predict::Listchars::read reads them; and
    -- whether Neovim's own highlighting looks at every character, blanks
    -- included (predict::Blanks).
    local lcs = {}
    if vim.wo.list then
      local l = vim.opt.listchars:get()
      for i, k in ipairs({ 'space', 'multispace', 'lead', 'leadmultispace', 'trail', 'nbsp',
          'eol', 'extends', 'precedes', 'leadtab' }) do
        lcs[i] = l[k] or ''
      end
    end
    local s = { buf, api.nvim_buf_line_count(buf), info.textoff, vim.bo[buf].tabstop,
      vim.wo.number, vim.wo.relativenumber, vim.wo.statuscolumn == '', vim.wo.wrap,
      vim.fn.winsaveview().leftcol, ok, so,
      tonumber(vim.o.mousescroll:match('ver:(%d+)')) or 3,
      vim.opt.fillchars:get().eob or '~', vim.wo.list,
      vim.wo.list and vim.opt.listchars:get().tab or '', vim.wo.scroll, page,
      vim.o.startofline, nmaps, imaps, siso, vim.o.sidescroll,
      tonumber(vim.o.mousescroll:match('hor:(%d+)')) or 6, lcs,
      vim.wo.linebreak or vim.wo.spell or vim.b[buf].current_syntax ~= nil }
    -- With acks, the rest of predict::Stepping: what decides where motions
    -- take the cursor and what moves with it. 'cursorlineopt' is bits (1
    -- number, 2 line, 4 screenline); centring is by rows with 'smoothscroll'
    -- and 'wrap'.
    if acks then
      local widths = vim.o.ambiwidth == 'double' or #vim.fn.getcellwidths() > 0
        or not vim.o.emoji
      local o = {}
      for _, k in ipairs(vim.split(vim.wo.cursorlineopt, ',')) do
        o[k] = true
      end
      local cul = not vim.wo.cursorline and 0
        or ((o.number or o.both) and 1 or 0) + ((o.line or o.both) and 2 or 0)
          + (o.screenline and 4 or 0)
      local got, ns = pcall(api.nvim_get_hl_ns, { winid = info.winid })
      vim.list_extend(s, { vim.o.whichwrap, vim.o.virtualedit, vim.wo.cursorcolumn,
        vim.wo.scrollbind or vim.wo.cursorbind, vim.o.langmap ~= '', widths, vim.wo.numberwidth,
        vim.wo.signcolumn:find('number') ~= nil, cul, idioms,
        vim.wo.winhighlight ~= '' or not got or ns > 0,
        vim.wo.conceallevel > 0 and not vim.wo.concealcursor:find('n'),
        colorcolumn(info.winid), vim.o.scrolljump, vim.wo.conceallevel > 0, classes(buf),
        vim.bo[buf].iminsert == 1, vim.o.sections, vim.o.paragraphs,
        vim.wo.wrap and vim.wo.smoothscroll })
    end
    return s
  end
  -- With acks, per window as of its last shape: the options that affect how
  -- the cursor line is drawn and how far motions past 'scrolloff' scroll,
  -- plus the buffer's line count, 'iskeyword' and 'iminsert'. Options set
  -- from other autocommands (e.g. 'relativenumber' toggled in Insert mode),
  -- a window highlight namespace and <C-^> fire no OptionSet, so these are
  -- re-read when Neovim waits for a key and changed shapes resent before
  -- the ack.
  local looked = {}
  local function looks(win)
    local out = { pcall(api.nvim_get_hl_ns, { winid = win }) }
    for _, name in ipairs({ 'number', 'relativenumber', 'cursorline', 'cursorlineopt',
        'numberwidth', 'signcolumn', 'foldcolumn', 'statuscolumn', 'winhighlight', 'conceallevel',
        'concealcursor', 'wrap', 'list' }) do
      out[#out + 1] = api.nvim_get_option_value(name, { win = win })
    end
    out[#out + 1] = vim.o.scrolljump
    out[#out + 1] = table.concat(colorcolumn(win), ' ')
    local buf = api.nvim_win_get_buf(win)
    out[#out + 1] = api.nvim_buf_line_count(buf)
    out[#out + 1] = vim.bo[buf].iskeyword
    out[#out + 1] = vim.bo[buf].iminsert
    return table.concat(vim.tbl_map(tostring, out), ',')
  end
  -- Lines cut to what could be shown plus one cell, so the client sees the
  -- overflow: a minified file can have a megabyte-long line.
  local function get(buf, from, to, cap)
    local lines = api.nvim_buf_get_lines(buf, from, to, false)
    for i, l in ipairs(lines) do
      if #l > cap then
        lines[i] = l:sub(1, cap)
      end
    end
    return lines
  end
  -- Groups whose attributes were sent: true; false for groups with none,
  -- whose spans are omitted.
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
  -- Highlights of rows from..to (tree-sitter captures and highlight
  -- extmarks such as semantic tokens): per line, start byte, end byte (-1 to
  -- the line's end) and group, later winning by priority. New groups'
  -- attributes are sent first.
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
  -- Per buffer, a few runs of rows whose sent colours may be stale.
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
    -- A parse reports every row whose injected trees it created or dropped,
    -- and parsing other rows drops the last parse's injections, so the
    -- agent's parses and the highlighter's would report each other's rows
    -- forever, each report a push and each push a parse (a C file's macros
    -- suffice). Only edits change colours, so per b:changedtick reports are
    -- heard until the end of the first push ending after the first report.
    -- Not just the edit's own parse: an edit inside an injected tree (e.g. a
    -- markdown code block) is reported by that tree alone, often first
    -- parsed by the push itself. Rows the edit changed are always resent
    -- (see flush).
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
  -- Per buffer, edits since the client's lines were sent, in order: each
  -- replaced lines a..b with a..c, whose text it keeps; one adjacent to the
  -- last is merged into it. Sent as splices on the next push so an edit
  -- costs only its lines; past 2000 lines or 100 edits, lines are resent.
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
  -- Sends each window one splice per edit, with the edit's text where it
  -- touches the client's lines and only the shift where it does not.
  local function flush()
    for buf, e in pairs(edits) do
      edits[buf], ticks[buf] = nil, e.tick
      -- Edited lines carry no colours; mark them stale, at the rows later
      -- edits moved them to.
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
    -- `reach` windows either side, as far as the client may scroll ahead;
    -- only two when sent afresh, the rest later a few hundred lines a push,
    -- so neither a new buffer nor a grown reach stalls Neovim's redraw or
    -- typed keys for long.
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
    if acks then
      looked[win] = looks(win)
    end
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
      .. '\\0' .. table.concat(s[24], '\\0') .. '\\0' .. tostring(s[25])
      .. '\\0' .. vim.json.encode({ unpack(s, 26) })
    if shapes[win] ~= key then
      shapes[win] = key
      send('shape', win, s)
    end
    if joins then
      -- At most 300 lines a push, split between the sides, the rest 20 ms
      -- later; at most 4000 kept, trimming the far end, so long paging never
      -- restarts the client from two windows.
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
    -- Follow colour changes from here on; the lines just sent have the
    -- current parse's.
    follow(buf)
  end
  local queued, scrolled = false, false
  local function push_all()
    if not pcall(api.nvim_get_autocmds, { group = group, event = 'SafeState' }) then
      return
    end
    flush()
    -- Resend what was stale; what this push makes stale (parsing new lines
    -- may) goes next time.
    local spoilt = stale
    stale = {}
    for _, win in ipairs(api.nvim_tabpage_list_wins(0)) do
      if api.nvim_win_is_valid(win) then
        pcall(api.nvim_win_call, win, function()
          push(win, spoilt)
        end)
      end
    end
    -- Stop hearing reports at this b:changedtick (see follow).
    for _, done in pairs(pushed) do
      done()
    end
  end
  soon = function()
    if not queued then
      queued = true
      vim.schedule(function()
        queued, scrolled = false, false
        -- Many rows to recolour, as when an edit opens a string or comment:
        -- flush Neovim's frame before pushing them, not after. Only in the
        -- modes on_key flushes in.
        local n, m = 0, api.nvim_get_mode()
        for _, runs in pairs(stale) do
          for _, r in ipairs(runs) do
            n = n + r[2] - r[1]
          end
        end
        if n > 100 and judged[m.mode] and not m.blocking then
          pcall(api.nvim__redraw, { flush = true, cursor = true })
        end
        push_all()
      end)
    end
  end
  local timer = vim.uv.new_timer()
  au({ 'WinResized', 'BufWinEnter', 'WinNew', 'TabEnter', 'VimResized', 'CursorHold',
    'CursorHoldI' }, {}, function()
    soon()
  end)
  -- A second scroll before the scheduled push ran means Neovim is behind on
  -- typed keys, which it takes before scheduled work: push at each scroll
  -- instead. Neovim may also send no frame (which lands the client's scrolls)
  -- until caught up, so tell the client it is still busy.
  au('WinScrolled', {}, function()
    if scrolled then
      send('behind')
      push_all()
    end
    scrolled = true
    soon()
  end)
  -- Horizontal scrolls are sent at once: Neovim draws them next, and nothing
  -- else it sends says so.
  au('WinScrolled', {}, function()
    for id, d in pairs(vim.v.event) do
      local win = tonumber(id)
      if win and d.leftcol ~= 0 and api.nvim_win_is_valid(win) then
        send('leftcol', win, vim.fn.getwininfo(win)[1].leftcol)
      end
    end
  end)
  -- On SafeState (typed-ahead keys taken, before their frame): curswant
  -- when it or the window changes, or with acks the full `ack` when
  -- anything changed (SafeState repeats after every request). Edits are
  -- flushed first so the client's lines match the ack's b:changedtick
  -- (predict::Predictor::fresh). The ack carries Neovim's widths of the
  -- cursor line and of blank-ended lines in view, since blank virtual text
  -- is invisible on screen. Mappings are rechecked a second later.
  local wanted, said, fixed, decorated, recheck, rehooked
  local function remapped()
    recheck = false
    local buf = api.nvim_get_current_buf()
    local m = maps[buf]
    maps[buf] = nil
    local nmaps, imaps, idioms = mapped(buf)
    if m and not (vim.deep_equal(m.keys, nmaps) and vim.deep_equal(m.ikeys, imaps)
        and vim.deep_equal(m.idioms, idioms)) then
      soon()
    end
  end
  au('SafeState', {}, function()
    local win, view = api.nvim_get_current_win(), vim.fn.winsaveview()
    if not acks then
      if not wanted or wanted[1] ~= win or wanted[2] ~= view.curswant then
        wanted = { win, view.curswant }
        send('want', win, view.curswant)
      end
      return
    end
    if not recheck then
      recheck = true
      vim.defer_fn(remapped, 1000)
    end
    if looked[win] and looked[win] ~= looks(win) then
      push_all()
    end
    if rehooked then
      rehooked()
    end
    flush()
    local buf = api.nvim_get_current_buf()
    local cap = 4 * (view.leftcol + api.nvim_win_get_width(win)) + 64
    local len, width = vim.fn.col({ view.lnum, '$' }) - 1, -1
    if len <= cap then
      width = len > 0 and vim.fn.virtcol({ view.lnum, len }) or 0
    end
    local blanks = {}
    for i, l in ipairs(api.nvim_buf_get_lines(buf, view.topline - 1, vim.fn.line('w$'), false)) do
      if #l <= cap and l:find('^%s', #l) then
        blanks[#blanks + 1] = { view.topline + i - 2, vim.fn.virtcol({ view.topline + i - 1, #l }) }
      end
    end
    -- Lines around the view with virtual text, signs or sign highlights,
    -- which the client leaves to Neovim when scrolling ahead
    -- (predict::Predictor::decorated): half-open runs plus the range
    -- searched, cut at the last mark when a kind hits the 1000 limit. Sent
    -- before the ack, when changed.
    local m = math.min(reach * api.nvim_win_get_height(win) + 10, 1000)
    local from = math.max(view.topline - 1 - m, 0)
    local to = math.min(vim.fn.line('w$') + m, api.nvim_buf_line_count(buf))
    local rows = {}
    for _, kind in ipairs({ 'virt_text', 'sign' }) do
      local marks = api.nvim_buf_get_extmarks(buf, -1, { from, 0 }, { math.max(to - 1, 0), -1 },
        { type = kind, limit = 1000 })
      if #marks >= 1000 then
        to = math.min(to, marks[#marks][2] + 1)
      end
      for _, m in ipairs(marks) do
        rows[#rows + 1] = m[2]
      end
    end
    table.sort(rows)
    local runs = {}
    for _, r in ipairs(rows) do
      if r < to and #runs > 0 and r <= runs[#runs] then
        runs[#runs] = math.max(runs[#runs], r + 1)
      elseif r < to then
        runs[#runs + 1] = r
        runs[#runs + 1] = r + 1
      end
    end
    local over = from .. ',' .. to .. ',' .. table.concat(runs, ' ')
    if not (decorated and decorated[1] == win and decorated[2] == over) then
      decorated = { win, over }
      send('decorated', win, from, to, runs)
    end
    -- With typing prediction, in-view lines whose virtual text would not
    -- move with the line's end as text is typed (predict::Predictor::pinned).
    -- Sent before the ack, when changed.
    if typing then
      local pinned = {}
      for _, m in ipairs(api.nvim_buf_get_extmarks(buf, -1, { view.topline - 1, 0 },
          { vim.fn.line('w$') - 1, -1 }, { details = true, type = 'virt_text' })) do
        local d, l = m[4], m[2]
        local at_end = not d.right_gravity and m[3] >= #(api.nvim_buf_get_lines(buf, l, l + 1,
          false)[1] or '')
        if d.virt_text_pos ~= 'eol' and (d.virt_text_pos ~= 'inline' or at_end)
            and pinned[#pinned] ~= l then
          pinned[#pinned + 1] = l
        end
      end
      if not (fixed and fixed[1] == win and vim.deep_equal(fixed[2], pinned)) then
        fixed = { win, pinned }
        send('pinned', win, pinned)
      end
    end
    local mode = api.nvim_get_mode().mode
    if mode == 'i' then
      settle()
    end
    local at = mode == 'i' and began and began[1] == buf and (sticks and orig or began)
    local a = { taken, mode, win, buf, api.nvim_buf_get_changedtick(buf),
      api.nvim_buf_line_count(buf), view.lnum - 1, view.col,
      view.curswant >= 2147483647 and -1 or view.curswant, view.topline - 1, view.leftcol,
      width, blanks, at and at[2] or -1, at and at[3] or -1, sticks }
    if said and vim.deep_equal(said, a) then
      return
    end
    said = a
    send('ack', a[1], table.concat(tail), unpack(a, 2))
    -- The client takes an ack with the frame Neovim flushes as it waits for
    -- the next key; with that key already in, Neovim neither waits nor
    -- flushes, and the next frame is for later keys. So on_key flushes that
    -- frame as the next key is taken. Not here: Neovim has yet to draw the
    -- ruler, statusline, folds and diffs. Nor only when a key is found
    -- waiting: peeking for one would swallow a typed-ahead <C-c>.
    if judged[mode] then
      unflushed = true
    end
  end)
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
  -- Edits are sent once typing pauses for 150 ms (see flush).
  au({ 'TextChanged', 'TextChangedI', 'TextChangedP' }, {}, function()
    timer:stop()
    timer:start(150, 0, vim.schedule_wrap(soon))
  end)
  au('OptionSet', { pattern = { 'number', 'relativenumber', 'statuscolumn', 'wrap',
    'tabstop', 'list', 'listchars', 'fillchars', 'scrolloff', 'mousescroll', 'foldenable',
    'foldcolumn', 'signcolumn', 'numberwidth', 'diff', 'buftype', 'scroll', 'window',
    'startofline', 'sidescrolloff', 'sidescroll', 'linebreak', 'spell',
    'syntax' } }, function()
    soon()
  end)
  if acks then
    au('OptionSet', { pattern = { 'whichwrap', 'virtualedit', 'cursorcolumn', 'scrollbind',
      'cursorbind', 'langmap', 'ambiwidth', 'emoji', 'cursorline', 'cursorlineopt',
      'winhighlight', 'conceallevel', 'concealcursor', 'colorcolumn', 'textwidth',
      'vartabstop', 'scrolljump', 'iskeyword', 'iminsert', 'sections',
      'paragraphs', 'smoothscroll' } }, function()
      soon()
    end)
  end
  au('WinClosed', {}, function(ev)
    local win = tonumber(ev.match)
    have[win], shapes[win], looked[win] = nil, nil, nil
  end)
  -- A new reach as the round trip changes; resend what acks depend on.
  au('User', { pattern = 'nvmux_client_reach_' .. chan }, function(ev)
    reach = tonumber(ev.data) or reach
    said, fixed, decorated = nil, nil, nil
    soon()
  end)
  au('UILeave', {}, function()
    if vim.v.event.chan == chan then
      timer:close()
    end
  end)
  -- With typing prediction, what would make an Insert-mode key differ from
  -- plain typing (typing::Facts). Sent after InsertEnter has run its course
  -- (so plugins loaded on the way in count), on BufEnter and on OptionSet,
  -- when changed. Prompt buffers count as not editable: text goes after
  -- the prompt wherever Insert mode starts.
  if typing then
    local factsof, entered = {}, {}
    local function unseen(buf, event)
      for _, ac in ipairs(api.nvim_get_autocmds({ event = event })) do
        local g = ac.group_name or ''
        if g ~= 'matchparen' and not g:find('^nvmux_client_')
            and (not ac.buflocal or ac.buffer == buf) then
          return true
        end
      end
      return false
    end
    -- Run on every SafeState (see rehooked), so asked an event at a time,
    -- stopping at the first match: nvim_get_autocmds builds a table of every
    -- autocommand it returns.
    local hookevents = { { 'InsertCharPre', 'TextChangedI', 'CursorMovedI' },
      { 'InsertEnter', 'ModeChanged' }, { 'InsertLeavePre', 'InsertLeave', 'ModeChanged' } }
    local function hooks(buf)
      local h = { false, false, false }
      for k, events in ipairs(hookevents) do
        for _, event in ipairs(events) do
          if unseen(buf, event) then
            h[k] = true
            break
          end
        end
      end
      return h
    end
    local function facts()
      if gone then
        return
      end
      local buf, win = api.nvim_get_current_buf(), api.nvim_get_current_win()
      local bo, wo = vim.bo[buf], vim.wo[win]
      local firsts = {}
      -- Neovim's default <C-W>/<C-U> mappings (break undo first) are
      -- followed as the keys, not as mappings.
      local undo = { ['<C-W>'] = false, ['<C-U>'] = false }
      local function heads(list, own)
        for _, m in ipairs(list) do
          local lhs = vim.fn.keytrans(m.lhsraw or vim.keycode(m.lhs))
          if own and undo[lhs] ~= nil and m.rhs == '<C-G>u' .. lhs and m.noremap == 1
              and m.expr == 0 then
            undo[lhs] = true
          else
            firsts[#firsts + 1] = lhs:match('^<[^<>]+>') or vim.fn.strcharpart(lhs, 0, 1)
          end
        end
      end
      heads(api.nvim_get_keymap('i'), true)
      heads(api.nvim_buf_get_keymap(buf, 'i'))
      if bo.iminsert == 1 then
        heads(api.nvim_get_keymap('l'))
        heads(api.nvim_buf_get_keymap(buf, 'l'))
      end
      local abbrevs = {}
      for _, a in ipairs(vim.fn.maplist(true)) do
        if a.mode:find('[i!]') then
          abbrevs[#abbrevs + 1] = a.lhs
        end
      end
      -- Neovim's comp_textwidth: with 'wrapmargin', the width less it, the
      -- fold and sign columns and 8 for any number column. Here the whole
      -- gutter and 8, which errs narrower.
      local tw = bo.textwidth
      if tw == 0 and bo.wrapmargin > 0 then
        local info = vim.fn.getwininfo(win)[1]
        local nu = (wo.number or wo.relativenumber) and 8 or 0
        tw = math.max(info.width - bo.wrapmargin - info.textoff - nu, 0)
      end
      local hooked = hooks(buf)
      local f = { buf, entered[buf] == true, firsts, abbrevs, hooked[1], tw,
        bo.formatoptions, bo.indentexpr ~= '' and bo.indentkeys or (bo.cindent and bo.cinkeys or ''),
        bo.smartindent, bo.lisp, vim.o.showmatch, bo.matchpairs,
        vim.fn.exists('+autocomplete') == 1 and vim.o.autocomplete or false,
        bo.modifiable and not (bo.readonly and not bo.modified) and bo.buftype ~= 'prompt',
        not (vim.o.revins or wo.rightleft or bo.iminsert == 2 or vim.o.paste or vim.o.digraph),
        vim.o.backspace,
        bo.softtabstop ~= 0 or bo.varsofttabstop ~= '', vim.o.smarttab, vim.o.delcombine,
        wo.conceallevel > 0 and wo.concealcursor or '', classes(buf), hooked[2], hooked[3],
        bo.autoindent, bo.cindent or bo.indentexpr ~= '', undo['<C-W>'], undo['<C-U>'] }
      if not vim.deep_equal(factsof[buf], f) then
        factsof[buf] = f
        send('insert', unpack(f))
      end
    end
    -- Such autocommands can appear at any time, so recheck them on
    -- SafeState and resend the facts before the ack if they changed.
    rehooked = function()
      local buf = api.nvim_get_current_buf()
      local f = factsof[buf]
      if f and not vim.deep_equal({ f[5], f[22], f[23] }, hooks(buf)) then
        facts()
      end
    end
    au('InsertEnter', {}, function(ev)
      entered[ev.buf] = true
      local c = api.nvim_win_get_cursor(0)
      began = { ev.buf, c[1] - 1, (entering == 'o' or entering == 'O') and 0 or c[2] }
      orig, sticks, entering, rubbed, typed_at, ctrl_g = began, false, nil, false, nil, false
      vim.schedule(facts)
    end)
    au('BufEnter', {}, function()
      vim.schedule(facts)
    end)
    au('OptionSet', { pattern = { 'iskeyword', 'textwidth', 'wrapmargin', 'formatoptions',
      'indentkeys', 'cinkeys', 'indentexpr', 'cindent', 'smartindent', 'lisp', 'showmatch',
      'matchpairs', 'autocomplete', 'modifiable', 'readonly', 'buftype', 'revins', 'rightleft',
      'iminsert', 'paste', 'digraph', 'backspace', 'softtabstop', 'varsofttabstop', 'smarttab',
      'delcombine', 'conceallevel', 'concealcursor', 'autoindent' } }, function()
      vim.schedule(facts)
    end)
    au('BufWipeout', {}, function(ev)
      factsof[ev.buf], entered[ev.buf] = nil, nil
    end)
    vim.schedule(facts)
  end
  soon()
end
local wins = {}
for _, w in ipairs(api.nvim_list_wins()) do
  wins[#wins + 1] = { w, api.nvim_win_get_buf(w) }
end
return { vim.o.laststatus, vim.o.splitright, vim.o.splitbelow, wins }";

/// Where a scroll ahead of Neovim lands the cursor: see [`landed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Landed {
    /// The display column it lands on, and its curswant from then on.
    vcol: usize,
    want: usize,
    /// The view's first display column after Neovim scrolls it to show the
    /// cursor ([`predict::showing`]); `None` if the cursor is at the end of a
    /// line sent cut short.
    left: Option<usize>,
}

impl Landed {
    /// The cursor's grid column in a view from display column `left`, or
    /// `None` outside the window.
    fn col(&self, left: usize, shape: &predict::Shape, width: usize) -> Option<usize> {
        self.vcol
            .checked_sub(left)
            .map(|c| shape.textoff + c)
            .filter(|&col| col < width)
    }
}

/// Where a scroll ahead lands the cursor from display column `want`; `None`
/// if the line has not been sent.
fn landed(
    p: &Predictor,
    win: i64,
    shape: &predict::Shape,
    l: anim::Land,
    want: usize,
    width: usize,
) -> Option<Landed> {
    let text = p.text(win, l.line)?;
    let (vcol, want) = if l.first {
        let v = predict::first_nonblank(text, shape);
        (v, v)
    } else {
        (predict::landing(text, shape, want, l.insert), want)
    };
    let left = (!p.cut(win, l.line) || !predict::at_end(text, shape, vcol))
        .then(|| predict::showing(text, shape, vcol, shape.leftcol, width));
    Some(Landed { vcol, want, left })
}

/// Whether a scrolling key can be predicted now: with acks, from the last
/// ack and keys since ([`Predictor::known`]); otherwise once the fences
/// after every unfollowed key are answered ([`predict::Typed::sure`]).
fn sure(p: &Predictor) -> bool {
    if p.acks() {
        p.known()
    } else {
        p.typed.sure()
    }
}

/// Whether the client can draw lines `shown` of `win` from display column
/// `left`: it has every line coloured, and lines sent cut short reach past
/// the right edge ([`predict::known`]).
fn drawable(
    p: &Predictor,
    win: i64,
    shape: &predict::Shape,
    shown: std::ops::Range<i64>,
    line: i64,
    left: usize,
    width: usize,
) -> bool {
    let at = predict::Across {
        left,
        line,
        vcol: 0,
    };
    shown.clone().all(|l| p.coloured(win, l).is_some())
        && predict::known(
            at,
            at,
            width,
            shape,
            |l| p.text(win, l),
            |l| p.cut(win, l),
            shown,
        )
}

/// The whole row for buffer line `line`, outside Neovim's view, drawn as a
/// scroll ahead would ([`Predictor::row`]) with the cursor on `cursor`: the
/// gutter numbers ([`predict::numbers`]), sign and fold columns in cursor
/// line groups where `'cursorline'` covers the number, and the
/// `'cursorline'` bar derived in `model` ([`model::Derive`]). `None` if the
/// line is missing or the bar cannot be derived. Only for windows
/// [`App::afresh`] allows.
fn drawn_afresh(
    p: &Predictor,
    model: &mut Model,
    win: i64,
    (line, cursor): (i64, i64),
    (h, width): (i64, usize),
    (look, ids): (&predict::Look, &predict::Gutter),
) -> Option<Vec<Cell>> {
    let shape = p.shape(win)?;
    let cul = shape.stepping.as_ref()?.cursorline;
    let mut row = p.row(win, line, width, look, cursor)?;
    let textoff = shape.textoff.min(width);
    let numbers = predict::numbers(line, cursor, shape, h, ids).unwrap_or_default();
    let marks = textoff.checked_sub(numbers.len())?;
    row.splice(marks..textoff, numbers);
    if line != cursor {
        return Some(row);
    }
    if cul.number {
        for cell in &mut row[..marks] {
            cell.hl = match cell.hl {
                hl if hl == ids.sign => ids.cursor_sign,
                hl if hl == ids.fold => ids.cursor_fold,
                hl => hl,
            };
        }
    }
    if cul.line {
        for cell in &mut row[textoff..] {
            cell.hl = model.derived(cell.hl, model::Derive::CursorLineOn)?;
        }
    }
    Some(row)
}

/// Where the next predicted cursor step starts ([`App::onward`]): Neovim's
/// view, the text rectangle in the grid, the window, gutter width and first
/// display column steps use, the cursor, and the newest step.
#[derive(Debug, Clone)]
struct Onward {
    win: i64,
    view: model::Viewport,
    rect: (usize, usize, usize, usize),
    shown: (i64, usize, usize),
    from: Pos,
    head: Option<chain::Step>,
}

impl Onward {
    fn neovims(&self, l: i64) -> bool {
        let h = (self.rect.1 - self.rect.0) as i64;
        (self.view.topline..self.view.topline + h).contains(&l)
    }

    /// Text of line `l` a step may go onto: as typed by the steps, or as
    /// sent if not cut short. Only lines steppable like Neovim
    /// ([`predict::ordinary`]), and ASCII-only where character widths may
    /// differ from Neovim's (`stepping.widths`).
    fn text<'a>(
        &'a self,
        p: &'a Predictor,
        stepping: &'a predict::Stepping,
    ) -> impl Fn(i64) -> Option<&'a str> + 'a {
        let echo = self.head.as_ref().and_then(|s| s.echo.as_ref());
        let lines = p.ordinary_texts(self.win, stepping.widths);
        move |l| match echo.filter(|e| e.line == l) {
            Some(e) => Some(&*e.text)
                .filter(|t| predict::ordinary(t) && (!stepping.widths || t.is_ascii())),
            None => lines(l),
        }
    }
}

/// Requests awaiting an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ask {
    ApiInfo,
    /// [`UIS_LUA`].
    Uis,
    /// [`LAYOUT_LUA`], for these grids and their windows.
    Layout(Vec<(u64, i64)>),
    /// The detach before re-attaching the other way.
    Detach,
    /// [`AGENT_LUA`].
    Agent,
    /// [`wide::MEASURE_LUA`].
    Measure,
    /// [`PING`], to time the round trip ([`predict`]).
    Ping,
    /// A request after keys whose answer means Neovim took them and drew
    /// the result ([`predict::Typed`]), with the input count it followed and
    /// the fence as [`predict::Sync`] knows it.
    Fence {
        sent: u64,
        fenced: predict::Fenced,
    },
}

/// What made a predicted scroll: keys (Normal/Visual, or Insert-mode
/// scrolling keys), predicted only when [`sure`]; a page-wise wheel turn
/// (Normal/Visual); or a line-wise one (Insert too).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum By {
    Keys,
    InsertKeys,
    PageWheel,
    Wheel,
}

/// A predicted scroll's input: its kind, sequence number
/// ([`predict::Sync`]) and send time, from which Neovim's deadline runs
/// ([`Predictor::deadline`]), whether rebuilt from an ack or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cause {
    by: By,
    seq: u64,
    at: Instant,
}

/// How the client is attached: see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attach {
    /// Not yet: the other UIs decide how.
    Asking,
    Attached {
        multigrid: bool,
    },
    /// Waiting for the detach's answer before re-attaching as `multigrid`
    /// says; everything before it belongs to the old attach.
    Leaving {
        multigrid: bool,
    },
}

/// The kitty keyboard query's progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keyboard {
    Asking,
    /// The terminal answered (the protocol is on); the device attributes
    /// reply that ends the query is still due.
    Closing,
    Settled,
}

#[derive(Debug, Clone, Copy)]
struct Refreshed {
    at: Instant,
    tries: u8,
}

pub struct App {
    model: Model,
    screen: Screen,
    anim: Animator,
    parser: Parser,
    outbox: Outbox,
    /// `redraw` events since the last `flush`.
    batch: Vec<Event>,
    /// Terminal size as (columns, rows).
    size: (usize, usize),
    attach: Attach,
    /// An effect, predictions or centring need per-window grids, so attach
    /// with `ext_multigrid` when possible.
    multigrid_wanted: bool,
    /// Whether the client is on a terminal, as told to Neovim.
    tty: term::Tty,
    keyboard: Keyboard,
    /// From `nvim_get_api_info`; identifies this client among the UIs.
    chan: Option<u64>,
    asked: HashMap<u32, Ask>,
    /// A UI listing is pending; the UIs changed again since it was sent.
    checking: bool,
    recheck: bool,
    /// When to list the UIs again after finding one passing through.
    recheck_at: Option<Instant>,
    /// The UIs changed and are not known to have settled.
    unsettled: bool,
    /// Unanswered layout requests.
    placing: usize,
    /// Visible window grids Neovim has not fully drawn as of the last flush,
    /// and the refreshes asked for them.
    lacking: Vec<u64>,
    refreshed: HashMap<u64, Refreshed>,
    /// A refresh is to be started: see [`App::take_refresh`].
    refresh: bool,
    /// The agent was installed this attach.
    agent: bool,
    /// The reach last sent to the agent ([`Predictor::reach`]).
    reach: i64,
    /// Each window's buffer, as the agent reports.
    bufs: HashMap<i64, i64>,
    /// Windows that switched buffer since the last flush.
    switched: Vec<i64>,
    /// Since when drawing has waited for what a multigrid attach lacks.
    hold_since: Option<Instant>,
    /// Since when the parser has held an escape or a partial sequence.
    escape_since: Option<Instant>,
    /// The grid a mouse button went down on; its drag and release go there
    /// too.
    pressed: Option<u64>,
    /// Mouse reporting as last set: (on, with motion).
    mouse: (bool, bool),
    dirty: bool,
    last_frame: Option<Instant>,
    /// When the last two frames that drew a Neovim flush were drawn, latest
    /// first: what [`App::dirty_due`] paces Neovim's frames by.
    flush_frames: (Option<Instant>, Option<Instant>),
    /// A Neovim flush came in since the last frame.
    flushed_since: bool,
    /// This attach has had its first flush, applied without animation;
    /// nothing is drawn before it.
    flushed: bool,
    /// Steps are being remade after a frame dropped them ([`App::remake`]):
    /// they put the cursor back where it was drawn, without travel.
    remaking: bool,
    /// Put aside: see [`AWAY_FRAME_GAP`].
    away: bool,
    /// Bytes for the terminal before the next frame (bell, title, the
    /// server's own), in order.
    say: Vec<u8>,
    /// The server said it is exiting.
    done: bool,
    predict: Option<Predictor>,
    /// The latest ack, for the frame it preceded: the sequence number of the
    /// last key it covers, and the ack ([`App::heard_ack`]).
    ack: Option<(u64, predict::Ack)>,
    /// The number of the key before one Neovim may count as two, once an ack
    /// counted past it ([`predict::Sync::passed`]): Neovim has taken every
    /// key to it. Applied with the frame the ack preceded ([`App::floor`]).
    passed: Option<u64>,
    /// Send times of requests that measure the round trip.
    asked_at: HashMap<u32, Instant>,
    /// Last key, mouse input or paste: the round trip is measured only
    /// while the user is active.
    typed: Option<Instant>,
    /// Views' new first columns per the agent, for the next frame
    /// ([`anim::side`]).
    lefts: Vec<(i64, usize)>,
    cards: cards::Cards,
    /// `[client] centre`: centre each window's text on a terminal wider than
    /// needed, sized by its content or by `window_width` ([`wide`]).
    centre: Centre,
    /// `[client] window_width`: columns a centred window's text gets; 0 for
    /// no limit.
    window_width: usize,
    /// The measure was installed this attach.
    measuring: bool,
    /// Window grids asked to be narrower than their windows, and the width.
    narrowed: HashMap<u64, usize>,
    /// Grids the client cut to the asked width that Neovim has not resized
    /// since, with Neovim's last width ([`App::narrow_windows`]).
    cut: HashMap<u64, usize>,
    /// Widths asked of each grid not yet applied, oldest first: tells which
    /// ask a resize answers ([`App::grid_resized`]).
    unanswered: HashMap<u64, Vec<usize>>,
    /// The measure's last wants for windows of the current tab page.
    wants: BTreeMap<i64, usize>,
    /// Text past each window grid's edge reported since the last `flush`,
    /// and since when; taken with that `flush` ([`wide::TAILS_LUA`]).
    tails: Vec<(i64, BTreeMap<usize, wide::Tail>)>,
    tails_since: Option<Instant>,
    /// Grids checked once this attach for a width another client asked for
    /// ([`App::narrow_windows`]).
    looked: HashSet<u64>,
    /// State for partial frames ([`App::frame`]); `None` only in tests that
    /// build every frame whole.
    partial: Option<Partial>,
}

/// What building a frame from changed rows only needs to know of the last
/// frame ([`App::frame`]).
#[derive(Debug, Default)]
struct Partial {
    /// The grids' clock at the last frame ([`grid::tick`]); later changes
    /// belong to the next.
    at: u64,
    /// The last frame was the editor's grids as placed, except for rows drawn
    /// over ([`Animator::still`]), with no card.
    still: bool,
    /// Rows of this frame to rebuild.
    rows: Vec<bool>,
    /// Rows of the last frame drawn over ([`Animator::patch`]), rebuilt in the
    /// next in case what covered them is gone.
    patched: Vec<usize>,
    /// Count of partial frames, for tests.
    #[cfg(test)]
    partly: usize,
}

impl App {
    /// A client with `effects`, predicting if `predict` (scrolls only until
    /// [`App::with_predictions`]), on a terminal of `size`.
    pub fn new(term: Palette, effects: Effects, predict: bool, size: (usize, usize)) -> Self {
        Self {
            model: Model::new(term),
            screen: Screen::moving_rows(),
            // Predictions are per window, so they need per-window grids.
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
            flush_frames: (None, None),
            flushed_since: false,
            flushed: false,
            remaking: false,
            away: false,
            say: Vec::new(),
            done: false,
            predict: predict.then(Predictor::default),
            ack: None,
            passed: None,
            asked_at: HashMap::new(),
            typed: None,
            lefts: Vec::new(),
            cards: cards::Cards::default(),
            centre: Centre::Off,
            window_width: 0,
            measuring: false,
            narrowed: HashMap::new(),
            cut: HashMap::new(),
            unanswered: HashMap::new(),
            wants: BTreeMap::new(),
            tails: Vec::new(),
            tails_since: None,
            looked: HashSet::new(),
            partial: Some(Partial::default()),
        }
    }

    /// Centre each window's text as `centre` says ([`wide`]).
    pub fn with_centre(self, centre: Centre, window_width: usize) -> Self {
        let mut app = Self {
            centre,
            window_width,
            ..self
        };
        // Narrowing needs per-window grids.
        app.multigrid_wanted |= app.centring();
        app
    }

    /// Which predictions to make, if predicting at all.
    pub fn with_predictions(mut self, predictions: Predictions) -> Self {
        if let Some(p) = self.predict.as_mut() {
            p.set_predictions(predictions);
        }
        self
    }

    /// Whether frames move rows with scroll regions ([`region`]), as by
    /// default, or rewrite them; for tests comparing the two.
    #[cfg(test)]
    pub fn with_scroll_regions(mut self, on: bool) -> Self {
        self.screen.set_regions(on);
        self
    }

    /// Whether frames are built from changed rows only ([`App::frame`]), as
    /// by default, or whole; for tests comparing the two.
    #[cfg(test)]
    pub fn with_partial_frames(mut self, on: bool) -> Self {
        if on != self.partial.is_some() {
            self.partial = on.then(Partial::default);
        }
        self
    }

    fn centring(&self) -> bool {
        self.centre != Centre::Off && self.window_width != 0
    }

    /// Sends client info and attaches, after listing the other UIs if
    /// multigrid is wanted.
    pub fn start(&mut self) {
        let id = self.outbox.request("nvim_get_api_info", &[]);
        self.asked.insert(id, Ask::ApiInfo);
        // Answered at once even when busy, so it measures the link alone.
        self.asked_at.insert(id, Instant::now());
        if let Some(p) = self.predict.as_mut() {
            p.attached();
        }
        self.client_info();
        // Sent before anything the attach waits for, so window wants are
        // known before windows are drawn.
        if self.centring() {
            self.measure(None);
        }
        if self.multigrid_wanted {
            self.ask_uis();
        } else {
            self.attach_as(false);
        }
    }

    /// Runs [`wide::MEASURE_LUA`]; given the channel, it stays installed and
    /// reports changes.
    fn measure(&mut self, chan: Option<u64>) {
        let smart = self.centre == Centre::Smart;
        let mut args = vec![Arg::Int(self.window_width as i64), Arg::Bool(smart)];
        if let Some(chan) = chan {
            args.push(Arg::Int(chan as i64));
        }
        let id = self.outbox.request(
            "nvim_exec_lua",
            &[Arg::str(wide::MEASURE_LUA), Arg::Array(args)],
        );
        self.asked.insert(id, Ask::Measure);
    }

    /// Installs the measure and [`wide::TAILS_LUA`] for this attach, once the
    /// channel is known.
    fn leave_measure(&mut self) {
        if !self.centring() || self.measuring || self.attach == Attach::Asking {
            return;
        }
        if let Some(chan) = self.chan {
            self.measuring = true;
            self.measure(Some(chan));
            self.outbox.notify(
                "nvim_exec_lua",
                &[
                    Arg::str(wide::TAILS_LUA),
                    Arg::Array(vec![Arg::Int(chan as i64)]),
                ],
            );
        }
    }

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
                self.model.set_laststatus(Some(l));
                self.dirty = true;
            }
        }
    }

    /// When centring, asks for each window wider than it wants to get a grid
    /// that wide and the rest their full width ([`wide`]); hidden windows
    /// keep theirs. When not, releases every narrowed grid. A window found on
    /// another client's narrowed grid that this one would not narrow is
    /// asked for whole. Narrowed grids are cut locally at once, as Neovim
    /// will. Returns whether anything was asked.
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
        // A grid not its window's width was asked for by another client,
        // attached now (grids are shared by all UIs) or killed before
        // releasing it; Neovim keeps such an ask, unchanged, until a UI asks
        // for the window whole, so a window narrowed since can be on a wider
        // grid. Ask for it whole unless this client narrows it, so the last
        // client to attach gets its way. Checked once per attach, when both
        // widths are known; later asks by others are left alone.
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
        // An ask restoring a grid the client cut (see below) to Neovim's last
        // width waits until Neovim has resized it: given both asks between
        // redraws, Neovim would see no change and send nothing, leaving the
        // local copy cut for good. The pass after that resize asks it.
        let mut narrowed = want;
        asks.retain(|&(grid, width)| {
            let Some(&before) = self.cut.get(&grid) else {
                return true;
            };
            if self.asked_width(grid, width) != before {
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
            // Cut the local copy now, keeping the left as a resize does, so
            // the text is placed right from the next frame (a split's first,
            // as it flies in) instead of a round trip later. Widening is left
            // to Neovim.
            if let Some(g) = self
                .model
                .grids
                .get_mut(&grid)
                .filter(|g| width > 0 && width < g.width())
            {
                self.cut.entry(grid).or_insert(g.width());
                g.resize(width, g.height());
                self.dirty = true;
                // Tails were for the old width; Neovim resends them.
                self.model.forget_tails(grid);
            }
            // Height 0 keeps the window's own.
            self.outbox.notify(
                "nvim_ui_try_resize_grid",
                &[Arg::Int(grid as i64), Arg::Int(width as i64), Arg::Int(0)],
            );
            let to = self.asked_width(grid, width);
            self.unanswered.entry(grid).or_default().push(to);
        }
        // Text that moved in or out of its window slides there.
        self.anim.insets(&self.model, now);
        asked
    }

    /// Adjusts Neovim's resize of `grid` to `width` before the model takes
    /// it: normally the client's cut ([`App::narrow_windows`]) is dropped,
    /// but the answer to an ask superseded since keeps the local width.
    ///
    /// A new split is asked for `window_width` before the measure reports,
    /// then for less; Neovim answers both, building the first width from the
    /// old grid without redrawing kept cells, while the local copy is already
    /// at the second. Taking the first whole would leave columns never drawn,
    /// and the client would hold frames (and the split's fly-in) for a
    /// refresh that the second answer makes moot.
    fn grid_resized(&mut self, grid: u64, width: &mut usize) {
        let asks = self.unanswered.remove(&grid).unwrap_or_default();
        let later = asks
            .iter()
            .rposition(|w| w == width)
            .map(|i| asks[i + 1..].to_vec())
            .filter(|later| !later.is_empty());
        let Some(later) = later else {
            // The answer to the last ask, or Neovim's own resize.
            self.cut.remove(&grid);
            return;
        };
        self.unanswered.insert(grid, later);
        match self.model.grids.get(&grid).map(|g| g.width()) {
            Some(copy) if copy < *width => {
                // Record Neovim's width for asks waiting on its resize.
                self.cut.insert(grid, *width);
                *width = copy;
            }
            _ => {
                self.cut.remove(&grid);
            }
        }
    }

    /// The grid width an ask of `width` yields; 0 means the window's width.
    fn asked_width(&self, grid: u64, width: usize) -> usize {
        match self.model.layout.get(&grid).map(|p| &p.place) {
            Some(Place::Window { width: whole, .. }) if width == 0 => *whole,
            _ => width,
        }
    }

    /// The measure's want for `grid`'s window, or `window_width` if none yet.
    fn want_of(&self, grid: u64) -> usize {
        self.model
            .wins
            .get(&grid)
            .and_then(|win| self.wants.get(win))
            .copied()
            .unwrap_or(self.window_width)
    }

    /// Releases every narrowed grid as the client exits, so other UIs see
    /// whole windows. Returns whether there were any.
    pub fn let_windows_go(&mut self, now: Instant) -> bool {
        // The local copy no longer matters, so nothing waits on resizes.
        self.cut.clear();
        self.unanswered.clear();
        let window_width = std::mem::take(&mut self.window_width);
        let asked = self.narrow_windows(now);
        self.window_width = window_width;
        asked
    }

    /// `nvim_set_client_info`, marking whether this UI wants `ext_multigrid`
    /// so other nvmux clients know it is only passing through without it
    /// ([`UIS_LUA`]).
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

    fn attach_as(&mut self, multigrid: bool) {
        self.attach = Attach::Attached { multigrid };
        self.flushed = false;
        let term = std::env::var("TERM").unwrap_or_default();
        let (w, h) = self.size;
        // This attach may not have the last one's windows.
        self.narrowed.clear();
        self.cut.clear();
        self.unanswered.clear();
        self.looked.clear();
        let mut opts = vec![
            ("rgb", Arg::Bool(true)),
            ("ext_linegrid", Arg::Bool(true)),
            ("ext_termcolors", Arg::Bool(true)),
            ("term_name", Arg::str(&term)),
            ("term_colors", Arg::Int(256)),
            // As Neovim's own TUI reports them: see `term::Tty`.
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

    /// Installs [`AGENT_LUA`] once a multigrid attach that needs it knows
    /// its channel.
    fn leave_agent(&mut self) {
        let Some(chan) = self.chan else { return };
        let predict = self.predict.is_some();
        if self.agent || !self.multigrid() || !(self.anim.wants_agent() || predict) {
            return;
        }
        let acks = self.predict.as_ref().is_some_and(Predictor::acks);
        let typing = self.predict.as_ref().is_some_and(Predictor::typing);
        self.agent = true;
        self.reach = self.predict.as_ref().map_or(2, Predictor::reach);
        let (keys, ikeys) = (predict::keys(acks), predict::insert_keys());
        fn list(keys: &[String]) -> Arg<'_> {
            Arg::Array(keys.iter().map(|k| Arg::str(k)).collect())
        }
        let mut args = vec![
            Arg::Int(chan as i64),
            Arg::Bool(predict),
            list(&keys),
            list(&ikeys),
            Arg::Int(self.reach),
            Arg::Bool(acks),
        ];
        if typing {
            args.push(Arg::Bool(true));
        }
        let id = self
            .outbox
            .request("nvim_exec_lua", &[Arg::str(AGENT_LUA), Arg::Array(args)]);
        self.asked.insert(id, Ask::Agent);
    }

    /// Sends the agent a changed [`Predictor::reach`].
    fn tell_reach(&mut self) {
        let (Some(chan), Some(p)) = (self.chan, self.predict.as_ref()) else {
            return;
        };
        let reach = p.reach();
        if !self.agent || reach == self.reach {
            return;
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

    /// Handles a notification from [`AGENT_LUA`].
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
            // Sent ahead of the `flush` it belongs to; taken with that frame.
            Some("tails") => {
                if let Some(win) = int(1) {
                    let sure = !matches!(params.get(2), Some(ValueRef::Boolean(false)));
                    self.tails.push((win, wide::tails(params.get(3), sure)));
                    self.tails_since.get_or_insert(now);
                }
            }
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
            // Sent just before Neovim draws it; taken with that frame.
            Some("leftcol") => {
                if let (Some(win), Some(left)) = (int(1), int(2)) {
                    self.lefts.push((win, usize::try_from(left).unwrap_or(0)));
                }
            }
            Some("ack") => self.heard_ack(&params[1..], now),
            // Neovim is behind on typed keys and may send no frame until
            // caught up: extend pending landings as for a scroll made now.
            Some("behind") => {
                if let Some(p) = self.predict.as_ref() {
                    self.anim.defer(p.deadline(now));
                }
            }
            Some(what) => {
                if let Some(p) = self.predict.as_mut() {
                    p.said(what, &params[1..]);
                }
                if what == "shape" {
                    if let Some(win) = int(1) {
                        self.reshaped(win, now);
                    }
                }
                // New insert facts for the ack's buffer may let keys typed
                // since be predicted.
                let base = self.predict.as_ref().and_then(|p| p.base.as_ref());
                if what == "insert" && base.is_some_and(|b| Some(b.ack.buf) == int(1)) {
                    self.remake(now);
                }
                // A scroll ahead may have been waiting for these lines.
                if matches!(what, "lines" | "splice") && self.flushed {
                    self.fill_in(now);
                }
            }
            None => {}
        }
    }

    /// A new shape for `win`: a window distrusted for drawing differently may
    /// match now, and steps made with another gutter width are misplaced,
    /// so they are dropped.
    fn reshaped(&mut self, win: i64, now: Instant) {
        let Some(p) = self.predict.as_mut() else {
            return;
        };
        p.trust.reshaped(win);
        let textoff = p.shape(win).map(|s| s.textoff);
        let grid = self
            .model
            .wins
            .iter()
            .find(|(_, w)| **w == win)
            .map(|(g, _)| *g);
        if let Some(grid) = grid {
            if self
                .anim
                .chain(grid)
                .is_some_and(|c| c.win == win && Some(c.textoff) != textoff)
            {
                self.drop_steps(grid, now);
            }
        }
    }

    /// Put aside ([`crate::card::Command::Away`]): animations jump to their
    /// end and none start until back in front; frames are limited by
    /// [`AWAY_FRAME_GAP`].
    fn go_away(&mut self, now: Instant) {
        self.away = true;
        self.anim.reset();
        self.forget_dropped();
        self.anim.hold_blink(true, now);
        self.dirty = true;
    }

    /// Back in front ([`crate::card::Command::Clear`]). Coming from away,
    /// draws a frame at once (changes of up to a second, and the cursor even
    /// if nothing changed), which nvmux waits for so the session returns
    /// current (see `crate::pty::relay`).
    fn come_back(&mut self, now: Instant) {
        if std::mem::take(&mut self.away) {
            self.screen.forget_cursor();
            self.dirty = true;
        }
        self.anim.hold_blink(false, now);
    }

    /// Applies changed settings ([`crate::card::Command::Settings`]).
    ///
    /// Effects change at once; running animations finish, disabled ones do
    /// not restart. Multigrid and the agent's predictions are applied by
    /// re-attaching (as when the UIs change), and widths by measuring again.
    pub fn reconfigure(&mut self, drawing: &crate::config::Drawing, now: Instant) {
        let effects = Effects::from_settings(&drawing.settings());
        let client = drawing.client;
        let predict = client.predicts(drawing.remote);
        let predicting = self.predict.is_some();
        let acking = self.predict.as_ref().is_some_and(Predictor::acks);
        let typed_ahead = self.predict.as_ref().is_some_and(Predictor::typing);
        let centred = (self.centre, self.window_width);
        self.centre = client.centre;
        self.window_width = usize::from(client.window_width);
        let wanted = effects.want_multigrid() || predict || self.centring();
        self.anim.set_effects(effects);
        self.dirty = true;
        if predict != predicting {
            self.predict = predict.then(|| {
                let mut p = Predictor::default();
                p.attached();
                p
            });
        }
        if let Some(p) = self.predict.as_mut() {
            p.set_predictions(client.predictions(drawing.remote));
        }
        let acks = self.predict.as_ref().is_some_and(Predictor::acks);
        let typing = self.predict.as_ref().is_some_and(Predictor::typing);
        let flipped = wanted != self.multigrid_wanted;
        self.multigrid_wanted = wanted;
        if flipped {
            self.client_info();
        }

        let again = match self.attach {
            // Multigrid newly wanted: the other UIs decide, as at start.
            Attach::Attached { multigrid: false } if flipped && wanted => {
                self.ask_uis();
                false
            }
            // Multigrid no longer wanted, or the agent's prediction, ack or
            // typing arguments changed.
            Attach::Attached { multigrid: true }
                if !wanted || predict != predicting || acks != acking || typing != typed_ahead =>
            {
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
        // A pending re-attach installs the agent and measure itself.
        if again {
            return;
        }
        self.leave_agent();
        if centred != (self.centre, self.window_width) {
            if self.centring() {
                // An installed measure is replaced by the next.
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

    /// Detaches, to re-attach as `multigrid` says once answered. Narrowed
    /// grids are released first, while Neovim still accepts it from this UI.
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

    /// Asks where these windows are; drawing waits for the answer.
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

    /// Handles one whole msgpack-RPC frame from the server.
    pub fn message(&mut self, frame: &[u8], now: Instant) {
        // Nearly every frame; decoded from bytes (see `redraw::decode_bytes`).
        if let Some(params) = redraw::redraw_params(frame) {
            // Between a detach and its answer, redraws are the old attach's.
            if !matches!(self.attach, Attach::Leaving { .. }) {
                redraw::decode_bytes(params, &mut self.batch);
                self.take_flushes(now);
            }
            return;
        }
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
                        if let Some(p) = self.predict.as_mut() {
                            p.sync.agent = p.acks();
                        }
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
                            // Window widths are known now: see
                            // `narrow_windows`.
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
                    Some(Ask::Fence { sent, fenced }) => {
                        // Take acks whose frame never came (a key that
                        // changed nothing) before the fence's answer.
                        self.take_acks(now);
                        let mode = Mode::of(&self.model.mode_name);
                        let mut upto = None;
                        if let Some(p) = self.predict.as_mut() {
                            p.typed.answered(sent, mode);
                            if p.acks() {
                                let taken = result.and_then(redraw::int).unwrap_or(-1);
                                upto = p.answered(fenced, taken).or_else(|| p.sync.passed());
                            }
                        }
                        if let Some(upto) = upto {
                            self.floor(upto, now);
                        }
                        self.forget_unsure(now);
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

    /// Acts on the UI listing: `foreign` keeps windows on grid 1 for good,
    /// `passing` only for now. See the module docs.
    fn consider(&mut self, foreign: bool, passing: bool, now: Instant) {
        match self.attach {
            Attach::Asking => self.attach_as(!foreign),
            Attach::Attached { multigrid: true } if foreign => self.leave(false, now),
            Attach::Attached { multigrid: true } => {
                if passing {
                    self.recheck_at = Some(now + RECHECK);
                } else {
                    // Settled. Windows may have moved while on grid 1, and
                    // Neovim does not resend their places.
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

    /// The UIs may have changed: list them, and hold a multigrid attach's
    /// drawing until they settle.
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

    /// Places windows from a layout answer, without animating: it is not a
    /// move that just happened.
    fn placed(&mut self, wins: &[(u64, i64)], answer: &[ValueRef<'_>], now: Instant) {
        let mut events = Vec::new();
        for (&(grid, win), a) in wins.iter().zip(answer) {
            // Gone since, or reused by another window.
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
            // The window's size if given; it differs from its grid's when
            // centred (see `wide`).
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

    /// Forgets the editor before a fresh attach describes it again; the
    /// screen is kept for the next frame to draw over.
    fn reset(&mut self) {
        let term = self.model.colors().term;
        self.model = Model::new(term);
        self.batch.clear();
        self.tails.clear();
        self.tails_since = None;
        self.anim.reset();
        self.anim.take_dropped();
        self.flushed = false;
        self.unsettled = false;
        self.placing = 0;
        self.lacking.clear();
        self.refreshed.clear();
        self.hold_since = None;
        self.pressed = None;
        self.ack = None;
        self.passed = None;
        if let Some(p) = self.predict.as_mut() {
            p.reset();
            p.attached();
        }
    }

    /// Applies every complete batch received so far.
    fn take_flushes(&mut self, now: Instant) {
        while let Some(end) = self.batch.iter().position(|e| *e == Event::Flush) {
            let rest = self.batch.split_off(end + 1);
            let batch = std::mem::replace(&mut self.batch, rest);
            self.flush(batch, now);
        }
    }

    fn flush(&mut self, batch: Vec<Event>, now: Instant) {
        // Neovim re-sends the editor-wide ext_* options when the UIs change.
        if self.flushed
            && batch
                .iter()
                .any(|e| matches!(e, Event::OptionSet { name, .. } if name.starts_with("ext_")))
        {
            self.uis_changed(now);
        }
        let animate = self.flushed && !self.holding(now) && !self.away;
        // Grids of windows that switched buffer; this batch draws them.
        let switched: Vec<u64> = std::mem::take(&mut self.switched)
            .into_iter()
            .filter_map(|win| {
                let (grid, _) = self.model.wins.iter().find(|(_, w)| **w == win)?;
                Some(*grid)
            })
            .collect();
        self.apply(batch, animate, &switched, now);
        self.flushed_since = true;
        self.flushed = true;
        self.landed_sideways(now);
        self.take_tails();
        self.take_acks(now);
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

    /// Applies a batch to the model and starts its animations if `animate`,
    /// else jumps everything to its place. `switched` are the grids the
    /// batch draws another buffer in.
    fn apply(&mut self, mut batch: Vec<Event>, animate: bool, switched: &[u64], now: Instant) {
        let multigrid = self.multigrid();
        let before = self.anim.before(&self.model, &batch, multigrid, switched);
        // Reconcile Neovim's resizes with grids the client cut.
        for event in &mut batch {
            match event {
                Event::GridResize { grid, width, .. } => self.grid_resized(*grid, width),
                Event::GridDestroy { grid } => {
                    self.cut.remove(grid);
                    self.unanswered.remove(grid);
                }
                _ => {}
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
        self.anim.set_predicting(self.predict.is_some());
        let judging = self.ack.as_ref().map(|(k, _)| *k);
        self.anim.after(
            before,
            &self.model,
            &changes,
            (multigrid, animate),
            judging,
            now,
        );
        if self.forget_dropped() {
            self.remake(now);
        }
        // Once animations have taken what they keep, release the rest of
        // what the model held from before the batch.
        if self.model.reclaiming() {
            let mut held = self.model.held();
            self.anim.hold(&mut held);
            self.model.reclaim(&held);
        }
        self.dirty = true;
    }

    /// After a flush, refreshes windows not drawn whole and asks the layout
    /// of unplaced ones. See the module docs.
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

    /// Starts or ends the hold on drawing.
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

    /// Whether drawing waits for an attach, or for what a multigrid one
    /// lacks (up to [`HOLD_LIMIT`]).
    fn holding(&self, now: Instant) -> bool {
        match self.attach {
            Attach::Asking | Attach::Leaving { .. } => true,
            Attach::Attached { multigrid: false } => false,
            Attach::Attached { multigrid: true } => self
                .hold_since
                .is_some_and(|since| now < since + HOLD_LIMIT),
        }
    }

    /// Input from the terminal.
    pub fn keys(&mut self, bytes: &[u8], now: Instant) {
        let mut out = Vec::new();
        self.parser.feed(bytes, &mut out);
        self.escape_since = self.parser.waiting().then_some(now);
        self.inputs(out, now);
    }

    /// When the parser's held bytes are taken as they are: `'ttimeoutlen'`
    /// after the last, capped at [`RELAYED_ESCAPE`], or [`REPLY_WAIT`] for a
    /// partial terminal reply ([`input`]).
    fn escape_deadline(&self) -> Option<Instant> {
        let since = self.escape_since?;
        if self.parser.in_reply() {
            return Some(since + REPLY_WAIT);
        }
        let o = &self.model.options;
        let wait = if o.ttimeout {
            o.ttimeoutlen.min(RELAYED_ESCAPE)
        } else {
            0
        };
        Some(since + Duration::from_millis(wait.max(1)))
    }

    pub fn escape_due(&self, now: Instant) -> bool {
        self.escape_deadline().is_some_and(|at| at <= now)
    }

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
                    let mode = Mode::of(&self.model.mode_name);
                    if let Some(p) = self.predict.as_mut() {
                        // Neovim counts a paste as typed keys on the command
                        // line, as none in Normal, Visual and Insert mode;
                        // if the mode is unsure, it may be keys.
                        let keys = mode == Mode::Other || !p.typed.sure();
                        p.typed.other();
                        if p.acks() {
                            p.sync.pasted(p.typed.seq(), keys);
                        }
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
                    Some(crate::card::Command::Away) => self.go_away(now),
                    Some(cmd) => {
                        if matches!(cmd, crate::card::Command::Clear) {
                            self.come_back(now);
                        }
                        if self.cards.apply(cmd, now) {
                            self.dirty = true;
                        }
                    }
                    None => tracing::debug!("client: a card that would not decode"),
                },
            }
        }
        self.forget_unsure(now);
        self.fence();
    }

    /// Feeds typed keys to the predictions, making any predictable scroll at
    /// once. See [`predict::Typed`].
    fn keys_typed(&mut self, keys: &str, now: Instant) {
        let grid = self.model.cursor.grid;
        let win = self.model.wins.get(&grid).copied();
        for key in predict::tokens(keys) {
            let Some(p) = self.predict.as_mut() else {
                return;
            };
            let command = p.key(win, key);
            let seq = p.typed.seq();
            p.sent(seq, Some(key), now);
            if let Some(command) = command {
                let by = if p.typed.inserting() {
                    By::InsertKeys
                } else {
                    By::Keys
                };
                self.predict_command(grid, command, Cause { by, seq, at: now }, now);
            }
        }
    }

    /// Sends a fence if the predictions want one ([`predict::Typed`]),
    /// after all queued keys, so Neovim answers once it has taken them. With
    /// acks the answer is the agent's key count ([`predict::Sync`]), or -1
    /// before the agent exists.
    fn fence(&mut self) {
        let chan = self.chan;
        if let Some(p) = self.predict.as_mut() {
            if p.wants_fence() {
                let sent = p.typed.fenced();
                let fenced = p.sync.fenced(p.typed.seq());
                let ask = match chan.filter(|_| p.acks()) {
                    Some(chan) => {
                        format!("luaeval('(_G.nvmux_taken or {{}})[{chan}] or -1')")
                    }
                    None => "0".to_string(),
                };
                let id = self.outbox.request("nvim_eval", &[Arg::str(&ask)]);
                self.asked.insert(id, Ask::Fence { sent, fenced });
            }
        }
    }

    /// A terminal reply: to the client's keyboard query, or for the server.
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

    /// Sends a mouse report to the grid it landed on.
    ///
    /// Beside a centred window's text ([`wide`]), presses and moves are
    /// dropped and the wheel scrolls that window; drags and releases clamp
    /// to the text's edge so selections are not cut short.
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
        // Counted as one key, as the agent counts it, before any scroll it
        // predicts (see `predict::Sync`).
        if let Some(p) = self.predict.as_mut() {
            let seq = p.typed.input();
            match m.button {
                "wheel" => {
                    let turn = predict::Turn {
                        grid,
                        way: m.action.into(),
                        mods: m.mods.as_str().into(),
                    };
                    p.turned(seq, turn, now);
                }
                _ => p.sent(seq, None, now),
            }
        }
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
                // A click or drag may move the cursor or change mode.
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

    fn predict_wheel(&mut self, grid: u64, way: &str, mods: &str, now: Instant) {
        let insert = Mode::of(&self.model.mode_name) == Mode::Insert;
        let command = self.wheel_command(grid, way, mods, insert);
        let Some(p) = self.predict.as_mut() else {
            return;
        };
        match command {
            Some((command, by)) => {
                let seq = p.typed.seq();
                self.predict_command(grid, command, Cause { by, seq, at: now }, now);
            }
            None => p.typed.missed(),
        }
    }

    /// The command a wheel turn `way` over `grid` makes, and by what: plain,
    /// `'mousescroll'` lines or columns; with Shift or Ctrl, a page or the
    /// window's width. `None` where scrolls are not predicted, the wheel key
    /// is mapped, or the client cannot say where Neovim is.
    fn wheel_command(
        &self,
        grid: u64,
        way: &str,
        mods: &str,
        insert: bool,
    ) -> Option<(Command, By)> {
        let p = self.predict.as_ref()?;
        // With acks, left to Neovim like a scrolling key while the counts are
        // untied or the client is blind from an earlier key it could not
        // make: drawn from Neovim's view and cursor, the turn would leave out
        // that key. It is remade from the ack ([`App::replay`]).
        if !p.predictions.scrolls || (p.acks() && !p.known()) {
            return None;
        }
        let shape = self.model.wins.get(&grid).and_then(|w| p.shape(*w));
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
        match (shape, mods) {
            (Some(s), _) if s.mapped.contains(&name) => None,
            (Some(s), "") if sideways => Some((Command::Side(Side::Wheel(dir * s.hor)), By::Wheel)),
            (Some(_), "S" | "C") if sideways => {
                Some((Command::Side(Side::Wheel(dir * width)), By::Wheel))
            }
            (Some(s), "") => Some((Command::Lines(dir * s.step), By::Wheel)),
            // In Insert mode a window height, not a page.
            (Some(_), "S" | "C") if insert => Some((Command::Screen(dir), By::Wheel)),
            (Some(_), "S" | "C") => Some((
                Command::Page {
                    down: dir > 0,
                    count: 1,
                },
                By::PageWheel,
            )),
            _ => None,
        }
    }

    /// Makes a predicted scroll or cursor motion in `grid` at once if the
    /// client can tell what Neovim will do. One made (or with nothing to do)
    /// shows what Neovim will draw and is not replayed
    /// ([`predict::Sent::made`]); later predictions wait for Neovim to draw
    /// one that is not. See [`predict`].
    fn predict_command(&mut self, grid: u64, command: Command, cause: Cause, now: Instant) {
        let entering = matches!(command, Command::Enter(_));
        let made = self.make(grid, command, cause, now);
        if let Some(p) = self.predict.as_mut() {
            if made {
                p.made(cause.seq);
            } else {
                if entering {
                    p.unentered(cause.seq);
                }
                p.typed.missed();
            }
        }
    }

    /// Returns whether the command was made.
    fn make(&mut self, grid: u64, command: Command, cause: Cause, now: Instant) -> bool {
        match command {
            Command::Side(side) => self.predict_side(grid, side, cause, now),
            Command::Move {
                motion,
                count,
                screen,
            } => self.predict_move(grid, (motion, count, screen), cause, now),
            Command::Type(e) => self.predict_echo(grid, e, cause, now),
            Command::Enter(e) => self.predict_mode(grid, Some(e), cause, now),
            Command::Leave => self.predict_mode(grid, None, cause, now),
            _ => self.predict_scroll(grid, command, cause, now),
        }
    }

    /// Whether the drawing state allows moving the cursor ahead in `grid`:
    /// multigrid, flushed, not holding, and `grid` a visible window with the
    /// cursor.
    fn steps_in(&self, grid: u64, now: Instant) -> bool {
        self.multigrid()
            && self.flushed
            && !self.holding(now)
            && self.model.cursor.grid == grid
            && matches!(
                self.model.layout.get(&grid),
                Some(Placement {
                    place: Place::Window { .. },
                    hidden: false
                })
            )
    }

    /// Draws a Normal-mode cursor motion (by screen rows if `screen`, see
    /// [`predict::Idiom`]) at once when the client knows exactly where
    /// Neovim will put the cursor ([`anim::chain`]), scrolling the view past
    /// `'scrolloff'` as Neovim would ([`moves::keep_in_view`],
    /// [`App::scroll_ahead`]). Returns whether it was made. While the buffer
    /// proves itself ([`predict::Trust`]) it is only judged, not drawn.
    fn predict_move(
        &mut self,
        grid: u64,
        (motion, count, screen): (moves::Motion, Option<i64>, bool),
        cause: Cause,
        now: Instant,
    ) -> bool {
        if count.is_some_and(|n| n >= predict::MAX_COUNT) {
            return false;
        }
        let Some(on) = self.onward(grid, now) else {
            return false;
        };
        let Some((p, ack, shape, stepping)) = self.stepped_by(on.win) else {
            return false;
        };
        let (top, bot, left, right) = on.rect;
        let (h, width) = (bot - top, right - left);
        let (from, head) = (on.from, on.head.as_ref());
        // The line as the steps typed it: the client's lines may already
        // hold edits from a key Neovim has not acked, and Neovim's rows are
        // not yet updated.
        let echo = head.and_then(|s| s.echo.as_ref());
        let texts = on.text(p, stepping);
        let text = |l: i64| texts(l);
        let cx = moves::Ctx {
            shape,
            line_count: ack.line_count,
            rows: h as i64,
            width,
            text: &text,
            whichwrap: Some(&stepping.whichwrap),
            scrolling: Some(stepping.scrolling()),
            keyword: Some(&stepping.classes),
            lmap: Some(stepping.lmap),
            macros: stepping.macros(),
        };
        let Some(to) = moves::moved(motion, count, &from, &cx) else {
            return false;
        };
        let Some(line) = text(to.line) else {
            return false;
        };
        // On one-row lines, screen-row motion equals line motion, except
        // that with 'wrap' Neovim's `nv_screengo` lands differently when
        // curswant is past the row's end (pulled back into the row) or in its
        // right half on a tab short of its end (lands before the tab). It
        // scrolls as `j` and `k` do.
        let text_width = width.saturating_sub(shape.textoff);
        if let (true, true, Want::Col(c)) = (screen, shape.wrap, from.want) {
            let sits = moves::drawn(line, shape, to.byte);
            if c >= text_width || (to.byte > 0 && sits > c && c > text_width / 2) {
                return false;
            }
        }
        // Lines left and reached (and passed, for motions that wrap across
        // lines), and lines scrolled in, must lay out as Neovim's
        // ([`App::laid_out`], [`App::brought`]).
        let passed = matches!(
            motion,
            moves::Motion::Left(_) | moves::Motion::Right(_) | moves::Motion::Word { .. }
        );
        let laid_out = |l| self.laid_out(grid, &on, l, &text);
        let mut lines = from.line.min(to.line)..=from.line.max(to.line);
        let mut shown = to.top..(to.top + h as i64).min(ack.line_count);
        if to.left != from.left
            || !(laid_out(from.line) && laid_out(to.line))
            || (passed && !lines.all(laid_out))
            || (to.top != from.top && !shown.all(|l| self.brought(&on, l, &text)))
            || !self.may_leave(grid, (from.line, to.line))
        {
            return false;
        }
        // A motion that stays on the same character keeps the cursor drawn
        // where it was: Neovim recomputes the draw position only on a move,
        // so on a tab it may still be at the tab's start as another mode
        // (e.g. Visual) left it.
        let still = (to.line, to.byte) == (from.line, from.byte);
        let vcol = match head {
            _ if !still => moves::drawn(line, shape, to.byte),
            Some(s) => s.vcol,
            None => match self.drawn_on(grid, &on) {
                Some(vcol) => vcol,
                None => return false,
            },
        };
        let step = chain::Step {
            seq: cause.seq,
            pos: to,
            vcol,
            proof: Proof::Move,
            credited: !still,
            tick: ack.tick,
            line_count: ack.line_count,
            echo: echo.cloned(),
            insstart: None,
            hidden: p.trust.hidden(ack.buf, Proof::Move) || head.is_some_and(|s| s.hidden),
            until: p.deadline(cause.at),
        };
        let drawn = self.seen(grid, to.line, Some(line));
        drop(texts);
        self.step(grid, (&on, step, drawn), cause, now)
    }

    /// Where the next cursor step in `grid` starts ([`anim::chain`]), if one
    /// may be taken. Nothing may be ahead but the steps (other scrolls ahead
    /// land the cursor themselves). It continues from the newest step, which
    /// must use the same window, gutter and view (a mismatched step is
    /// dropped once heard of), or from the ack if the view is still the
    /// ack's; unless the steps typed, the client's lines must be at the ack's
    /// tick.
    fn onward(&self, grid: u64, now: Instant) -> Option<Onward> {
        let p = self.predict.as_ref().filter(|p| p.acks() && p.known())?;
        if !self.steps_in(grid, now) {
            return None;
        }
        let (win, view, rect) = (
            *self.model.wins.get(&grid)?,
            *self.model.viewports.get(&grid)?,
            anim::text_rect(&self.model, grid)?,
        );
        let ack = &p.base.as_ref()?.ack;
        let shape = p
            .shape(win)
            .filter(|s| s.predictable && s.line_count == view.line_count && s.buf == ack.buf)?;
        shape.stepping.as_ref().filter(|s| s.plain())?;
        let chain = self.anim.chain(grid);
        let ahead = self.anim.ahead(grid);
        let shown = (win, shape.textoff, shape.leftcol);
        if !view.one_to_one(rect.1 - rect.0)
            || self.anim.side_left(grid).is_some()
            || p.trust.refused(win, now)
            || chain.map_or(ahead != (0, 0), |c| !c.views(view.topline + ahead.0))
            || chain
                .is_some_and(|c| (c.win, c.textoff, c.left) != shown || c.len() >= chain::LONGEST)
        {
            return None;
        }
        let head = chain.and_then(|c| c.head()).cloned();
        let from = head.as_ref().map_or(ack.pos, |s| s.pos);
        let echo = head.as_ref().is_some_and(|s| s.echo.is_some());
        let ok = from.win == win
            && from.rest == Rest::Normal
            && from.left == shape.leftcol
            && (head.is_some() || from.top == view.topline)
            && (echo || p.fresh(win, ack.buf, ack.tick));
        ok.then_some(Onward {
            win,
            view,
            rect,
            shown,
            from,
            head,
        })
    }

    /// Whether Neovim lays out line `l` as the client would, for a step to
    /// go onto: true if the steps typed it; in view, judged by Neovim's
    /// drawing and, for the ack's line and blank-ended lines, by its measure,
    /// since blank in-between content is invisible on screen but counted
    /// ([`App::explained`]); out of view, per [`App::brought`].
    fn laid_out<'a>(
        &self,
        grid: u64,
        on: &Onward,
        l: i64,
        text: &dyn Fn(i64) -> Option<&'a str>,
    ) -> bool {
        let Some((t, (_, ack, shape, _))) = text(l).zip(self.stepped_by(on.win)) else {
            return false;
        };
        let echo = on.head.as_ref().and_then(|s| s.echo.as_ref());
        if echo.is_some_and(|e| e.line == l) {
            return true;
        }
        if !on.neovims(l) {
            return self.brought(on, l, text);
        }
        let width = ack.width_of(l);
        let measured = width == Some(predict::width(t, shape));
        (measured || (width.is_none() && l != ack.pos.line))
            && self.explained(grid, l, t, measured) == Some(true)
    }

    /// Whether line `l`, shown by a scroll ahead, is laid out as Neovim
    /// would: in Neovim's view it is Neovim's; out of it, the client must
    /// have it whole with no virtual text or signs
    /// ([`Predictor::decorated`]) and no concealing
    /// ([`predict::Stepping::concealing`]).
    fn brought<'a>(&self, on: &Onward, l: i64, text: &dyn Fn(i64) -> Option<&'a str>) -> bool {
        let Some((p, _, _, stepping)) = self.stepped_by(on.win) else {
            return false;
        };
        on.neovims(l) || (text(l).is_some() && !p.decorated(on.win, l) && !stepping.concealing)
    }

    /// The predictor, last ack, shape and stepping that steps in `win` use.
    fn stepped_by(
        &self,
        win: i64,
    ) -> Option<(
        &Predictor,
        &predict::Ack,
        &predict::Shape,
        &predict::Stepping,
    )> {
        let p = self.predict.as_ref()?;
        let shape = p.shape(win)?;
        Some((p, &p.base.as_ref()?.ack, shape, shape.stepping.as_ref()?))
    }

    /// The cursor's drawn display column, for a step from the ack that stays
    /// on its character: Neovim's, if its frame's cursor matches the ack.
    fn drawn_on(&self, grid: u64, on: &Onward) -> Option<usize> {
        let shape = self.predict.as_ref()?.shape(on.win)?;
        let (v, from) = (&on.view, &on.from);
        if (v.curline, v.curcol) != (from.line, from.byte as i64) {
            return None;
        }
        let c = self.model.cursor;
        let col = c.col.checked_sub(on.rect.2 + shape.textoff)?;
        (c.grid == grid).then_some(col + shape.leftcol)
    }

    /// Whether the cursor may move ahead from line `from` to `to`, as far as
    /// cursor-line drawing goes: the gutter must follow Neovim's rules and
    /// the `'cursorline'` bar must be derivable (`'screenline'` is not
    /// followed). Not with window-local highlights for cursor-line groups,
    /// which the client does not know, nor where the left line may become
    /// concealed. The repaint ([`App::repaint`]) slides with the rows.
    fn may_leave(&self, grid: u64, (from, to): (i64, i64)) -> bool {
        let shape = self
            .model
            .wins
            .get(&grid)
            .and_then(|win| self.predict.as_ref()?.shape(*win));
        let Some(stepping) = shape.and_then(|s| s.stepping.as_ref()) else {
            return false;
        };
        let cul = stepping.cursorline;
        let decided = shape.is_some_and(|s| s.relativenumber) || cul.number || cul.line;
        to == from
            || !(cul.screenline
                || stepping.conceals
                || (decided && stepping.own_highlights)
                || (cul.line && !self.model.bar_derivable())
                || !self.regutters(grid, to))
    }

    /// Whether the cursor-line decoration can be repainted for the cursor on
    /// `to` instead of Neovim's `from` ([`App::repaint`]), including lines
    /// outside Neovim's view: [`App::may_leave`], and where the cursor line
    /// decides anything, [`App::afresh`]. True for a window without stepping
    /// options: nothing is drawn over it, as no motion is made in it.
    fn redrawn_with(&self, grid: u64, (from, to): (i64, i64)) -> bool {
        let shape = self
            .model
            .wins
            .get(&grid)
            .and_then(|win| self.predict.as_ref()?.shape(*win));
        let Some((shape, stepping)) = shape.and_then(|s| Some((s, s.stepping.as_ref()?))) else {
            return true;
        };
        let cul = stepping.cursorline;
        let decided = shape.relativenumber || cul.number || cul.line;
        to == from || (self.may_leave(grid, (from, to)) && (!decided || self.afresh(grid)))
    }

    /// Moves the cursor ahead as `step` says, its target line as `drawn`
    /// ([`chain::Seen`]), scrolling the view ahead too if it moves
    /// ([`App::scroll_ahead`]), unless the step is hidden (judged only).
    /// Returns whether it was made.
    fn step(
        &mut self,
        grid: u64,
        (on, step, drawn): (&Onward, chain::Step, chain::Drawn),
        cause: Cause,
        now: Instant,
    ) -> bool {
        let (from, to) = (on.from, step.pos);
        if to.top != from.top && !step.hidden {
            let at = |p: Pos| predict::At {
                top: p.top,
                cursor: p.line,
            };
            // On a line change, where the scroll lands the cursor
            // ([`anim::Ahead::col`]).
            let col = step
                .vcol
                .checked_sub(to.left)
                .filter(|_| to.line != from.line)
                .map(|c| (on.shown.1 + c, to.want.col()));
            if !self.scroll_ahead(grid, (at(from), at(to)), col, cause, now) {
                return false;
            }
        }
        self.anim.step(
            &self.model,
            grid,
            (on.shown, !self.remaking),
            (step, move || drawn),
            now,
        );
        self.dirty = true;
        true
    }

    /// Scrolls `grid` ahead from view `from` to `to` for a step
    /// ([`App::step`]), drawing new rows from the agent's lines at once
    /// ([`anim::Animator::predict`]) and landing the cursor at `col` (grid
    /// column, curswant). If a line is missing, every scroll ahead and step
    /// in the window is dropped, to be remade from Neovim's next ack.
    /// Returns whether it was made.
    fn scroll_ahead(
        &mut self,
        grid: u64,
        (from, to): (predict::At, predict::At),
        col: Option<(usize, usize)>,
        cause: Cause,
        now: Instant,
    ) -> bool {
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
        let Some(shape) = p.shape(win) else {
            return false;
        };
        let Some(look) = self.look(grid, rect, &view, shape) else {
            return false;
        };
        let width = rect.3 - rect.2;
        let go = anim::Ahead {
            rows: to.top - from.top,
            cursor: to.cursor - from.cursor,
            col,
            land: None,
            until: p.deadline(cause.at),
            scrolloff: shape.scrolloff,
            seq: Some(cause.seq),
        };
        let mut fill = |i: i64| p.row(win, view.topline + i, width, &look, to.cursor);
        let mut land = |_: anim::Land, _: Option<usize>| Some(None);
        // First fill in what an earlier scroll lacked.
        self.anim
            .fill_in(&self.model, grid, (&mut fill, &mut land), now);
        let made = self
            .anim
            .predict(&self.model, grid, go, (&mut fill, &mut land), now)
            && !self.anim.held().contains(&grid);
        if !made {
            self.drop_steps(grid, now);
        }
        made
    }

    /// What a step onto `line` records ([`chain::Seen`]): Neovim's row past
    /// the gutter, or out of view, `text` as the client lays it out; `None`
    /// if it cannot.
    fn seen(&self, grid: u64, line: i64, text: Option<&str>) -> chain::Drawn {
        let shape = self.predict.as_ref()?.shape(*self.model.wins.get(&grid)?)?;
        if let Some(text) = anim::line_drawn(&self.model, grid, line, shape.textoff) {
            return Some(chain::Seen { text, whole: true });
        }
        let (_, _, left, right) = anim::text_rect(&self.model, grid)?;
        let text = text?;
        let look = predict::Look::default();
        let row = predict::render(Some((text, &[])), shape, line, right - left, &look, line)?;
        let from = shape.textoff.min(row.len());
        let end = (shape.textoff + predict::reach(text, shape).saturating_sub(shape.leftcol))
            .clamp(from, row.len());
        Some(chain::Seen {
            text: row[from..end].iter().map(|c| c.text.clone()).collect(),
            whole: false,
        })
    }

    /// Draws an Insert-mode edit `e` of the cursor line at once: Neovim's row
    /// re-laid around the new text, and the cursor after it ([`typing`]).
    /// Returns whether it was made. Refused where Insert-mode facts bear on
    /// the key ([`typing::refused`]), on lines with pinned virtual text
    /// ([`Predictor::pinned`]) or a centred tail past the grid ([`wide`]), and
    /// where the result does not fit the row ([`typing::relayout`]). While
    /// the buffer proves itself it is only judged ([`predict::Trust`]).
    fn predict_echo(&mut self, grid: u64, e: typing::Edit, cause: Cause, now: Instant) -> bool {
        let Some(p) = self.predict.as_ref().filter(|p| p.typing() && p.known()) else {
            return false;
        };
        if !self.steps_in(grid, now) {
            return false;
        }
        let (Some(&win), Some(&view), Some(rect), Some(base)) = (
            self.model.wins.get(&grid),
            self.model.viewports.get(&grid),
            anim::text_rect(&self.model, grid),
            p.base.as_ref(),
        ) else {
            return false;
        };
        let (top, bot, left, right) = rect;
        let ack = &base.ack;
        let Some(shape) = p
            .shape(win)
            .filter(|s| s.predictable && s.line_count == view.line_count && s.buf == ack.buf)
        else {
            return false;
        };
        let Some(stepping) = shape.stepping.as_ref().filter(|s| s.plain()) else {
            return false;
        };
        // Not over gliding rows ([`anim::Animator::gliding`]): the typed
        // line, drawn over its row, would not slide with them.
        if !view.one_to_one(bot - top)
            || self.anim.ahead(grid) != (0, 0)
            || self.anim.side_left(grid).is_some()
            || self.anim.gliding(grid)
            || p.trust.refused(win, now)
        {
            return false;
        }
        let shown = (win, shape.textoff, shape.leftcol);
        let chain = self.anim.chain(grid);
        if chain.is_some_and(|c| (c.win, c.textoff, c.left) != shown || c.len() >= chain::LONGEST) {
            return false;
        }
        let head = chain.and_then(|c| c.head());
        let from = head.map_or(ack.pos, |s| s.pos);
        let row = usize::try_from(from.line - view.topline)
            .ok()
            .filter(|r| *r < bot - top)
            .map(|r| top + r);
        let (Some(row), Some(facts)) = (row, p.facts(ack.buf)) else {
            return false;
        };
        if from.win != win
            || p.pinned(win, from.line)
            || from.rest != Rest::Insert
            || from.left != shape.leftcol
            || from.top != view.topline
            || self
                .model
                .tails
                .get(&grid)
                .is_some_and(|t| t.contains_key(&row))
        {
            return false;
        }
        let key = p
            .queue
            .iter()
            .find(|s| s.seq == cause.seq)
            .and_then(|s| s.input.key());
        // The line as the previous steps typed it, or as of the ack if the
        // client's lines are still at its tick, whole and steppable.
        let echo = head.and_then(|s| s.echo.as_ref());
        let text = match echo {
            Some(echo) if echo.line == from.line => Some(&*echo.text),
            Some(_) => None,
            None => (p.fresh(win, ack.buf, ack.tick) && !p.cut(win, from.line))
                .then(|| p.text(win, from.line))
                .flatten(),
        };
        let ordinary = |t: &str| predict::ordinary(t) && (!stepping.widths || t.is_ascii());
        let (Some(key), Some(text)) = (key, text.filter(|t| ordinary(t))) else {
            return false;
        };
        let vcol = typing::sits(text, shape, from.byte);
        if let Some(why) = typing::refused(key, e, (text, from.byte, vcol), facts, shape) {
            tracing::debug!(key, why, "client: a key typed left to Neovim");
            return false;
        }
        // Where Insert mode began, where `<C-w>` and `<C-u>` stop, from the
        // previous step or the ack; if unknown they are left to Neovim.
        let insstart = head.map_or(ack.insstart, |s| s.insstart);
        let rubs = matches!(e, typing::Edit::Word | typing::Edit::Line);
        if rubs && insstart.is_none() && !facts.nostop() {
            tracing::debug!(
                key,
                "client: a key typed left to Neovim, not knowing where it stops"
            );
            return false;
        }
        let stop = insstart
            .filter(|s| s.line == from.line && !facts.nostop())
            .map(|s| s.byte);
        let edit = match e {
            typing::Edit::Word | typing::Edit::Line => {
                let keeps = facts.autoindent || facts.cindent;
                typing::rubbed(e, (text, from.byte), stop, &facts.classes, keeps)
            }
            _ => typing::edited(e, text, from.byte),
        };
        let Some((new, byte)) = edit.filter(|(t, _)| ordinary(t)) else {
            return false;
        };
        // The row as the previous steps laid it out, or Neovim's if it
        // draws and measures the line as the client would.
        let cells = match echo {
            Some(echo) => echo.cells.clone(),
            None => {
                let measured = ack.width_of(from.line) == Some(predict::width(text, shape));
                let drawn = self.model.grids.get(&grid).map(|g| g.row(row));
                let past = drawn.and_then(|r| r.get((left + shape.textoff).min(right)..right));
                match past {
                    Some(cells)
                        if measured
                            && self.explained(grid, from.line, text, true) == Some(true) =>
                    {
                        cells.to_vec()
                    }
                    _ => return false,
                }
            }
        };
        // New `'list'` glyphs take their group, laid over the `'cursorline'`
        // bar where there is one; not predicted if the bar cannot be derived
        // ([`model::Model::bar_derivable`]).
        let cul = stepping.cursorline;
        let bar = cul.line || cul.screenline;
        if bar && shape.list && !self.model.bar_derivable() {
            return false;
        }
        let mut glyph = |name: &str| {
            let hl = self.model.groups.get(name).copied()?;
            if bar {
                self.model.derived(hl, model::Derive::CursorLineOn)
            } else {
                Some(hl)
            }
        };
        let (whitespace, nontext, specialkey) =
            (glyph("Whitespace"), glyph("NonText"), glyph("SpecialKey"));
        let groups = |name: &str| match name {
            "Whitespace" => whitespace,
            "NonText" => nontext,
            "SpecialKey" => specialkey,
            _ => None,
        };
        let look = predict::Look::of(std::slice::from_ref(&cells), 1, 0, None, groups);
        let to = typing::sits(&new, shape, byte);
        let (at, gone, put) = match e {
            typing::Edit::Char(c) => (from.byte, 0, c.len_utf8()),
            typing::Edit::Rub | typing::Edit::Word | typing::Edit::Line => {
                (byte, from.byte - byte, 0)
            }
        };
        let laid = typing::relayout(
            &cells,
            (text, &new),
            (at, gone, put),
            to,
            (shape, &look, facts),
        );
        let Some(laid) = laid else {
            return false;
        };
        let proof = match e {
            typing::Edit::Char(c) => Proof::Char(c),
            typing::Edit::Rub | typing::Edit::Word | typing::Edit::Line => Proof::Rub,
        };
        let hidden = p.trust.hidden(ack.buf, proof)
            || head.is_some_and(|s| s.hidden)
            || (facts.unseen && !p.trust.proven(ack.buf, Unseen::Typed));
        let step = chain::Step {
            seq: cause.seq,
            pos: predict::Pos {
                byte,
                want: Want::Col(to),
                ..from
            },
            vcol: to,
            proof,
            credited: !matches!(e, typing::Edit::Char(c) if c.is_whitespace()),
            tick: ack.tick,
            line_count: ack.line_count,
            echo: Some(chain::Echo {
                line: from.line,
                text: new.into(),
                cells: laid,
            }),
            insstart: insstart.map(|s| match e {
                typing::Edit::Char(_) => s,
                typing::Edit::Rub => s.rubbed((from.line, byte), false),
                typing::Edit::Word => s.rubbed((from.line, byte), facts.word_undo),
                typing::Edit::Line => s.rubbed((from.line, byte), facts.line_undo),
            }),
            hidden,
            until: p.deadline(cause.at),
        };
        let drawn = self.seen(grid, step.pos.line, None);
        self.anim.step(
            &self.model,
            grid,
            (shown, !self.remaking),
            (step, move || drawn),
            now,
        );
        self.dirty = true;
        true
    }

    /// Draws `i`, `a`, `A` or `I` entering Insert mode (`enter`), or `<Esc>`
    /// leaving it (`None`), at once: the cursor moved and reshaped for the
    /// new mode ([`typing::entered`], [`typing::left`]). Returns whether it
    /// was made. `-- INSERT --` is left to Neovim. Entering needs an
    /// editable buffer ([`typing::Facts::editable`]); leaving needs `<Esc>`
    /// to do no more than step back ([`typing::unleft`]).
    fn predict_mode(
        &mut self,
        grid: u64,
        enter: Option<typing::Enter>,
        cause: Cause,
        now: Instant,
    ) -> bool {
        let Some(p) = self.predict.as_ref().filter(|p| p.typing() && p.known()) else {
            return false;
        };
        if !self.steps_in(grid, now) {
            return false;
        }
        let (Some(&win), Some(&view), Some(rect), Some(base)) = (
            self.model.wins.get(&grid),
            self.model.viewports.get(&grid),
            anim::text_rect(&self.model, grid),
            p.base.as_ref(),
        ) else {
            return false;
        };
        let (top, bot, left, right) = rect;
        let ack = &base.ack;
        let Some(shape) = p
            .shape(win)
            .filter(|s| s.predictable && s.line_count == view.line_count && s.buf == ack.buf)
        else {
            return false;
        };
        let (Some(stepping), Some(facts)) = (
            shape.stepping.as_ref().filter(|s| s.plain()),
            p.facts(ack.buf),
        ) else {
            return false;
        };
        if !view.one_to_one(bot - top)
            || self.anim.ahead(grid) != (0, 0)
            || self.anim.side_left(grid).is_some()
            || p.trust.refused(win, now)
            || facts.conceals.contains(['n', 'i'])
        {
            return false;
        }
        let shown = (win, shape.textoff, shape.leftcol);
        let chain = self.anim.chain(grid);
        if chain.is_some_and(|c| (c.win, c.textoff, c.left) != shown || c.len() >= chain::LONGEST) {
            return false;
        }
        let head = chain.and_then(|c| c.head());
        let from = head.map_or(ack.pos, |s| s.pos);
        let rest = if enter.is_some() {
            Rest::Normal
        } else {
            Rest::Insert
        };
        let in_view = (0..(bot - top) as i64).contains(&(from.line - view.topline));
        if from.win != win
            || from.rest != rest
            || from.left != shape.leftcol
            || from.top != view.topline
            || !in_view
        {
            return false;
        }
        // The line as the steps typed it, or as of the ack if current and
        // drawn and measured as the client would.
        let echo = head.and_then(|s| s.echo.as_ref());
        let ordinary = |t: &str| predict::ordinary(t) && (!stepping.widths || t.is_ascii());
        let text = match echo.filter(|e| e.line == from.line) {
            Some(e) => Some(&*e.text).filter(|t| ordinary(t)),
            None => {
                let width = ack.width_of(from.line);
                let text = (p.fresh(win, ack.buf, ack.tick) && !p.cut(win, from.line))
                    .then(|| p.text(win, from.line))
                    .flatten()
                    .filter(|t| ordinary(t));
                text.filter(|t| {
                    let measured = width == Some(predict::width(t, shape));
                    (measured || (width.is_none() && from.line != ack.pos.line))
                        && self.explained(grid, from.line, t, measured) == Some(true)
                })
            }
        };
        // Not over gliding rows when drawing a typed line (see
        // [`App::predict_echo`]).
        let Some(text) = text.filter(|_| echo.is_none() || !self.anim.gliding(grid)) else {
            return false;
        };
        let (byte, vcol, proof) = match enter {
            Some(e) => {
                if !facts.editable {
                    return false;
                }
                let (byte, vcol) = typing::entered(e, text, shape, from.byte);
                // At the line's end the cursor may go after pinned virtual
                // text ([`Predictor::pinned`]).
                if byte == text.len() && p.pinned(win, from.line) {
                    return false;
                }
                (byte, vcol, Proof::Enter)
            }
            None => {
                if let Some(why) = typing::unleft(text, from.byte, facts) {
                    tracing::debug!(why, "client: <Esc> left to Neovim");
                    return false;
                }
                let (byte, vcol) = typing::left(text, shape, from.byte);
                (byte, vcol, Proof::Leave)
            }
        };
        // A plugin autocommand on entering or leaving may move the cursor or
        // edit (e.g. a trailing-blank trimmer whose `:%s` moves the cursor),
        // so such steps, and the keys after, are only judged until enough
        // prove right. Leaving counts too when the facts predate the first
        // InsertEnter, since a plugin loaded then may add its own.
        let what = match proof {
            Proof::Enter => Unseen::Entered,
            _ => Unseen::Left,
        };
        let unseen = self.unseen(ack.buf, proof) && !p.trust.proven(ack.buf, what);
        // The cursor must stay on the line's row: with 'wrap', not past a
        // full row; without, within the current view.
        let width = right - left;
        let room = width.saturating_sub(shape.textoff);
        if (shape.wrap && (vcol >= room || predict::reach(text, shape) > room))
            || predict::showing(text, shape, vcol, shape.leftcol, width) != shape.leftcol
        {
            return false;
        }
        let step = chain::Step {
            seq: cause.seq,
            pos: predict::Pos {
                byte,
                want: Want::Col(vcol),
                rest: if enter.is_some() {
                    Rest::Insert
                } else {
                    Rest::Normal
                },
                ..from
            },
            vcol,
            proof,
            credited: true,
            tick: ack.tick,
            line_count: ack.line_count,
            echo: echo.cloned(),
            insstart: enter.map(|_| predict::Stop {
                line: from.line,
                byte,
                sticks: false,
            }),
            hidden: unseen || p.trust.hidden(ack.buf, proof) || head.is_some_and(|s| s.hidden),
            until: p.deadline(cause.at),
        };
        let drawn = self.seen(grid, step.pos.line, None);
        self.anim.step(
            &self.model,
            grid,
            (shown, !self.remaking),
            (step, move || drawn),
            now,
        );
        self.dirty = true;
        true
    }

    /// Before every frame, patches each window with predicted cursor steps
    /// ([`anim::chain`]) and, with acks, each scrolled ahead or sliding, with
    /// what Neovim will draw besides the cursor: the typed line; for a
    /// cursor on another line, the gutter ([`predict::regutter`]) and the
    /// `'cursorline'` bar, re-derived from Neovim's last frame (rows it
    /// cannot derive stay as drawn); and rows outside Neovim's view, drawn
    /// afresh.
    fn repaint(&mut self) {
        let mut grids = self.anim.chained();
        if self.predict.as_ref().is_some_and(Predictor::acks) {
            grids.extend(self.anim.scrolled());
            grids.sort_unstable();
            grids.dedup();
        }
        for grid in self.anim.patched() {
            if !grids.contains(&grid) {
                self.anim.patch(grid, BTreeMap::new());
            }
        }
        for grid in grids {
            let patches = self.patches_of(grid).unwrap_or_default();
            self.anim.patch(grid, patches);
        }
    }

    /// [`App::repaint`]'s patches for `grid`, by grid row. Rows Neovim drew
    /// (including ones the scroll keeps, [`anim::scroll::Kept`]) get the
    /// typed line, the gutter re-derived for the drawn cursor line, and the
    /// moved `'cursorline'` bar ([`model::Derive`]); rows the client brought
    /// into view are drawn afresh ([`drawn_afresh`]). Cursor-line drawing is
    /// skipped where [`App::redrawn_with`] refuses.
    fn patches_of(&mut self, grid: u64) -> Option<BTreeMap<usize, Vec<compose::Patch>>> {
        let view = *self.model.viewports.get(&grid)?;
        let win = *self.model.wins.get(&grid)?;
        let rect = anim::text_rect(&self.model, grid)?;
        let (top, bot, left, right) = rect;
        let (h, width) = (bot - top, right - left);
        let to = self.anim.cursor_line(&self.model, grid, h as i64)?;
        let echo = self
            .anim
            .shown_step(&self.model, grid)
            .and_then(|s| s.echo.clone());
        let redrawn = self.redrawn_with(grid, (view.curline, to));
        let ids = predict::Gutter::of(|name| self.model.groups.get(name).copied());
        let p = self.predict.as_ref()?;
        let shape = p.shape(win)?;
        let textoff = shape.textoff;
        let text = (left + textoff).min(right);
        let cul = shape.stepping.as_ref().map(|s| s.cursorline);
        let bars = redrawn && cul.is_some_and(|c| c.line && !c.screenline);
        let decided = redrawn && (shape.relativenumber || cul.is_some_and(|c| c.number || c.line));
        let ahead = self.anim.rows_shown(grid);
        // Shared look for the rows drawn afresh, built only where there can
        // be any: rows shown ahead, in a window whose cursor line decides how
        // they are drawn.
        let afresh = (decided && ahead != 0 && self.afresh(grid))
            .then(|| self.look(grid, rect, &view, shape).zip(ids))
            .flatten();
        let mut patches: BTreeMap<usize, Vec<compose::Patch>> = BTreeMap::new();
        for r in 0..h {
            let i = r as i64 + ahead;
            let line = view.topline + i;
            if !(0..view.line_count).contains(&line) {
                continue;
            }
            let row = top + r;
            // Neovim's cells for the line from the window's first column, and
            // its cursor line when drawn; or the client's.
            let own = usize::try_from(i).ok().filter(|i| *i < h);
            let (drawn, from): (&[Cell], i64) = match own {
                Some(i) => {
                    let g = self.model.grids.get(&grid)?;
                    let Some(cells) = g.row(top + i).get(left..right) else {
                        continue;
                    };
                    (cells, view.curline)
                }
                None => match self.anim.kept(grid, i) {
                    Some(anim::scroll::Kept {
                        cells,
                        by: anim::scroll::Drawer::Neovim(Some(c)),
                    }) => (cells.as_slice(), *c),
                    Some(anim::scroll::Kept {
                        by: anim::scroll::Drawer::Client,
                        ..
                    }) => {
                        let made = afresh.as_ref().filter(|_| decided).and_then(|(look, ids)| {
                            drawn_afresh(
                                p,
                                &mut self.model,
                                win,
                                (line, to),
                                (h as i64, width),
                                (look, ids),
                            )
                        });
                        if let Some(cells) = made {
                            let patch = compose::Patch { col: left, cells };
                            patches.entry(row).or_default().push(patch);
                        }
                        continue;
                    }
                    _ => continue,
                },
            };
            let typed = own.and(echo.as_ref()).filter(|e| e.line == line);
            let barred = bars && from != to && (line == from || line == to);
            let gutter = (decided && from != to)
                .then(|| {
                    let drawn = drawn.get(..textoff)?;
                    let now = predict::regutter(drawn, line, (from, to), shape, h as i64, &ids?)?;
                    (now != drawn).then_some(now)
                })
                .flatten();
            let drawn = barred.then(|| match typed {
                Some(e) => e.cells.clone(),
                None => drawn
                    .get(textoff.min(drawn.len())..)
                    .unwrap_or_default()
                    .to_vec(),
            });
            if let Some(cells) = gutter {
                let patch = compose::Patch { col: left, cells };
                patches.entry(row).or_default().push(patch);
            }
            if let Some(e) = typed.filter(|_| !barred) {
                let patch = compose::Patch {
                    col: text,
                    cells: e.cells.clone(),
                };
                patches.entry(row).or_default().push(patch);
            }
            let Some(drawn) = drawn else {
                continue;
            };
            let d = if line == to {
                model::Derive::CursorLineOn
            } else {
                model::Derive::CursorLineOff
            };
            let made: Option<Vec<Cell>> = drawn
                .into_iter()
                .map(|cell| {
                    let hl = self.model.derived(cell.hl, d)?;
                    Some(Cell { hl, ..cell })
                })
                .collect();
            if let Some(cells) = made {
                let patch = compose::Patch { col: text, cells };
                patches.entry(row).or_default().push(patch);
            }
        }
        Some(patches)
    }

    /// Whether rows of `grid` can be drawn afresh with cursor-line drawing
    /// as Neovim's ([`drawn_afresh`]): a plain gutter in global groups (no
    /// window highlights, no signs over numbers), all groups known, and any
    /// `'cursorline'` bar line-wide and derivable.
    fn afresh(&self, grid: u64) -> bool {
        let shape = self
            .model
            .wins
            .get(&grid)
            .and_then(|win| self.predict.as_ref()?.shape(*win));
        let Some((shape, stepping)) = shape.and_then(|s| Some((s, s.stepping.as_ref()?))) else {
            return false;
        };
        let cul = stepping.cursorline;
        shape.plain_gutter
            && !stepping.number_signs
            && !stepping.own_highlights
            && !cul.screenline
            && (!cul.line || self.model.bar_derivable())
            && predict::Gutter::of(|name| self.model.groups.get(name).copied()).is_some()
    }

    /// Whether `grid`'s gutter can be redrawn for the cursor on `to`
    /// ([`predict::regutter`]): Neovim's own, not `'statuscolumn'` (which can
    /// draw anything), and where the cursor line matters, every row derivable
    /// and no signs over numbers.
    fn regutters(&self, grid: u64, to: i64) -> bool {
        let shape = self
            .model
            .wins
            .get(&grid)
            .and_then(|win| self.predict.as_ref()?.shape(*win));
        let Some((shape, stepping)) = shape.and_then(|s| Some((s, s.stepping.as_ref()?))) else {
            return false;
        };
        if !shape.relativenumber && !stepping.cursorline.number {
            return shape.plain_gutter;
        }
        shape.plain_gutter && !stepping.number_signs && self.gutters(grid, to) == Some(true)
    }

    /// Whether every gutter row showing a line can be redrawn for the cursor
    /// on `to` ([`predict::regutter`]). `None` without a shape or groups, or
    /// with `'statuscolumn'`.
    fn gutters(&self, grid: u64, to: i64) -> Option<bool> {
        let (win, view) = (
            self.model.wins.get(&grid)?,
            self.model.viewports.get(&grid)?,
        );
        let shape = self.predict.as_ref()?.shape(*win)?;
        let ids = predict::Gutter::of(|name| self.model.groups.get(name).copied())?;
        let (top, bot, left, _) = anim::text_rect(&self.model, grid)?;
        let g = self.model.grids.get(&grid)?;
        if !shape.plain_gutter {
            return None;
        }
        let h = bot - top;
        let cursor = (view.curline, to);
        let all = (0..h)
            .map(|r| (r, view.topline + r as i64))
            .take_while(|&(_, lnum)| lnum < view.line_count)
            .all(|(r, lnum)| {
                g.row(top + r)
                    .get(left..left + shape.textoff)
                    .is_none_or(|drawn| {
                        predict::regutter(drawn, lnum, cursor, shape, h as i64, &ids).is_some()
                    })
            });
        Some(all)
    }

    /// Whether Neovim drew `line` as the client would lay out `text`: the
    /// text cells from the gutter to the line's or window's end match. A
    /// mismatch means an unheard edit, or virtual text, concealing or a
    /// width the client does not know. `None` if not in view or too long for
    /// a wrapping window. Only text is compared, not colours.
    ///
    /// Blank virtual text among blanks is invisible, so unless `measured`
    /// (Neovim's width agrees, [`predict::Ack::blanks`]) a line whose last
    /// cell is blank does not count. An empty line does.
    fn explained(&self, grid: u64, line: i64, text: &str, measured: bool) -> Option<bool> {
        let p = self.predict.as_ref()?;
        let win = *self.model.wins.get(&grid)?;
        let view = self.model.viewports.get(&grid)?;
        let shape = p.shape(win)?;
        let (top, bot, left, right) = anim::text_rect(&self.model, grid)?;
        let r = usize::try_from(line - view.topline)
            .ok()
            .filter(|r| *r < bot - top)?;
        let width = right - left;
        let look = predict::Look::default();
        let ours = predict::render(Some((text, &[])), shape, line, width, &look, line)?;
        let theirs = self.model.grids.get(&grid)?.row(top + r).get(left..right)?;
        let from = shape.textoff.min(width);
        let end = (shape.textoff + predict::reach(text, shape).saturating_sub(shape.leftcol))
            .clamp(from, width);
        let blank = ours[from..end]
            .last()
            .is_some_and(|c| c.text == Text::Char(' '));
        Some(
            (measured || !blank)
                && ours[from..end]
                    .iter()
                    .zip(&theirs[from..end])
                    .all(|(a, b)| a.text == b.text),
        )
    }

    fn predict_scroll(&mut self, grid: u64, command: Command, cause: Cause, now: Instant) -> bool {
        let (Some(&win), Some(rect)) = (
            self.model.wins.get(&grid),
            anim::text_rect(&self.model, grid),
        ) else {
            return false;
        };
        let (top, bot, left, right) = rect;
        let h = (bot - top) as i64;
        // A count sets 'scroll' in Neovim, whether predicted or not.
        if let (Command::Half { count: Some(n), .. }, Some(p)) = (command, self.predict.as_mut()) {
            p.set_scroll(win, n.min(h), cause.seq);
        }
        if self.predict.is_none() || !self.multigrid() || !self.flushed || self.holding(now) {
            return false;
        }
        // Paging back from a view whose top is the buffer's last line or the
        // one before keeps fewer lines (Neovim's `get_scroll_overlap` runs
        // out of lines), which [`predict::outcome`] does not model. With
        // acks judging, it is left to Neovim; under "scrolls" it is drawn.
        if let (Command::Page { down: false, .. }, Some(view), true) = (
            command,
            self.model.viewports.get(&grid),
            self.predict.as_ref().is_some_and(Predictor::acks),
        ) {
            let top = match self.anim.chain(grid).and_then(chain::Chain::head) {
                Some(s) => s.pos.top,
                None => view.topline + self.anim.ahead(grid).0,
            };
            if top > 0 && top >= view.line_count - 2 {
                return false;
            }
        }
        // With acks, a Normal-mode scroll key, or a wheel turn over cursor
        // steps, is itself a step.
        if let Some(made) = self.scroll_step(grid, command, cause, now) {
            return made;
        }
        // A plain scroll is computed from Neovim's cursor, which steps have
        // moved, so it waits for their ack.
        if self.anim.chain(grid).is_some() {
            return false;
        }
        let Some(p) = self.predict.as_ref() else {
            return false;
        };
        // Keys and page wheel in Normal/Visual; line wheel in Insert too.
        let mode = self.model.mode_name.as_str();
        let normal = matches!(mode, "normal" | "visual");
        let insert = matches!(mode, "insert" | "replace");
        let ok = match cause.by {
            By::Keys => normal && sure(p),
            By::InsertKeys => insert && sure(p),
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
        // From the view and cursor after earlier scrolls ahead.
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
        // Only the cursor moves: left to Neovim.
        if to.top == at.top {
            return false;
        }
        let width = right - left;
        // If the cursor changes line or goes to the first non-blank, its
        // column comes from the line's text once earlier scrolls are drawn.
        let first = match command {
            Command::Line { first, .. } => first,
            Command::Half { .. } | Command::Page { .. } => shape.startofline,
            Command::Lines(_)
            | Command::Screen(_)
            | Command::Side(_)
            | Command::Move { .. }
            | Command::Type(_)
            | Command::Enter(_)
            | Command::Leave => false,
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
            until: p.deadline(cause.at),
            scrolloff: shape.scrolloff,
            seq: p.acks().then_some(cause.seq),
        };
        let mut fill = |i: i64| p.row(win, view.topline + i, width, &look, to.cursor);
        let wants = self.wants(win, shape);
        // If the cursor lands out of view or too near an edge, Neovim scrolls
        // sideways to show it. That is drawn too (see `side_rows`) when this
        // is the only scroll ahead and all its lines are known; otherwise the
        // landing is left to Neovim.
        let alone = !self.anim.scrolled_ahead(grid);
        let shown = to.top..to.top + (view.line_count - to.top).clamp(0, h);
        let mut aside = None;
        let mut land = |l: anim::Land, want: Option<usize>| {
            let at = landed(p, win, shape, l, want.unwrap_or(wants), width)?;
            let left = match at.left {
                Some(left) if left == shape.leftcol => left,
                Some(left)
                    if alone && drawable(p, win, shape, shown.clone(), l.line, left, width) =>
                {
                    aside = Some((left, at.vcol));
                    left
                }
                _ => return Some(None),
            };
            Some(at.col(left, shape, width).map(|col| (col, at.want)))
        };
        let made = self
            .anim
            .predict(&self.model, grid, go, (&mut fill, &mut land), now);
        if !made {
            return false;
        }
        self.dirty = true;
        if let Some((left, vcol)) = aside {
            let cursor = (
                top + (to.cursor - to.top) as usize,
                shape.textoff + vcol - left,
            );
            let until = p.deadline(cause.at);
            let Some(rows) = self.side_rows(grid, left) else {
                self.give_up(grid);
                return false;
            };
            // Rows moved sideways do not also slide.
            self.anim.settle(grid);
            self.anim
                .side(&self.model, grid, rows, (left, until), Some(cursor), now);
        }
        true
    }

    /// With acks, makes a Normal-mode scroll key a cursor step
    /// ([`anim::chain`]), scrolling as Neovim would ([`predict::outcome`])
    /// and landing as a vertical motion does ([`moves::onto`]), so motions
    /// and scrolls chain. Returns whether it was made, or `None` to make a
    /// plain scroll instead ([`App::predict_scroll`], [`App::alone`]).
    ///
    /// A wheel turn over cursor steps is a step too: a plain scroll from
    /// Neovim's view would undo the steps' scrolls.
    fn scroll_step(
        &mut self,
        grid: u64,
        command: Command,
        cause: Cause,
        now: Instant,
    ) -> Option<bool> {
        let stepped = match cause.by {
            By::Keys => true,
            By::Wheel | By::PageWheel => self.anim.chain(grid).is_some(),
            By::InsertKeys => false,
        };
        if !stepped || !self.predict.as_ref().is_some_and(Predictor::acks) {
            return None;
        }
        match self.scrolled_step(grid, command, cause, now) {
            Some((on, step, drawn)) => Some(self.step(grid, (&on, step, drawn), cause, now)),
            None => self.alone(grid, now),
        }
    }

    /// The step for [`App::scroll_step`], if possible: [`App::onward`] holds,
    /// the target line is laid out as Neovim does ([`App::laid_out`]),
    /// [`App::may_leave`] allows it, and every line in the new view is
    /// Neovim's or fits one row and passes [`App::brought`]. Without a
    /// previous step, hidden or no-op steps are left to a plain scroll.
    fn scrolled_step(
        &self,
        grid: u64,
        command: Command,
        cause: Cause,
        now: Instant,
    ) -> Option<(Onward, chain::Step, chain::Drawn)> {
        let on = self.onward(grid, now)?;
        let (p, ack, shape, stepping) = self.stepped_by(on.win)?;
        let (top, bot, left, right) = on.rect;
        let (h, width) = ((bot - top) as i64, right - left);
        let (from, head) = (on.from, on.head.as_ref());
        let hidden = p.trust.hidden(ack.buf, Proof::Move) || head.is_some_and(|s| s.hidden);
        let at = predict::At {
            top: from.top,
            cursor: from.line,
        };
        let to = predict::outcome(command, at, h, ack.line_count, shape)?;
        if head.is_none() && (hidden || to == at) {
            return None;
        }
        let how = match command {
            Command::Line { first: true, .. } => moves::Landing::First,
            Command::Line { line: Some(_), .. } => moves::Landing::Byte,
            Command::Half { .. } | Command::Page { .. } if shape.startofline => {
                moves::Landing::First
            }
            _ => moves::Landing::Want,
        };
        let texts = on.text(p, stepping);
        let text = |l: i64| texts(l);
        let cx = moves::Ctx {
            shape,
            line_count: ack.line_count,
            rows: h,
            width,
            text: &text,
            whichwrap: Some(&stepping.whichwrap),
            scrolling: Some(stepping.scrolling()),
            keyword: Some(&stepping.classes),
            lmap: Some(stepping.lmap),
            macros: stepping.macros(),
        };
        let pos = if how == moves::Landing::First || to.cursor != at.cursor {
            moves::onto(to.cursor, how, to.top, &from, &cx)?
        } else {
            Pos {
                top: to.top,
                ..from
            }
        };
        let line = text(pos.line).filter(|_| pos.left == from.left)?;
        let fits = |l: i64| text(l).is_some_and(|t| predict::fits(t, shape, width));
        let mut shown = to.top..(to.top + h).min(ack.line_count);
        let still = (pos.line, pos.byte) == (from.line, from.byte);
        if !shown.all(|l| on.neovims(l) || (fits(l) && self.brought(&on, l, &text)))
            || (!still && !self.laid_out(grid, &on, pos.line, &text))
            || !self.may_leave(grid, (from.line, pos.line))
        {
            return None;
        }
        let vcol = match head {
            _ if !still => moves::drawn(line, shape, pos.byte),
            Some(s) => s.vcol,
            None => self.drawn_on(grid, &on)?,
        };
        let step = chain::Step {
            seq: cause.seq,
            pos,
            vcol,
            proof: Proof::Move,
            credited: !still,
            tick: ack.tick,
            line_count: ack.line_count,
            echo: head.and_then(|s| s.echo.clone()),
            insstart: None,
            hidden,
            until: p.deadline(cause.at),
        };
        let drawn = self.seen(grid, pos.line, Some(line));
        drop(texts);
        Some((on, step, drawn))
    }

    /// For a scroll key that cannot be a step: `None` (make a plain scroll)
    /// if only scrolls are ahead, i.e. no steps or only scrolling ones
    /// ([`anim::Animator::scrolls_alone`]), which are unchained and kept as
    /// scrolls; `Some(false)` (leave to Neovim) if a motion is ahead.
    fn alone(&mut self, grid: u64, now: Instant) -> Option<bool> {
        self.anim.chain(grid)?;
        if !self.anim.scrolls_alone(grid) {
            return Some(false);
        }
        self.anim.unchain(&self.model, grid, now);
        None
    }

    /// The colours of `grid`'s rows, read from its text rectangle
    /// ([`predict::Look`]).
    fn look(
        &self,
        grid: u64,
        rect: (usize, usize, usize, usize),
        view: &model::Viewport,
        shape: &predict::Shape,
    ) -> Option<predict::Look> {
        let (top, bot, left, right) = rect;
        let g = self.model.grids.get(&grid)?;
        let rows: Vec<&[Cell]> = (top..bot.min(g.height()))
            .map(|r| {
                let row = g.row(r);
                &row[left.min(row.len())..right.min(row.len())]
            })
            .collect();
        let cursor_row = (self.model.cursor.grid == grid)
            .then(|| self.model.cursor.row.checked_sub(top))
            .flatten();
        Some(predict::Look::of(
            &rows,
            view.lines_shown(bot - top),
            shape.textoff,
            cursor_row,
            |name| self.model.groups.get(name).copied(),
        ))
    }

    /// `curswant` in `win` per the agent ([`Predictor::want`]), else the
    /// cursor's display column.
    fn wants(&self, win: i64, shape: &predict::Shape) -> usize {
        self.predict
            .as_ref()
            .and_then(|p| p.want(win))
            .unwrap_or_else(|| {
                (self.model.cursor.col + shape.leftcol).saturating_sub(shape.textoff)
            })
    }

    /// Draws what scrolled-ahead windows lacked, now that lines arrived or
    /// Neovim redrew; gives up windows no longer predictable, predicting
    /// nothing more until Neovim catches up.
    fn fill_in(&mut self, now: Instant) {
        for grid in self.anim.held() {
            if !self.fill_in_grid(grid, now) {
                self.give_up(grid);
                if let Some(p) = self.predict.as_mut() {
                    p.typed.missed();
                }
            }
            self.dirty = true;
        }
    }

    /// Stops scrolling `grid` ahead: it shows Neovim's view again, and the
    /// scrolls' inputs are replayed from the next ack ([`App::replay`]).
    fn give_up(&mut self, grid: u64) {
        let seqs = self.anim.give_up(grid);
        if let Some(p) = self.predict.as_mut() {
            p.given_up(&seqs);
        }
    }

    /// [`App::fill_in`] for one grid; returns whether it is still
    /// predictable.
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
        let wants = self.wants(win, shape);
        // A held scroll whose landing scrolls sideways is dropped with those
        // after it; later keys wait for Neovim as after any miss.
        let mut let_go = false;
        let mut land = |l, want: Option<usize>| {
            let at = landed(p, win, shape, l, want.unwrap_or(wants), width)?;
            let kept = at
                .left
                .filter(|&left| left == shape.leftcol)
                .and_then(|left| at.col(left, shape, width))
                .map(|col| (col, at.want));
            let_go |= kept.is_none();
            Some(kept)
        };
        self.anim
            .fill_in(&self.model, grid, (&mut fill, &mut land), now);
        if let (true, Some(p)) = (let_go, self.predict.as_mut()) {
            p.typed.missed();
        }
        true
    }

    /// Makes a predicted sideways scroll at once if possible. See
    /// [`predict_command`](Self::predict_command).
    fn predict_side(&mut self, grid: u64, side: Side, cause: Cause, now: Instant) -> bool {
        if self.predict.is_none() || !self.multigrid() || !self.flushed || self.holding(now) {
            return false;
        }
        // As in `App::predict_scroll`: steps moved the cursor from where
        // Neovim has it, and sideways scrolls are never steps.
        if self.anim.chain(grid).is_some() {
            return false;
        }
        let Some(p) = self.predict.as_ref() else {
            return false;
        };
        let mode = Mode::of(&self.model.mode_name);
        let ok = match cause.by {
            By::Keys => mode == Mode::Normal && sure(p),
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
        // From the view after any sideways scroll ahead; the cursor where it
        // is drawn if in this window, else from Neovim's byte column.
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
        let until = p.deadline(cause.at);
        self.anim
            .side(&self.model, grid, rows, (to.left, until), cursor, now);
        self.dirty = true;
        true
    }

    /// `grid`'s rows as shown (Neovim's or scrolled ahead), shifted to start
    /// at display column `to`. See [`predict::shifted`].
    fn side_rows(&self, grid: u64, to: usize) -> Option<Vec<Vec<Cell>>> {
        let p = self.predict.as_ref()?;
        let win = *self.model.wins.get(&grid)?;
        let view = self.model.viewports.get(&grid)?;
        let shape = p.shape(win)?;
        let rect = anim::text_rect(&self.model, grid)?;
        let (top, bot, left, right) = rect;
        let (rows, topline) = match self.anim.rows_ahead(&self.model, grid) {
            Some((rows, ahead)) => (rows, view.topline + ahead),
            None => (
                self.model.grids.get(&grid)?.lines(top, bot, left, right),
                view.topline,
            ),
        };
        let lines = (view.line_count - topline).clamp(0, (bot - top) as i64) as usize;
        let look = self.look(grid, rect, view, shape)?;
        predict::shifted(&rows, shape.leftcol, to, shape, &look, |r| {
            if r < lines {
                Some(Some(p.coloured(win, topline + r as i64)?))
            } else {
                Some(None)
            }
        })
    }

    /// Moves reported tails into the model, with their batch or after
    /// [`TAILS_WAIT`]; tails of closed windows are dropped.
    fn take_tails(&mut self) {
        self.tails_since = None;
        for (win, tails) in std::mem::take(&mut self.tails) {
            if let Some((&grid, _)) = self.model.wins.iter().find(|(_, w)| **w == win) {
                if self.model.set_tails(grid, tails) {
                    self.dirty = true;
                }
            }
        }
    }

    /// Records an ack ([`predict::Ack`]) if the counts are trusted. It
    /// precedes its frame and is normally taken with it (the frame is the
    /// truth, and on a lossy link may lag by a lost segment's time), or at
    /// once if the screen already shows it, unless the keys edited the
    /// buffer or one was not understood ([`predict::Sent::lost`]), either of
    /// which the frame may still change. Nor at once if it covers a scroll
    /// made ahead in a window without the cursor, whose view it does not
    /// give. The newest wins.
    fn heard_ack(&mut self, params: &[ValueRef<'_>], now: Instant) {
        let Some(ack) = predict::Ack::read(params) else {
            return;
        };
        let Some(p) = self.predict.as_mut().filter(|p| p.acks()) else {
            return;
        };
        let edited = p
            .base
            .as_ref()
            .is_some_and(|b| (b.ack.buf, b.ack.tick) != (ack.buf, ack.tick));
        let Some(k) = p.heard(&ack, now) else {
            // Counted past a key Neovim may count as two: drop the steps for
            // the keys before it with the ack's frame, or at once if the
            // screen already shows it, as for an ack.
            if let Some(upto) = p.sync.passed() {
                if !edited && self.shows(&ack) {
                    self.floor(upto, now);
                } else {
                    self.passed = self.passed.max(Some(upto));
                }
            }
            self.forget_unsure(now);
            return;
        };
        let made_out = p.made_out(k);
        let elsewhere = self
            .anim
            .unmade(k)
            .iter()
            .any(|grid| self.model.wins.get(grid) != Some(&ack.pos.win));
        let shown = !edited && made_out && !elsewhere && self.shows(&ack);
        self.ack = Some((k, ack));
        if shown {
            self.take_acks(now);
        }
    }

    /// Whether Neovim's screen already matches `ack`: window, cursor, view
    /// and mode. A no-op key (`l` at a line's end) gets an ack but no frame.
    fn shows(&self, ack: &predict::Ack) -> bool {
        let grid = self.model.cursor.grid;
        let pos = &ack.pos;
        let view = self.model.viewports.get(&grid);
        let shape = self.predict.as_ref().and_then(|p| p.shape(pos.win));
        self.model.wins.get(&grid) == Some(&pos.win)
            && view.is_some_and(|v| {
                (v.curline, v.curcol, v.topline) == (pos.line, pos.byte as i64, pos.top)
            })
            && shape.is_some_and(|s| s.leftcol == pos.left)
            && Rest::drawn(&self.model.mode_name) == pos.rest
    }

    /// Takes the pending ack after its frame as Neovim's last word
    /// ([`Predictor::judge`]). A scroll ahead for a covered key that the
    /// frame did not make was wrong: the view reverts at once instead of at
    /// its deadline (which on a lossy link may be much later). Later keys
    /// are replayed from there ([`App::replay`]).
    fn take_acks(&mut self, now: Instant) {
        if let Some(upto) = self.passed.take() {
            self.floor(upto, now);
        }
        let Some((k, ack)) = self.ack.take() else {
            return;
        };
        let unmade = self.anim.unmade(k);
        for &grid in &unmade {
            tracing::debug!(
                grid,
                k,
                "client: a scroll ahead its key's frame did not make, given up"
            );
            self.give_up(grid);
            self.dirty = true;
        }
        // Whether the screen is this ack's frame (not one drawn after more
        // keys): only then can later scrolls be made from it.
        let fresh = self.shows(&ack);
        self.judge_steps(k, &ack, fresh, now);
        // Steps after a given-up scroll assumed a view no longer shown.
        for &grid in &unmade {
            if self.anim.chain(grid).is_some() {
                self.drop_steps(grid, now);
            }
        }
        let grid = self
            .model
            .wins
            .iter()
            .find(|(_, w)| **w == ack.pos.win)
            .map(|(g, _)| *g);
        let view = grid
            .and_then(|g| anim::text_rect(&self.model, g))
            .map(|(top, bot, left, right)| ((bot - top) as i64, right - left));
        if let Some(p) = self.predict.as_mut() {
            p.judge(k, ack, view, !unmade.is_empty());
        }
        self.replay(fresh, now);
    }

    /// Judges cursor steps ([`anim::chain`]) and typing by `ack`, which
    /// covers keys up to `k`; `fresh` if the screen is its frame.
    ///
    /// - *Made*: the step for `k` matches the ack's cursor and view (typed
    ///   lines also its text, [`Predictor::fresh`]). Steps up to `k` are
    ///   dropped. If a mode change's autocommands edited the buffer
    ///   ([`App::edited`]), the kind must prove itself again
    ///   ([`predict::Trust::edited`]); if they did or may have, later steps
    ///   are remade. If fresh and the frame draws the cursor or typed line
    ///   differently, the window is distrusted for a while; a difference only
    ///   past the line's end drops later steps for relayout.
    /// - *Wrong*: the cursor reverts at once, the window's steps are dropped
    ///   and the kind must prove itself again ([`predict::Trust`]).
    /// - Nothing to judge (no step for `k`, buffer changed, typed text
    ///   unknown): steps are dropped.
    ///
    /// The last two replay keys after `k` from the ack ([`App::replay`]).
    fn judge_steps(&mut self, k: u64, ack: &predict::Ack, fresh: bool, now: Instant) {
        for grid in self.anim.chained() {
            let Some(chain) = self.anim.chain(grid) else {
                continue;
            };
            let (here, textoff) = (chain.win == ack.pos.win, chain.textoff);
            // Same buffer tick, unless the steps typed or the key changed
            // mode: then the judge must find what e.g. a plugin's
            // `InsertLeave` trimming trailing blanks did.
            let step = chain.step_for(k).cloned().filter(|s| {
                let mode = matches!(s.proof, Proof::Enter | Proof::Leave);
                here && (s.echo.is_some() || mode || s.tick == ack.tick)
            });
            let Some(p) = self.predict.as_ref() else {
                return;
            };
            // Whether the typed line matches the ack's text; `None` if that
            // text is unknown.
            let typed = step.as_ref().map(|s| match &s.echo {
                None => Some(true),
                Some(e) => {
                    let win = ack.pos.win;
                    let known = p.fresh(win, ack.buf, ack.tick) && !p.cut(win, e.line);
                    known.then(|| p.text(win, e.line) == Some(&*e.text))
                }
            });
            match (step, typed.flatten()) {
                (Some(s), Some(true)) if s.pos == ack.pos && s.line_count == ack.line_count => {
                    self.anim.drop_upto(grid, k);
                    let at = (textoff, s.vcol);
                    let moded = matches!(s.proof, Proof::Enter | Proof::Leave);
                    let edited = moded.then(|| self.edited(k, &s, ack)).flatten();
                    let unseen = moded && self.unseen(ack.buf, s.proof);
                    let Some(p) = self.predict.as_mut() else {
                        return;
                    };
                    let credited = s.credited && (!moded || edited == Some(false));
                    p.trust.made(ack.buf, s.proof, credited, !s.hidden);
                    if edited == Some(true) {
                        p.trust.edited(ack.buf, s.proof);
                    }
                    // A mode autocommand changed (or may have changed) the
                    // buffer under later steps: remake them.
                    if moded && (edited == Some(true) || (edited.is_none() && unseen)) {
                        self.drop_steps(grid, now);
                        continue;
                    }
                    if !fresh || s.hidden {
                        continue;
                    }
                    let echoed = s.echo.as_ref().map(|e| self.echoed(grid, e));
                    if !self.drawn_at(grid, &s, at) || echoed.is_some_and(|(text, _)| !text) {
                        tracing::info!(
                            k,
                            step = ?s.pos,
                            vcol = s.vcol,
                            cursor = ?self.model.cursor,
                            "client: Neovim drew the cursor elsewhere than the client did, or \
                             the line typed into otherwise; leaving the window to it a while"
                        );
                        if let Some(p) = self.predict.as_mut() {
                            p.trust.distrust(ack.pos.win, now + predict::DISTRUST);
                        }
                    }
                    if echoed.is_some_and(|(text, row)| !(text && row)) {
                        self.drop_steps(grid, now);
                    }
                }
                (Some(s), Some(_)) => {
                    if let Some(p) = self.predict.as_mut() {
                        let keys: Vec<&str> = p
                            .queue
                            .iter()
                            .filter(|q| q.seq <= k)
                            .filter_map(|q| q.input.key())
                            .collect();
                        if !s.hidden {
                            tracing::info!(
                                k,
                                ?keys,
                                step = ?s.pos,
                                neovim = ?ack.pos,
                                line = ?p.text(ack.pos.win, ack.pos.line),
                                typed = ?s.echo.as_ref().map(|e| &e.text),
                                "client: a prediction drawn ahead of Neovim was not what it made"
                            );
                        }
                        p.trust.wrong(ack.buf, s.proof, !s.hidden);
                    }
                    // Drop their scrolls ahead too (a hidden step made none,
                    // nor any after it).
                    self.give_up(grid);
                    let seqs = self.anim.drop_chain(&self.model, grid, now);
                    let rest: Vec<u64> = seqs.into_iter().filter(|&q| q != k).collect();
                    self.forget(&rest);
                    self.dirty = true;
                }
                _ => self.drop_steps(grid, now),
            }
        }
    }

    /// Whether mode-change key `k`, made as step `s`, also edited the buffer
    /// (via a mode autocommand): by the step's `b:changedtick`, or after
    /// typing ahead, by the previous ack (or an earlier one at the same
    /// tick). `None` if one ack covered both the typing and the key.
    fn edited(&self, k: u64, s: &chain::Step, ack: &predict::Ack) -> Option<bool> {
        if s.echo.is_none() {
            return Some(s.tick != ack.tick);
        }
        let base = self.predict.as_ref()?.base.as_ref()?;
        let before = (base.seq < k && base.ack.buf == ack.buf).then_some(base.ack.tick)?;
        if before == ack.tick {
            Some(false)
        } else {
            (base.seq + 1 == k).then_some(true)
        }
    }

    /// Whether a plugin autocommand in `buf` may act on entering or leaving
    /// Insert mode, per `proof` ([`App::predict_mode`]).
    fn unseen(&self, buf: i64, proof: Proof) -> bool {
        let facts = self.predict.as_ref().and_then(|p| p.facts(buf));
        facts.is_some_and(|f| match proof {
            Proof::Enter => f.unseen_entered,
            _ => f.unseen_left || !f.from_insert,
        })
    }

    /// Whether Neovim drew typed line `e` as laid out: (its text, the whole
    /// row including past its end); `(false, false)` if not in view.
    fn echoed(&self, grid: u64, e: &chain::Echo) -> (bool, bool) {
        let drawn = (|| {
            let view = self.model.viewports.get(&grid)?;
            let (top, bot, left, right) = anim::text_rect(&self.model, grid)?;
            let win = self.model.wins.get(&grid)?;
            let shape = self.predict.as_ref()?.shape(*win)?;
            let r = usize::try_from(e.line - view.topline)
                .ok()
                .filter(|r| *r < bot - top)?;
            let row = self.model.grids.get(&grid)?.row(top + r);
            let cells = row.get((left + shape.textoff).min(right)..right)?;
            let end = predict::reach(&e.text, shape).min(cells.len());
            let same = |a: &[Cell], b: &[Cell]| {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.text == y.text)
            };
            let text = e
                .cells
                .get(..end)
                .is_some_and(|ours| same(ours, &cells[..end]));
            Some((text, text && same(&e.cells, cells)))
        })();
        drawn.unwrap_or((false, false))
    }

    /// Whether Neovim's cursor is where step `s` drew it, with a gutter
    /// `textoff` wide and the step at display column `vcol`.
    fn drawn_at(&self, grid: u64, s: &chain::Step, (textoff, vcol): (usize, usize)) -> bool {
        let c = self.model.cursor;
        let (Some(view), Some((top, ..))) = (
            self.model.viewports.get(&grid),
            anim::text_rect(&self.model, grid),
        ) else {
            return false;
        };
        let row = usize::try_from(s.pos.line - view.topline).map(|r| top + r);
        let col = (textoff + vcol).checked_sub(s.pos.left);
        c.grid == grid && Ok(c.row) == row && Some(c.col) == col
    }

    /// Drops `grid`'s steps unjudged: cursor and view revert to Neovim's
    /// (the steps' scrolls are the only ones there, see [`App::onward`]),
    /// and their keys are replayed from the next ack, blind until then
    /// ([`predict::Typed::blinded`]).
    fn drop_steps(&mut self, grid: u64, now: Instant) {
        if self.anim.chain(grid).is_some() {
            self.give_up(grid);
        }
        let seqs = self.anim.drop_chain(&self.model, grid, now);
        self.forget(&seqs);
        self.dirty = true;
    }

    /// Steps for `seqs` were dropped unjudged: see [`App::drop_steps`].
    fn forget(&mut self, seqs: &[u64]) {
        if let (Some(p), false) = (self.predict.as_mut(), seqs.is_empty()) {
            p.given_up(seqs);
            p.trust.unjudged(seqs.len() as u64);
            p.typed.blinded();
        }
    }

    /// Forgets ([`App::forget`]) steps the animator dropped itself
    /// ([`anim::Animator::take_dropped`]); returns whether there were any.
    fn forget_dropped(&mut self) -> bool {
        let seqs = self.anim.take_dropped();
        self.forget(&seqs);
        !seqs.is_empty()
    }

    /// Remakes steps a Neovim frame dropped ([`anim::chain`]) at once, from
    /// the last ack over the new window state ([`App::replay`]). If the line
    /// still lays out the same (only the window size or past-end content
    /// changed), the cursor stays put instead of bouncing to Neovim's for a
    /// round trip; otherwise they wait for the next ack.
    fn remake(&mut self, now: Instant) {
        let Some(p) = self.predict.as_mut() else {
            return;
        };
        if p.rebase() {
            self.remaking = true;
            self.replay(false, now);
            self.remaking = false;
        }
    }

    /// Drops steps when key counts are in doubt ([`predict::Sync`]): no ack
    /// can be matched to their keys. In doubt only from a key Neovim may
    /// count as two, steps for earlier keys stay, judged by their acks or
    /// dropped by [`App::floor`] once a count passes them: dropped now, they
    /// would take the screen back to Neovim's last frame. A paste only
    /// defers acks until a fence; in Normal mode it is no key, so earlier
    /// steps stay valid until its frame says otherwise.
    fn forget_unsure(&mut self, now: Instant) {
        let Some(from) = self
            .predict
            .as_ref()
            .filter(|p| p.acks())
            .and_then(|p| p.sync.doubted_from())
        else {
            return;
        };
        for grid in self.anim.chained() {
            if self
                .anim
                .chain(grid)
                .is_some_and(|c| c.seqs().iter().any(|&s| s >= from))
            {
                self.drop_steps(grid, now);
            }
        }
    }

    /// A fence says Neovim took keys up to `upto`, or an ack or fence counted
    /// past a key Neovim may count as two, `upto` being the key before it. A
    /// step for one still drawn was taken without an ack (operator pending,
    /// completion, read with that key), so the frame is the truth: drop
    /// every step and go blind from the first key after the last ack until
    /// the next.
    fn floor(&mut self, upto: u64, now: Instant) {
        let taken = self.anim.chained().into_iter().any(|grid| {
            self.anim
                .chain(grid)
                .is_some_and(|c| c.seqs().iter().any(|&s| s <= upto))
        });
        if !taken {
            return;
        }
        for grid in self.anim.chained() {
            self.drop_steps(grid, now);
        }
        if let Some(p) = self.predict.as_mut() {
            p.typed.blinded();
        }
    }

    /// Re-reads inputs sent since the last ack from its state
    /// ([`predict::Typed::rebase`]) and makes those not yet made (a scroll
    /// after a blind key or a given-up one, a motion whose step was
    /// dropped), each once; made scrolls are still ahead or done by Neovim.
    /// An unmakeable key or a click blinds from there on. So does an unmade
    /// scroll unless `fresh`: Neovim may have handled more keys in that
    /// frame. Motions come from the ack, not the frame, so they are made
    /// regardless.
    fn replay(&mut self, fresh: bool, now: Instant) {
        let Some(p) = self.predict.as_ref() else {
            return;
        };
        let queue: Vec<predict::Sent> = p.queue.iter().cloned().collect();
        let grid = self.model.cursor.grid;
        let win = self.model.wins.get(&grid).copied();
        for sent in queue {
            // A wheel turn is remade over its own window, by the wheel's rules.
            let turned = match &sent.input {
                predict::Input::Wheel(t) => {
                    let insert = self.predict.as_ref().is_some_and(|p| p.typed.inserting());
                    Some((t.grid, self.wheel_command(t.grid, &t.way, &t.mods, insert)))
                }
                _ => None,
            };
            let Some(p) = self.predict.as_mut() else {
                return;
            };
            let seq = sent.seq;
            let (command, onto) = match &sent.input {
                predict::Input::Key(key) => (p.remake(win, key), None),
                predict::Input::Wheel(_) => match turned {
                    Some((grid, Some((command, by)))) => (Some(command), Some((grid, by))),
                    _ => (None, None),
                },
                predict::Input::Click => {
                    p.typed.clicked();
                    (None, None)
                }
            };
            p.remade(seq);
            // Insert mode entered by an unmade key is not known to be plain
            // ([`predict::Typed::unentered`]).
            let entering = matches!(command, Some(Command::Enter(_)));
            if sent.made || !p.typed.followed() {
                if entering && !sent.made {
                    p.unentered(seq);
                }
                continue;
            }
            let made = match (command, fresh) {
                (Some(command), fresh)
                    if fresh
                        || matches!(
                            command,
                            Command::Move { .. }
                                | Command::Type(_)
                                | Command::Enter(_)
                                | Command::Leave
                        ) =>
                {
                    let (grid, by) = onto.unwrap_or_else(|| {
                        let by = if p.typed.inserting() {
                            By::InsertKeys
                        } else {
                            By::Keys
                        };
                        (grid, by)
                    });
                    let cause = Cause {
                        by,
                        seq,
                        at: sent.at,
                    };
                    self.make(grid, command, cause, now)
                }
                // Nothing to make yet: a count, or `z` awaiting its second key.
                (None, _) if matches!(sent.input, predict::Input::Key(_)) => continue,
                _ => false,
            };
            if let Some(p) = self.predict.as_mut() {
                if made {
                    p.made(seq);
                } else {
                    if entering {
                        p.unentered(seq);
                    }
                    p.typed.blinded();
                }
            }
        }
    }

    /// After a Neovim frame: sideways view moves land the scrolls ahead up to
    /// them, and those still ahead are rebuilt from Neovim's rows.
    fn landed_sideways(&mut self, now: Instant) {
        for (win, left) in std::mem::take(&mut self.lefts) {
            if let Some(p) = self.predict.as_mut() {
                p.set_leftcol(win, left);
            }
            if let Some((&grid, _)) = self.model.wins.iter().find(|(_, w)| **w == win) {
                self.anim.landed(grid, left);
                // Steps assumed a view Neovim has since moved.
                if self.anim.chain(grid).is_some_and(|c| c.left != left) {
                    self.drop_steps(grid, now);
                }
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

    /// Where a grid is drawn, inset when centred in its window ([`wide`]).
    fn origin_of(&self, grid: u64) -> (i64, i64) {
        self.model.drawn_origin(grid)
    }

    /// The centred window ([`wide`]) with `row`, `col` in its margin.
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

    /// The topmost grid at a cell: a mouse-enabled float, the message area,
    /// a window, or grid 1.
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

    pub fn resized(&mut self, size: (usize, usize)) {
        self.size = size;
        // Even at the same size: nvmux nudges the pty a row and back to force
        // a repaint (`pty::nudge`), and the server answers with a full
        // redraw. When not attached, the coming attach uses the size.
        if let Attach::Attached { .. } = self.attach {
            self.outbox.notify(
                "nvim_ui_try_resize",
                &[Arg::Int(size.0 as i64), Arg::Int(size.1 as i64)],
            );
        }
        self.screen.invalidate();
        self.anim.reset();
        self.forget_dropped();
        self.dirty = true;
    }

    pub fn size(&self) -> (usize, usize) {
        self.size
    }

    /// Timed work: round-trip pings, fences, UI rechecks, waiting tails.
    pub fn tick(&mut self, now: Instant) {
        if let Some(p) = self.predict.as_mut() {
            if p.wants_ping(now, self.typed) {
                p.pinged(now);
                let id = self
                    .outbox
                    .request(PING, &[Arg::str("0"), Arg::str(""), Arg::Bool(false)]);
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
        if self
            .tails_since
            .is_some_and(|since| since + TAILS_WAIT <= now)
        {
            self.take_tails();
        }
    }

    /// Whether to start a refresh now ([`refresh`]); clears the request.
    pub fn take_refresh(&mut self) -> bool {
        std::mem::take(&mut self.refresh)
    }

    /// When the loop must next wake for the client's own timers.
    pub fn wake_at(&self, now: Instant) -> Option<Instant> {
        let frame = if !self.flushed {
            None
        } else if self.holding(now) {
            // Never a past time: an attach or detach waits for its answer
            // however long the hold, and a past deadline would busy-poll.
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
        [
            self.escape_deadline(),
            frame,
            self.recheck_at,
            self.tails_since.map(|since| since + TAILS_WAIT),
        ]
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

    /// When a pending change may be drawn: at once for input since the last
    /// frame, which the hand paces. Otherwise [`MIN_FRAME_GAP`] after the
    /// last frame that drew a Neovim flush, or at once if that one followed a
    /// quiet spell (a frame's time or more). With `'showcmd'` (the default) a
    /// Normal-mode key makes Neovim flush twice within a millisecond (the key
    /// shown, then its effect), and the gap would delay the effect; so would
    /// counting frames drawn ahead of Neovim. Bursts are still limited after
    /// their first two frames. Away, [`AWAY_FRAME_GAP`] applies.
    fn dirty_due(&self, now: Instant) -> Instant {
        let Some(last) = self.last_frame else {
            return now;
        };
        if self.away {
            return (last + AWAY_FRAME_GAP).max(now);
        }
        if self.typed.is_some_and(|typed| typed > last) {
            return now;
        }
        let (Some(last), before) = self.flush_frames else {
            return now;
        };
        let quiet =
            before.is_none_or(|before| last.saturating_duration_since(before) >= anim::FRAME);
        if quiet {
            return now;
        }
        (last + MIN_FRAME_GAP).max(now)
    }

    fn cards_change(&self, now: Instant) -> Option<Instant> {
        self.cards.next_frame(self.last_frame.unwrap_or(now))
    }

    /// The next frame's bytes, preceded by any queued output.
    ///
    /// When possible, only changed rows ([`App::changed_rows`]) are rebuilt
    /// over the shown frame and diffed.
    pub fn frame(&mut self, now: Instant) -> Vec<u8> {
        // Steps not acked in time are dropped like scrolls ahead, blind
        // until the next ack.
        for (grid, seqs) in self.anim.lapsed(&self.model, now) {
            self.give_up(grid);
            self.forget(&seqs);
        }
        self.anim.advance(now);
        self.repaint();
        let (w, h) = self.size;
        self.cards.expire(now);
        // The shown frame, if only changed rows can differ.
        let shown = if self.changed_rows(h) {
            self.screen.take_like_shown(w, h)
        } else {
            None
        };
        let partly = shown.is_some();
        let (frame, cursor) = match shown {
            Some(mut frame) => {
                let rows = self.partial.as_ref().map_or(&[][..], |p| &p.rows);
                compose::compose_rows(&mut frame, &self.model, &self.anim, rows);
                // Still, so nothing is painted over; this only yields the
                // cursor.
                let cursor = self.anim.paint(&mut frame, &self.model, now);
                (frame, cursor)
            }
            None => {
                // Built in a spare frame buffer and mended once at the end,
                // plus before overlays that read the cells under them (cards,
                // smear, sparks, blink).
                let over = self.anim.paints_over() || !self.cards.is_empty();
                let mut frame = self.screen.take_spare();
                compose::compose_into(&mut frame, &self.model, &self.anim, w, h, over);
                let cursor = self.anim.paint(&mut frame, &self.model, now);
                // Cards go over everything ([`cards`]).
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
                (frame, cursor)
            }
        };
        let sync = self.model.options.termsync;
        let mut out = std::mem::take(&mut self.say);
        out.reserve(4096);
        let links = self.model.links();
        match self.partial.as_ref().filter(|_| partly) {
            Some(p) => self
                .screen
                .draw_rows_into(&mut out, frame, &cursor, sync, links, &p.rows),
            None => self.screen.draw_into(&mut out, frame, &cursor, sync, links),
        }
        #[cfg(test)]
        if let Some(p) = self.partial.as_mut().filter(|_| partly) {
            p.partly += 1;
        }
        self.dirty = false;
        self.last_frame = Some(now);
        if std::mem::take(&mut self.flushed_since) {
            self.flush_frames = (Some(now), self.flush_frames.0);
        }
        out
    }

    /// Whether this frame can be built from changed rows only, marking them
    /// in [`Partial::rows`]: this and the last frame are the editor's grids as
    /// placed (nothing moving, held, painted over, no cards) and only cells of
    /// visible grids, or what is drawn over rows, changed since. Rows drawn
    /// over in this frame or the last count as changed. Records this frame
    /// for the next either way.
    fn changed_rows(&mut self, height: usize) -> bool {
        let Some(partial) = self.partial.as_mut() else {
            return false;
        };
        let since = std::mem::replace(&mut partial.at, grid::tick());
        // Frame rows drawn over now and in the last frame (grids placed as
        // when nothing moves).
        let patched: Vec<usize> = self
            .anim
            .patched_rows()
            .filter_map(|(grid, r)| {
                usize::try_from(self.model.drawn_origin(grid).0 + r as i64).ok()
            })
            .collect();
        let was_patched = std::mem::replace(&mut partial.patched, patched);
        let still = self.anim.still() && self.cards.is_empty();
        let was_still = std::mem::replace(&mut partial.still, still);
        if !(still && was_still && self.model.unchanged_since(since)) {
            return false;
        }
        let rows = &mut partial.rows;
        rows.clear();
        rows.resize(height, false);
        for &y in partial.patched.iter().chain(&was_patched) {
            if let Some(row) = rows.get_mut(y) {
                *row = true;
            }
        }
        let shown = self
            .model
            .layout
            .iter()
            .filter(|(grid, p)| **grid != 1 && !p.hidden)
            .map(|(grid, _)| (*grid, self.model.drawn_origin(*grid).0));
        for (grid, top) in std::iter::once((1, 0)).chain(shown) {
            let Some(g) = self.model.grids.get(&grid) else {
                continue;
            };
            for r in g.changed_since(since) {
                if let Some(row) = usize::try_from(top + r as i64)
                    .ok()
                    .and_then(|y| rows.get_mut(y))
                {
                    *row = true;
                }
            }
        }
        true
    }

    /// Output that cannot wait for a frame (the keyboard mode, before the
    /// first one).
    pub fn take_said(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.say)
    }

    /// Drains what was queued for the server.
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

/// Runs the client for the session at `sock` until either is done.
pub fn run(sock: &Path) -> anyhow::Result<()> {
    // The config file is read only by a client nvmux did not start, which
    // then assumes a local session.
    let drawing = std::env::var(SETTINGS_ENV)
        .ok()
        .and_then(|text| crate::config::Drawing::from_env(&text))
        .unwrap_or_else(|| crate::config::load().unwrap_or_default().drawing());
    let settings = drawing.settings();
    let effects = Effects::from_settings(&settings);
    let term = palette_from_env().unwrap_or(FALLBACK);

    let mut stream =
        UnixStream::connect(sock).with_context(|| format!("connecting to {}", sock.display()))?;
    stream.set_nonblocking(true)?;

    let _raw = term::Raw::enter()?;
    let hangup = term::Hangup::install()?;
    let winch = crate::winch::Winch::install()?;
    crate::term::write_stdout(term::OPENING)?;

    let predict = settings.client.predicts(drawing.remote);
    let mut app = App::new(term, effects, predict, term::size())
        .with_centre(
            settings.client.centre,
            usize::from(settings.client.window_width),
        )
        .with_predictions(settings.client.predictions(drawing.remote));
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
                    Ok(Some(frame)) => app.message(frame, now),
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
        // Send keys before composing the frame they may have predicted,
        // which would otherwise delay them about a millisecond each on an
        // already slow link.
        if let Err(e) = app.outbox.send(&mut stream) {
            break Err(anyhow::Error::from(e).context("writing to the session"));
        }
        if app.due(now) {
            // Empty when nothing changed and nothing was queued (see
            // `Screen::draw_into`).
            let bytes = app.frame(now);
            if !bytes.is_empty() {
                if let Err(e) = crate::term::write_stdout(&bytes) {
                    break Err(e.into());
                }
            }
        }
        if app.done {
            break Ok(());
        }
    };
    if let Some(p) = app.predict.as_ref().filter(|p| p.acks()) {
        p.shadow.log();
        p.trust.tally.log();
    }
    // However it ended, restore the terminal and release narrowed windows
    // for other UIs.
    if app.let_windows_go(Instant::now()) {
        leave(stream, inbox, std::mem::take(&mut app.outbox));
    } else {
        let _ = app.outbox.send(&mut stream);
    }
    let _ = crate::term::write_stdout(term::CLOSING);
    outcome
}

/// How long an exiting client waits for Neovim to process its last requests
/// ([`leave`]); well under the 2 s nvmux allows before killing it.
const LEAVE_WAIT: Duration = Duration::from_millis(500);

/// Sends what is queued and waits up to [`LEAVE_WAIT`] for Neovim to process
/// it. Neovim drops a channel's queued requests once it reads EOF
/// (`request_event` in `msgpack_rpc/channel.c`), losing the last ones about
/// one time in four under load, so a fence request follows them.
fn leave(stream: UnixStream, inbox: Inbox, mut out: Outbox) {
    let fence = out.request("nvim_eval", &[Arg::str("0")]);
    // Blocking last, so it never blocks without timeouts.
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

/// Makes Neovim redraw the current tab page's windows for multigrid UIs: a
/// non-multigrid UI attaches on its own connection (moving every window to
/// grid 1), and detaches once drawn for, so the windows get their own grids
/// back with full contents. See the module docs.
///
/// Runs on its own thread so the client keeps reading and drawing; its
/// effect arrives as redraws on the client's connection.
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
    // Neovim aborts on its 17th UI (see `crate::rpc::Client::list_uis`), so
    // with more than a few attached, skip the refresh rather than risk it
    // with several clients refreshing at once.
    let uis = line.out.request("nvim_list_uis", &[]);
    let count = line.answer(uis, |v| v.as_array().map_or(usize::MAX, Vec::len))?;
    if count > REFRESH_MAX_UIS {
        tracing::debug!(count, "client: too many UIs attached to refresh");
        return Ok(());
    }
    // Mark this UI as only passing through grid 1 (see `UIS_LUA`).
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
    // The client's size, never less than the editor's, so nothing resizes.
    // `ext_termcolors` as the client has it, or default colours would
    // briefly change for every UI.
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
    // Answered once Neovim has drawn for it with windows on grid 1.
    line.answer(attach, |_| ())?;
    // Detaching returns windows to their own grids; closing would too, but
    // this does not wait for that.
    line.out.notify("nvim_ui_detach", &[]);
    let _ = line.out.send(&mut line.stream);
    Ok(())
}

/// Most UIs already attached for a refresh to add one ([`refresh_now`]).
const REFRESH_MAX_UIS: usize = 4;

/// A blocking connection with a deadline: a refresh's, or an exiting
/// client's.
struct Line {
    stream: UnixStream,
    inbox: Inbox,
    out: Outbox,
    deadline: Instant,
}

impl Line {
    /// Sends what is queued and returns `read` of the result of request
    /// `id`; anything else (e.g. redraws for an attached UI) is discarded.
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
                // Skip other frames undecoded (a refresh gets a full redraw).
                if redraw::response_id(frame) != Some(u64::from(id)) {
                    continue;
                }
                let mut rd = frame;
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

    /// Time left before the deadline; `TimedOut` if none.
    fn left(&self) -> std::io::Result<Duration> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        Ok(left)
    }
}

fn palette_from_env() -> Option<Palette> {
    Palette::from_env(&std::env::var(PALETTE_ENV).ok()?)
}

/// Colours for a terminal that never reported its own: Neovim's default
/// dark scheme, so fades have real colours to work with.
const FALLBACK: Palette = Palette {
    fg: Rgb(0xe0, 0xe2, 0xea),
    bg: Rgb(0x14, 0x16, 0x1b),
    ansi: crate::palette::XTERM_ANSI,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::predict::PING_EVERY;
    use crate::config::Predict;
    use rmpv::Value;

    type Call = (Option<u64>, String, Vec<Value>);

    /// `MOTIONS` is `predict_typing = "off"`; `TYPING` the `[client]` default.
    const MOTIONS: Predictions = Predictions {
        scrolls: true,
        motions: true,
        typing: false,
    };
    const TYPING: Predictions = Predictions {
        scrolls: true,
        motions: true,
        typing: true,
    };

    fn palette() -> Palette {
        Palette {
            fg: Rgb(255, 255, 255),
            bg: Rgb(0, 0, 0),
            ansi: [Rgb(0, 0, 0); 16],
        }
    }

    /// A client with an effect that wants multigrid, or with none.
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

    /// Calls sent since last asked, as `(id, method, args)`; notifications
    /// have no id.
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

    /// The id of the `method` request (for `nvim_exec_lua`, the one whose
    /// Lua starts with `lua`).
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

    /// A `grid_line` of `text` at column 0, one cell per character.
    fn line(grid: u64, row: u64, text: &str) -> (&'static str, Vec<Value>) {
        line_at(grid, row, 0, text)
    }

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

    /// Every cell of a new `width` × `height` grid: `rows`, then blanks.
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

    /// Started and attached (multigrid unless `foreign`), grid 1 drawn.
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

    /// Test clients' settings: windows animated, nothing predicted or
    /// centred.
    fn drawing() -> crate::config::Drawing {
        let mut d = crate::config::Drawing::default();
        d.client.predict_scrolls = Predict::Off;
        d.client.predict_motions = Predict::Off;
        d.client.predict_typing = Predict::Off;
        d.effects.scroll.enabled = false;
        d
    }

    fn runs_lua(calls: &[Call], lua: &str) -> bool {
        calls.iter().any(|(_, m, args)| {
            m == "nvim_exec_lua"
                && args
                    .first()
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.starts_with(lua))
        })
    }

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

    #[test]
    fn a_client_told_to_predict_attaches_again_for_the_agent() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);
        let mut d = drawing();
        d.client.predict_scrolls = Predict::Auto;
        d.remote = true;
        a.reconfigure(&d, t0);
        assert!(a.predict.is_some());
        let sent = calls(&mut a);
        request(&sent, "nvim_ui_detach", "");
        assert!(
            !runs_lua(&sent, AGENT_LUA),
            "the agent waits for the attach"
        );
    }

    #[test]
    fn auto_predicts_only_over_ssh_and_on_here_as_well() {
        let t0 = Instant::now();
        let mut d = drawing();
        d.client.predict_scrolls = Predict::Auto;

        let mut here = attached(false, t0);
        here.reconfigure(&d, t0);
        assert!(here.predict.is_none(), "predicting on this machine");
        let sent = calls(&mut here);
        assert!(!sent.iter().any(|(_, m, _)| m == "nvim_ui_detach"));

        d.client.predict_scrolls = Predict::On;
        let mut on = attached(false, t0);
        on.reconfigure(&d, t0);
        assert!(
            on.predict.is_some(),
            "not predicting on this machine, told on"
        );
        request(&calls(&mut on), "nvim_ui_detach", "");

        d.client.predict_scrolls = Predict::Auto;
        d.remote = true;
        let mut there = attached(false, t0);
        there.reconfigure(&d, t0);
        assert!(there.predict.is_some(), "not predicting over ssh");
        request(&calls(&mut there), "nvim_ui_detach", "");
    }

    /// The measure's `smart` argument in `calls`.
    fn measured_smart(calls: &[Call]) -> Option<bool> {
        let (_, _, args) = calls.iter().find(|(_, m, args)| {
            m == "nvim_exec_lua"
                && args
                    .first()
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.starts_with("local most"))
        })?;
        args.get(1)?.as_array()?.get(1)?.as_bool()
    }

    /// Centring changes re-measure or uninstall the measure, never
    /// re-attach.
    #[test]
    fn a_client_told_to_centre_measures_and_told_not_to_stops() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);
        let mut d = drawing();
        d.client.centre = Centre::Smart;
        a.reconfigure(&d, t0);
        let sent = calls(&mut a);
        assert_eq!(measured_smart(&sent), Some(true), "measured: {sent:?}");
        assert!(!sent.iter().any(|(_, m, _)| m == "nvim_ui_detach"));

        d.client.centre = Centre::Fixed;
        a.reconfigure(&d, t0);
        let sent = calls(&mut a);
        assert_eq!(measured_smart(&sent), Some(false), "measured: {sent:?}");
        assert!(!sent.iter().any(|(_, m, _)| m == "nvim_ui_detach"));

        d.client.centre = Centre::Off;
        a.reconfigure(&d, t0);
        let sent = calls(&mut a);
        assert!(runs_lua(&sent, wide::UNMEASURE_LUA), "{sent:?}");
        assert!(!sent.iter().any(|(_, m, _)| m == "nvim_ui_detach"));
    }

    /// Centring alone wants multigrid: at start (lists UIs first) and via
    /// reconfigure (re-attaches with it, and without once turned off).
    #[test]
    fn a_client_centring_wants_windows_of_their_own() {
        let t0 = Instant::now();
        let mut a = app(false).with_centre(Centre::Smart, 8);
        a.start();
        let sent = calls(&mut a);
        asks_uis(&sent);
        assert_eq!(attached_with_multigrid(&sent), None, "attached at once");

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
        let mut d = drawing();
        d.effects.windows.enabled = false;
        d.client.centre = Centre::Smart;
        a.reconfigure(&d, t0);
        let sent = calls(&mut a);
        answer(&mut a, asks_uis(&sent), uis(false, false), t0);
        let sent = calls(&mut a);
        let detach = request(&sent, "nvim_ui_detach", "");
        answer(&mut a, detach, Value::Nil, t0);
        assert_eq!(attached_with_multigrid(&calls(&mut a)), Some(true));

        d.client.centre = Centre::Off;
        a.reconfigure(&d, t0);
        let sent = calls(&mut a);
        let detach = request(&sent, "nvim_ui_detach", "");
        answer(&mut a, detach, Value::Nil, t0);
        assert_eq!(attached_with_multigrid(&calls(&mut a)), Some(false));
    }

    /// Adjacent changed rows are joined by CR LF, not a CUP.
    #[test]
    fn a_client_writes_lean_frames() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);
        a.frame(t0);
        redraw(&mut a, vec![line(1, 1, "ab"), line(1, 2, "cd")], t0);
        let text = String::from_utf8_lossy(&a.frame(t0)).into_owned();
        assert!(text.contains("ab\r\ncd"), "{text:?}");
    }

    #[test]
    fn a_client_moves_the_rows_neovim_scrolled() {
        let t0 = Instant::now();
        let scrolled = |a: &mut App, uncovered: &str| {
            let up = (
                "grid_scroll",
                vec![
                    1.into(),
                    0.into(),
                    5.into(),
                    0.into(),
                    20.into(),
                    1.into(),
                    0.into(),
                ],
            );
            redraw(a, vec![up, line(1, 4, uncovered)], t0);
            String::from_utf8_lossy(&a.frame(t0)).into_owned()
        };
        let drawn = |a: App| {
            let mut a = a;
            let lines = (0..5).map(|r| line(1, r, &format!("line {r}"))).collect();
            redraw(&mut a, lines, t0);
            a.frame(t0);
            a
        };
        // Moved rows rewritten: a CUP to column 6, where `line N` differs.
        let rewritten = |text: &str| {
            (1..=4)
                .filter(|r| text.contains(&format!("\x1b[{r};6H")))
                .count()
        };
        let mut a = drawn(attached(false, t0));
        let text = scrolled(&mut a, "line 5");
        assert!(
            text.contains("\x1b[0m\x1b[1;5r\x1b[M\x1b[r\x1b[5;1H"),
            "{text:?}"
        );
        assert!(text.contains("line 5"), "{text:?}");
        assert_eq!(rewritten(&text), 0, "a row moved is written: {text:?}");

        let mut b = drawn(attached(false, t0).with_scroll_regions(false));
        let text = scrolled(&mut b, "line 5");
        assert!(!text.contains("\x1b[M"), "{text:?}");
        assert_eq!(rewritten(&text), 4, "{text:?}");
    }

    /// Partial frames need two prior frames (shown and spare); placing a
    /// window forces one whole frame.
    #[test]
    fn a_frame_after_a_line_is_put_together_from_its_row_alone() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);
        let lines = (0..5).map(|r| line(1, r, &format!("line {r}"))).collect();
        redraw(&mut a, lines, t0);
        a.frame(t0);
        a.frame(t0);
        let partly = |a: &App| a.partial.as_ref().map_or(0, |p| p.partly);
        let rows = |a: &App| a.partial.as_ref().map(|p| p.rows.clone());
        assert_eq!(partly(&a), 0, "two frames whole");

        redraw(&mut a, vec![line(1, 2, "typed")], t0);
        let text = String::from_utf8_lossy(&a.frame(t0)).into_owned();
        assert_eq!(partly(&a), 1, "{text:?}");
        assert_eq!(
            rows(&a),
            Some(vec![false, false, true, false, false, false])
        );
        assert!(text.contains("typed"), "{text:?}");

        redraw(
            &mut a,
            vec![resize(2, 20, 2), place(2, 1000, 0, 0, 20, 2)],
            t0,
        );
        a.frame(t0);
        assert_eq!(partly(&a), 1, "a window placed");
        let t1 = t0 + Duration::from_secs(1);
        a.frame(t1);
        assert_eq!(partly(&a), 2, "nothing changed");
        assert_eq!(rows(&a), Some(vec![false; 6]));
        redraw(&mut a, vec![line(2, 0, "in the window")], t1);
        a.frame(t1);
        assert_eq!(partly(&a), 3);
        assert_eq!(
            rows(&a),
            Some(vec![true, false, false, false, false, false])
        );
    }

    /// Deterministic xorshift walk.
    struct Walk(u64);

    impl Walk {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A multigrid client on a 30×12 terminal, with scroll regions and
    /// partial frames as given.
    fn thirty_by_twelve(partial: bool, effects: Effects, regions: bool, t0: Instant) -> App {
        let mut a = App::new(palette(), effects, false, (30, 12))
            .with_scroll_regions(regions)
            .with_partial_frames(partial);
        a.start();
        let sent = calls(&mut a);
        answer(
            &mut a,
            request(&sent, "nvim_get_api_info", ""),
            Value::Array(vec![7.into()]),
            t0,
        );
        answer(&mut a, asks_uis(&sent), uis(false, false), t0);
        calls(&mut a);
        a
    }

    /// Effects by `seed % 4`: 1 windows only, 2 adds scroll, 3 adds smear,
    /// 0 has scroll and blink.
    fn walk_effects(seed: u64) -> Effects {
        let mut s = crate::config::Settings::default();
        let turn = seed % 4;
        s.effects.scroll.enabled = turn != 1;
        s.effects.smear.enabled = turn == 3;
        s.effects.blink.enabled = turn == 0;
        Effects::from_settings(&s)
    }

    /// (grid, width, height): grid 1, two windows, a float, the message area.
    const WALK_GRIDS: [(u64, u64, u64); 5] =
        [(1, 30, 12), (2, 30, 5), (3, 30, 4), (4, 10, 3), (5, 30, 2)];

    fn line_hl(grid: u64, row: u64, col: u64, text: &str, hl: u64) -> (&'static str, Vec<Value>) {
        let cells = text
            .chars()
            .enumerate()
            .map(|(i, ch)| {
                let mut cell = vec![Value::from(ch.to_string())];
                if i == 0 {
                    cell.push(hl.into());
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

    /// [`random_batch`]'s starting editor: all grids placed and drawn, a few
    /// highlights, a blinking block cursor and an Insert-mode bar.
    fn walk_start() -> Vec<(&'static str, Vec<Value>)> {
        let mut events = Vec::new();
        for id in 1..5u64 {
            events.push(hl_define(id, 0x10_2030 * id, 0x30_2010 * id, id == 3, 0));
        }
        let mode = |name: &str, shape: &str| {
            Value::Map(vec![
                ("name".into(), name.into()),
                ("cursor_shape".into(), shape.into()),
                ("blinkwait".into(), 300.into()),
                ("blinkon".into(), 200.into()),
                ("blinkoff".into(), 150.into()),
            ])
        };
        let modes = vec![mode("normal", "block"), mode("insert", "vertical")];
        events.push(("mode_info_set", vec![true.into(), Value::Array(modes)]));
        events.push(("mode_change", vec!["normal".into(), 0.into()]));
        for &(grid, width, height) in &WALK_GRIDS {
            events.push(resize(grid, width, height));
        }
        events.push(place(2, 1000, 0, 0, 30, 5));
        events.push(place(3, 1001, 6, 0, 30, 4));
        events.push(float_at(4, 2, 8, 50));
        events.push(msg_at(5, 10, false));
        for &(grid, width, height) in &WALK_GRIDS {
            for r in 0..height {
                let text: String = (0..width)
                    .map(|c| ['a', 'b', ' ', '~'][((grid + r + c) % 4) as usize])
                    .collect();
                events.push(line_hl(grid, r, 0, &text, (grid + r) % 5));
            }
        }
        events
    }

    fn hl_define(id: u64, fg: u64, bg: u64, bold: bool, blend: u64) -> (&'static str, Vec<Value>) {
        let rgb = Value::Map(vec![
            ("foreground".into(), fg.into()),
            ("background".into(), bg.into()),
            ("bold".into(), bold.into()),
            ("blend".into(), blend.into()),
        ]);
        (
            "hl_attr_define",
            vec![id.into(), rgb, Value::Map(vec![]), Value::Array(vec![])],
        )
    }

    fn float_at(grid: u64, row: i64, col: i64, zindex: i64) -> (&'static str, Vec<Value>) {
        (
            "win_float_pos",
            vec![
                grid.into(),
                1002.into(),
                "NW".into(),
                1.into(),
                row.into(),
                col.into(),
                true.into(),
                zindex.into(),
                1.into(),
                row.into(),
                col.into(),
            ],
        )
    }

    fn msg_at(grid: u64, row: u64, scrolled: bool) -> (&'static str, Vec<Value>) {
        (
            "msg_set_pos",
            vec![
                grid.into(),
                row.into(),
                scrolled.into(),
                "-".into(),
                200.into(),
                2.into(),
            ],
        )
    }

    fn random_batch(w: &mut Walk) -> Vec<(&'static str, Vec<Value>)> {
        let mut events = Vec::new();
        for _ in 0..1 + w.below(3) {
            let (grid, width, height) = WALK_GRIDS[w.below(5) as usize];
            match w.below(28) {
                0..=1 if width > 2 => {
                    // A double-width character, which a float's edge may cut.
                    let (row, col) = (w.below(height), w.below(width - 1));
                    let cells = vec![
                        Value::Array(vec!["中".into(), w.below(5).into()]),
                        Value::Array(vec!["".into()]),
                    ];
                    events.push((
                        "grid_line",
                        vec![
                            grid.into(),
                            row.into(),
                            col.into(),
                            Value::Array(cells),
                            false.into(),
                        ],
                    ));
                }
                0..=13 => {
                    let row = w.below(height);
                    let col = w.below(width);
                    let n = 1 + w.below(8).min(width - col - 1);
                    let text: String = (0..n)
                        .map(|_| ['a', 'b', ' ', '~', 'z'][w.below(5) as usize])
                        .collect();
                    events.push(line_hl(grid, row, col, &text, w.below(5)));
                }
                14..=16 if height > 2 => {
                    let top = w.below(height - 1);
                    let bot = top + 2 + w.below(height - top - 1);
                    let bot = bot.min(height);
                    let rows = 1 + w.below(bot - top - 1);
                    let rows = if w.below(2) == 0 {
                        rows as i64
                    } else {
                        -(rows as i64)
                    };
                    events.push((
                        "grid_scroll",
                        vec![
                            grid.into(),
                            top.into(),
                            bot.into(),
                            0.into(),
                            width.into(),
                            rows.into(),
                            0.into(),
                        ],
                    ));
                    let fresh = if rows > 0 {
                        bot - rows as u64..bot
                    } else {
                        top..top + rows.unsigned_abs()
                    };
                    for r in fresh {
                        events.push(line_hl(grid, r, 0, &"x".repeat(width as usize), w.below(5)));
                    }
                }
                17 => events.push(("grid_clear", vec![grid.into()])),
                18 => {
                    let row = w.below(7);
                    events.push(place(2, 1000, row, 0, 30, 5));
                }
                // Sometimes past grid 1's edge, on a terminal wider than it.
                19 => events.push(float_at(
                    4,
                    w.below(10) as i64,
                    w.below(25) as i64,
                    40 + w.below(20) as i64,
                )),
                20 => events.push(hl_define(
                    1 + w.below(4),
                    w.next() & 0xff_ffff,
                    w.next() & 0xff_ffff,
                    w.below(2) == 0,
                    w.below(3) * 30,
                )),
                21 | 22 => events.push((
                    "grid_cursor_goto",
                    vec![grid.into(), w.below(height).into(), w.below(width).into()],
                )),
                23 => events.push((
                    "mode_change",
                    vec![
                        ["normal", "insert"][w.below(2) as usize].into(),
                        w.below(2).into(),
                    ],
                )),
                24 => {
                    if w.below(2) == 0 {
                        events.push(("win_hide", vec![3.into()]));
                    } else {
                        events.push(place(3, 1001, 6, 0, 30, 4));
                    }
                }
                25 if w.below(3) == 0 => events.push(msg_at(5, 9 + w.below(2), w.below(2) == 0)),
                25 if w.below(2) == 0 => events.push((
                    "option_set",
                    vec!["termguicolors".into(), (w.below(2) == 0).into()],
                )),
                26 | 27 => {
                    // A window scroll as Neovim does it: viewport, grid
                    // scroll, then the new rows.
                    let (grid, win, height) = [(2, 1000, 5), (3, 1001, 4)][w.below(2) as usize];
                    let rows = 1 + w.below(height - 1) as i64;
                    let rows = if w.below(2) == 0 { rows } else { -rows };
                    let view = [10, 10 + height as i64, 10, 0, 100, rows];
                    let mut args = vec![grid.into(), win.into()];
                    args.extend(view.map(Value::from));
                    events.push(("win_viewport", args));
                    events.push((
                        "grid_scroll",
                        vec![
                            grid.into(),
                            0.into(),
                            height.into(),
                            0.into(),
                            30.into(),
                            rows.into(),
                            0.into(),
                        ],
                    ));
                    let fresh = if rows > 0 {
                        height - rows as u64..height
                    } else {
                        0..rows.unsigned_abs()
                    };
                    for r in fresh {
                        events.push(line_hl(grid, r, 0, &"y".repeat(30), w.below(5)));
                    }
                }
                _ => events.push((
                    "default_colors_set",
                    vec![
                        (w.next() & 0xff_ffff).into(),
                        (w.next() & 0xff_ffff).into(),
                        (-1).into(),
                        0.into(),
                        0.into(),
                    ],
                )),
            }
        }
        events
    }

    /// Random walk ([`random_batch`]) with resizes, cards and both scroll
    /// modes. Hundreds of frames must be partial for every effect set but
    /// blink, which paints over every frame.
    #[test]
    fn partial_frames_write_what_whole_frames_write() {
        let mut partly = [0; 4];
        for seed in 1..=8u64 {
            let mut w = Walk(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut t = Instant::now();
            let regions = seed > 4;
            let mut whole = thirty_by_twelve(false, walk_effects(seed), regions, t);
            let mut part = thirty_by_twelve(true, walk_effects(seed), regions, t);
            redraw(&mut whole, walk_start(), t);
            redraw(&mut part, walk_start(), t);
            for step in 0..1000 {
                let batch = random_batch(&mut w);
                redraw(&mut whole, batch.clone(), t);
                redraw(&mut part, batch, t);
                if w.below(3) != 0 {
                    let (a, b) = (whole.frame(t), part.frame(t));
                    assert_eq!(
                        String::from_utf8_lossy(&a),
                        String::from_utf8_lossy(&b),
                        "seed {seed}, step {step}"
                    );
                }
                if w.below(40) == 0 {
                    let size = [(30, 12), (28, 11), (32, 13)][w.below(3) as usize];
                    whole.resized(size);
                    part.resized(size);
                }
                if w.below(100) == 0 {
                    // A short-lived card (one leaving a mark would force
                    // whole frames from then on).
                    let card = crate::card::Card {
                        life_ms: 600,
                        fade_ms: 0,
                        ..crate::card::Card::news(
                            step,
                            crate::card::Kind::Finished,
                            "s",
                            "done",
                            3,
                            "",
                        )
                    }
                    .leaving(None);
                    let show = crate::card::encode(&crate::card::Command::Show(card));
                    whole.keys(&show, t);
                    part.keys(&show, t);
                }
                t += Duration::from_millis(if w.below(10) == 0 {
                    400
                } else {
                    5 + w.below(40)
                });
            }
            partly[(seed % 4) as usize] += part.partial.as_ref().map_or(0, |p| p.partly);
        }
        assert!(
            partly[1..].iter().all(|&n| n > 150),
            "{partly:?} frames put together in part"
        );
    }

    /// Feeds `events` and `keys` to both clients, asserts equal frames, and
    /// returns `part`'s partial frame count.
    fn both(
        whole: &mut App,
        part: &mut App,
        events: Vec<(&'static str, Vec<Value>)>,
        keys: &[u8],
        t: Instant,
    ) -> usize {
        for a in [&mut *whole, &mut *part] {
            if !events.is_empty() {
                redraw(a, events.clone(), t);
            }
            a.keys(keys, t);
        }
        let (a, b) = (whole.frame(t), part.frame(t));
        assert_eq!(String::from_utf8_lossy(&a), String::from_utf8_lossy(&b));
        part.partial.as_ref().map_or(0, |p| p.partly)
    }

    /// A view scrolled ahead (down or sideways) is not the editor's grids as
    /// placed, though nothing moves, so frames are whole until Neovim
    /// catches up or the view reverts.
    #[test]
    fn frames_ahead_of_neovim_are_put_together_whole() {
        let rtt = Duration::from_millis(200);
        let mut whole = predicting(rtt).with_partial_frames(false);
        let mut part = predicting(rtt);
        let t1 = Instant::now() + Duration::from_secs(1);
        let ms = |n: u64| t1 + Duration::from_millis(n);
        let status = |text: &str| vec![line(1, 5, text)];
        both(&mut whole, &mut part, vec![], b"", t1);
        both(&mut whole, &mut part, vec![], b"", t1);
        assert_eq!(both(&mut whole, &mut part, status("one"), b"", ms(10)), 1);

        // Down three, ahead of Neovim.
        assert_eq!(
            both(&mut whole, &mut part, vec![], b"\x1b[<65;3;2M", ms(20)),
            1
        );
        assert_ne!(part.anim.ahead(2), (0, 0));
        assert_eq!(top_row(&part), "line 3");
        assert_eq!(both(&mut whole, &mut part, status("two"), b"", ms(30)), 1);
        // Neovim's scroll, when it comes.
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
        both(&mut whole, &mut part, events, b"", ms(220));
        assert_eq!(part.anim.ahead(2), (0, 0));
        assert_eq!(
            both(&mut whole, &mut part, status("three"), b"", ms(230)),
            2
        );

        // Sideways, ahead of Neovim.
        assert_eq!(
            both(&mut whole, &mut part, vec![], b"\x1b[<67;3;2M", ms(240)),
            2
        );
        assert_eq!(part.anim.ahead(2), (0, 0));
        assert!(part.anim.side_left(2).is_some());
        assert_eq!(both(&mut whole, &mut part, status("four"), b"", ms(250)), 2);
        // Neovim never moved it: the view goes back.
        both(&mut whole, &mut part, vec![], b"", ms(5000));
        assert_eq!(part.anim.side_left(2), None);
        assert_eq!(
            both(&mut whole, &mut part, status("five"), b"", ms(5010)),
            3
        );
    }

    #[test]
    fn settings_in_the_input_reconfigure_the_client() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);
        let mut d = drawing();
        d.client.centre = Centre::Smart;
        a.keys(&crate::card::encode(&crate::card::Command::Settings(d)), t0);
        assert_eq!(a.centre, Centre::Smart);
        assert!(runs_lua(&calls(&mut a), "local most"));
    }

    /// See [`App::dirty_due`]: both `'showcmd'` flushes are drawn at once,
    /// a burst after them is limited by [`MIN_FRAME_GAP`].
    #[test]
    fn what_a_key_did_is_drawn_at_once_after_a_quiet_spell() {
        let t0 = Instant::now();
        let mut a = attached(true, t0);
        a.frame(t0);
        // After a quiet spell, the key shown...
        let shown = t0 + Duration::from_secs(1);
        redraw(&mut a, vec![line(1, 0, "k")], shown);
        assert!(a.due(shown), "the key shown, at once");
        a.frame(shown);
        // ...and its effect 0.5 ms later, also at once.
        let did = shown + Duration::from_micros(500);
        redraw(&mut a, vec![line(1, 1, "did")], did);
        assert_eq!(a.wake_at(did), Some(did));
        assert!(a.due(did), "what the key did, at once");
        a.frame(did);
        // A burst from there is limited by the gap.
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

    #[test]
    fn frames_drawn_ahead_do_not_pace_neovims_and_are_not_paced_by_them() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.frame(t1);
        // Neovim's two flushes for a key after a quiet spell, 0.5 ms apart.
        let shown = t1 + Duration::from_millis(100);
        redraw(&mut a, vec![line(1, 5, "x")], shown);
        assert!(a.due(shown), "the key shown, at once");
        a.frame(shown);
        let did = shown + Duration::from_micros(500);
        redraw(&mut a, vec![line(1, 5, " ")], did);
        assert!(a.due(did), "what it did, at once");
        a.frame(did);
        // A key drawn ahead 1 ms later: at once, though the last two frames
        // were 0.5 ms apart.
        let key = did + Duration::from_millis(1);
        a.keys(b"l", key);
        assert_eq!(a.wake_at(key), Some(key), "the key's frame, at once");
        assert!(a.due(key));
        a.frame(key);
        // Neovim's flushes for it: 15.5 ms after the key's frame but 16.5 ms
        // (over a frame's time) after Neovim's last, so both at once.
        let shown = key + Duration::from_micros(15_500);
        redraw(&mut a, vec![line(1, 5, "l")], shown);
        assert!(a.due(shown), "the key shown, at once");
        a.frame(shown);
        let did = shown + Duration::from_micros(500);
        ack(&mut a, 1, "l", "n", (0, 1, 1, 0), did);
        let mut events = cursor_to(0, 1);
        events.push(line(1, 5, " "));
        redraw(&mut a, events, did);
        assert_eq!(a.wake_at(did), Some(did), "what it did, at once");
        assert!(a.due(did));
        a.frame(did);
        // A burst from there is limited by the gap.
        let more = did + Duration::from_micros(500);
        redraw(&mut a, vec![line(1, 5, "m")], more);
        assert!(!a.due(more), "the third, held");
        assert_eq!(a.wake_at(more), Some(did + MIN_FRAME_GAP));
    }

    /// Frames when a card arrives, its age ticks or its words go; none for
    /// the static mark it leaves.
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
        // Its life runs from the frame it was first drawn on.
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

    fn told(a: &mut App, cmd: &crate::card::Command, now: Instant) {
        a.keys(&crate::card::encode(cmd), now);
    }

    fn goto(row: u64, col: u64) -> (&'static str, Vec<Value>) {
        ("grid_cursor_goto", vec![1.into(), row.into(), col.into()])
    }

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
        // A change after a quiet gap is drawn at once.
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
        // A key's two flushes after a quiet spell: both at once.
        let shown = front + Duration::from_millis(100);
        redraw(&mut a, vec![line(1, 4, "k")], shown);
        assert!(a.due(shown));
        a.frame(shown);
        let did = shown + Duration::from_micros(500);
        redraw(&mut a, vec![line(1, 5, "did")], did);
        assert!(a.due(did), "drawn as in front");
    }

    /// Coming back always yields a frame, at least the cursor, which nvmux
    /// waits for (see `crate::pty::relay`). A Clear while in front with no
    /// card draws nothing.
    #[test]
    fn a_client_back_from_away_draws_at_once_whatever_changed() {
        use crate::card::Command::{Away, Clear};
        let t0 = Instant::now();
        let mut a = attached(true, t0);
        redraw(&mut a, vec![line(1, 0, "here"), goto(0, 2)], t0);
        a.frame(t0);
        // Away with nothing left to draw.
        let gone = t0 + AWAY_FRAME_GAP;
        told(&mut a, &Away, gone);
        assert!(a.due(gone));
        a.frame(gone);
        assert!(!a.due(gone + 3 * AWAY_FRAME_GAP), "nothing to draw");

        let front = gone + Duration::from_millis(200);
        told(&mut a, &Clear, front);
        assert!(a.due(front), "at once");
        let text = String::from_utf8_lossy(&a.frame(front)).into_owned();
        assert!(text.contains("\x1b[1;3H\x1b[?25h"), "{text:?}");
        assert!(!text.contains("here"), "nothing else: {text:?}");

        // Away again; a change held by the crawl is drawn on return.
        let again = front + Duration::from_secs(5);
        told(&mut a, &Away, again);
        a.frame(again);
        let drew = again + Duration::from_millis(100);
        redraw(&mut a, vec![line(1, 1, "since")], drew);
        let back = again + Duration::from_millis(300);
        assert!(!a.due(back), "held at a crawl");
        told(&mut a, &Clear, back);
        assert!(a.due(back));
        let text = String::from_utf8_lossy(&a.frame(back)).into_owned();
        assert!(text.contains("since"), "{text:?}");

        let later = back + Duration::from_secs(2);
        told(&mut a, &Clear, later);
        assert!(!a.due(later), "in front");
    }

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

    /// A card up when going away (e.g. a switch's) stops waking the client
    /// or drawing frames; it is cleared on return.
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
        // The first move after the stop only places the cursor; the next
        // would travel.
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

    /// A buffer change in a known window is marked for the next flush; an
    /// unknown window is new, not a switch.
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

    #[test]
    fn the_agents_options_are_read() {
        let said = [Value::from(3), Value::from(true), Value::from(false)];
        let refs: Vec<ValueRef<'_>> = said.iter().map(Value::as_ref).collect();
        let o = options(&refs);
        assert_eq!((o.laststatus, o.splitright, o.splitbelow), (3, true, false));
        assert_eq!(options(&[]), anim::layout::Options::default());
    }

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
        // Without anything needing multigrid: at once, without it.
        let mut a = app(false);
        a.start();
        assert_eq!(attached_with_multigrid(&calls(&mut a)), Some(false));
    }

    /// Only cells changed since the other UI saw them arrive; refreshed like
    /// an empty window.
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

    /// Windows arrive unplaced and empty: drawing waits for the layout and a
    /// refresh.
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

    /// On a terminal taller or wider than the last UI's.
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

            // The refresh's redraw: every cell.
            redraw(&mut a, whole(2, 20, 5, &["kept from before"]), t0);
            assert!(a.due(t0), "{grew}: drawn once it is whole");
            assert!(!a.take_refresh(), "{grew}: nothing lacking now");
            assert_eq!(top_row(&a), "kept from before", "{grew}");
        }
    }

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

    /// A re-attach requested during a hold waits for the detach's answer
    /// without a past wake time, which would busy-poll the loop.
    #[test]
    fn a_detach_answered_late_is_waited_for_without_spinning() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);
        // The UIs changed and the listing is pending: the hold begins.
        redraw(&mut a, vec![ui_change()], t0);
        calls(&mut a);
        // Told to predict meanwhile, it detaches to re-attach.
        let mut d = drawing();
        d.client.predict_scrolls = Predict::Auto;
        d.remote = true;
        a.reconfigure(&d, t0 + Duration::from_millis(100));
        let sent = calls(&mut a);
        let detach = request(&sent, "nvim_ui_detach", "");
        assert_eq!(
            a.wake_at(t0 + Duration::from_millis(100)),
            Some(t0 + HOLD_LIMIT)
        );
        let late = t0 + 2 * HOLD_LIMIT;
        assert!(!a.due(late));
        assert!(
            a.wake_at(late).is_none_or(|at| at > late),
            "woken at once, for a hold long over: {:?}",
            a.wake_at(late).map(|at| late.saturating_duration_since(at))
        );
        answer(&mut a, detach, Value::Nil, late);
        assert_eq!(attached_with_multigrid(&calls(&mut a)), Some(true));
    }

    /// Redraws arriving after the detach request belong to the old attach
    /// and are ignored.
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

    /// Once settled, window places are asked again.
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

    /// Its windows are placed on grids the client was never sent.
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

    /// A predicting multigrid client with no effects and round trip `rtt`,
    /// agent installed: one 20 × 5 window over 50 lines, `line 0` at the
    /// top with the cursor on it, Normal mode, mouse on.
    fn predicting(rtt: Duration) -> App {
        predicting_as(Predictions::default(), rtt).0
    }

    /// [`predicting`] with `predictions`; also returns the agent's
    /// unanswered request id.
    fn predicting_as(predictions: Predictions, rtt: Duration) -> (App, u64) {
        let t0 = Instant::now();
        let mut a =
            App::new(palette(), Effects::none(), true, (20, 6)).with_predictions(predictions);
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
        (a, agent)
    }

    /// `win_viewport` for [`predicting`]'s grid 2.
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

        // Up past the top stops there.
        let t2 = t1 + Duration::from_millis(300);
        a.keys(b"\x1b[<64;3;2M\x1b[<64;3;2M", t2);
        assert_eq!(top_row(&a), "line 0");
        assert_eq!(a.anim.ahead(2), (-3, 0));

        // Ctrl: a page (three of five rows), the cursor to the top.
        a.keys(b"\x1b[<81;3;2M", t2);
        assert_eq!(top_row(&a), "line 3");
        assert_eq!(a.anim.cursor_at(&a.model), (0, 0));
    }

    /// The agent's `curswant`, not the clamped column on a short line; the
    /// current column if none was sent.
    #[test]
    fn a_scroll_lands_the_cursor_in_the_column_it_wants() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let land = |want: Option<i64>| {
            let mut a = predicting(Duration::from_millis(200));
            // `line 0` short, the cursor at its end; the rest long.
            let text: Vec<String> = (0..20)
                .map(|n| match n {
                    0 => "line 0".to_string(),
                    _ => format!("line {n} and on and on"),
                })
                .collect();
            let lines = vec![
                "lines".into(),
                1000.into(),
                1.into(),
                1.into(),
                0.into(),
                Value::Array(text.iter().map(|t| Value::from(t.as_str())).collect()),
                true.into(),
            ];
            notify(&mut a, "nvmux_client", lines, t1);
            let mut events = whole(2, 20, 5, &text[..5]);
            events.push(("grid_cursor_goto", vec![2.into(), 0.into(), 5.into()]));
            redraw(&mut a, events, t1);
            if let Some(col) = want {
                notify(
                    &mut a,
                    "nvmux_client",
                    vec!["want".into(), 1000.into(), col.into()],
                    t1,
                );
            }
            // Wheel three lines down; the cursor moves with the top.
            a.keys(b"\x1b[<65;3;2M", t1);
            assert_eq!(top_row(&a), "line 3 and on and on");
            a.anim.cursor_at(&a.model)
        };
        assert_eq!(land(Some(12)), (0, 12), "where it wants to be");
        assert_eq!(land(Some(2_147_483_647)), (0, 19), "as `$` has it");
        assert_eq!(land(None), (0, 5), "where it is, as far as is known");
    }

    /// Without 'wrap', from column 8: drawn with the sideways move only when
    /// it is the sole scroll ahead, else left to Neovim; Neovim's frame then
    /// moves nothing.
    #[test]
    fn a_scroll_landing_out_of_view_moves_the_view_sideways() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let from = |left: usize, text: &[String]| -> Vec<String> {
            text.iter()
                .map(|t| t.chars().skip(left).collect())
                .collect()
        };
        // `abc` on line `short`, the cursor on line 0, column 10.
        let sideways = |short: i64| {
            let mut a = predicting(Duration::from_millis(200));
            let text: Vec<String> = (0..20)
                .map(|n| match n {
                    n if n == short => "abc".to_string(),
                    _ => format!("line {n:02} and on and on and on"),
                })
                .collect();
            let shape: Vec<Value> = vec![
                1.into(),
                50.into(),
                0.into(),
                8.into(),
                false.into(),
                false.into(),
                true.into(),
                false.into(),
                8.into(),
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
            let lines = vec![
                "lines".into(),
                1000.into(),
                1.into(),
                1.into(),
                0.into(),
                Value::Array(text.iter().map(|t| Value::from(t.as_str())).collect()),
                true.into(),
            ];
            notify(
                &mut a,
                "nvmux_client",
                vec!["shape".into(), 1000.into(), Value::Array(shape)],
                t1,
            );
            notify(&mut a, "nvmux_client", lines, t1);
            let mut events = whole(2, 20, 5, &from(8, &text[..5]));
            events.push(("grid_cursor_goto", vec![2.into(), 0.into(), 2.into()]));
            redraw(&mut a, events, t1);
            let want = vec!["want".into(), 1000.into(), 10.into()];
            notify(&mut a, "nvmux_client", want, t1);
            (a, text)
        };
        let wheel = |a: &mut App| a.keys(b"\x1b[<65;3;2M", t1);

        // Down three to `abc`: its `c` is six columns left of the view, which
        // moves that far.
        let (mut a, text) = sideways(3);
        wheel(&mut a);
        assert_eq!(a.anim.ahead(2), (3, 3));
        assert_eq!(a.anim.side_left(2), Some(2));
        assert_eq!(top_row(&a), "c");
        assert_eq!(a.anim.cursor_at(&a.model), (0, 0), "on its `c`");
        wheel(&mut a);
        assert_eq!(a.anim.ahead(2), (3, 3), "the next waits for Neovim");
        // A Neovim frame before the scroll's keeps showing the moved rows.
        redraw(&mut a, vec![], t1);
        assert_eq!(top_row(&a), "c");
        // Neovim's frame, its leftcol sent just before.
        let left = vec!["leftcol".into(), 1000.into(), 2.into()];
        notify(&mut a, "nvmux_client", left, t1);
        let mut events = vec![at(3, 3, 3)];
        events.extend(whole(2, 20, 5, &from(2, &text[3..8])));
        events.push(("grid_cursor_goto", vec![2.into(), 0.into(), 0.into()]));
        redraw(&mut a, events, t1);
        assert_eq!((a.anim.ahead(2), a.anim.side_left(2)), ((0, 0), None));
        assert_eq!(top_row(&a), "c");
        assert_eq!(a.anim.cursor_at(&a.model), (0, 0));

        // Down three in view, then three more to `abc` behind it: dropped.
        let (mut a, _) = sideways(6);
        wheel(&mut a);
        wheel(&mut a);
        assert_eq!(a.anim.ahead(2), (3, 3));
        assert_eq!(a.anim.side_left(2), None);
        assert_eq!(top_row(&a), "and on and on and on", "line 3, from column 8");
    }

    /// Only once Neovim has caught up with every earlier key.
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
        // After a cursor key the scroll waits for Neovim, and a fence
        // follows both.
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

    #[test]
    fn a_turn_of_the_wheel_waits_where_it_cannot_be_predicted() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = predicting(Duration::from_millis(200));
        let mut a2 = predicting(Duration::from_millis(200));
        // A view that is not one line per row (a wrapped line).
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

    /// Each `behind` extends the deadline as for a scroll made then, and the
    /// loop wakes for it. Without one, a scroll is given up at its usual
    /// deadline (a little over two round trips).
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

    #[test]
    fn the_round_trip_is_measured_while_at_work() {
        let mut a = predicting(Duration::from_millis(200));
        let t1 = Instant::now() + Duration::from_secs(10);
        a.tick(t1);
        assert!(calls(&mut a).iter().all(|(_, m, _)| m != PING), "idle");
        a.keys(b"j", t1);
        a.tick(t1);
        let ping = request(&calls(&mut a), PING, "");
        answer(&mut a, ping, Value::Nil, t1 + Duration::from_millis(1));
        a.tick(t1 + PING_EVERY);
        assert!(
            calls(&mut a).iter().all(|(_, m, _)| m != PING),
            "nothing typed since"
        );
        // The link speeds up to 1 ms.
        let mut t = t1;
        for _ in 0..40 {
            t += PING_EVERY;
            a.keys(b"j", t);
            a.tick(t);
            let ping = request(&calls(&mut a), PING, "");
            answer(&mut a, ping, Value::Nil, t + Duration::from_millis(1));
        }
        let p = a.predict.as_ref().expect("predicting");
        assert_eq!(p.reach(), 3, "a fast link now: a page each way, and two");
    }

    /// The reach ([`Predictor::reach`]) is sent at install and only when it
    /// changes.
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

        // Reaches sent after each measurement.
        let mut t = t0 + Duration::from_secs(1);
        let mut measure = |a: &mut App, rtt: Duration| {
            t += PING_EVERY;
            a.keys(b"j", t);
            a.tick(t);
            let ping = request(&calls(a), PING, "");
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
        // 1 s, smoothed to 300 ms: fifteen pages.
        assert_eq!(measure(&mut a, Duration::from_secs(1)), [17]);
        // Down to instant: never below 3 (one page, rounded up, and two).
        let mut told = Vec::new();
        for _ in 0..60 {
            told.extend(measure(&mut a, Duration::ZERO));
        }
        assert_eq!(told.last(), Some(&3));
        assert!(
            told.windows(2).all(|w| w[0] > w[1]),
            "only as it moved: {told:?}"
        );
    }

    #[test]
    fn each_kind_is_drawn_ahead_however_fast_the_link() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let only = |scrolls, motions, typing| Predictions {
            scrolls,
            motions,
            typing,
        };
        let (mut a, _) = predicting_as(only(true, false, false), Duration::ZERO);
        assert_eq!(a.reach, 2, "lines kept from the first");
        a.keys(b"\x1b[<65;3;2M", t1);
        assert_eq!(top_row(&a), "line 3", "a turn of the wheel");

        let mut a = stepping_as(only(false, true, false), Duration::ZERO, &[]);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 1), "a motion");

        let mut a = stepping_as(only(false, false, true), Duration::ZERO, &[]);
        facts(&mut a, |_| {}, t1);
        ack_i(&mut a, 0, "", 0, 1, t1);
        redraw(&mut a, typed_frame("line 0", 0), t1);
        a.keys(b"abc", t1);
        assert_eq!(composed(&mut a)[0], "abcline 0", "what is typed");
    }

    #[test]
    fn a_link_grown_fast_goes_on_predicting() {
        let mut a = predicting(Duration::from_millis(200));
        let mut t = Instant::now() + Duration::from_secs(1);
        for _ in 0..60 {
            t += PING_EVERY;
            a.keys(b"j", t);
            a.tick(t);
            let ping = request(&calls(&mut a), PING, "");
            answer(&mut a, ping, Value::Nil, t);
            calls(&mut a);
        }
        let p = a.predict.as_ref().expect("predicting");
        assert_eq!(a.reach, 3, "as far as a fast link pages");
        assert_eq!(p.reach(), 3);
        assert!(p.shape(1000).is_some(), "the window's shape kept");
        a.keys(b"\x1b[<65;3;2M", t);
        assert_eq!(top_row(&a), "line 3");
    }

    /// [`predicting`] with acks: agent answered, counts tied by the first
    /// fence, and an initial ack (no keys, cursor at `line 0` start) taken.
    fn acking(rtt: Duration) -> App {
        acking_as(MOTIONS, rtt)
    }

    fn acking_as(predictions: Predictions, rtt: Duration) -> App {
        let t1 = Instant::now() + Duration::from_secs(1);
        let (mut a, agent) = predicting_as(predictions, rtt);
        let wins = Value::Array(vec![Value::Array(vec![1000.into(), 1.into()])]);
        let options = Value::Array(vec![2.into(), false.into(), false.into(), wins]);
        answer(&mut a, agent, options, t1);
        a.tick(t1);
        let fence = request(&calls(&mut a), "nvim_eval", "luaeval");
        answer(&mut a, fence, Value::from(0), t1);
        ack(&mut a, 0, "", "n", (0, 0, 0, 0), t1);
        assert_eq!(base(&a).map(|b| b.seq), Some(0), "the counts tied");
        a
    }

    /// An `ack` for [`predicting`]'s window (buffer 1, tick 1, 50 lines):
    /// `count` keys, the last `tail`, `mode`, cursor at 0-based `line`, byte
    /// `col`, curswant `want` (-1 for end of line), view from `top`, and the
    /// cursor line's width as the client lays it out, if it has it.
    fn ack(
        a: &mut App,
        count: u64,
        tail: &str,
        mode: &str,
        (line, col, want, top): (i64, i64, i64, i64),
        now: Instant,
    ) {
        let width = a.predict.as_ref().and_then(|p| {
            let width = predict::width(p.text(1000, line)?, p.shape(1000)?);
            i64::try_from(width).ok()
        });
        let params: Vec<Value> = vec![
            "ack".into(),
            count.into(),
            tail.into(),
            mode.into(),
            1000.into(),
            1.into(),
            1.into(),
            50.into(),
            line.into(),
            col.into(),
            want.into(),
            top.into(),
            0.into(),
            width.unwrap_or(-1).into(),
        ];
        notify(a, "nvmux_client", params, now);
    }

    /// Neovim's frame for the cursor at 0-based `line`, byte `col`, view
    /// from line 0.
    fn cursor_to(line: i64, col: i64) -> Vec<(&'static str, Vec<Value>)> {
        let n = Value::from;
        vec![
            ("grid_cursor_goto", vec![2.into(), n(line), n(col)]),
            (
                "win_viewport",
                vec![
                    2.into(),
                    1000.into(),
                    n(0),
                    n(6),
                    n(line),
                    n(col),
                    n(50),
                    n(0),
                ],
            ),
        ]
    }

    fn base(a: &App) -> Option<&predict::Base> {
        a.predict.as_ref().and_then(|p| p.base.as_ref())
    }

    /// Key counting and the counting fence only beyond scroll prediction.
    #[test]
    fn the_agent_counts_keys_only_when_asked() {
        let t1 = Instant::now() + Duration::from_secs(1);
        for (keys, acks, eval) in [
            (Predictions::default(), false, "0"),
            (MOTIONS, true, "luaeval('(_G.nvmux_taken or {})[7] or -1')"),
            (TYPING, true, "luaeval('(_G.nvmux_taken or {})[7] or -1')"),
            (
                Predictions {
                    scrolls: false,
                    motions: false,
                    typing: true,
                },
                true,
                "luaeval('(_G.nvmux_taken or {})[7] or -1')",
            ),
        ] {
            let t0 = Instant::now();
            let mut a = App::new(palette(), Effects::none(), true, (20, 6)).with_predictions(keys);
            a.start();
            let sent = calls(&mut a);
            let rtt = Duration::from_millis(200);
            let id = request(&sent, "nvim_get_api_info", "");
            answer(&mut a, id, Value::Array(vec![7.into()]), t0 + rtt);
            answer(&mut a, asks_uis(&sent), uis(false, false), t0 + rtt);
            let sent = calls(&mut a);
            let agent = request(&sent, "nvim_exec_lua", "local chan");
            let (_, _, args) = sent
                .iter()
                .find(|(id, ..)| *id == Some(agent))
                .expect("agent");
            let args = args[1].as_array().expect("its arguments");
            assert_eq!(args[5], acks.into(), "{keys:?}");
            a.tick(t1);
            let sent = calls(&mut a);
            let fence = request(&sent, "nvim_eval", "");
            let (_, _, args) = sent
                .iter()
                .find(|(id, ..)| *id == Some(fence))
                .expect("fence");
            assert_eq!(args[0], eval.into(), "{keys:?}");
        }
    }

    /// Unchanged settings do not re-attach.
    #[test]
    fn a_client_told_to_count_the_keys_or_not_attaches_again_for_the_agent() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = acking(Duration::from_millis(200));
        let mut d = drawing();
        d.remote = true;
        d.client.predict_scrolls = Predict::Auto;
        d.client.predict_motions = Predict::Auto;
        a.reconfigure(&d, t1);
        let sent = calls(&mut a);
        assert!(
            !sent.iter().any(|(_, m, _)| m == "nvim_ui_detach"),
            "as it was"
        );
        d.client.predict_motions = Predict::Off;
        a.reconfigure(&d, t1);
        let detach = request(&calls(&mut a), "nvim_ui_detach", "");
        answer(&mut a, detach, Value::Nil, t1);
        let sent = calls(&mut a);
        let agent = request(&sent, "nvim_exec_lua", "local chan");
        let (_, _, args) = sent
            .iter()
            .find(|(id, ..)| *id == Some(agent))
            .expect("agent");
        assert_eq!(args[1].as_array().expect("its arguments")[5], false.into());
    }

    /// Otherwise counts would stay untied until the user paused after an
    /// unpredicted key.
    #[test]
    fn a_fence_asked_before_the_agent_is_asked_again_once_it_is_there() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let (mut a, agent) = predicting_as(MOTIONS, Duration::from_millis(200));
        a.tick(t1);
        let fence = request(&calls(&mut a), "nvim_eval", "luaeval");
        answer(&mut a, fence, Value::from(-1), t1);
        a.tick(t1);
        assert!(
            calls(&mut a).iter().all(|(_, m, _)| m != "nvim_eval"),
            "no agent to ask"
        );
        let wins = Value::Array(vec![Value::Array(vec![1000.into(), 1.into()])]);
        let options = Value::Array(vec![2.into(), false.into(), false.into(), wins]);
        answer(&mut a, agent, options, t1);
        a.tick(t1);
        let fence = request(&calls(&mut a), "nvim_eval", "luaeval");
        answer(&mut a, fence, Value::from(0), t1);
        let p = a.predict.as_ref().expect("predicting");
        assert!(p.sync.synced(), "tied");
        a.tick(t1);
        assert!(
            calls(&mut a).iter().all(|(_, m, _)| m != "nvim_eval"),
            "and asked no more"
        );
    }

    #[test]
    fn an_ack_waits_for_the_frame_it_comes_ahead_of() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = acking(Duration::from_millis(200));
        a.keys(b"j", t1);
        ack(&mut a, 1, "j", "n", (1, 0, 0, 0), t1);
        assert_eq!(base(&a).map(|b| b.seq), Some(0), "no frame yet");
        redraw(&mut a, cursor_to(1, 0), t1);
        let b = base(&a).expect("Neovim's word");
        assert_eq!((b.seq, b.ack.pos.line), (1, 1));
    }

    /// A no-op key gets an ack but no frame.
    #[test]
    fn an_ack_that_changes_nothing_on_the_screen_is_taken_at_once() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = acking(Duration::from_millis(200));
        a.keys(b"k", t1);
        ack(&mut a, 1, "k", "n", (0, 0, 0, 0), t1);
        assert_eq!(base(&a).map(|b| b.seq), Some(1));
        // An ack in a mode not yet drawn waits for its frame.
        a.keys(b"v", t1);
        ack(&mut a, 2, "v", "v", (0, 0, 0, 0), t1);
        assert_eq!(base(&a).map(|b| b.seq), Some(1));
    }

    #[test]
    fn an_ack_is_taken_at_the_fence_asked_after_it() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = acking(Duration::from_millis(200));
        a.keys(b"j", t1);
        let fence = request(&calls(&mut a), "nvim_eval", "luaeval");
        ack(&mut a, 1, "j", "n", (1, 0, 0, 0), t1);
        a.keys(b"j", t1);
        assert_eq!(base(&a).map(|b| b.seq), Some(0));
        answer(&mut a, fence, Value::from(1), t1);
        let b = base(&a).expect("Neovim's word");
        assert_eq!((b.seq, b.ack.pos.line), (1, 1));
        let p = a.predict.as_ref().expect("predicting");
        assert!(p.sync.synced(), "a fence with a key after it, in step");
    }

    /// With acks, curswant comes in the ack; see
    /// `a_scroll_lands_the_cursor_in_the_column_it_wants`.
    #[test]
    fn want_comes_with_the_ack() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let land = |want: i64| {
            let mut a = acking(Duration::from_millis(200));
            let text: Vec<String> = (0..20)
                .map(|n| match n {
                    0 => "line 0".to_string(),
                    _ => format!("line {n} and on and on"),
                })
                .collect();
            let lines = vec![
                "lines".into(),
                1000.into(),
                1.into(),
                1.into(),
                0.into(),
                Value::Array(text.iter().map(|t| Value::from(t.as_str())).collect()),
                true.into(),
            ];
            notify(&mut a, "nvmux_client", lines, t1);
            let mut events = whole(2, 20, 5, &text[..5]);
            events.push(("grid_cursor_goto", vec![2.into(), 0.into(), 5.into()]));
            redraw(&mut a, events, t1);
            ack(&mut a, 0, "", "n", (0, 5, want, 0), t1);
            a.keys(b"\x1b[<65;3;2M", t1);
            assert_eq!(top_row(&a), "line 3 and on and on");
            a.anim.cursor_at(&a.model)
        };
        assert_eq!(land(12), (0, 12), "where it wants to be");
        assert_eq!(land(-1), (0, 19), "as `$` has it");
    }

    /// Undrawn predictions are judged too: `jjl` agrees, a `j` Neovim made
    /// otherwise disagrees, an unfollowed key is unknown.
    #[test]
    fn the_shadow_judge_agrees_with_neovim() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = acking(Duration::from_millis(200));
        let shadow = |a: &App| {
            let s = a.predict.as_ref().expect("predicting").shadow;
            (s.agree, s.disagree, s.unknown)
        };
        a.keys(b"jjl", t1);
        ack(&mut a, 3, "jjl", "n", (2, 1, 1, 0), t1);
        redraw(&mut a, cursor_to(2, 1), t1);
        assert_eq!(shadow(&a), (1, 0, 0));
        a.keys(b"j", t1);
        ack(&mut a, 4, "j", "n", (4, 1, 1, 0), t1);
        redraw(&mut a, cursor_to(4, 1), t1);
        assert_eq!(shadow(&a), (1, 1, 0), "not the line Neovim went to");
        a.keys(b"x", t1);
        ack(&mut a, 5, "x", "n", (4, 1, 1, 0), t1);
        redraw(&mut a, cursor_to(4, 1), t1);
        assert_eq!(shadow(&a), (1, 1, 1));
    }

    /// Reverted at once rather than at its deadline, and counted.
    #[test]
    fn a_scroll_neovim_did_not_make_is_given_up_at_its_ack() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = acking(Duration::from_millis(200));
        a.keys(b"\x1b[<65;3;2M", t1);
        assert_eq!(a.anim.ahead(2), (3, 3));
        ack(&mut a, 1, "<ScrollWheelDown>", "n", (0, 0, 0, 0), t1);
        redraw(&mut a, cursor_to(0, 0), t1);
        let p = a.predict.as_ref().expect("predicting");
        assert_eq!(p.shadow.unmade, 1);
        assert_eq!(a.anim.ahead(2), (0, 0), "given up");
        assert_eq!(top_row(&a), "line 0");
        // A frame making only part of the scroll also gives it up.
        a.keys(b"2\x05", t1);
        assert_eq!(a.anim.ahead(2), (2, 2));
        ack(&mut a, 3, "2<C-E>", "n", (1, 0, 0, 1), t1);
        let mut events = vec![at(1, 1, 1)];
        events.extend((0..5).map(|r| line(2, r, &format!("line {}", r + 1))));
        redraw(&mut a, events, t1);
        assert_eq!(a.anim.ahead(2), (0, 0), "a line of the two");
        assert_eq!(top_row(&a), "line 1");
    }

    /// Drawn from Neovim's view and cursor, the turn would leave out `4j`: the
    /// client's cursor would end a line short of Neovim's. It is made with
    /// the ack's frame, as a scroll typed then is, and later turns at once.
    #[test]
    fn a_turn_of_the_wheel_after_a_key_left_to_neovim_lands_with_its_ack() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = acking(Duration::from_millis(200));
        a.keys(b"4j\x1b[<65;3;2M", t1);
        assert_eq!(a.anim.ahead(2), (0, 0), "not drawn");
        assert_eq!(top_row(&a), "line 0");
        ack(&mut a, 2, "4j", "n", (4, 0, 0, 0), t1);
        redraw(&mut a, cursor_to(4, 0), t1);
        assert_eq!(top_row(&a), "line 3", "made with its ack");
        assert_eq!(drawn(&a), (1, 0), "the cursor on line 4");
        assert!(a.predict.as_ref().is_some_and(Predictor::known));
        a.keys(b"\x1b[<65;3;2M", t1);
        assert_eq!(top_row(&a), "line 6", "and the next at once");
    }

    /// A `<C-c>` before the ack's frame puts the counts in doubt: it may have
    /// discarded the turn.
    #[test]
    fn a_turn_of_the_wheel_is_not_made_again_with_the_counts_in_doubt() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = acking(Duration::from_millis(200));
        a.keys(b"4j\x1b[<65;3;2M", t1);
        ack(&mut a, 2, "4j", "n", (4, 0, 0, 0), t1);
        a.keys(b"\x03", t1);
        redraw(&mut a, cursor_to(4, 0), t1);
        assert_eq!(a.anim.ahead(2), (0, 0), "not made");
        assert_eq!(top_row(&a), "line 0");
    }

    /// The ack does not give that window's view: taken at once, it would undo
    /// the scroll and remake the turns after it from a view without it.
    #[test]
    fn a_turn_over_a_window_without_the_cursor_waits_for_its_frame() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = acking(Duration::from_millis(200));
        let n = Value::from;
        let viewport = |top: i64, delta: i64| {
            let bottom = top + 6;
            (
                "win_viewport",
                vec![
                    n(4),
                    n(1001),
                    n(top),
                    n(bottom),
                    n(top),
                    n(0),
                    n(50),
                    n(delta),
                ],
            )
        };
        let rows: Vec<String> = (0..5).map(|r| format!("line {r}")).collect();
        let mut events = vec![
            resize(2, 10, 5),
            place(2, 1000, 0, 0, 10, 5),
            resize(4, 10, 5),
            place(4, 1001, 0, 10, 10, 5),
            viewport(0, 0),
        ];
        events.extend(whole(2, 10, 5, &rows));
        events.extend(whole(4, 10, 5, &rows));
        redraw(&mut a, events, t1);
        let shape = shaped(0, (false, false, true), (false, 0), &[], Vec::new());
        notify(
            &mut a,
            "nvmux_client",
            vec!["shape".into(), n(1001), Value::Array(shape)],
            t1,
        );
        let text: Vec<Value> = (0..20).map(|l| Value::from(format!("line {l}"))).collect();
        let lines = vec![
            "lines".into(),
            n(1001),
            n(1),
            n(1),
            n(0),
            Value::Array(text),
            true.into(),
        ];
        notify(&mut a, "nvmux_client", lines, t1);
        // Over the right window; the cursor is in the left one.
        a.keys(b"\x1b[<65;16;2M", t1);
        assert_eq!(a.anim.ahead(4).0, 3, "scrolled ahead");
        ack(&mut a, 1, "<ScrollWheelDown>", "n", (0, 0, 0, 0), t1);
        assert_eq!(a.anim.ahead(4).0, 3, "till its frame");
        let mut events = vec![viewport(3, 3)];
        events.extend((0..5).map(|r| line(4, r, &format!("line {}", r + 3))));
        redraw(&mut a, events, t1);
        assert_eq!(a.anim.ahead(4).0, 0, "made by it");
    }

    /// Made with that key's ack and frame, not once Neovim catches up with
    /// all keys.
    #[test]
    fn a_scroll_after_a_key_left_to_neovim_lands_with_its_ack() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = acking(Duration::from_millis(200));
        a.keys(b"j\x05", t1);
        assert_eq!(top_row(&a), "line 0", "blind after `j`");
        ack(&mut a, 1, "j", "n", (1, 0, 0, 0), t1);
        redraw(&mut a, cursor_to(1, 0), t1);
        assert_eq!(top_row(&a), "line 1", "made with its ack");
        assert_eq!(a.anim.ahead(2), (1, 0));
        // Neovim's own scroll lands it.
        ack(&mut a, 2, "j<C-E>", "n", (1, 0, 0, 1), t1);
        let mut events = vec![at(1, 1, 1)];
        events.extend((0..5).map(|r| line(2, r, &format!("line {}", r + 1))));
        redraw(&mut a, events, t1);
        assert_eq!(a.anim.ahead(2), (0, 0));
        assert_eq!(top_row(&a), "line 1");
    }

    /// Any settings change sends a card; if predictions are unchanged, a
    /// scroll after an unfollowed key must still wait for that key's ack,
    /// or it would scroll from the wrong line.
    #[test]
    fn a_settings_card_leaves_the_client_blind_as_it_was() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = acking(Duration::from_millis(200));
        a.keys(b"j", t1);
        let mut d = drawing();
        d.remote = true;
        d.client.predict_scrolls = Predict::Auto;
        d.client.predict_motions = Predict::Auto;
        a.reconfigure(&d, t1);
        a.keys(b"\x05", t1);
        assert_eq!(top_row(&a), "line 0", "blind after `j`");
        ack(&mut a, 1, "j", "n", (1, 0, 0, 0), t1);
        redraw(&mut a, cursor_to(1, 0), t1);
        assert_eq!(top_row(&a), "line 1", "made with its ack");
    }

    /// Replayed from an ack (here the cursor moved before Neovim took the
    /// key), a scroll still ahead is not made twice.
    #[test]
    fn a_scroll_already_made_is_not_made_again() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = acking(Duration::from_millis(200));
        a.keys(b"\x05", t1);
        assert_eq!(a.anim.ahead(2), (1, 1));
        ack(&mut a, 0, "", "n", (0, 3, 3, 0), t1);
        redraw(&mut a, cursor_to(0, 3), t1);
        let b = base(&a).expect("Neovim's word");
        assert_eq!((b.seq, b.ack.pos.byte), (0, 3));
        assert_eq!(a.anim.ahead(2), (1, 1), "the one scroll");
    }

    /// An ack taken with a later frame (Neovim did not flush between keys):
    /// what that frame shows is not made again, and the client stays blind
    /// until an ack with its own frame.
    #[test]
    fn a_word_taken_with_a_later_frame_makes_nothing_again() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = acking(Duration::from_millis(200));
        a.keys(b"j\x05", t1);
        ack(&mut a, 1, "j", "n", (1, 0, 0, 0), t1);
        let mut events = vec![
            at(1, 1, 1),
            ("grid_cursor_goto", vec![2.into(), 0.into(), 0.into()]),
        ];
        events.extend((0..5).map(|r| line(2, r, &format!("line {}", r + 1))));
        redraw(&mut a, events, t1);
        assert_eq!(base(&a).map(|b| b.seq), Some(1));
        assert_eq!(a.anim.ahead(2), (0, 0), "not made again");
        assert_eq!(top_row(&a), "line 1");
        let p = a.predict.as_ref().expect("predicting");
        assert!(!p.known(), "blind");
        ack(&mut a, 2, "j<C-E>", "n", (1, 0, 0, 1), t1);
        assert!(a.predict.as_ref().is_some_and(Predictor::known));
    }

    #[test]
    fn a_scroll_no_ack_covers_is_given_up_at_its_deadline() {
        let ms = Duration::from_millis;
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = acking(ms(200));
        a.keys(b"\x1b[<65;3;2M", t1);
        let made = a.predict.as_ref().expect("predicting").deadline(t1);
        a.frame(made - ms(1));
        assert_eq!(a.anim.ahead(2), (3, 3));
        a.frame(made);
        assert_eq!(a.anim.ahead(2), (0, 0), "not heard from");
    }

    /// [`acking`] with an ack-mode shape ([`predict::Stepping`] only up to
    /// `widths`, so no `'scrolljump'`) and `mapped` keys, so motions are
    /// predicted; cursor on `line 0` of `line 0` to `line 19`.
    fn stepping_with(rtt: Duration, mapped: &[&str]) -> App {
        stepping_as(MOTIONS, rtt, mapped)
    }

    fn stepping_as(predictions: Predictions, rtt: Duration, mapped: &[&str]) -> App {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = acking_as(predictions, rtt);
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
            Value::Array(mapped.iter().map(|k| Value::from(*k)).collect()),
            Value::Array(vec![]),
            0.into(),
            1.into(),
            6.into(),
            Value::Array(vec![]),
            false.into(),
            "b,s".into(),
            "".into(),
            false.into(),
            false.into(),
            false.into(),
            false.into(),
        ];
        notify(
            &mut a,
            "nvmux_client",
            vec!["shape".into(), 1000.into(), Value::Array(shape)],
            t1,
        );
        a
    }

    fn stepping(rtt: Duration) -> App {
        stepping_with(rtt, &[])
    }

    /// An ack-mode shape for [`stepping`]'s window: gutter `textoff` wide,
    /// `'number'`, `'relativenumber'`, `'statuscolumn'` unless `plain`,
    /// `'wrap'`, `'cursorline'` bits `cul` ([`predict::Cursorline`]),
    /// `mapped` keys and their `idioms` ([`predict::Idiom`]).
    fn shaped(
        textoff: i64,
        (number, relative, plain): (bool, bool, bool),
        (wrap, cul): (bool, i64),
        mapped: &[&str],
        idioms: Vec<Value>,
    ) -> Vec<Value> {
        vec![
            1.into(),
            50.into(),
            textoff.into(),
            8.into(),
            number.into(),
            relative.into(),
            plain.into(),
            wrap.into(),
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
            Value::Array(mapped.iter().map(|k| Value::from(*k)).collect()),
            Value::Array(vec![]),
            0.into(),
            1.into(),
            6.into(),
            Value::Array(vec![]),
            false.into(),
            "b,s".into(),
            "".into(),
            false.into(),
            false.into(),
            false.into(),
            false.into(),
            4.into(),
            false.into(),
            cul.into(),
            Value::Array(idioms),
        ]
    }

    fn reshape(a: &mut App, shape: Vec<Value>, now: Instant) {
        let params = vec!["shape".into(), 1000.into(), Value::Array(shape)];
        notify(a, "nvmux_client", params, now);
    }

    /// Gutter groups and their `hl_group_set` ids in these tests.
    const GUTTER: [(&str, u64); 8] = [
        ("LineNr", 10),
        ("LineNrAbove", 11),
        ("LineNrBelow", 12),
        ("CursorLineNr", 13),
        ("SignColumn", 14),
        ("CursorLineSign", 15),
        ("FoldColumn", 16),
        ("CursorLineFold", 17),
    ];

    /// Neovim's drawing of [`numbered`]'s window with the cursor at
    /// (`cursor`, `byte`): hybrid numbers in their groups (the cursor's in
    /// `CursorLineNr` if `cul`; rows in `numhl` in sign group 20), text,
    /// cursor and view.
    fn numbers_drawn(
        (cursor, byte): (i64, i64),
        cul: bool,
        numhl: &[i64],
    ) -> Vec<(&'static str, Vec<Value>)> {
        let n = Value::from;
        let mut events = Vec::new();
        for r in 0..5i64 {
            let (number, hl) = match r.cmp(&cursor) {
                std::cmp::Ordering::Equal => (format!("{:<3} ", r + 1), if cul { 13 } else { 10 }),
                std::cmp::Ordering::Less => (format!("{:>3} ", cursor - r), 11),
                std::cmp::Ordering::Greater => (format!("{:>3} ", r - cursor), 12),
            };
            let hl = if numhl.contains(&r) { 20 } else { hl };
            events.push(line_hl(2, r as u64, 0, &number, hl));
            events.push(line_hl(
                2,
                r as u64,
                4,
                &format!("{:16}", format!("line {r}")),
                0,
            ));
        }
        events.push(("grid_cursor_goto", vec![n(2), n(cursor), n(4 + byte)]));
        events.push((
            "win_viewport",
            vec![n(2), n(1000), n(0), n(6), n(cursor), n(byte), n(50), n(0)],
        ));
        events
    }

    /// [`stepping`] with hybrid numbers in a 4-column gutter, `'cursorline'`
    /// bits `cul`, sign-coloured numbers on rows `numhl`, drawn with the
    /// cursor on line 0 and gutter groups set.
    fn numbered(rtt: Duration, cul: i64, numhl: &[i64]) -> App {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(rtt);
        reshape(
            &mut a,
            shaped(4, (true, true, true), (false, cul), &[], vec![]),
            t1,
        );
        let mut events: Vec<(&str, Vec<Value>)> = GUTTER
            .iter()
            .map(|(name, id)| ("hl_group_set", vec![(*name).into(), (*id).into()]))
            .collect();
        events.extend(numbers_drawn((0, 0), cul & 1 != 0, numhl));
        redraw(&mut a, events, t1);
        a
    }

    /// The next frame's rows as text.
    fn composed(a: &mut App) -> Vec<String> {
        let f = composed_frame(a);
        f.text().lines().map(|l| l.trim_end().to_string()).collect()
    }

    fn composed_frame(a: &mut App) -> compose::Frame {
        a.repaint();
        let (cols, rows) = a.size();
        compose::compose(&a.model, &a.anim, cols, rows)
    }

    /// Patch groups on each row of grid 2; `None` where Neovim's drawing
    /// stands.
    fn repainted(a: &mut App) -> Vec<Option<Vec<u32>>> {
        a.repaint();
        let patches = a.anim.patches(2).cloned().unwrap_or_default();
        (0..5)
            .map(|r| {
                patches.get(&r).map(|p| {
                    p.iter()
                        .flat_map(|p| p.cells.iter().map(|c| c.hl))
                        .collect()
                })
            })
            .collect()
    }

    /// Every relative number is redrawn in Neovim's group, the cursor line's
    /// absolute and left-aligned; Neovim's frame matches.
    #[test]
    fn j_moves_the_cursor_at_once_and_renumbers_the_gutter() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = numbered(Duration::from_millis(200), 0, &[]);
        a.keys(b"j", t1);
        assert_eq!(drawn(&a), (1, 4), "before Neovim has heard of it");
        let rows = [
            "  1 line 0",
            "2   line 1",
            "  1 line 2",
            "  2 line 3",
            "  3 line 4",
        ];
        assert_eq!(composed(&mut a)[..5], rows);
        let groups = repainted(&mut a);
        assert_eq!(
            groups,
            [
                Some(vec![11; 4]),
                Some(vec![10; 4]),
                Some(vec![12; 4]),
                Some(vec![12; 4]),
                Some(vec![12; 4]),
            ]
        );
        ack(&mut a, 1, "j", "n", (1, 0, 0, 0), t1);
        redraw(&mut a, numbers_drawn((1, 0), false, &[]), t1);
        assert!(a.anim.chain(2).is_none(), "let go of");
        assert_eq!(composed(&mut a)[..5], rows, "as Neovim draws it");
        for (keys, at, second) in [
            (&b"3j"[..], (3, 4), "  2 line 1"),
            (b"jjk", (1, 4), "2   line 1"),
            (b"4j2k", (2, 4), "  1 line 1"),
            (b"+", (1, 4), "2   line 1"),
            (b"\r\r", (2, 4), "  1 line 1"),
            (b"jj-", (1, 4), "2   line 1"),
            (b"3_", (2, 4), "  1 line 1"),
            (b"2$", (1, 9), "2   line 1"),
            (b"$ ", (1, 4), "2   line 1"),
            (b"j\x7f", (0, 9), "  1 line 1"),
        ] {
            let mut a = numbered(Duration::from_millis(200), 0, &[]);
            a.keys(keys, t1);
            let name = String::from_utf8_lossy(keys);
            assert_eq!(drawn(&a), at, "{name:?}");
            assert_eq!(composed(&mut a)[1], second, "{name:?}");
        }
    }

    /// A sign-coloured number keeps the sign's colours as it renumbers. With
    /// `'cursorline'` numbering, Neovim blends them over the cursor line's
    /// group, which the client has no id for, so moves onto it are left to
    /// Neovim.
    #[test]
    fn a_numhl_number_is_kept() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = numbered(Duration::from_millis(200), 0, &[3]);
        a.keys(b"j", t1);
        assert_eq!(drawn(&a), (1, 4));
        assert_eq!(composed(&mut a)[3], "  2 line 3");
        assert_eq!(repainted(&mut a)[3], Some(vec![20; 4]), "its colours");
        let mut a = numbered(Duration::from_millis(200), 1, &[3]);
        a.keys(b"j", t1);
        assert_eq!(drawn(&a), (1, 4), "another line's");
        let groups = repainted(&mut a);
        assert_eq!(
            (groups[0].clone(), groups[1].clone(), groups[3].clone()),
            (Some(vec![11; 4]), Some(vec![13; 4]), Some(vec![20; 4]))
        );
        let mut a = numbered(Duration::from_millis(200), 1, &[1]);
        a.keys(b"j", t1);
        assert_eq!(drawn(&a), (0, 4), "onto it");
    }

    /// Steps are dropped only when a frame draws a stepped-onto line's text
    /// differently; other lines or gutter renumbering (as in frames for
    /// earlier keys) leave them.
    #[test]
    fn a_frame_lets_go_of_the_steps_only_where_it_draws_a_line_gone_onto_otherwise() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = numbered(Duration::from_millis(200), 0, &[]);
        a.keys(b"jj", t1);
        assert_eq!(drawn(&a), (2, 4));
        redraw(&mut a, numbers_drawn((1, 0), false, &[]), t1);
        assert_eq!(drawn(&a), (2, 4), "Neovim's frame for the first, unsaid");
        assert_eq!(
            composed(&mut a)[..3],
            ["  2 line 0", "  1 line 1", "3   line 2"]
        );
        redraw(&mut a, vec![line_at(2, 4, 4, "lime 4")], t1);
        assert_eq!(drawn(&a), (2, 4), "another line");
        redraw(&mut a, vec![line_at(2, 1, 4, "li: ne 1")], t1);
        assert!(a.anim.chain(2).is_none(), "a line gone onto");
        assert_eq!(drawn(&a), (1, 4), "where Neovim has it");
        assert_eq!(
            composed(&mut a)[..3],
            ["  1 line 0", "2   li: ne 1", "  1 line 2"]
        );
    }

    /// Defines `CursorLine` as `id`: a background, plus a foreground if
    /// `front`.
    fn bar_defined(id: u64, front: bool) -> Vec<(&'static str, Vec<Value>)> {
        let mut rgb = vec![("background".into(), 0x30_30_30.into())];
        if front {
            rgb.push(("foreground".into(), 0xff_ff_ff.into()));
        }
        vec![
            (
                "hl_attr_define",
                vec![
                    id.into(),
                    Value::Map(rgb),
                    Value::Map(vec![]),
                    Value::Array(vec![]),
                ],
            ),
            ("hl_group_set", vec!["CursorLine".into(), id.into()]),
        ]
    }

    /// Row `row`'s text under the `'cursorline'` bar (id 21).
    fn barred(row: u64) -> (&'static str, Vec<Value>) {
        line_hl(2, row, 4, &format!("{:16}", format!("line {row}")), 21)
    }

    /// With a background-only bar and cursor-line numbering, the predicted
    /// frame matches Neovim's cell for cell, styles included.
    #[test]
    fn the_bar_moves_with_the_cursor() {
        let t1 = Instant::now() + Duration::from_secs(1);
        for keys in [&b"j"[..], b"3j", b"jjk"] {
            let mut a = numbered(Duration::from_millis(200), 3, &[]);
            let mut bar = bar_defined(21, false);
            bar.push(barred(0));
            redraw(&mut a, bar, t1);
            a.keys(keys, t1);
            let line = drawn(&a).0;
            assert_ne!(
                line,
                0,
                "{:?} before Neovim has heard of it",
                String::from_utf8_lossy(keys)
            );
            let predicted = composed_frame(&mut a);
            let mut neovim = numbers_drawn((line, 0), true, &[]);
            neovim.push(barred(line as u64));
            ack(&mut a, keys.len() as u64, "", "n", (line, 0, 0, 0), t1);
            redraw(&mut a, neovim, t1);
            assert!(a.anim.chain(2).is_none(), "let go of");
            let drawn_by_neovim = composed_frame(&mut a);
            assert_eq!(predicted.text(), drawn_by_neovim.text());
            for (i, (p, n)) in predicted
                .cells
                .iter()
                .zip(&drawn_by_neovim.cells)
                .enumerate()
            {
                assert_eq!(
                    p.style,
                    n.style,
                    "{keys:?}: row {}, column {}",
                    i / 20,
                    i % 20
                );
            }
        }
    }

    /// A bar with a foreground (Neovim blends it over the text), an unnamed
    /// `CursorLine`, `'screenline'`, or a `'statuscolumn'` leave line changes
    /// to Neovim; moves along the line are still drawn.
    #[test]
    fn a_bar_the_client_cannot_lay_refuses_line_changing_moves() {
        let t1 = Instant::now() + Duration::from_secs(1);
        for (cul, front) in [(2, true), (3, true), (4, false)] {
            let mut a = numbered(Duration::from_millis(200), cul, &[]);
            redraw(&mut a, bar_defined(21, front), t1);
            a.keys(b"j", t1);
            assert_eq!(drawn(&a), (0, 4), "'cursorlineopt' {cul}");
            let mut a = numbered(Duration::from_millis(200), cul, &[]);
            redraw(&mut a, bar_defined(21, front), t1);
            a.keys(b"l", t1);
            assert_eq!(drawn(&a), (0, 5), "along the line, {cul}");
        }
        let mut a = numbered(Duration::from_millis(200), 2, &[]);
        a.keys(b"j", t1);
        assert_eq!(drawn(&a), (0, 4), "no CursorLine named");
        let mut a = numbered(Duration::from_millis(200), 2, &[]);
        redraw(&mut a, bar_defined(21, false), t1);
        a.keys(b"j", t1);
        assert_eq!(drawn(&a), (1, 4), "a background alone");
        let mut a = numbered(Duration::from_millis(200), 0, &[]);
        reshape(
            &mut a,
            shaped(4, (true, true, false), (false, 0), &[], vec![]),
            t1,
        );
        a.keys(b"j", t1);
        assert_eq!(drawn(&a), (0, 4), "'statuscolumn'");
    }

    /// [`stepping`]'s shape lacks `'scrolljump'` (see [`scrolling`]); a
    /// motion to the view's last row is still drawn.
    #[test]
    fn a_motion_past_the_edge_is_left_to_neovim_without_scrolljump() {
        let t1 = Instant::now() + Duration::from_secs(1);
        for (keys, at) in [
            (&b"5j"[..], (0, 0)),
            (b"4j", (4, 0)),
            (b"4jj", (4, 0)),
            (b"k", (0, 0)),
            (b"40 ", (0, 0)),
            (b"6$", (0, 0)),
        ] {
            let mut a = stepping(Duration::from_millis(200));
            a.keys(keys, t1);
            assert_eq!(drawn(&a), at, "{:?}", String::from_utf8_lossy(keys));
        }
    }

    /// [`stepping`] reshaped with `'scrolljump'` 1 and no concealing, and no
    /// decorated lines, so motions past the edge scroll ahead and scroll
    /// keys are steps.
    fn scrolling(rtt: Duration) -> App {
        scrolling_as(
            rtt,
            shaped(0, (false, false, true), (false, 0), &[], vec![]),
        )
    }

    fn scrolling_as(rtt: Duration, mut shape: Vec<Value>) -> App {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(rtt);
        shape.extend([
            false.into(),
            false.into(),
            Value::Array(vec![]),
            1.into(),
            false.into(),
        ]);
        reshape(&mut a, shape, t1);
        decorated(&mut a, &[], t1);
        a
    }

    /// Reports `lines` among 0..20 as decorated.
    fn decorated(a: &mut App, lines: &[i64], now: Instant) {
        let runs = lines
            .iter()
            .flat_map(|l| [Value::from(*l), Value::from(l + 1)]);
        let params = vec![
            "decorated".into(),
            1000.into(),
            0.into(),
            20.into(),
            Value::Array(runs.collect()),
        ];
        notify(a, "nvmux_client", params, now);
    }

    /// Neovim's frame scrolling the view from `from` by `by` (< 5) rows,
    /// with new rows drawn and the cursor at (`cursor`, `col`).
    fn scrolled(from: i64, by: i64, (cursor, col): (i64, i64)) -> Vec<(&'static str, Vec<Value>)> {
        let n = Value::from;
        let to = from + by;
        let mut events = vec![(
            "grid_scroll",
            vec![n(2), n(0), n(5), n(0), n(20), n(by), n(0)],
        )];
        let brought: Vec<i64> = if by > 0 {
            (5 - by..5).collect()
        } else {
            (0..-by).collect()
        };
        for r in brought {
            let text = format!("{:20}", format!("line {}", to + r));
            events.push(line(2, r as u64, &text));
        }
        events.push(("grid_cursor_goto", vec![n(2), n(cursor - to), n(col)]));
        events.push((
            "win_viewport",
            vec![
                n(2),
                n(1000),
                n(to),
                n(to + 6),
                n(cursor),
                n(col),
                n(50),
                n(by),
            ],
        ));
        events
    }

    /// One scroll ahead plus one step; Neovim's frame and ack make both and
    /// the screen does not change.
    #[test]
    fn j_at_the_bottom_scrolls_with_the_cursor() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = scrolling(Duration::from_millis(200));
        a.keys(b"4j", t1);
        assert_eq!((drawn(&a), a.anim.ahead(2)), ((4, 0), (0, 0)), "in view");
        a.keys(b"j", t1);
        assert_eq!(top_row(&a), "line 1", "before Neovim has heard of it");
        assert_eq!(drawn(&a), (4, 0));
        assert_eq!(a.anim.ahead(2), (1, 1));
        let head = a.anim.chain(2).and_then(|c| c.head()).map(|s| s.pos);
        assert_eq!(head.map(|p| (p.line, p.top)), Some((5, 1)));
        let predicted = composed(&mut a);
        ack(&mut a, 3, "j", "n", (5, 0, 0, 1), t1);
        redraw(&mut a, scrolled(0, 1, (5, 0)), t1);
        assert!(a.anim.chain(2).is_none(), "made");
        assert_eq!(a.anim.ahead(2), (0, 0), "made");
        assert_eq!(composed(&mut a), predicted);
        assert_eq!(drawn(&a), (4, 0));
        let p = a.predict.as_ref().expect("predicting");
        assert_eq!(p.shadow.disagree, 0);
    }

    #[test]
    fn pages_after_motions_start_where_they_left_the_cursor() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = scrolling(Duration::from_millis(200));
        // `'scroll'` is 2.
        a.keys(b"jjjj\x04", t1);
        assert_eq!(top_row(&a), "line 2", "before Neovim has heard of it");
        assert_eq!(drawn(&a), (4, 0), "on line 6");
        assert_eq!(a.anim.ahead(2), (2, 2));
        assert_eq!(a.anim.chain(2).map(chain::Chain::len), Some(5));
        ack(&mut a, 4, "j", "n", (4, 0, 0, 0), t1);
        redraw(&mut a, cursor_to(4, 0), t1);
        assert_eq!(a.anim.chain(2).map(chain::Chain::len), Some(1));
        assert_eq!((top_row(&a), drawn(&a)), ("line 2".into(), (4, 0)));
        ack(&mut a, 5, "<C-D>", "n", (6, 0, 0, 2), t1);
        redraw(&mut a, scrolled(0, 2, (6, 0)), t1);
        assert!(a.anim.chain(2).is_none(), "made");
        assert_eq!(a.anim.ahead(2), (0, 0));
        assert_eq!((top_row(&a), drawn(&a)), ("line 2".into(), (4, 0)));
    }

    #[test]
    fn motions_after_a_page() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = scrolling(Duration::from_millis(200));
        a.keys(b"\x04jj", t1);
        assert_eq!(top_row(&a), "line 2", "before Neovim has heard of it");
        assert_eq!(drawn(&a), (2, 0), "on line 4");
        assert_eq!(a.anim.ahead(2), (2, 2));
        ack(&mut a, 1, "<C-D>", "n", (2, 0, 0, 2), t1);
        redraw(&mut a, scrolled(0, 2, (2, 0)), t1);
        assert_eq!(a.anim.chain(2).map(chain::Chain::len), Some(2));
        assert_eq!((top_row(&a), drawn(&a)), ("line 2".into(), (2, 0)));
        ack(&mut a, 3, "j", "n", (4, 0, 0, 2), t1);
        let n = Value::from;
        redraw(
            &mut a,
            vec![("grid_cursor_goto", vec![n(2), n(2), n(0)]), at(2, 4, 0)],
            t1,
        );
        assert!(a.anim.chain(2).is_none(), "made");
        assert_eq!((top_row(&a), drawn(&a)), ("line 2".into(), (2, 0)));
    }

    /// Motions alone: scroll keys and the wheel wait for Neovim. Scrolls
    /// alone (typing on, so acks stay on): the reverse.
    #[test]
    fn the_motions_and_the_scrolls_are_switched_apart() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let apart = |scrolls: bool, motions: bool| {
            let mut a = scrolling(Duration::from_millis(200));
            let p = a.predict.as_mut().expect("predicting");
            p.set_predictions(Predictions {
                scrolls,
                motions,
                typing: !motions,
            });
            assert!(p.acks());
            a
        };
        let mut a = apart(false, true);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 1), "a motion before Neovim has heard of it");
        a.keys(b"\x05", t1);
        assert_eq!((top_row(&a), a.anim.ahead(2)), ("line 0".into(), (0, 0)));
        let mut a = apart(false, true);
        a.keys(b"\x1b[<65;3;2M", t1);
        assert_eq!((top_row(&a), a.anim.ahead(2)), ("line 0".into(), (0, 0)));
        let mut a = apart(true, false);
        a.keys(b"\x1b[<65;3;2M", t1);
        assert_eq!(top_row(&a), "line 3", "the wheel with the scrolls");

        let mut a = apart(true, false);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 0), "a motion left to Neovim");
        let mut a = apart(true, false);
        a.keys(b"\x05", t1);
        assert_eq!(
            top_row(&a),
            "line 1",
            "a scroll before Neovim has heard of it"
        );
    }

    /// A page past the client's lines is a plain scroll held until they
    /// arrive; earlier page steps are unchained into plain scrolls. A motion
    /// after it waits for Neovim.
    #[test]
    fn a_held_page_with_no_chain_still_waits_for_its_lines() {
        let t1 = Instant::now() + Duration::from_secs(1);
        // The client has lines up to 20: pages of three rows up to line 15.
        for (keys, made) in [
            (&b"\x06\x06\x06\x06\x06\x06"[..], Some(5)),
            (b"6\x06", None),
        ] {
            let mut a = scrolling(Duration::from_millis(200));
            a.keys(&keys[..keys.len() - 1], t1);
            assert_eq!(a.anim.chain(2).map(chain::Chain::len), made);
            a.keys(&keys[keys.len() - 1..], t1);
            assert_eq!(
                top_row(&a),
                if made.is_some() { "line 15" } else { "line 0" }
            );
            assert!(a.anim.chain(2).is_none(), "{keys:?}: a scroll alone");
            assert_eq!(a.anim.ahead(2), (18, 18), "{keys:?}");
            a.keys(b"j", t1);
            assert!(a.anim.chain(2).is_none(), "{keys:?}: the motion waits");
            let text: Vec<Value> = (20..40).map(|n| Value::from(format!("line {n}"))).collect();
            let params = vec![
                "lines".into(),
                1000.into(),
                1.into(),
                1.into(),
                20.into(),
                Value::Array(text),
                false.into(),
            ];
            notify(&mut a, "nvmux_client", params, t1);
            assert_eq!(
                top_row(&a),
                "line 18",
                "{keys:?}: drawn once its lines came"
            );
            assert_eq!(drawn(&a), (0, 0), "{keys:?}");
        }
    }

    /// A count on `<C-d>` sets `'scroll'`; a shape sent before Neovim took
    /// the key (with the old value) does not undo that until the key's ack.
    #[test]
    fn a_scroll_counted_outlives_a_shape_said_before_neovim_took_it() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let stale = || {
            let mut shape = shaped(0, (false, false, true), (false, 0), &[], vec![]);
            let rest: [Value; 5] = [
                false.into(),
                false.into(),
                Value::Array(vec![]),
                1.into(),
                false.into(),
            ];
            shape.extend(rest);
            shape
        };
        let mut a = scrolling(Duration::from_millis(200));
        a.keys(b"3\x04", t1);
        assert_eq!(a.anim.ahead(2), (3, 3));
        reshape(&mut a, stale(), t1);
        a.keys(b"\x04", t1);
        assert_eq!(a.anim.ahead(2), (6, 6), "three more, not two");
        ack(&mut a, 2, "3<C-D>", "n", (3, 0, 0, 3), t1);
        redraw(&mut a, scrolled(0, 3, (3, 0)), t1);
        ack(&mut a, 3, "3<C-D><C-D>", "n", (6, 0, 0, 6), t1);
        redraw(&mut a, scrolled(3, 3, (6, 0)), t1);
        assert!(a.anim.chain(2).is_none(), "made");
        reshape(&mut a, stale(), t1);
        a.keys(b"\x04", t1);
        assert_eq!(a.anim.ahead(2), (2, 2), "as Neovim's word has it");
    }

    /// From a top on the last line or the one before (see
    /// [`App::predict_scroll`]); from further up it is made.
    #[test]
    fn a_page_back_from_the_end_of_the_buffer_is_left_to_neovim() {
        let t1 = Instant::now() + Duration::from_secs(1);
        for (top, made) in [(49, false), (48, false), (47, true)] {
            let mut a = scrolling(Duration::from_millis(200));
            let n = Value::from;
            let events = vec![
                ("grid_cursor_goto", vec![n(2), n(0), n(0)]),
                (
                    "win_viewport",
                    vec![n(2), n(1000), n(top), n(51), n(top), n(0), n(50), n(0)],
                ),
            ];
            redraw(&mut a, events, t1);
            ack(&mut a, 0, "", "n", (top, 0, 0, top), t1);
            a.keys(b"\x02", t1);
            assert_eq!(a.anim.ahead(2) != (0, 0), made, "from line {top}");
        }
    }

    /// The client would draw it without them, so the motion is left to
    /// Neovim.
    #[test]
    fn a_line_with_virtual_text_or_a_sign_is_not_scrolled_into_view() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = scrolling(Duration::from_millis(200));
        decorated(&mut a, &[3, 5], t1);
        a.keys(b"4jj", t1);
        assert_eq!((top_row(&a), drawn(&a)), ("line 0".into(), (4, 0)));
        assert_eq!(a.anim.ahead(2), (0, 0));
        // Nor past the range the agent searched.
        let mut a = scrolling(Duration::from_millis(200));
        a.keys(b"4j", t1);
        let params = vec![
            "decorated".into(),
            1000.into(),
            0.into(),
            5.into(),
            Value::Array(vec![]),
        ];
        notify(&mut a, "nvmux_client", params, t1);
        a.keys(b"j", t1);
        assert_eq!((top_row(&a), drawn(&a)), ("line 0".into(), (4, 0)));
    }

    #[test]
    fn a_view_neovim_moves_otherwise_lets_go_of_the_steps_scrolled() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = scrolling(Duration::from_millis(200));
        a.keys(b"4jj", t1);
        assert_eq!(a.anim.ahead(2), (1, 1));
        redraw(&mut a, scrolled(0, 2, (2, 0)), t1);
        assert!(a.anim.chain(2).is_none(), "let go of");
        assert_eq!(a.anim.ahead(2), (0, 0));
        assert_eq!((top_row(&a), drawn(&a)), ("line 2".into(), (0, 0)));
    }

    /// While distrusted, hidden motions past the edge (no scroll ahead) are
    /// judged like in-view ones; after eight right they are drawn again.
    #[test]
    fn motions_past_the_edge_prove_themselves_as_those_in_view_do() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = scrolling(Duration::from_millis(200));
        a.keys(b"4j", t1);
        ack(&mut a, 2, "4j", "n", (4, 0, 0, 0), t1);
        redraw(&mut a, cursor_to(4, 0), t1);
        let p = a.predict.as_mut().expect("predicting");
        p.trust.wrong(1, Proof::Move, false);
        let mut sent = vec!["4", "j"];
        for i in 0..8i64 {
            a.keys(b"j", t1);
            assert!(a.anim.chain(2).is_some(), "{i}: worked out");
            assert_eq!(a.anim.ahead(2), (0, 0), "{i}: nothing scrolled ahead");
            sent.push("j");
            let tail: String = sent[sent.len().saturating_sub(4)..].concat();
            let k = sent.len() as u64;
            ack(&mut a, k, &tail, "n", (5 + i, 0, 0, 1 + i), t1);
            redraw(&mut a, scrolled(i, 1, (5 + i, 0)), t1);
            assert!(a.anim.chain(2).is_none(), "{i}: judged");
        }
        let p = a.predict.as_ref().expect("predicting");
        assert!(!p.trust.hidden(1, Proof::Move), "proved");
        assert_eq!(p.trust.tally.unjudged, 0);
        a.keys(b"j", t1);
        assert_eq!(a.anim.ahead(2), (1, 1), "drawn again");
    }

    /// With hybrid numbers, `j` past the bottom renumbers Neovim's rows and
    /// draws the new line with the cursor's number, in the right groups.
    #[test]
    fn a_line_scrolled_into_view_is_numbered_with_the_rest() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = scrolling_as(
            Duration::from_millis(200),
            shaped(4, (true, true, true), (false, 0), &[], vec![]),
        );
        let mut events: Vec<(&str, Vec<Value>)> = GUTTER
            .iter()
            .map(|(name, id)| ("hl_group_set", vec![(*name).into(), (*id).into()]))
            .collect();
        events.extend(numbers_drawn((4, 0), false, &[]));
        redraw(&mut a, events, t1);
        ack(&mut a, 0, "", "n", (4, 0, 0, 0), t1);
        a.keys(b"j", t1);
        assert_eq!(drawn(&a), (4, 4), "before Neovim has heard of it");
        let rows = [
            "  4 line 1",
            "  3 line 2",
            "  2 line 3",
            "  1 line 4",
            "6   line 5",
        ];
        assert_eq!(composed(&mut a)[..5], rows);
        let groups: Vec<Option<Vec<u32>>> = repainted(&mut a)
            .into_iter()
            .map(|g| g.map(|g| g[..4].to_vec()))
            .collect();
        assert_eq!(
            groups,
            [
                Some(vec![11; 4]),
                Some(vec![11; 4]),
                Some(vec![11; 4]),
                Some(vec![11; 4]),
                Some(vec![10; 4]),
            ]
        );
    }

    /// A row Neovim scrolled out but the scroll keeps, brought back by `k`
    /// ahead of Neovim, keeps Neovim's colours (which the client's lines
    /// lack) with only its gutter renumbered.
    #[test]
    fn a_line_neovim_drew_scrolled_back_into_view_is_renumbered_as_it_was() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = scrolling_as(
            Duration::from_millis(200),
            shaped(4, (true, true, true), (false, 0), &[], vec![]),
        );
        let mut events: Vec<(&str, Vec<Value>)> = GUTTER
            .iter()
            .map(|(name, id)| ("hl_group_set", vec![(*name).into(), (*id).into()]))
            .collect();
        events.extend(numbers_drawn((4, 0), false, &[]));
        // Line 0 in its own colours, as from `:syntax`.
        events.push(line_hl(2, 0, 4, &format!("{:16}", "line 0"), 5));
        redraw(&mut a, events, t1);
        ack(&mut a, 0, "", "n", (4, 0, 0, 0), t1);
        a.keys(b"jj", t1);
        assert_eq!(a.anim.ahead(2), (2, 2));
        // Neovim's frame for the first `j`: line 0 out of view.
        let n = Value::from;
        let mut events = vec![(
            "grid_scroll",
            vec![n(2), n(0), n(5), n(0), n(20), n(1), n(0)],
        )];
        for r in 0..5i64 {
            let line = r + 1;
            let number = match line {
                5 => "6   ".to_string(),
                _ => format!("{:>3} ", 5 - line),
            };
            events.push(line_hl(2, r as u64, 0, &number, 10));
            let text = format!("{:16}", format!("line {line}"));
            events.push(line_hl(2, r as u64, 4, &text, 0));
        }
        events.push(("grid_cursor_goto", vec![n(2), n(4), n(4)]));
        events.push(at(1, 5, 1));
        ack(&mut a, 1, "j", "n", (5, 0, 0, 1), t1);
        redraw(&mut a, events, t1);
        assert_eq!(a.anim.ahead(2), (1, 1), "the second still ahead");
        a.keys(b"kkkkkk", t1);
        assert_eq!(a.anim.ahead(2).0, -1, "before Neovim has heard of them");
        assert_eq!(composed(&mut a)[..2], ["1   line 0", "  1 line 1"]);
        assert_eq!(repainted(&mut a)[0], Some(vec![10; 4]), "the gutter alone");
    }

    #[test]
    fn a_scroll_alone_renumbers_the_rows_with_the_cursor() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = numbered(Duration::from_millis(200), 0, &[]);
        a.keys(b"\x1b[<65;3;2M", t1);
        assert_eq!(a.anim.ahead(2), (3, 3), "before Neovim has heard of it");
        assert!(a.anim.chain(2).is_none(), "a scroll alone");
        assert_eq!(drawn(&a), (0, 4));
        let rows = [
            "4   line 3",
            "  1 line 4",
            "  2 line 5",
            "  3 line 6",
            "  4 line 7",
        ];
        assert_eq!(composed(&mut a)[..5], rows);
        let groups: Vec<Option<Vec<u32>>> = repainted(&mut a)
            .into_iter()
            .map(|g| g.map(|g| g[..4].to_vec()))
            .collect();
        assert_eq!(
            groups,
            [
                Some(vec![10; 4]),
                Some(vec![12; 4]),
                Some(vec![12; 4]),
                Some(vec![12; 4]),
                Some(vec![12; 4]),
            ]
        );
    }

    /// With window-local highlights, `'statuscolumn'` or concealing, the
    /// scroll is still drawn with Neovim's numbering, for its frame to fix.
    /// So is one keeping the cursor on its line.
    #[test]
    fn a_scroll_alone_the_gutter_cannot_follow_leaves_it_as_neovim_drew_it() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let shape = |plain: bool, own: bool, conceals: bool| {
            let mut s = shaped(4, (true, true, plain), (false, 0), &[], vec![]);
            s.extend([own.into(), conceals.into()]);
            s
        };
        for (name, shape) in [
            ("groups of its own", shape(true, true, false)),
            ("a 'statuscolumn'", shape(false, false, false)),
            ("text concealed", shape(true, false, true)),
        ] {
            let mut a = numbered(Duration::from_millis(200), 0, &[]);
            reshape(&mut a, shape, t1);
            a.keys(b"\x1b[<65;3;2M", t1);
            assert_eq!(a.anim.ahead(2), (3, 3), "{name}: the cursor pushed down");
            assert_eq!(drawn(&a), (0, 4), "{name}");
            let rows = composed(&mut a);
            assert_eq!(rows[..2], ["  3 line 3", "  4 line 4"], "{name}");
            assert!(repainted(&mut a).iter().all(Option::is_none), "{name}");
        }
        let mut a = numbered(Duration::from_millis(200), 0, &[]);
        reshape(&mut a, shape(true, true, false), t1);
        redraw(&mut a, numbers_drawn((4, 0), false, &[]), t1);
        ack(&mut a, 0, "", "n", (4, 0, 0, 0), t1);
        a.keys(b"\x1b[<65;3;2M", t1);
        assert_eq!(a.anim.ahead(2), (3, 0), "the cursor on its line");
        assert_eq!(composed(&mut a)[1], "5   line 4");
    }

    /// Such a row keeps Neovim's drawing (virtual text included), with the
    /// gutter renumbered from the cursor Neovim had when drawing it.
    #[test]
    fn a_row_neovim_drew_that_a_slide_still_shows_is_renumbered_as_it_was() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = numbered(Duration::from_millis(200), 0, &[]);
        let mut s = crate::config::Settings::default();
        s.effects.scroll.enabled = true;
        s.effects.windows.enabled = false;
        a.anim.set_effects(Effects::from_settings(&s));
        redraw(&mut a, vec![line_hl(2, 1, 11, "VT", 0)], t1);
        // Neovim scrolls two lines down, the cursor to line 3.
        let n = Value::from;
        let mut events = vec![(
            "grid_scroll",
            vec![n(2), n(0), n(5), n(0), n(20), n(2), n(0)],
        )];
        for r in 0..5i64 {
            let line = r + 2;
            let number = match line.cmp(&3) {
                std::cmp::Ordering::Equal => "4   ".to_string(),
                _ => format!("{:>3} ", (line - 3).abs()),
            };
            events.push(line_hl(2, r as u64, 0, &number, 10));
            let text = format!("{:16}", format!("line {line}"));
            events.push(line_hl(2, r as u64, 4, &text, 0));
        }
        events.push(("grid_cursor_goto", vec![n(2), n(1), n(4)]));
        events.push((
            "win_viewport",
            vec![n(2), n(1000), n(2), n(8), n(3), n(0), n(50), n(2)],
        ));
        redraw(&mut a, events, t1);
        let rows = composed(&mut a);
        assert_eq!(rows[..2], ["  2 line 1 VT", "  1 line 2"], "a row behind");
    }

    /// LazyVim maps them to `gj`/`gk` without a count, which on one-row
    /// lines equal `j`/`k`. Other mappings are not followed, nor `gj` with
    /// curswant past the row's end (it pulls the cursor back).
    #[test]
    fn jk_mapped_the_lazyvim_way_are_followed_as_the_keys() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let lazy = |rhs: &str| -> Vec<Value> {
            vec![Value::Array(vec![
                "j".into(),
                rhs.into(),
                true.into(),
                true.into(),
                true.into(),
            ])]
        };
        for (rhs, wrap, keys, at) in [
            ("v:count == 0 ? 'gj' : 'j'", false, &b"j"[..], (1, 0)),
            ("v:count == 0 ? 'gj' : 'j'", true, b"j", (1, 0)),
            ("v:count == 0 ? 'gj' : 'j'", false, b"3j", (3, 0)),
            ("v:count == 0 ? 'gj' : 'j'", false, b"30|j", (1, 5)),
            ("v:count == 0 ? 'gj' : 'j'", true, b"30|j", (0, 5)),
            ("v:count == 0 ? 'gj' : 'j'", true, b"$j", (1, 5)),
            ("v:count==0 ? 'gj' : 'j'", false, b"j", (0, 0)),
            ("v:count == 0 ? 'gj' : 'k'", false, b"j", (0, 0)),
        ] {
            let mut a = stepping(Duration::from_millis(200));
            reshape(
                &mut a,
                shaped(0, (false, false, true), (wrap, 0), &["j"], lazy(rhs)),
                t1,
            );
            a.keys(keys, t1);
            let name = String::from_utf8_lossy(keys);
            assert_eq!(drawn(&a), at, "{rhs:?}, wrap {wrap}, {name:?}");
        }
    }

    /// Sends lines `text` then `line N` up to 20, and draws rows `drawn`.
    fn lined(a: &mut App, text: &[&str], drawn: &[&str], now: Instant) {
        let lines: Vec<Value> = text
            .iter()
            .map(|t| t.to_string())
            .chain((text.len()..20).map(|n| format!("line {n}")))
            .map(Value::from)
            .collect();
        let params: Vec<Value> = vec![
            "lines".into(),
            1000.into(),
            1.into(),
            1.into(),
            0.into(),
            Value::Array(lines),
            true.into(),
        ];
        notify(a, "nvmux_client", params, now);
        let rows = drawn
            .iter()
            .enumerate()
            .map(|(r, t)| line(2, r as u64, &format!("{t:20}")))
            .collect();
        redraw(a, rows, now);
    }

    /// `gj` onto a tab under curswant in a wrapped row's right half, short of
    /// where `j` lands, steps back before it (`nv_screengo`): left to
    /// Neovim. In the left half, or with `j` unmapped, it goes where `j` does.
    #[test]
    fn gj_onto_a_tab_in_the_right_half_of_a_row_is_left_to_neovim() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let lazy = vec![Value::Array(vec![
            "j".into(),
            "v:count == 0 ? 'gj' : 'j'".into(),
            true.into(),
            true.into(),
            true.into(),
        ])];
        let text = ["xxxxxxxxxxxxxxx", "yyyyyyyyyyy\tz", "yy\tz"];
        let drawn_rows = ["xxxxxxxxxxxxxxx", "yyyyyyyyyyy     z", "yy      z"];
        for (idioms, keys, at) in [
            (lazy.clone(), &b"14|j"[..], (0, 13)),
            (vec![], b"14|j", (1, 15)),
            (lazy.clone(), b"5|jj", (2, 7)),
        ] {
            let mut a = stepping(Duration::from_millis(200));
            let mapped: &[&str] = if idioms.is_empty() { &[] } else { &["j"] };
            reshape(
                &mut a,
                shaped(0, (false, false, true), (true, 0), mapped, idioms),
                t1,
            );
            lined(&mut a, &text, &drawn_rows, t1);
            ack0(&mut a, 0, "", 0, (1, 15), t1);
            a.keys(keys, t1);
            assert_eq!(drawn(&a), at, "{:?}", String::from_utf8_lossy(keys));
        }
    }

    /// `zc` may change the rows without moving the cursor or view, so its
    /// ack waits for the frame, and the next motion uses the new rows.
    #[test]
    fn a_word_on_a_key_the_client_cannot_make_out_waits_for_its_frame() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"zcj", t1);
        assert_eq!(drawn(&a), (0, 0), "blind after `zc`");
        ack(&mut a, 2, "zc", "n", (0, 0, 0, 0), t1);
        assert!(a.anim.chain(2).is_none(), "not over the rows as they were");
        let mut fold = vec![line(2, 0, "+--  4 lines: line 0")];
        fold.extend((1..5).map(|r| line(2, r, &format!("line {:<15}", r + 3))));
        fold.extend(cursor_to(0, 0));
        redraw(&mut a, fold, t1);
        assert!(a.anim.chain(2).is_none(), "nor over the fold");
        assert_eq!(drawn(&a), (0, 0));
    }

    /// Invisible blank virtual text may hide among the blanks: drawn only
    /// where the ack's measure agrees with the client's.
    #[test]
    fn a_line_ending_in_blanks_is_drawn_onto_as_neovim_measures_it() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let text = ["abcdefgh", "x     ", "abcdefgh"];
        let rows = ["abcdefgh", "x", "abcdefgh"];
        for (blanks, keys, at) in [
            (&[(1, 6)][..], &b"j"[..], (1, 0)),
            (&[(1, 6)], b"jl", (1, 1)),
            (&[(1, 9)], b"j", (0, 0)),
            (&[(1, 9)], b"2jk", (2, 0)),
            (&[], b"j", (0, 0)),
            (&[], b"2j", (2, 0)),
        ] {
            let mut a = stepping(Duration::from_millis(200));
            lined(&mut a, &text, &rows, t1);
            let blanks: Vec<Value> = blanks
                .iter()
                .map(|&(l, w): &(i64, i64)| Value::Array(vec![l.into(), w.into()]))
                .collect();
            let params: Vec<Value> = vec![
                "ack".into(),
                0.into(),
                "".into(),
                "n".into(),
                1000.into(),
                1.into(),
                1.into(),
                50.into(),
                0.into(),
                0.into(),
                0.into(),
                0.into(),
                0.into(),
                8.into(),
                Value::Array(blanks),
            ];
            notify(&mut a, "nvmux_client", params, t1);
            a.keys(keys, t1);
            assert_eq!(drawn(&a), at, "{:?}", String::from_utf8_lossy(keys));
        }
    }

    /// With `'winhighlight'` or a window namespace, cursor-line drawing uses
    /// unknown groups: line changes are left to Neovim if the cursor line
    /// decides anything, drawn otherwise.
    #[test]
    fn a_window_with_highlights_of_its_own_leaves_line_changing_moves_to_neovim() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = numbered(Duration::from_millis(200), 0, &[]);
        let mut own = shaped(4, (true, true, true), (false, 0), &[], vec![]);
        own.extend([true.into(), false.into()]);
        reshape(&mut a, own, t1);
        a.keys(b"j", t1);
        assert_eq!(drawn(&a), (0, 4), "'relativenumber'");
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 4), "blind after it");
        let mut a = stepping(Duration::from_millis(200));
        let mut own = shaped(0, (false, false, true), (false, 0), &[], vec![]);
        own.extend([true.into(), false.into()]);
        reshape(&mut a, own, t1);
        a.keys(b"j", t1);
        assert_eq!(drawn(&a), (1, 0), "nothing the line decides");
    }

    /// The line left would become concealed, which the client cannot draw;
    /// moves along the line are still drawn.
    #[test]
    fn concealed_text_leaves_line_changing_moves_to_neovim() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        let mut conceals = shaped(0, (false, false, true), (false, 0), &[], vec![]);
        conceals.extend([false.into(), true.into()]);
        reshape(&mut a, conceals, t1);
        a.keys(b"j", t1);
        assert_eq!(drawn(&a), (0, 0));
        let mut a = stepping(Duration::from_millis(200));
        let mut conceals = shaped(0, (false, false, true), (false, 0), &[], vec![]);
        conceals.extend([false.into(), true.into()]);
        reshape(&mut a, conceals, t1);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 1));
    }

    /// The renumbered gutter slides with the rows.
    #[test]
    fn a_motion_to_another_line_is_drawn_over_the_rows_sliding_by() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let sliding = [
            "  2 line 1",
            "  1 line 2",
            "4   line 3",
            "  1 line 4",
            "  2 line 5",
        ];
        let landed = [
            "  1 line 2",
            "4   line 3",
            "  1 line 4",
            "  2 line 5",
            "  3 line 6",
        ];
        for (wait, rows) in [(Duration::ZERO, sliding), (Duration::from_secs(1), landed)] {
            let mut a = numbered(Duration::from_millis(200), 0, &[]);
            a.anim.set_effects(Effects {
                scroll: Some((0.1, 100)),
                ..Effects::none()
            });
            a.frame(t1);
            // Neovim scrolls two lines, the cursor on the first in view.
            let up = (
                "grid_scroll",
                vec![2, 0, 5, 0, 20, 2, 0]
                    .into_iter()
                    .map(Value::from)
                    .collect(),
            );
            let mut events = vec![up];
            for r in 0..5u64 {
                let (number, hl) = if r == 0 {
                    ("3   ".to_string(), 10)
                } else {
                    (format!("{r:>3} "), 12)
                };
                events.push(line_hl(2, r, 0, &number, hl));
            }
            for r in 3..5u64 {
                events.push(line_hl(
                    2,
                    r,
                    4,
                    &format!("{:16}", format!("line {}", r + 2)),
                    0,
                ));
            }
            events.push(("grid_cursor_goto", vec![2.into(), 0.into(), 4.into()]));
            events.push(at(2, 2, 2));
            redraw(&mut a, events, t1);
            ack(&mut a, 0, "", "n", (2, 0, 0, 2), t1);
            a.frame(t1 + wait);
            a.keys(b"j", t1 + wait);
            let line = a.anim.chain(2).and_then(|c| c.head()).map(|s| s.pos.line);
            assert_eq!(line, Some(3), "after {wait:?}");
            assert_eq!(composed(&mut a)[..5], rows, "after {wait:?}");
        }
    }

    /// The drawn cursor; grid 2 is at the screen's top left.
    fn drawn(a: &App) -> (i64, i64) {
        a.anim.cursor_at(&a.model)
    }

    /// [`shaped`] extended as [`scrolling`] does, plus the default
    /// `'iskeyword'` classes ([`predict::Stepping::classes`]).
    fn worded() -> Vec<Value> {
        let mut shape = shaped(0, (false, false, true), (false, 0), &[], Vec::new());
        shape.extend([
            false.into(),
            false.into(),
            Value::Array(vec![]),
            1.into(),
            false.into(),
        ]);
        let classes: String = words::defaults()
            .iter()
            .map(|&c| char::from(b'0' + c))
            .collect();
        shape.push(classes.into());
        shape
    }

    /// Only once the agent has sent the `'iskeyword'` classes.
    #[test]
    fn motions_by_words_move_the_cursor_at_once() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"w", t1);
        assert_eq!(drawn(&a), (0, 0), "not knowing 'iskeyword'");
        let mut a = stepping(Duration::from_millis(200));
        reshape(&mut a, worded(), t1);
        for (keys, at) in [
            (&b"w"[..], (0, 5)),
            (b"w", (1, 0)),
            (b"e", (1, 3)),
            (b"2b", (0, 5)),
            (b"ge", (0, 3)),
            (b"3W", (1, 5)),
            (b"gE", (1, 3)),
        ] {
            a.keys(keys, t1);
            assert_eq!(
                drawn(&a),
                at,
                "{} before Neovim has heard of it",
                String::from_utf8_lossy(keys)
            );
        }
        assert!(a.due(t1), "and drawn");
    }

    /// `f`, `t`, `F`, `T`, unless `:lmap` may translate the character.
    #[test]
    fn motions_to_a_character_move_the_cursor_at_once() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"f", t1);
        assert_eq!(drawn(&a), (0, 0), "waiting for the character");
        a.keys(b"e", t1);
        assert_eq!(drawn(&a), (0, 3), "fe before Neovim has heard of it");
        for (keys, at) in [(&b"Fl"[..], (0, 0)), (b"t0", (0, 4)), (b"Ti", (0, 2))] {
            a.keys(keys, t1);
            assert_eq!(
                drawn(&a),
                at,
                "{} before Neovim has heard of it",
                String::from_utf8_lossy(keys)
            );
        }
        assert!(a.due(t1), "and drawn");
        let mut a = stepping(Duration::from_millis(200));
        let mut lmapped = worded();
        lmapped.push(true.into());
        reshape(&mut a, lmapped, t1);
        a.keys(b"fe", t1);
        assert_eq!(drawn(&a), (0, 0), "with :lmap mappings on");
    }

    #[test]
    fn motions_move_the_cursor_at_once() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 1), "before Neovim has heard of it");
        assert!(a.due(t1), "and drawn");
        assert!(calls(&mut a)
            .iter()
            .any(|(_, m, args)| m == "nvim_input" && args[0] == Value::from("l")));
        ack(&mut a, 1, "l", "n", (0, 1, 1, 0), t1);
        redraw(&mut a, cursor_to(0, 1), t1);
        assert!(a.anim.chain(2).is_none(), "let go of");
        assert_eq!(drawn(&a), (0, 1));
        let tally = a.predict.as_ref().expect("predicting").trust.tally;
        assert_eq!((tally.made, tally.moved, tally.wrong), (1, 1, 0));
        for (keys, col) in [
            (&b"$"[..], 5),
            (b"$0", 0),
            (b"$^", 0),
            (b"3|", 2),
            (b"\x1b[F", 5),
            (b"$\x1b[H", 0),
            (b"g_", 5),
            (b"5l2h", 3),
            (b"20l", 5),
            (b"l ", 2),
            (b"ll\x7f", 1),
        ] {
            let mut a = stepping(Duration::from_millis(200));
            a.keys(keys, t1);
            assert_eq!(drawn(&a), (0, col), "{:?}", String::from_utf8_lossy(keys));
        }
    }

    /// Frames for earlier keys, or unacked ones part way through, move
    /// nothing.
    #[test]
    fn motions_typed_faster_than_acks_stay_predicted() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"llll", t1);
        assert_eq!(drawn(&a), (0, 4));
        ack(&mut a, 1, "l", "n", (0, 1, 1, 0), t1);
        redraw(&mut a, cursor_to(0, 1), t1);
        assert_eq!(drawn(&a), (0, 4), "the frame for the first");
        redraw(&mut a, cursor_to(0, 3), t1);
        assert_eq!(drawn(&a), (0, 4), "one part way through, unsaid");
        ack(&mut a, 4, "llll", "n", (0, 4, 4, 0), t1);
        redraw(&mut a, cursor_to(0, 4), t1);
        assert_eq!(drawn(&a), (0, 4));
        assert!(a.anim.chain(2).is_none());
    }

    #[test]
    fn a_key_the_client_cannot_follow_is_caught_up_with_at_its_ack() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"lwhh", t1);
        assert_eq!(drawn(&a), (0, 1), "blind after `w`");
        ack(&mut a, 1, "l", "n", (0, 1, 1, 0), t1);
        redraw(&mut a, cursor_to(0, 1), t1);
        assert_eq!(drawn(&a), (0, 1));
        ack(&mut a, 2, "lw", "n", (0, 5, 5, 0), t1);
        redraw(&mut a, cursor_to(0, 5), t1);
        assert_eq!(drawn(&a), (0, 3), "the two after it at once");
        ack(&mut a, 4, "lwhh", "n", (0, 3, 3, 0), t1);
        redraw(&mut a, cursor_to(0, 3), t1);
        assert!(a.anim.chain(2).is_none());
        assert_eq!(drawn(&a), (0, 3));
    }

    /// The buffer must then prove itself: eight cursor-moving motions in a
    /// row judged right while hidden.
    #[test]
    fn a_wrong_move_goes_back_to_neovims_cursor_and_proves_itself() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"ll", t1);
        assert_eq!(drawn(&a), (0, 2));
        ack(&mut a, 2, "ll", "n", (0, 3, 3, 0), t1);
        redraw(&mut a, cursor_to(0, 3), t1);
        assert!(a.anim.chain(2).is_none());
        assert_eq!(drawn(&a), (0, 3), "where Neovim has it");
        let tally = |a: &App| a.predict.as_ref().expect("predicting").trust.tally;
        assert_eq!(tally(&a).wrong, 1);
        // Proving (hidden): right, then wrong again.
        a.keys(b"l", t1);
        assert!(a.anim.chain(2).is_some(), "worked out");
        assert_eq!(drawn(&a), (0, 3), "not drawn");
        ack(&mut a, 3, "lll", "n", (0, 4, 4, 0), t1);
        redraw(&mut a, cursor_to(0, 4), t1);
        assert_eq!(tally(&a).made, 1);
        a.keys(b"l", t1);
        ack(&mut a, 4, "llll", "n", (0, 6, 6, 0), t1);
        redraw(&mut a, cursor_to(0, 6), t1);
        assert_eq!((tally(&a).wrong, tally(&a).hidden_wrong), (1, 1));
        assert_eq!(drawn(&a), (0, 6));
    }

    #[test]
    fn a_buffer_drawn_wrong_is_drawn_again_once_proved() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"l", t1);
        ack(&mut a, 1, "l", "n", (0, 2, 2, 0), t1);
        redraw(&mut a, cursor_to(0, 2), t1);
        let mut col = 2;
        for k in 2..=9u64 {
            let (key, to) = if k % 2 == 0 {
                (b"l", col + 1)
            } else {
                (b"h", col - 1)
            };
            a.keys(key, t1);
            assert_eq!(drawn(&a), (0, col), "proving, step {k}");
            let tail = if k % 2 == 0 { "l" } else { "h" };
            ack(&mut a, k, tail, "n", (0, to, to, 0), t1);
            redraw(&mut a, cursor_to(0, to), t1);
            col = to;
        }
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, col + 1), "proved");
    }

    /// A right motion whose cursor Neovim draws elsewhere (e.g. a character
    /// of another width) distrusts the window for a while.
    #[test]
    fn a_self_check_mismatch_distrusts_the_window() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"l", t1);
        ack(&mut a, 1, "l", "n", (0, 1, 1, 0), t1);
        let mut events = cursor_to(0, 1);
        events[0] = ("grid_cursor_goto", vec![2.into(), 0.into(), 2.into()]);
        redraw(&mut a, events, t1);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 2), "left to Neovim");
        assert!(a.anim.chain(2).is_none());
        let later = t1 + predict::DISTRUST;
        ack(&mut a, 2, "ll", "n", (0, 2, 2, 0), later);
        redraw(&mut a, cursor_to(0, 2), later);
        a.keys(b"l", later);
        assert_eq!(drawn(&a), (0, 3));
    }

    /// Edits precede their ack; an ack at a tick the client's lines lack
    /// leaves motions to Neovim.
    #[test]
    fn motions_wait_for_lines_after_an_edit() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"x", t1);
        let mut params: Vec<Value> = vec![
            "ack".into(),
            1.into(),
            "x".into(),
            "n".into(),
            1000.into(),
            1.into(),
            2.into(),
            50.into(),
            0.into(),
            0.into(),
            0.into(),
            0.into(),
            0.into(),
            5.into(),
        ];
        notify(&mut a, "nvmux_client", params.clone(), t1);
        redraw(&mut a, vec![line(2, 0, "ine 0")], t1);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 0), "lines a change behind");
        // The edit's splice, then the next ack.
        let splice: Vec<Value> = vec![
            "splice".into(),
            1000.into(),
            1.into(),
            1.into(),
            2.into(),
            0.into(),
            1.into(),
            1.into(),
            Value::Array(vec!["ine 0".into()]),
        ];
        notify(&mut a, "nvmux_client", splice, t1);
        params[1] = 2.into();
        params[2] = "xl".into();
        params[9] = 1.into();
        params[10] = 1.into();
        notify(&mut a, "nvmux_client", params, t1);
        redraw(&mut a, cursor_to(0, 1), t1);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 2), "the lines as the buffer has them");
    }

    /// Like a scroll ahead, then blind until the next ack.
    #[test]
    fn no_ack_gives_up_at_the_deadline() {
        let ms = Duration::from_millis;
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(ms(200));
        a.keys(b"l", t1);
        let until = a.predict.as_ref().expect("predicting").deadline(t1);
        assert_eq!(a.wake_at(t1 + ms(1)).filter(|_| !a.dirty), None);
        a.frame(t1);
        assert_eq!(a.wake_at(t1 + ms(1)), Some(until), "woken for it");
        a.frame(until - ms(1));
        assert_eq!(drawn(&a), (0, 1));
        a.frame(until);
        assert_eq!(drawn(&a), (0, 0), "given up on");
        a.keys(b"l", until);
        assert_eq!(drawn(&a), (0, 0), "blind till Neovim says");
    }

    #[test]
    fn mapped_motions_are_not_predicted() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping_with(Duration::from_millis(200), &["l", "g_"]);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 0));
        let mut a = stepping_with(Duration::from_millis(200), &["g_"]);
        a.keys(b"g_", t1);
        assert_eq!(drawn(&a), (0, 0));
        a.keys(b"$", t1);
        assert_eq!(drawn(&a), (0, 0), "and blind after it");
    }

    /// `g` prefixes Neovim's default mappings, yet `g_` is followed.
    #[test]
    fn g_is_followed_past_a_g_prefix() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"g", t1);
        assert_eq!(drawn(&a), (0, 0), "pending");
        a.keys(b"_", t1);
        assert_eq!(drawn(&a), (0, 5));
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"gjl", t1);
        assert_eq!(
            drawn(&a),
            (0, 0),
            "`gj` is not followed, nor what follows it"
        );
    }

    /// Keys taken without an ack (e.g. operator pending).
    #[test]
    fn the_fence_floor_lets_go_of_steps_neovim_took_without_saying() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 1));
        calls(&mut a);
        a.keys(b"d", t1);
        let fence = request(&calls(&mut a), "nvim_eval", "luaeval");
        answer(&mut a, fence, Value::from(2), t1);
        assert!(a.anim.chain(2).is_none());
        assert_eq!(drawn(&a), (0, 0));
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 0), "blind");
    }

    /// Neovim counts `<C-c>` twice, so counts are in doubt until a fence
    /// ties them again.
    #[test]
    fn ctrl_c_drops_the_chain_and_resyncs() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 1));
        calls(&mut a);
        a.keys(b"\x03", t1);
        assert!(a.anim.chain(2).is_none());
        assert_eq!(drawn(&a), (0, 0));
        let asked = request(&calls(&mut a), "nvim_eval", "luaeval");
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 0), "the counts in doubt");
        // The fence after `<C-c>` has a key after it, so ties nothing; the
        // next, with none after, does.
        ack(&mut a, 4, "l<C-C><C-C>l", "n", (0, 2, 2, 0), t1);
        redraw(&mut a, cursor_to(0, 2), t1);
        answer(&mut a, asked, Value::from(3), t1);
        a.tick(t1);
        let fence = request(&calls(&mut a), "nvim_eval", "luaeval");
        answer(&mut a, fence, Value::from(4), t1);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 3), "tied again");
    }

    /// Neovim may take an Alt key as `<Esc>` and the key, so counts are in
    /// doubt from it on only: the step before it stays, judged by its ack,
    /// and nothing is drawn from the Alt key until a fence ties the counts.
    #[test]
    fn a_key_with_alt_keeps_the_steps_drawn_before_it() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 1));
        calls(&mut a);
        a.keys(b"\x1bh", t1);
        assert!(a.anim.chain(2).is_some(), "the step before it stays");
        assert_eq!(drawn(&a), (0, 1));
        let asked = request(&calls(&mut a), "nvim_eval", "luaeval");
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 1), "nothing drawn from the Alt key on");
        ack(&mut a, 1, "l", "n", (0, 1, 1, 0), t1);
        redraw(&mut a, cursor_to(0, 1), t1);
        assert!(a.anim.chain(2).is_none(), "made, by Neovim's word on it");
        assert_eq!(drawn(&a), (0, 1));
        // The ack past the Alt key (`<M-h>` taken as one key) and the fence
        // with a key after it tie nothing; the next, with none after, does.
        ack(&mut a, 3, "l<M-h>l", "n", (0, 1, 1, 0), t1);
        redraw(&mut a, cursor_to(0, 1), t1);
        answer(&mut a, asked, Value::from(2), t1);
        a.tick(t1);
        let fence = request(&calls(&mut a), "nvim_eval", "luaeval");
        answer(&mut a, fence, Value::from(3), t1);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 2), "tied again");
    }

    /// An ack covering an Alt key and the key before it says only that Neovim
    /// took that key: its step goes with the ack's frame. Dropped sooner, the
    /// screen would go back behind it; kept, it would stay drawn over
    /// Neovim's later frames.
    #[test]
    fn a_count_past_a_key_with_alt_lets_go_of_the_steps_before_it_with_its_frame() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 1));
        a.keys(b"\x1bj", t1);
        ack(&mut a, 3, "l<Esc>j", "n", (1, 1, 1, 0), t1);
        assert!(a.anim.chain(2).is_some(), "drawn till the frame");
        assert_eq!(drawn(&a), (0, 1));
        redraw(&mut a, cursor_to(1, 1), t1);
        assert!(a.anim.chain(2).is_none(), "let go of with it");
        assert_eq!(drawn(&a), (1, 1));
    }

    /// If Neovim's last frame already shows what such an ack says, the step
    /// goes at once, as for an ack; not if the keys changed the buffer
    /// (`b:changedtick` 2), which only their frame draws.
    #[test]
    fn a_count_past_a_key_with_alt_lets_go_at_once_only_of_what_neovim_has_drawn() {
        for (tick, at_once) in [(1, true), (2, false)] {
            let t1 = Instant::now() + Duration::from_secs(1);
            let mut a = stepping(Duration::from_millis(200));
            a.keys(b"l", t1);
            a.keys(b"\x1bh", t1);
            let params: Vec<Value> = vec![
                "ack".into(),
                2.into(),
                "l<Esc>h".into(),
                "n".into(),
                1000.into(),
                1.into(),
                tick.into(),
                50.into(),
                0.into(),
                0.into(),
                0.into(),
                0.into(),
                0.into(),
                5.into(),
            ];
            notify(&mut a, "nvmux_client", params, t1);
            assert_eq!(a.anim.chain(2).is_none(), at_once, "b:changedtick {tick}");
            redraw(&mut a, cursor_to(0, 0), t1);
            assert!(a.anim.chain(2).is_none(), "with the frame");
            assert_eq!(drawn(&a), (0, 0));
        }
    }

    /// A fence counting past the Alt key, with keys after it and no ack:
    /// Neovim has drawn by the time it answers, so the step goes at once.
    #[test]
    fn a_fence_past_a_key_with_alt_lets_go_of_the_steps_before_it() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"l", t1);
        calls(&mut a);
        a.keys(b"\x1bj", t1);
        let asked = request(&calls(&mut a), "nvim_eval", "luaeval");
        a.keys(b"l", t1);
        redraw(&mut a, cursor_to(1, 1), t1);
        assert!(a.anim.chain(2).is_some(), "nothing says Neovim took it yet");
        answer(&mut a, asked, Value::from(3), t1);
        assert!(a.anim.chain(2).is_none(), "the fence does");
        assert_eq!(drawn(&a), (1, 1));
    }

    /// Changes on other lines (virtual text, diagnostics, timers) leave the
    /// steps. A change to their line drops them: remade at once if only past
    /// its end, else left to Neovim with the cursor snapped back.
    #[test]
    fn a_frame_lets_go_of_the_steps_only_where_it_draws_their_line_otherwise() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 1));
        redraw(&mut a, vec![line(2, 1, "lime 1")], t1);
        let tally = |a: &App| a.predict.as_ref().expect("predicting").trust.tally;
        assert_eq!(tally(&a).unjudged, 0, "another line");
        assert_eq!(drawn(&a), (0, 1));
        redraw(&mut a, vec![line(2, 0, "line 0  E")], t1);
        assert_eq!(tally(&a).unjudged, 1);
        assert_eq!(drawn(&a), (0, 1), "after its end: worked out again");
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 2));
        redraw(&mut a, vec![line(2, 0, "li: ne 0")], t1);
        assert!(a.anim.chain(2).is_none());
        assert_eq!(drawn(&a), (0, 0), "between its characters");
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 0), "blind");
        // The ack does not change that the line is laid out otherwise.
        ack(&mut a, 3, "lll", "n", (0, 3, 5, 0), t1);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 0));
    }

    /// The frame is still just the editor's grids, so it is partial: no
    /// rows, only the cursor.
    #[test]
    fn a_cursor_only_step_draws_a_partial_frame() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200)).with_partial_frames(true);
        a.frame(t1);
        a.frame(t1);
        let partly = |a: &App| a.partial.as_ref().map_or(0, |p| p.partly);
        let before = partly(&a);
        a.keys(b"l", t1);
        assert!(a.anim.still());
        let bytes = a.frame(t1);
        assert_eq!(partly(&a), before + 1);
        assert!(
            String::from_utf8_lossy(&bytes).contains("\x1b[1;2H"),
            "the cursor to column 1: {:?}",
            String::from_utf8_lossy(&bytes)
        );
    }

    /// A splice replacing line 0 with `text`, tick `from` to `to`.
    fn splice0(a: &mut App, (from, to): (i64, i64), text: &str, now: Instant) {
        let params: Vec<Value> = vec![
            "splice".into(),
            1000.into(),
            1.into(),
            from.into(),
            to.into(),
            0.into(),
            1.into(),
            1.into(),
            Value::Array(vec![text.into()]),
        ];
        notify(a, "nvmux_client", params, now);
    }

    /// A Normal-mode [`ack`] on line 0 at `tick`, byte `col`, Neovim's line
    /// width `width`.
    fn ack0(
        a: &mut App,
        count: u64,
        tail: &str,
        col: i64,
        (tick, width): (i64, i64),
        now: Instant,
    ) {
        let params: Vec<Value> = vec![
            "ack".into(),
            count.into(),
            tail.into(),
            "n".into(),
            1000.into(),
            1.into(),
            tick.into(),
            50.into(),
            0.into(),
            col.into(),
            col.into(),
            0.into(),
            0.into(),
            width.into(),
        ];
        notify(a, "nvmux_client", params, now);
    }

    /// E.g. invisible blank virtual text, detected by Neovim's measure.
    #[test]
    fn a_line_neovim_measures_otherwise_is_left_to_it() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        ack0(&mut a, 0, "", 0, (1, 9), t1);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 0), "nine columns wide to Neovim, six to us");
        let mut a = stepping(Duration::from_millis(200));
        ack0(&mut a, 0, "", 0, (1, 6), t1);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 1));
    }

    /// On a tab that may be its start: Neovim recomputes the drawn column
    /// only on a move (leaving Visual mode keeps Visual's).
    #[test]
    fn a_motion_that_moves_nothing_leaves_the_cursor_where_neovim_drew_it() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        let text: Vec<Value> = std::iter::once("\tfoo bar".to_string())
            .chain((1..20).map(|n| format!("line {n}")))
            .map(Value::from)
            .collect();
        let lines: Vec<Value> = vec![
            "lines".into(),
            1000.into(),
            1.into(),
            1.into(),
            0.into(),
            Value::Array(text),
            true.into(),
        ];
        notify(&mut a, "nvmux_client", lines, t1);
        redraw(&mut a, vec![line(2, 0, "        foo bar")], t1);
        ack0(&mut a, 0, "", 0, (1, 15), t1);
        a.keys(b"0", t1);
        assert_eq!(drawn(&a), (0, 0), "where Neovim drew it, the tab's start");
        a.keys(b"\x1b[H", t1);
        assert_eq!(drawn(&a), (0, 0));
        a.keys(b"lh", t1);
        assert_eq!(drawn(&a), (0, 7), "moved: on the tab's end");
        a.keys(b"0", t1);
        assert_eq!(drawn(&a), (0, 7));
        // A no-op key's frame predates it: the cursor stays.
        let mut a = stepping(Duration::from_millis(200));
        redraw(&mut a, cursor_to(0, 3), t1);
        a.keys(b"0", t1);
        assert_eq!(drawn(&a), (0, 3), "Neovim's word and its frame disagree");
    }

    /// `x` in a run of one character leaves the cursor put, but its ack
    /// precedes the edit's frame; later motions wait for that frame.
    #[test]
    fn an_edit_is_drawn_before_the_motions_after_it() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        splice0(&mut a, (1, 2), "abbbb", t1);
        let mut events = vec![line(2, 0, "abbbb ")];
        events.extend(cursor_to(0, 2));
        redraw(&mut a, events, t1);
        ack0(&mut a, 0, "", 2, (2, 5), t1);
        a.keys(b"x", t1);
        splice0(&mut a, (2, 3), "abbb", t1);
        ack0(&mut a, 1, "x", 2, (3, 4), t1);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 2), "not over `abbbb`, Neovim's still");
        let mut events = vec![line(2, 0, "abbb  ")];
        events.extend(cursor_to(0, 2));
        redraw(&mut a, events, t1);
        assert_eq!(drawn(&a), (0, 3), "with the edit's frame");
    }

    /// A Normal-mode paste is no key: acks wait for a fence and the client is
    /// blind after it, but earlier steps stay until the paste's frame.
    #[test]
    fn a_paste_leaves_the_motions_before_it_drawn() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"l", t1);
        a.keys(b"\x1b[200~XY\x1b[201~", t1);
        assert!(a.anim.chain(2).is_some());
        assert_eq!(drawn(&a), (0, 1));
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 1), "blind after it");
        let mut events = vec![line(2, 0, "lXYine 0")];
        events.extend(cursor_to(0, 2));
        redraw(&mut a, events, t1);
        assert!(a.anim.chain(2).is_none());
        assert_eq!(drawn(&a), (0, 2), "Neovim's frame for the paste");
    }

    /// It is a step after theirs, scrolling from their cursor. If it cannot
    /// be (an unsent line) it waits for Neovim, blind after; sideways turns
    /// always wait.
    #[test]
    fn the_wheel_over_motions_drawn_ahead_goes_on_from_them() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = scrolling(Duration::from_millis(200));
        a.keys(b"jl", t1);
        assert_eq!(drawn(&a), (1, 1));
        a.keys(b"\x1b[<65;3;2M", t1);
        assert_eq!(a.anim.chain(2).map(chain::Chain::len), Some(3));
        assert_eq!(a.anim.ahead(2), (3, 2), "scrolled ahead from the steps");
        assert_eq!((top_row(&a), drawn(&a)), ("line 3".into(), (0, 1)));
        assert!(a.predict.as_ref().is_some_and(Predictor::known));
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"l", t1);
        assert!(a.anim.chain(2).is_some());
        a.keys(b"\x1b[<65;3;2M", t1);
        assert_eq!(a.anim.chain(2).map(chain::Chain::len), Some(1));
        assert_eq!((a.anim.ahead(2), drawn(&a)), ((0, 0), (0, 1)));
        assert!(!a.predict.as_ref().is_some_and(Predictor::known));
        let mut a = scrolling(Duration::from_millis(200));
        a.keys(b"l\x1b[<67;3;2M", t1);
        assert_eq!(a.anim.chain(2).map(chain::Chain::len), Some(1));
        assert!(a.anim.side_left(2).is_none(), "not sideways");
        assert!(!a.predict.as_ref().is_some_and(Predictor::known));
    }

    /// Even with smear: travelling back would show a move no key made.
    #[test]
    fn steps_let_go_of_by_a_frame_put_the_cursor_back_at_once() {
        let ms = Duration::from_millis;
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(ms(200));
        let mut s = crate::config::Settings::default();
        s.effects.smear.enabled = true;
        s.effects.scroll.enabled = false;
        s.effects.windows.enabled = false;
        a.anim.set_effects(Effects::from_settings(&s));
        a.keys(b"$", t1);
        assert!(!a.anim.still(), "the smear travels to it");
        let mut t2 = t1;
        while !a.anim.still() && t2 < t1 + ms(400) {
            t2 += ms(16);
            a.frame(t2);
        }
        assert!(a.anim.still() && a.anim.chain(2).is_some());
        assert_eq!(drawn(&a), (0, 5));
        // Remade at once: no travel either way.
        redraw(&mut a, vec![line(2, 0, "line 0  E")], t2);
        assert_eq!(drawn(&a), (0, 5));
        assert!(a.anim.still(), "not travelling back and forth");
        redraw(&mut a, vec![line(2, 0, "li: ne 0")], t2);
        assert_eq!(drawn(&a), (0, 0));
        assert!(a.anim.still(), "not travelling back");
    }

    /// Default `'iskeyword'` classes as the agent sends them
    /// ([`typing::Facts::classes`]).
    fn classes() -> String {
        (0u8..=255)
            .map(|b| match b {
                0 | b'\t' | b' ' | 0xa0 => '0',
                b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'_' | 192..=255 => '2',
                _ => '1',
            })
            .collect()
    }

    /// Sends no pinned lines ([`Predictor::pinned`]) and buffer 1's Insert
    /// facts at Neovim's defaults, edited by `change` by position
    /// ([`typing::Facts::read`]; index 0 is the method name).
    fn facts(a: &mut App, change: impl FnOnce(&mut Vec<Value>), now: Instant) {
        let pinned = vec!["pinned".into(), 1000.into(), Value::Array(vec![])];
        notify(a, "nvmux_client", pinned, now);
        let mut v: Vec<Value> = vec![
            "insert".into(),
            1.into(),
            true.into(),
            Value::Array(vec![]),
            Value::Array(vec![]),
            false.into(),
            0.into(),
            "tcqj".into(),
            "".into(),
            false.into(),
            false.into(),
            false.into(),
            "(:),{:},[:]".into(),
            false.into(),
            true.into(),
            true.into(),
            "indent,eol,start".into(),
            false.into(),
            true.into(),
            false.into(),
            "".into(),
            classes().into(),
        ];
        change(&mut v);
        notify(a, "nvmux_client", v, now);
    }

    /// An Insert-mode [`ack`] on line 0 at `tick`, byte `col` (ASCII, so
    /// also the curswant), with the client's width for the line.
    fn ack_i(a: &mut App, count: u64, tail: &str, col: i64, tick: i64, now: Instant) {
        let width = a.predict.as_ref().and_then(|p| {
            let width = predict::width(p.text(1000, 0)?, p.shape(1000)?);
            i64::try_from(width).ok()
        });
        let params: Vec<Value> = vec![
            "ack".into(),
            count.into(),
            tail.into(),
            "i".into(),
            1000.into(),
            1.into(),
            tick.into(),
            50.into(),
            0.into(),
            col.into(),
            col.into(),
            0.into(),
            0.into(),
            width.unwrap_or(-1).into(),
        ];
        notify(a, "nvmux_client", params, now);
    }

    /// Neovim's Insert-mode frame with line 0 as `text`, cursor at `col`.
    fn typed_frame(text: &str, col: i64) -> Vec<(&'static str, Vec<Value>)> {
        let mut events = vec![
            ("mode_change", vec!["insert".into(), 1.into()]),
            line(2, 0, &format!("{text:20}")),
        ];
        events.extend(cursor_to(0, col));
        events
    }

    /// [`stepping`] with typing prediction, in Insert mode at the start of
    /// `line 0` (acked and drawn), with default facts.
    fn typing(rtt: Duration) -> App {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping_as(TYPING, rtt, &[]);
        facts(&mut a, |_| {}, t1);
        ack_i(&mut a, 0, "", 0, 1, t1);
        redraw(&mut a, typed_frame("line 0", 0), t1);
        a
    }

    /// For each of `keys` (after `from` earlier ones), Neovim's splice, ack
    /// and frame leaving line 0 as the matching `texts` entry.
    fn neovim_typed(a: &mut App, from: usize, keys: &[&str], texts: &[&str], now: Instant) {
        let mut tail = String::new();
        for (k, (key, text)) in keys.iter().zip(texts).enumerate() {
            let k = (from + k) as i64 + 1;
            tail.push_str(key);
            let col = text.find("line").unwrap_or(text.len()) as i64;
            splice0(a, (k, k + 1), text, now);
            ack_i(a, k as u64, &tail, col, k + 1, now);
            redraw(a, typed_frame(text, col), now);
        }
    }

    /// The line typed into ahead is drawn over Neovim's rows; frames with it,
    /// and after it goes (on Neovim's ack, or as the client is sent away), are
    /// built from changed rows.
    #[test]
    fn frames_drawn_over_are_put_together_in_part() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let ms = |n: u64| t1 + Duration::from_millis(n);
        let mut whole = typing(Duration::from_millis(200)).with_partial_frames(false);
        let mut part = typing(Duration::from_millis(200));
        both(&mut whole, &mut part, vec![], b"", t1);
        let before = both(&mut whole, &mut part, vec![], b"", t1);
        assert_eq!(
            both(&mut whole, &mut part, vec![], b"ab", ms(10)),
            before + 1
        );
        assert!(
            !part.anim.patched().is_empty(),
            "the line typed into, drawn over"
        );
        assert_eq!(composed(&mut part)[0], "abline 0");
        for a in [&mut whole, &mut part] {
            neovim_typed(a, 0, &["a"], &["aline 0"], ms(20));
        }
        assert_eq!(both(&mut whole, &mut part, vec![], b"", ms(20)), before + 2);
        // Sent away: the step goes and its row is put back as Neovim drew it.
        let away = crate::card::encode(&crate::card::Command::Away);
        assert_eq!(
            both(&mut whole, &mut part, vec![], &away, ms(30)),
            before + 3
        );
        assert!(part.anim.patched().is_empty());
        assert_eq!(composed(&mut part)[0], "aline 0");
    }

    #[test]
    fn typing_is_echoed_then_confirmed() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = typing(Duration::from_millis(200));
        a.keys(b"abc", t1);
        assert_eq!(
            composed(&mut a)[0],
            "abcline 0",
            "before Neovim has heard of them"
        );
        assert_eq!(drawn(&a), (0, 3));
        assert!(a.due(t1), "and drawn");
        neovim_typed(
            &mut a,
            0,
            &["a", "b", "c"],
            &["aline 0", "abline 0", "abcline 0"],
            t1,
        );
        assert!(a.anim.chain(2).is_none(), "made, and let go of");
        assert_eq!(composed(&mut a)[0], "abcline 0");
        assert_eq!(drawn(&a), (0, 3));
        let tally = a.predict.as_ref().expect("predicting").trust.tally;
        assert_eq!((tally.made, tally.wrong), (3, 0));
        a.keys(b"\x7f\x7f", t1);
        assert_eq!(composed(&mut a)[0], "aline 0");
        assert_eq!(drawn(&a), (0, 1));
    }

    /// In a buffer without facts yet, `i` is left to Neovim; the keys after
    /// it appear together once the facts arrive, after its ack.
    #[test]
    fn the_keys_typed_while_i_is_on_its_way_appear_at_its_ack() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping_as(TYPING, Duration::from_millis(200), &[]);
        a.keys(b"iab", t1);
        assert_eq!(composed(&mut a)[0], "line 0", "blind after `i`");
        ack_i(&mut a, 1, "i", 0, 1, t1);
        redraw(&mut a, typed_frame("line 0", 0), t1);
        assert_eq!(composed(&mut a)[0], "line 0", "nothing said of the buffer");
        facts(&mut a, |_| {}, t1);
        assert_eq!(composed(&mut a)[0], "abline 0", "once it is");
        assert_eq!(drawn(&a), (0, 2));
    }

    /// Splice, ack and frame may arrive apart on a lossy link; keys typed in
    /// between continue from the steps' text, not the client's lines.
    #[test]
    fn the_edits_for_a_word_on_its_way_hold_up_nothing_typed_after() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = typing(Duration::from_millis(200));
        a.keys(b"a", t1);
        splice0(&mut a, (1, 2), "aline 0", t1);
        a.keys(b"b", t1);
        ack_i(&mut a, 1, "a", 1, 2, t1);
        a.keys(b"c", t1);
        assert_eq!(composed(&mut a)[0], "abcline 0");
        assert_eq!(drawn(&a), (0, 3));
        redraw(&mut a, typed_frame("aline 0", 1), t1);
        assert_eq!(composed(&mut a)[0], "abcline 0", "Neovim's frame for `a`");
        assert_eq!(drawn(&a), (0, 3));
    }

    /// New past-end virtual text (e.g. a diagnostic) is kept and moves with
    /// the line's end as later keys are relaid. If the typed text itself
    /// differs, the window is distrusted for a while.
    #[test]
    fn a_row_drawn_anew_past_its_end_is_typed_into_as_it_is() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = typing(Duration::from_millis(200));
        a.keys(b"ab", t1);
        assert_eq!(composed(&mut a)[0], "abline 0");
        splice0(&mut a, (1, 2), "aline 0", t1);
        ack_i(&mut a, 1, "a", 1, 2, t1);
        redraw(&mut a, typed_frame("aline 0  E", 1), t1);
        assert_eq!(composed(&mut a)[0], "abline 0  E");
        assert_eq!(drawn(&a), (0, 2));
        let p = a.predict.as_ref().expect("predicting");
        assert!(
            !p.trust.refused(1000, t1),
            "the text as the client laid it out"
        );
        // Drawn otherwise, e.g. a character of an unexpected width.
        let mut a = typing(Duration::from_millis(200));
        a.keys(b"ab", t1);
        splice0(&mut a, (1, 2), "aline 0", t1);
        ack_i(&mut a, 1, "a", 1, 2, t1);
        redraw(&mut a, typed_frame("a line 0", 1), t1);
        let p = a.predict.as_ref().expect("predicting");
        assert!(p.trust.refused(1000, t1), "left to Neovim");
        assert_eq!(composed(&mut a)[0], "a line 0");
    }

    /// The typed line, drawn over its row, would not slide with the rows.
    #[test]
    fn typing_waits_for_the_rows_sliding_by_to_land() {
        let t1 = Instant::now() + Duration::from_secs(1);
        for (wait, typed) in [(Duration::ZERO, None), (Duration::from_secs(1), Some(2))] {
            let mut a = typing(Duration::from_millis(200));
            a.anim.set_effects(Effects {
                scroll: Some((0.1, 100)),
                ..Effects::none()
            });
            a.frame(t1);
            // Neovim scrolls two lines, the cursor on the first in view.
            let up = (
                "grid_scroll",
                vec![2, 0, 5, 0, 20, 2, 0]
                    .into_iter()
                    .map(Value::from)
                    .collect(),
            );
            let mut events = vec![up];
            for r in 3..5u64 {
                events.push(line(2, r, &format!("{:20}", format!("line {}", r + 2))));
            }
            events.push(("grid_cursor_goto", vec![2.into(), 0.into(), 0.into()]));
            events.push(at(2, 2, 2));
            redraw(&mut a, events, t1);
            let params: Vec<Value> = vec![
                "ack".into(),
                0.into(),
                "".into(),
                "i".into(),
                1000.into(),
                1.into(),
                1.into(),
                50.into(),
                2.into(),
                0.into(),
                0.into(),
                2.into(),
                0.into(),
                6.into(),
            ];
            notify(&mut a, "nvmux_client", params, t1);
            a.frame(t1 + wait);
            a.keys(b"x", t1 + wait);
            let line = a.anim.chain(2).and_then(|c| c.head()).map(|s| s.pos.line);
            assert_eq!(line, typed, "after {wait:?}");
        }
    }

    /// Typing into a pinned line, or entering Insert mode at its end, is
    /// left to Neovim; entering elsewhere on it is drawn.
    #[test]
    fn a_line_with_virtual_text_that_stays_put_is_left_to_neovim() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let pinned = |a: &mut App| {
            let params = vec!["pinned".into(), 1000.into(), Value::Array(vec![0.into()])];
            notify(a, "nvmux_client", params, t1);
        };
        let mut a = typing(Duration::from_millis(200));
        pinned(&mut a);
        a.keys(b"x", t1);
        assert!(a.anim.chain(2).is_none(), "typed into");
        for (key, entered) in [(&b"A"[..], false), (b"$a", false), (b"i", true)] {
            let mut a = moding(Duration::from_millis(200));
            pinned(&mut a);
            a.keys(key, t1);
            let made = a.anim.chain(2).and_then(|c| c.head()).map(|s| s.pos.rest);
            assert_eq!(made == Some(Rest::Insert), entered, "{key:?}");
        }
    }

    /// A new `trail` glyph uses its group blended over the bar, as Neovim
    /// draws it, not its bare group, which would leave a hole in the bar.
    #[test]
    fn a_glyph_new_to_a_barred_line_is_drawn_over_the_bar() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping_as(TYPING, Duration::from_millis(200), &[]);
        let mut shape = shaped(0, (false, false, true), (false, 2), &[], vec![]);
        shape[13] = true.into();
        let mut lcs = vec![Value::from(""); 10];
        lcs[4] = "-".into();
        shape[23] = Value::Array(lcs);
        reshape(&mut a, shape, t1);
        facts(&mut a, |_| {}, t1);
        let define = |id: u64, rgb: Vec<(Value, Value)>| {
            (
                "hl_attr_define",
                vec![
                    id.into(),
                    Value::Map(rgb),
                    Value::Map(vec![]),
                    Value::Array(vec![]),
                ],
            )
        };
        let mut events = vec![("option_set", vec!["termguicolors".into(), true.into()])];
        events.extend(bar_defined(21, false));
        events.push(define(22, vec![("foreground".into(), 0xaa_00_aa.into())]));
        events.push(("hl_group_set", vec!["Whitespace".into(), 22.into()]));
        events.push(define(
            23,
            vec![
                ("foreground".into(), 0xaa_00_aa.into()),
                ("background".into(), 0x30_30_30.into()),
            ],
        ));
        events.push(("mode_change", vec!["insert".into(), 1.into()]));
        events.push(line_hl(2, 0, 0, &format!("{:20}", "line 0"), 21));
        events.extend(cursor_to(0, 6));
        redraw(&mut a, events, t1);
        ack_i(&mut a, 0, "", 6, 1, t1);
        a.keys(b" ", t1);
        let predicted = composed_frame(&mut a);
        assert_eq!(composed(&mut a)[0], "line 0-");
        let mut neovim = vec![line_hl(2, 0, 0, &format!("{:20}", "line 0"), 21)];
        neovim.push(line_hl(2, 0, 6, "-", 23));
        neovim.extend(cursor_to(0, 7));
        redraw(&mut a, neovim, t1);
        let drawn_by_neovim = composed_frame(&mut a);
        assert_ne!(predicted.cells[6].style.bg, style::Color::Default);
        assert_eq!(predicted.cells[6].style, drawn_by_neovim.cells[6].style);
    }

    #[test]
    fn motions_alone_type_nothing_ahead() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        facts(&mut a, |_| {}, t1);
        ack_i(&mut a, 0, "", 0, 1, t1);
        redraw(&mut a, typed_frame("line 0", 0), t1);
        a.keys(b"a", t1);
        assert_eq!(composed(&mut a)[0], "line 0");
        assert!(a.anim.chain(2).is_none());
    }

    /// The keys after it wait for its ack too.
    #[test]
    fn a_key_a_mapping_starts_with_waits_for_neovim() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = typing(Duration::from_millis(200));
        facts(&mut a, |v| v[3] = Value::Array(vec!["(".into()]), t1);
        a.keys(b"x(y", t1);
        assert_eq!(
            composed(&mut a)[0],
            "xline 0",
            "`x`, and not `(` nor what follows"
        );
        assert_eq!(drawn(&a), (0, 1));
    }

    /// An autopair mapped after the last facts turns `(` into `()`: judged
    /// wrong, the row and cursor revert at once. Other keys are still drawn;
    /// `(` stays hidden until proved.
    #[test]
    fn an_autopair_defined_late_is_put_right_and_proven() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = typing(Duration::from_millis(200));
        a.keys(b"(", t1);
        assert_eq!(composed(&mut a)[0], "(line 0");
        splice0(&mut a, (1, 2), "()line 0", t1);
        ack_i(&mut a, 1, "(", 1, 2, t1);
        redraw(&mut a, typed_frame("()line 0", 1), t1);
        assert!(a.anim.chain(2).is_none(), "wrong");
        assert_eq!(composed(&mut a)[0], "()line 0");
        assert_eq!(drawn(&a), (0, 1));
        let tally = |a: &App| a.predict.as_ref().expect("predicting").trust.tally;
        assert_eq!(tally(&a).wrong, 1);
        a.keys(b"x", t1);
        assert_eq!(composed(&mut a)[0], "(x)line 0", "the rest drawn as ever");
        assert_eq!(drawn(&a), (0, 2));
        splice0(&mut a, (2, 3), "(x)line 0", t1);
        ack_i(&mut a, 2, "(x", 2, 3, t1);
        redraw(&mut a, typed_frame("(x)line 0", 2), t1);
        assert_eq!(tally(&a).made, 1);
        a.keys(b"(", t1);
        assert!(a.anim.chain(2).is_some(), "worked out");
        assert_eq!(composed(&mut a)[0], "(x)line 0", "and not drawn");
        assert_eq!(drawn(&a), (0, 2));
    }

    /// With a plugin's `InsertCharPre` or `TextChangedI`, typing is hidden
    /// until eight non-blank characters are judged right.
    #[test]
    fn insert_char_pre_hides_echo_until_proven() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = typing(Duration::from_millis(200));
        facts(&mut a, |v| v[5] = true.into(), t1);
        a.keys(b"abcd efg", t1);
        assert!(a.anim.chain(2).is_some(), "worked out");
        assert_eq!(composed(&mut a)[0], "line 0", "and not drawn");
        let keys = ["a", "b", "c", "d", "<Space>", "e", "f", "g", "h", "i"];
        let texts: Vec<String> = (1..=10)
            .map(|n| format!("{}line 0", &"abcd efghi"[..n]))
            .collect();
        let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
        neovim_typed(&mut a, 0, &keys[..8], &texts[..8], t1);
        a.keys(b"h", t1);
        assert_eq!(
            composed(&mut a)[0],
            "abcd efgline 0",
            "seven so far, a blank aside"
        );
        neovim_typed(&mut a, 8, &keys[8..9], &texts[8..9], t1);
        a.keys(b"i", t1);
        assert_eq!(composed(&mut a)[0], "abcd efghiline 0", "proved");
    }

    /// `<C-n>` is taken without an ack; the fence shows earlier characters
    /// taken, so the echo is dropped and later keys wait.
    #[test]
    fn completion_drops_the_echo_at_the_fence() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = typing(Duration::from_millis(200));
        a.keys(b"ab", t1);
        assert_eq!(composed(&mut a)[0], "abline 0");
        calls(&mut a);
        a.keys(b"\x0e", t1);
        let fence = request(&calls(&mut a), "nvim_eval", "luaeval");
        answer(&mut a, fence, Value::from(3), t1);
        assert!(a.anim.chain(2).is_none());
        assert_eq!(composed(&mut a)[0], "line 0");
        assert_eq!(drawn(&a), (0, 0));
        a.keys(b"c", t1);
        assert_eq!(composed(&mut a)[0], "line 0", "blind");
    }

    /// [`stepping`] with typing prediction in Normal mode, default facts,
    /// and default `'guicursor'` shapes (block, Insert bar).
    fn moding(rtt: Duration) -> App {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping_as(TYPING, rtt, &[]);
        facts(&mut a, |_| {}, t1);
        let mode = |name: &str, shape: &str| {
            Value::Map(vec![
                ("name".into(), name.into()),
                ("cursor_shape".into(), shape.into()),
                ("cell_percentage".into(), 25.into()),
            ])
        };
        let modes = vec![mode("normal", "block"), mode("insert", "vertical")];
        let events = vec![
            ("mode_info_set", vec![true.into(), Value::Array(modes)]),
            ("mode_change", vec!["normal".into(), 0.into()]),
        ];
        redraw(&mut a, events, t1);
        a
    }

    /// The next frame's terminal cursor and its `DECSCUSR` shape.
    fn terminal_cursor(a: &mut App) -> ((usize, usize), u8) {
        let mut frame = composed_frame(a);
        let c = a.anim.paint(&mut frame, &a.model, Instant::now());
        ((c.row, c.col), c.shape)
    }

    /// `i`, `a`, `A`, `I` move the cursor and make it a bar at once.
    #[test]
    fn i_draws_the_bar_at_once() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = moding(Duration::from_millis(200));
        assert_eq!(terminal_cursor(&mut a), ((0, 0), 2), "a block");
        a.keys(b"i", t1);
        assert_eq!(terminal_cursor(&mut a), ((0, 0), 6), "a bar, at once");
        ack_i(&mut a, 1, "i", 0, 1, t1);
        redraw(&mut a, typed_frame("line 0", 0), t1);
        assert!(a.anim.chain(2).is_none(), "made");
        assert_eq!(terminal_cursor(&mut a), ((0, 0), 6), "Neovim's own");
        for (keys, col) in [(&b"a"[..], 1), (b"A", 6), (b"$I", 0), (b"$a", 6)] {
            let mut a = moding(Duration::from_millis(200));
            a.keys(keys, t1);
            assert_eq!(
                terminal_cursor(&mut a),
                ((0, col), 6),
                "{:?}",
                String::from_utf8_lossy(keys)
            );
        }
    }

    /// Later motions continue over the typed text.
    #[test]
    fn esc_after_typing_goes_back_one_with_the_block() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = moding(Duration::from_millis(200));
        a.keys(b"Afoo", t1);
        assert_eq!(composed(&mut a)[0], "line 0foo");
        assert_eq!(terminal_cursor(&mut a), ((0, 9), 6));
        a.keys(b"\x1b", t1);
        a.escape_timeout(t1);
        assert_eq!(composed(&mut a)[0], "line 0foo", "as typed");
        assert_eq!(
            terminal_cursor(&mut a),
            ((0, 8), 2),
            "back a character, a block"
        );
        a.keys(b"0", t1);
        assert_eq!(terminal_cursor(&mut a), ((0, 0), 2));
        a.keys(b"$", t1);
        assert_eq!(terminal_cursor(&mut a), ((0, 8), 2), "the end as typed");
        a.keys(b"j", t1);
        assert_eq!(terminal_cursor(&mut a), ((1, 5), 2));
        assert_eq!(composed(&mut a)[0], "line 0foo", "still as typed");
    }

    /// Neovim removes an unused indent, and expands abbreviations, on
    /// `<Esc>`; both wait for Neovim.
    #[test]
    fn esc_on_an_autoindented_blank_line_waits() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = moding(Duration::from_millis(200));
        lined(&mut a, &["    "], &[""], t1);
        ack0(&mut a, 0, "", 0, (1, 4), t1);
        a.keys(b"A", t1);
        assert_eq!(terminal_cursor(&mut a), ((0, 4), 6));
        a.keys(b"\x1b", t1);
        a.escape_timeout(t1);
        assert_eq!(terminal_cursor(&mut a), ((0, 4), 6), "left to Neovim");
        let mut a = moding(Duration::from_millis(200));
        facts(&mut a, |v| v[4] = Value::Array(vec!["teh".into()]), t1);
        a.keys(b"Ateh", t1);
        a.keys(b"\x1b", t1);
        a.escape_timeout(t1);
        assert_eq!(terminal_cursor(&mut a), ((0, 9), 6), "an abbreviation");
    }

    /// Judged wrong by the acked text, the line reverts at once and `<Esc>`
    /// must prove itself again.
    #[test]
    fn an_insert_leave_that_trims_the_line_is_put_right_and_proven() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = moding(Duration::from_millis(200));
        lined(&mut a, &["ab  "], &["ab"], t1);
        ack0(&mut a, 0, "", 0, (1, 4), t1);
        a.keys(b"ix", t1);
        a.keys(b"\x1b", t1);
        a.escape_timeout(t1);
        assert_eq!(composed(&mut a)[0], "xab");
        assert_eq!(terminal_cursor(&mut a), ((0, 0), 2), "back on the `x`");
        splice0(&mut a, (1, 2), "xab  ", t1);
        splice0(&mut a, (2, 3), "xab", t1);
        let params: Vec<Value> = vec![
            "ack".into(),
            3.into(),
            "ix<Esc>".into(),
            "n".into(),
            1000.into(),
            1.into(),
            3.into(),
            50.into(),
            0.into(),
            0.into(),
            0.into(),
            0.into(),
            0.into(),
            3.into(),
        ];
        notify(&mut a, "nvmux_client", params, t1);
        let mut events = vec![line(2, 0, &format!("{:20}", "xab"))];
        events.extend(cursor_to(0, 0));
        redraw(&mut a, events, t1);
        assert!(a.anim.chain(2).is_none());
        let tally = a.predict.as_ref().expect("predicting").trust.tally;
        assert_eq!(tally.wrong, 1, "{tally:?}");
        a.keys(b"iy", t1);
        a.keys(b"\x1b", t1);
        a.escape_timeout(t1);
        assert_eq!(composed(&mut a)[0], "yxab", "typed as ever");
        assert_eq!(
            terminal_cursor(&mut a),
            ((0, 1), 6),
            "the <Esc> proving itself"
        );
    }

    /// The `i` is judged wrong by its ack, the cursor reverts at once, and
    /// `i` must prove itself again.
    #[test]
    fn an_insert_enter_that_moves_the_cursor_is_put_right_and_proven() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = moding(Duration::from_millis(200));
        a.keys(b"i", t1);
        assert_eq!(terminal_cursor(&mut a), ((0, 0), 6));
        splice0(&mut a, (1, 2), "> line 0", t1);
        ack_i(&mut a, 1, "i", 2, 2, t1);
        redraw(&mut a, typed_frame("> line 0", 2), t1);
        let tally = a.predict.as_ref().expect("predicting").trust.tally;
        assert_eq!(tally.wrong, 1, "{tally:?}");
        assert_eq!(terminal_cursor(&mut a), ((0, 2), 6), "Neovim's");
        a.keys(b"\x1b", t1);
        a.escape_timeout(t1);
        ack0(&mut a, 2, "i<Esc>", 1, (2, 8), t1);
        let mut events = vec![("mode_change", vec!["normal".into(), 0.into()])];
        events.extend(cursor_to(0, 1));
        redraw(&mut a, events, t1);
        a.keys(b"i", t1);
        assert_eq!(terminal_cursor(&mut a), ((0, 1), 2), "proving itself");
    }

    /// [`moding`] with plugin autocommands on Insert enter and/or leave.
    fn unseen_moding(entered: bool, left: bool, now: Instant) -> App {
        let mut a = moding(Duration::from_millis(200));
        facts(&mut a, |v| v.extend([entered.into(), left.into()]), now);
        a
    }

    /// The last four keys, as an ack's tail.
    fn tail(keys: &[&str]) -> String {
        keys[keys.len().saturating_sub(4)..].concat()
    }

    fn normal_frame(line: i64, col: i64) -> Vec<(&'static str, Vec<Value>)> {
        let mut events = vec![("mode_change", vec!["normal".into(), 0.into()])];
        events.extend(cursor_to(line, col));
        events
    }

    /// With an `InsertLeave`, `InsertLeavePre` or `ModeChanged` hook (e.g. a
    /// trailing-blank trimmer), `<Esc>` and the keys after it are hidden
    /// until eight leave the buffer unchanged. Entering is drawn throughout.
    #[test]
    fn insert_leave_hides_esc_until_proven() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = unseen_moding(false, true, t1);
        a.keys(b"A", t1);
        assert_eq!(terminal_cursor(&mut a), ((0, 6), 6), "A drawn");
        a.keys(b"\x1b", t1);
        a.escape_timeout(t1);
        a.keys(b"0", t1);
        assert_eq!(
            terminal_cursor(&mut a),
            ((0, 6), 6),
            "<Esc> and 0 worked out, and not drawn"
        );
        assert_eq!(a.anim.chain(2).map(chain::Chain::len), Some(3));
        let mut keys = vec!["A"];
        ack_i(&mut a, 1, &tail(&keys), 6, 1, t1);
        redraw(&mut a, typed_frame("line 0", 6), t1);
        keys.push("<Esc>");
        ack0(&mut a, 2, &tail(&keys), 5, (1, 6), t1);
        redraw(&mut a, normal_frame(0, 5), t1);
        keys.push("0");
        ack0(&mut a, 3, &tail(&keys), 0, (1, 6), t1);
        redraw(&mut a, normal_frame(0, 0), t1);
        assert!(a.anim.chain(2).is_none(), "made");
        for n in 1..=8 {
            a.keys(b"A", t1);
            keys.push("A");
            ack_i(&mut a, keys.len() as u64, &tail(&keys), 6, 1, t1);
            redraw(&mut a, typed_frame("line 0", 6), t1);
            a.keys(b"\x1b", t1);
            a.escape_timeout(t1);
            let drawn = terminal_cursor(&mut a);
            if n < 8 {
                assert_eq!(drawn, ((0, 6), 6), "{n} proved so far");
            } else {
                assert_eq!(drawn, ((0, 5), 2), "proved");
            }
            keys.push("<Esc>");
            ack0(&mut a, keys.len() as u64, &tail(&keys), 5, (1, 6), t1);
            redraw(&mut a, normal_frame(0, 5), t1);
        }
        let tally = a.predict.as_ref().expect("predicting").trust.tally;
        assert_eq!((tally.wrong, tally.hidden_wrong), (0, 0), "{tally:?}");
    }

    /// An `InsertLeave` hook added after `<Esc>` was proved (by hand, or a
    /// late plugin) makes it hidden and prove itself again.
    #[test]
    fn an_insert_leave_made_after_esc_was_drawn_has_it_prove_itself() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = unseen_moding(false, false, t1);
        let mut keys = vec![];
        for n in 0..9 {
            a.keys(b"A", t1);
            keys.push("A");
            ack_i(&mut a, keys.len() as u64, &tail(&keys), 6, 1, t1);
            redraw(&mut a, typed_frame("line 0", 6), t1);
            a.keys(b"\x1b", t1);
            a.escape_timeout(t1);
            assert_eq!(
                terminal_cursor(&mut a),
                ((0, 5), 2),
                "{n}: nothing to prove"
            );
            keys.push("<Esc>");
            ack0(&mut a, keys.len() as u64, &tail(&keys), 5, (1, 6), t1);
            redraw(&mut a, normal_frame(0, 5), t1);
        }
        facts(&mut a, |v| v.extend([false.into(), true.into()]), t1);
        a.keys(b"A", t1);
        keys.push("A");
        ack_i(&mut a, keys.len() as u64, &tail(&keys), 6, 1, t1);
        redraw(&mut a, typed_frame("line 0", 6), t1);
        a.keys(b"\x1b", t1);
        a.escape_timeout(t1);
        assert_eq!(terminal_cursor(&mut a), ((0, 6), 6), "proving itself");
    }

    /// Likewise with `InsertEnter` or `ModeChanged`: `i` and what follows
    /// are hidden until eight are right; `<Esc>` is drawn throughout.
    #[test]
    fn insert_enter_hides_i_until_proven() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = unseen_moding(true, false, t1);
        a.keys(b"ix", t1);
        assert_eq!(terminal_cursor(&mut a), ((0, 0), 2), "a block still");
        assert_eq!(composed(&mut a)[0], "line 0", "nothing typed");
        ack_i(&mut a, 1, "i", 0, 1, t1);
        redraw(&mut a, typed_frame("line 0", 0), t1);
        splice0(&mut a, (1, 2), "xline 0", t1);
        ack_i(&mut a, 2, "ix", 1, 2, t1);
        redraw(&mut a, typed_frame("xline 0", 1), t1);
        a.keys(b"\x1b", t1);
        a.escape_timeout(t1);
        assert_eq!(terminal_cursor(&mut a), ((0, 0), 2), "<Esc> drawn");
        let mut keys = vec!["i", "x", "<Esc>"];
        ack0(&mut a, 3, &tail(&keys), 0, (2, 7), t1);
        redraw(&mut a, normal_frame(0, 0), t1);
        for n in 1..=8 {
            a.keys(b"i", t1);
            let drawn = terminal_cursor(&mut a);
            if n < 8 {
                assert_eq!(drawn, ((0, 0), 2), "{n} proved so far");
            } else {
                assert_eq!(drawn, ((0, 0), 6), "proved");
            }
            keys.push("i");
            ack_i(&mut a, keys.len() as u64, &tail(&keys), 0, 2, t1);
            redraw(&mut a, typed_frame("xline 0", 0), t1);
            a.keys(b"\x1b", t1);
            a.escape_timeout(t1);
            keys.push("<Esc>");
            ack0(&mut a, keys.len() as u64, &tail(&keys), 0, (2, 7), t1);
            redraw(&mut a, normal_frame(0, 0), t1);
        }
    }

    /// A drawn `<Esc>` made as predicted but also editing another line (an
    /// unknown or newly active autocommand): the motion after it is remade
    /// over the new buffer, and `<Esc>` must prove itself again, though it
    /// is not counted wrong.
    #[test]
    fn an_esc_that_edits_another_line_proves_itself_again() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = moding(Duration::from_millis(200));
        a.keys(b"Ax", t1);
        a.keys(b"\x1b", t1);
        a.escape_timeout(t1);
        a.keys(b"j", t1);
        assert_eq!(terminal_cursor(&mut a), ((1, 5), 2), "<Esc> and j drawn");
        ack_i(&mut a, 1, "A", 6, 1, t1);
        redraw(&mut a, typed_frame("line 0", 6), t1);
        splice0(&mut a, (1, 2), "line 0x", t1);
        ack_i(&mut a, 2, "Ax", 7, 2, t1);
        redraw(&mut a, typed_frame("line 0x", 7), t1);
        let trimmed: Vec<Value> = vec![
            "splice".into(),
            1000.into(),
            1.into(),
            2.into(),
            3.into(),
            1.into(),
            2.into(),
            1.into(),
            Value::Array(vec!["line 1".into()]),
        ];
        notify(&mut a, "nvmux_client", trimmed, t1);
        ack0(&mut a, 3, "Ax<Esc>", 6, (3, 7), t1);
        redraw(&mut a, normal_frame(0, 6), t1);
        let tally = a.predict.as_ref().expect("predicting").trust.tally;
        assert_eq!(
            (tally.made, tally.wrong, tally.unjudged),
            (3, 0, 1),
            "j let go of: {tally:?}"
        );
        assert_eq!(a.anim.chain(2).map(chain::Chain::len), Some(1));
        assert_eq!(terminal_cursor(&mut a), ((1, 5), 2), "and worked out again");
        let params: Vec<Value> = vec![
            "ack".into(),
            4.into(),
            "Ax<Esc>j".into(),
            "n".into(),
            1000.into(),
            1.into(),
            3.into(),
            50.into(),
            1.into(),
            5.into(),
            6.into(),
            0.into(),
            0.into(),
            6.into(),
        ];
        notify(&mut a, "nvmux_client", params, t1);
        redraw(&mut a, normal_frame(1, 5), t1);
        assert!(a.anim.chain(2).is_none(), "made");
        a.keys(b"A", t1);
        assert_eq!(terminal_cursor(&mut a), ((1, 6), 6), "A drawn");
        a.keys(b"\x1b", t1);
        a.escape_timeout(t1);
        assert_eq!(terminal_cursor(&mut a), ((1, 6), 6), "<Esc> proving itself");
    }

    /// One ack covering the typing and `<Esc>` cannot tell what leaving did
    /// to the buffer: with a leave hook, that `<Esc>` proves nothing and the
    /// motion after it is remade.
    #[test]
    fn esc_judged_with_the_text_before_it_proves_nothing() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = unseen_moding(false, true, t1);
        a.keys(b"Ax", t1);
        a.keys(b"\x1b", t1);
        a.escape_timeout(t1);
        a.keys(b"0", t1);
        ack_i(&mut a, 1, "A", 6, 1, t1);
        redraw(&mut a, typed_frame("line 0", 6), t1);
        splice0(&mut a, (1, 2), "line 0x", t1);
        ack0(&mut a, 3, "Ax<Esc>", 6, (2, 7), t1);
        let mut events = vec![line(2, 0, &format!("{:20}", "line 0x"))];
        events.extend(normal_frame(0, 6));
        redraw(&mut a, events, t1);
        let tally = a.predict.as_ref().expect("predicting").trust.tally;
        assert_eq!(tally.unjudged, 1, "0 let go of: {tally:?}");
        assert_eq!(
            a.anim.chain(2).map(chain::Chain::len),
            Some(1),
            "and remade"
        );
        let mut keys = vec!["A", "x", "<Esc>", "0"];
        ack0(&mut a, 4, &tail(&keys), 0, (2, 7), t1);
        redraw(&mut a, normal_frame(0, 0), t1);
        for n in 1..=9 {
            a.keys(b"A", t1);
            keys.push("A");
            ack_i(&mut a, keys.len() as u64, &tail(&keys), 7, 2, t1);
            redraw(&mut a, typed_frame("line 0x", 7), t1);
            a.keys(b"\x1b", t1);
            a.escape_timeout(t1);
            let drawn = terminal_cursor(&mut a);
            if n <= 8 {
                assert_eq!(drawn, ((0, 7), 6), "{} proved so far", n - 1);
            } else {
                assert_eq!(drawn, ((0, 6), 2), "proved");
            }
            keys.push("<Esc>");
            ack0(&mut a, keys.len() as u64, &tail(&keys), 6, (2, 7), t1);
            redraw(&mut a, normal_frame(0, 6), t1);
        }
    }

    /// `<Esc>` repeats the text; the typing between is drawn after `2i`'s ack.
    #[test]
    fn a_count_before_i_leaves_esc_to_neovim() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = moding(Duration::from_millis(200));
        a.keys(b"2ix", t1);
        assert_eq!(terminal_cursor(&mut a), ((0, 0), 2), "blind after `2i`");
        ack_i(&mut a, 2, "2i", 0, 1, t1);
        redraw(&mut a, typed_frame("line 0", 0), t1);
        assert_eq!(composed(&mut a)[0], "xline 0");
        a.keys(b"\x1b", t1);
        a.escape_timeout(t1);
        assert_eq!(terminal_cursor(&mut a), ((0, 1), 6), "left to Neovim");
    }

    /// Replayed from the counted `i`'s ack, its `<Esc>` is left to Neovim
    /// whatever was predicted when typed.
    #[test]
    fn esc_after_a_count_waits_whatever_is_typed_after_it() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = moding(Duration::from_millis(200));
        a.keys(b"3ix", t1);
        a.keys(b"\x1b", t1);
        a.escape_timeout(t1);
        a.keys(b"1ay", t1);
        ack_i(&mut a, 2, "3i", 0, 1, t1);
        redraw(&mut a, typed_frame("line 0", 0), t1);
        assert_eq!(composed(&mut a)[0], "xline 0");
        assert_eq!(
            terminal_cursor(&mut a),
            ((0, 1), 6),
            "Insert mode still, till Neovim says"
        );
    }

    /// Block `I` inserts on every line at `<Esc>`. The client never enters
    /// Insert mode from Visual, so the `<Esc>` is Neovim's; the typing is
    /// drawn.
    #[test]
    fn esc_after_a_visual_block_insert_waits_for_neovim() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = moding(Duration::from_millis(200));
        a.keys(b"\x16jj", t1);
        let visual: Vec<Value> = vec![
            "ack".into(),
            3.into(),
            "<C-V>jj".into(),
            "\x16".into(),
            1000.into(),
            1.into(),
            1.into(),
            50.into(),
            2.into(),
            2.into(),
            2.into(),
            0.into(),
            0.into(),
            6.into(),
        ];
        notify(&mut a, "nvmux_client", visual, t1);
        let mut events = vec![("mode_change", vec!["visual".into(), 2.into()])];
        events.extend(cursor_to(2, 2));
        redraw(&mut a, events, t1);
        a.keys(b"Ifoo", t1);
        ack_i(&mut a, 4, "<C-V>jjI", 0, 1, t1);
        redraw(&mut a, typed_frame("line 0", 0), t1);
        assert_eq!(composed(&mut a)[0], "fooline 0");
        a.keys(b"\x1b", t1);
        a.escape_timeout(t1);
        assert_eq!(terminal_cursor(&mut a), ((0, 3), 6), "left to Neovim");
    }

    /// The first device attributes reply ends the client's kitty query;
    /// later ones go to the server.
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

    /// Capped at [`RELAYED_ESCAPE`], whatever wakes the loop meanwhile.
    #[test]
    fn an_escape_is_let_go_on_time() {
        let t0 = Instant::now();
        let mut a = app(false);
        a.keys(b"\x1b", t0);
        let at = t0 + Duration::from_millis(RELAYED_ESCAPE);
        assert_eq!(a.wake_at(t0 + Duration::from_millis(5)), Some(at));
        assert!(!a.escape_due(at - Duration::from_millis(1)));
        assert!(a.escape_due(at));
        a.escape_timeout(at);
        let sent = calls(&mut a);
        assert!(sent
            .iter()
            .any(|(_, m, args)| m == "nvim_input" && args[0] == Value::from("<Esc>")));
        assert_eq!(a.wake_at(at), None);
    }

    /// `'nottimeout'` releases an escape at once.
    #[test]
    fn a_shorter_ttimeoutlen_is_kept() {
        let t0 = Instant::now();
        let mut a = app(false);
        redraw(
            &mut a,
            vec![("option_set", vec!["ttimeoutlen".into(), 5.into()])],
            t0,
        );
        a.keys(b"\x1b", t0);
        assert_eq!(a.wake_at(t0), Some(t0 + Duration::from_millis(5)));
        a.escape_timeout(t0 + Duration::from_millis(5));
        redraw(
            &mut a,
            vec![("option_set", vec!["ttimeout".into(), false.into()])],
            t0,
        );
        a.keys(b"\x1b", t0);
        assert_eq!(a.wake_at(t0), Some(t0 + Duration::from_millis(1)));
    }

    /// [`REPLY_WAIT`] from the last byte.
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

    fn notify(app: &mut App, method: &str, params: Vec<Value>, now: Instant) {
        let v = Value::Array(vec![2.into(), method.into(), Value::Array(params)]);
        app.message(&encoded(v), now);
    }

    /// `nvim_ui_try_resize_grid` args: grid, width, height.
    fn grids_asked(calls: &[Call]) -> Vec<Vec<Value>> {
        calls
            .iter()
            .filter(|(_, m, _)| m == "nvim_ui_try_resize_grid")
            .map(|(_, _, args)| args.clone())
            .collect()
    }

    /// Attached with fixed centring at 8 columns, the sent calls returned.
    fn wide_app(t0: Instant) -> (App, Vec<Call>) {
        let mut a = app(true).with_centre(Centre::Fixed, 8);
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

    /// The terminal rows after a frame at `now`.
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

    /// Released on exit so other UIs see it whole.
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

    /// Centred from the next frame, not a round trip later; Neovim's resize
    /// then changes nothing.
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

    /// A float anchored to a centred window shifts with the text but stays
    /// on screen: Neovim already fitted it there.
    #[test]
    fn a_float_goes_in_with_the_text_no_further_than_the_screen() {
        let t0 = Instant::now();
        let (mut a, _) = wide_app(t0);
        redraw(
            &mut a,
            vec![
                resize(1, 20, 6),
                resize(2, 20, 5),
                place(2, 1000, 0, 0, 20, 5),
                viewport(2, 1000),
            ],
            t0,
        );
        redraw(&mut a, vec![resize(2, 8, 5)], t0);
        // Grid 3: an 8-wide float on the window, at column `col`.
        let float = |col: i64| -> (&'static str, Vec<Value>) {
            let n = |v: i64| Value::from(v);
            (
                "win_float_pos",
                vec![
                    n(3),
                    n(1001),
                    "NW".into(),
                    n(2),
                    n(1),
                    n(col),
                    true.into(),
                    n(50),
                    n(1),
                    n(1),
                    n(col),
                ],
            )
        };
        redraw(
            &mut a,
            vec![resize(3, 8, 1), float(2), line(3, 0, "abcdefgh")],
            t0,
        );
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(
            shown(&mut a, t1)[1],
            "        abcdefgh",
            "6 in, with the text"
        );
        redraw(&mut a, vec![float(10)], t1);
        let t2 = t1 + Duration::from_secs(1);
        assert_eq!(
            shown(&mut a, t2)[1],
            "            abcdefgh",
            "2 in, to the screen's edge and not past it"
        );
    }

    /// A `tails` notification: window 1000's row `row` continues with `text`
    /// from column `start`.
    fn tails_of(row: i64, start: i64, text: &str) -> Vec<Value> {
        let piece = Value::Array(vec![text.into(), 0.into()]);
        let row = Value::Array(vec![row.into(), start.into(), Value::Array(vec![piece])]);
        vec![
            "tails".into(),
            1000.into(),
            true.into(),
            Value::Array(vec![row]),
        ]
    }

    /// A window narrowed to 8 of 20 columns (inset 6), `text` on row 0.
    fn narrowed_with(text: &str, t0: Instant) -> App {
        let (mut a, _) = wide_app(t0);
        redraw(
            &mut a,
            vec![
                resize(1, 20, 6),
                resize(2, 20, 5),
                place(2, 1000, 0, 0, 20, 5),
                viewport(2, 1000),
            ],
            t0,
        );
        redraw(&mut a, vec![resize(2, 8, 5), line(2, 0, text)], t0);
        a
    }

    /// Up to the window's edge, and only with its flush.
    #[test]
    fn what_runs_past_a_grids_edge_is_drawn_in_the_margin() {
        let t0 = Instant::now();
        let mut a = narrowed_with("abc ■ de", t0);
        notify(&mut a, "nvmux_client", tails_of(0, 4, "■ defgh-and-on"), t0);
        assert_eq!(shown(&mut a, t0)[0], "      abc ■ de", "not yet");
        redraw(&mut a, vec![], t0);
        a.screen.invalidate();
        assert_eq!(
            shown(&mut a, t0)[0],
            "      abc ■ defgh-an",
            "on in the margin, to the window's edge"
        );
        notify(&mut a, "nvmux_client", tails_of(0, 4, ""), t0);
        redraw(&mut a, vec![], t0);
        a.screen.invalidate();
        assert_eq!(shown(&mut a, t0)[0], "      abc ■ de", "said gone");
    }

    /// No flush follows when nothing on screen changed; [`TAILS_WAIT`] then.
    #[test]
    fn what_runs_past_a_grids_edge_is_drawn_without_a_flush_in_time() {
        let t0 = Instant::now();
        let mut a = narrowed_with("abc ■ de", t0);
        notify(&mut a, "nvmux_client", tails_of(0, 4, "■ defgh"), t0);
        assert_eq!(a.wake_at(t0), Some(t0 + TAILS_WAIT));
        a.tick(t0 + TAILS_WAIT / 2);
        assert_eq!(shown(&mut a, t0)[0], "      abc ■ de", "still waiting");
        a.tick(t0 + TAILS_WAIT);
        a.screen.invalidate();
        assert_eq!(shown(&mut a, t0)[0], "      abc ■ defgh");
        assert_eq!(a.tails_since, None, "nothing waits");
    }

    #[test]
    fn what_runs_past_a_grids_edge_slides_with_its_row() {
        let t0 = Instant::now();
        let mut a = narrowed_with("line 0", t0);
        let mut rows: Vec<_> = (1..5).map(|r| line(2, r, &format!("line {r}"))).collect();
        rows.push(line(2, 3, "line 3 ■"));
        redraw(&mut a, rows, t0);
        notify(&mut a, "nvmux_client", tails_of(3, 7, "■ tail"), t0);
        redraw(&mut a, vec![], t0);
        let t1 = t0 + Duration::from_secs(1);
        a.anim.set_effects(Effects {
            scroll: Some((0.1, 1)),
            ..Effects::none()
        });
        let shows = |a: &mut App, now: Instant| -> Vec<String> {
            a.frame(now);
            let f = compose::compose(&a.model, &a.anim, 20, 6);
            f.text().lines().map(|l| l.trim_end().to_string()).collect()
        };
        assert_eq!(shows(&mut a, t1)[3], "      line 3 ■ tail");

        let up = (
            "grid_scroll",
            vec![2, 0, 5, 0, 8, 2, 0]
                .into_iter()
                .map(Value::from)
                .collect(),
        );
        redraw(
            &mut a,
            vec![up, at(2, 2, 2), line(2, 3, "line 5"), line(2, 4, "line 6")],
            t1,
        );
        let sliding = shows(&mut a, t1);
        assert_eq!(sliding[1], "      line 2", "a row behind");
        assert_eq!(sliding[2], "      line 3 ■ tail", "with its row");
        let landed = shows(&mut a, t1 + Duration::from_secs(1));
        assert_eq!(landed[1], "      line 3 ■ tail", "landed with it");
        assert_eq!(landed[2], "      line 4");
    }

    /// The tails were for the old width.
    #[test]
    fn a_grid_cut_narrower_lets_go_of_what_ran_past_its_edge() {
        let t0 = Instant::now();
        let mut a = narrowed_with("abc ■ de", t0);
        notify(&mut a, "nvmux_client", tails_of(0, 4, "■ defgh"), t0);
        redraw(&mut a, vec![], t0);
        assert!(a.model.tails.contains_key(&2));
        let narrower = Value::Array(vec![Value::Array(vec![1000.into(), 6.into()])]);
        notify(
            &mut a,
            "nvmux_client",
            vec!["measure".into(), 2.into(), narrower],
            t0,
        );
        assert_eq!(a.model.grids[&2].width(), 6, "cut");
        assert!(!a.model.tails.contains_key(&2), "let go of");
    }

    /// Re-attaching without multigrid for a foreign UI releases narrowed
    /// grids before the detach, while Neovim still accepts it, so that UI
    /// sees them whole.
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

    /// Narrower than the window: a grid that wide. As wide or wider (a
    /// terminal window wants all of it): released. Unmeasured:
    /// `window_width`.
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

    /// Asked at once, Neovim could see no change between redraws and leave
    /// the local copy cut for good.
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

    fn wanting(width: i64) -> Vec<Value> {
        let each = vec![Value::Array(vec![1000.into(), width.into()])];
        vec!["measure".into(), 2.into(), Value::Array(each)]
    }

    /// [`wide_app`] with one full-width drawn window asked for 8
    /// (`window_width`, unmeasured), then 6 once measured, as for a new
    /// split. Neither ask answered.
    fn asked_twice(t0: Instant) -> App {
        let (mut a, _) = wide_app(t0);
        let n = Value::from;
        let mut events = vec![
            resize(1, 20, 6),
            resize(2, 20, 5),
            place(2, 1000, 0, 0, 20, 5),
            viewport(2, 1000),
        ];
        events.extend(whole(2, 20, 5, &["text"]));
        redraw(&mut a, events, t0);
        assert_eq!(grids_asked(&calls(&mut a)), [vec![n(2), n(8), n(0)]]);
        notify(&mut a, "nvmux_client", wanting(6), t0);
        assert_eq!(grids_asked(&calls(&mut a)), [vec![n(2), n(6), n(0)]]);
        assert_eq!(a.model.grids[&2].width(), 6, "cut again");
        a
    }

    /// See [`App::grid_resized`].
    #[test]
    fn an_answer_to_an_ask_asked_again_for_less_is_not_taken_whole() {
        let t0 = Instant::now();
        let mut a = asked_twice(t0);
        redraw(&mut a, vec![resize(2, 8, 5)], t0);
        assert_eq!(a.model.grids[&2].width(), 6, "kept to the cut");
        assert!(a.model.lacking().is_empty(), "nothing undrawn");
        assert!(!a.take_refresh(), "so nothing refreshed");
        assert!(!a.holding(t0), "and nothing held");
        assert!(grids_asked(&calls(&mut a)).is_empty(), "nor asked again");

        redraw(&mut a, vec![resize(2, 6, 5)], t0);
        assert_eq!(a.model.grids[&2].width(), 6, "answered");
        assert!(
            a.cut.is_empty() && a.unanswered.is_empty(),
            "nothing waits on Neovim"
        );
        assert!(!a.take_refresh());
    }

    /// Given both asks before a redraw, Neovim answers only the last.
    #[test]
    fn the_answer_to_the_last_ask_is_taken_whole() {
        let t0 = Instant::now();
        let (mut a, _) = wide_app(t0);
        let n = Value::from;
        let mut events = vec![
            resize(1, 20, 6),
            resize(2, 20, 5),
            place(2, 1000, 0, 0, 20, 5),
            viewport(2, 1000),
        ];
        events.extend(whole(2, 20, 5, &["text"]));
        redraw(&mut a, events, t0);
        assert_eq!(grids_asked(&calls(&mut a)), [vec![n(2), n(8), n(0)]]);
        assert_eq!(a.model.grids[&2].width(), 8, "cut");
        notify(&mut a, "nvmux_client", wanting(12), t0);
        assert_eq!(grids_asked(&calls(&mut a)), [vec![n(2), n(12), n(0)]]);
        assert_eq!(
            a.model.grids[&2].width(),
            8,
            "wider has to come from Neovim"
        );

        // Neovim draws the added columns.
        let mut events = vec![resize(2, 12, 5)];
        events.extend((0..5).map(|r| line_at(2, r, 8, "    ")));
        redraw(&mut a, events, t0);
        assert_eq!(a.model.grids[&2].width(), 12, "taken");
        assert!(a.cut.is_empty() && a.unanswered.is_empty());
        assert!(!a.take_refresh(), "drawn whole");
    }

    /// A split asked twice still flies in when the first answer arrives
    /// alone and the windows are placed again, instead of being held for a
    /// refresh (which would land it at once).
    #[test]
    fn a_split_flies_in_through_an_answer_to_an_ask_asked_again() {
        let t0 = Instant::now();
        let (mut a, _) = wide_app(t0);
        let n = Value::from;
        let measure = |each: &[(i64, i64)]| {
            let each = each
                .iter()
                .map(|&(win, w)| Value::Array(vec![n(win), n(w)]))
                .collect();
            vec!["measure".into(), 2.into(), Value::Array(each)]
        };
        let mut events = vec![
            resize(1, 20, 6),
            resize(2, 20, 5),
            place(2, 1000, 0, 0, 20, 5),
            viewport(2, 1000),
        ];
        events.extend(whole(2, 20, 5, &["left"]));
        redraw(&mut a, events, t0);
        notify(&mut a, "nvmux_client", measure(&[(1000, 8)]), t0);
        redraw(&mut a, vec![resize(2, 8, 5)], t0);
        calls(&mut a);

        // `:vsplit`: the window 10 wide, a new one 9 wide to its right.
        let t1 = t0 + Duration::from_secs(1);
        let mut events = vec![
            place(2, 1000, 0, 0, 10, 5),
            resize(4, 9, 5),
            place(4, 1001, 0, 11, 9, 5),
            viewport(4, 1001),
        ];
        events.extend(whole(4, 9, 5, &["right"]));
        events.extend((0..5).map(|r| line_at(1, r, 10, "\u{2502}")));
        redraw(&mut a, events, t1);
        assert_eq!(grids_asked(&calls(&mut a)), [vec![n(4), n(8), n(0)]]);
        notify(&mut a, "nvmux_client", measure(&[(1000, 8), (1001, 6)]), t1);
        assert_eq!(grids_asked(&calls(&mut a)), [vec![n(4), n(6), n(0)]]);

        let ms = |m: u64| t1 + Duration::from_millis(m);
        redraw(&mut a, vec![resize(4, 8, 5)], ms(5));
        redraw(
            &mut a,
            vec![place(2, 1000, 0, 0, 10, 5), place(4, 1001, 0, 11, 9, 5)],
            ms(10),
        );
        // The separator's column on the top row.
        let line = |a: &mut App, at| {
            shown(a, at)[0]
                .chars()
                .position(|c| c == '\u{2502}')
                .unwrap_or(0)
        };
        let flying = line(&mut a, ms(40));
        assert!(flying > 10, "landed at once: the line at {flying}");
        assert_eq!(line(&mut a, ms(1000)), 10, "landed");
    }

    /// Neovim's grid already has that width, so the ask waits for the next
    /// answer (see [`a_grid_cut_is_asked_back_whole_once_neovim_has_cut_it`]).
    #[test]
    fn an_ask_back_to_an_answer_not_taken_waits_for_the_next() {
        let t0 = Instant::now();
        let n = Value::from;
        let mut a = asked_twice(t0);
        redraw(&mut a, vec![resize(2, 8, 5)], t0);
        notify(&mut a, "nvmux_client", wanting(8), t0);
        assert!(
            grids_asked(&calls(&mut a)).is_empty(),
            "asked before Neovim has cut it"
        );
        redraw(&mut a, vec![resize(2, 6, 5)], t0);
        assert_eq!(
            grids_asked(&calls(&mut a)),
            [vec![n(2), n(8), n(0)]],
            "not asked once it has"
        );
    }

    #[test]
    fn nothing_centred_measures_nothing() {
        let t0 = Instant::now();
        let mut a = app(true).with_centre(Centre::Off, 8);
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

    /// E.g. by a killed client. Asked once the window's width is known (from
    /// placement or the layout), and only once per attach: later narrowing
    /// is a live client's.
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

        // Unplaced (a session another UI drew): wait for the layout.
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

    /// Windows on other clients' grids get this client's width, or whole if
    /// it would not narrow them (whether the grid is narrower or, after the
    /// window shrank, wider).
    #[test]
    fn a_centring_client_has_the_windows_it_finds_as_it_wants_them() {
        let t0 = Instant::now();
        let (mut a, _) = wide_app(t0);
        let n = Value::from;
        redraw(
            &mut a,
            vec![
                resize(1, 20, 6),
                // Wider than its text's 8, on a grid of 6.
                resize(2, 6, 2),
                place(2, 1000, 0, 0, 13, 2),
                viewport(2, 1000),
                // Narrower than 8, on a grid of 4.
                resize(3, 4, 2),
                place(3, 1001, 0, 14, 6, 2),
                viewport(3, 1001),
                // On a grid of 7, wider than itself.
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

    /// A client with a `block-blinkwait700-blinkoff400-blinkon250` cursor,
    /// drawn once.
    fn blinking(smear: bool, blink: bool, t0: Instant) -> App {
        let mut s = crate::config::Settings::default();
        s.effects.smear.enabled = smear;
        s.effects.blink.enabled = blink;
        s.effects.scroll.enabled = false;
        s.effects.windows.enabled = false;
        let mut a = App::new(palette(), Effects::from_settings(&s), false, (20, 6));
        a.start();
        let sent = calls(&mut a);
        answer(
            &mut a,
            request(&sent, "nvim_get_api_info", ""),
            Value::Array(vec![7.into()]),
            t0,
        );
        let mode = Value::Map(vec![
            ("name".into(), "normal".into()),
            ("cursor_shape".into(), "block".into()),
            ("blinkwait".into(), 700.into()),
            ("blinkoff".into(), 400.into()),
            ("blinkon".into(), 250.into()),
        ]);
        redraw(
            &mut a,
            vec![
                resize(1, 20, 6),
                ("mode_info_set", vec![true.into(), Value::Array(vec![mode])]),
                ("mode_change", vec!["normal".into(), 0.into()]),
                line(1, 0, "hello"),
                goto(0, 0),
            ],
            t0,
        );
        calls(&mut a);
        assert!(a.due(t0));
        a.frame(t0);
        a
    }

    /// Simulates `run`'s idle loop from `from` to `until` (wake times rounded
    /// up to the ms as `run` does); returns (wakes, frames).
    fn left_alone(a: &mut App, from: Instant, until: Instant) -> (usize, usize) {
        let (mut wakes, mut frames) = (0, 0);
        let mut t = from;
        loop {
            if a.due(t) {
                a.frame(t);
                frames += 1;
            }
            let Some(at) = a.wake_at(t) else {
                break;
            };
            let ms = at.saturating_duration_since(t).as_micros().div_ceil(1000);
            t += Duration::from_millis(u64::try_from(ms).unwrap_or(u64::MAX).max(1));
            if t >= until {
                break;
            }
            wakes += 1;
        }
        (wakes, frames)
    }

    /// Nearly every wake draws a frame, and it sleeps between fades.
    #[test]
    fn a_blinking_cursor_is_drawn_as_it_fades() {
        let t0 = Instant::now();
        let mut a = blinking(false, true, t0);
        let (wakes, frames) = left_alone(&mut a, t0, t0 + Duration::from_secs(10));
        // Fades are 40% of each half of a 650 ms blink: ~15 frames a blink,
        // 15 blinks after the wait.
        assert!(frames >= 200, "{frames} frames for {wakes} wakes");
        assert!(
            frames * 10 >= wakes * 9,
            "{frames} frames for {wakes} wakes"
        );
        let mut still = blinking(false, false, t0);
        assert_eq!(
            left_alone(&mut still, t0, t0 + Duration::from_secs(10)),
            (0, 0),
            "a cursor that does not blink sleeps"
        );
    }

    /// The blink must not stop the animation clock restarting after a quiet
    /// spell, or the smear would jump in one step.
    #[test]
    fn a_smear_after_a_quiet_spell_moves_with_the_blink_on() {
        let frames_after_jump = |blink: bool| {
            let t0 = Instant::now();
            let mut a = blinking(true, blink, t0);
            let quiet = t0 + Duration::from_secs(2);
            left_alone(&mut a, t0, quiet);
            redraw(&mut a, vec![goto(5, 15)], quiet);
            // Frames while the smear moves, before the blink wait ends.
            left_alone(&mut a, quiet, quiet + Duration::from_millis(300)).1
        };
        let still = frames_after_jump(false);
        assert!(still > 3, "the smear moves over {still} frames");
        assert_eq!(frames_after_jump(true), still);
    }

    /// One more frame within [`AWAY_FRAME_GAP`] shows the cursor whole, then
    /// nothing. Back in front it draws at once and restarts the blink wait.
    #[test]
    fn a_client_away_holds_its_blink_whole() {
        let t0 = Instant::now();
        let mut a = blinking(false, true, t0);
        // Mid first fade-out.
        let t1 = t0 + Duration::from_millis(760);
        left_alone(&mut a, t0, t1);
        told(&mut a, &crate::card::Command::Away, t1);
        let once = a.wake_at(t1).expect("drawn once more");
        assert!(once <= t1 + AWAY_FRAME_GAP, "drawn at {once:?}");
        assert!(a.due(once));
        let held = String::from_utf8_lossy(&a.frame(once)).into_owned();
        // Compare the SGR the cursor's `h` is written in: whole as at the
        // start, not part faded.
        let mut fresh = blinking(false, true, t0);
        fresh.screen.invalidate();
        let whole = String::from_utf8_lossy(&fresh.frame(t0)).into_owned();
        let mut faded = blinking(false, true, t0);
        left_alone(&mut faded, t0, t1);
        faded.screen.invalidate();
        let faded = String::from_utf8_lossy(&faded.frame(t1)).into_owned();
        let cell = |f: &str| {
            let end = f.find("mh")?;
            f[..end].rfind("\x1b[").map(|at| f[at..end].to_string())
        };
        assert_eq!(
            cell(&held),
            cell(&whole),
            "the cursor drawn whole: {held:?}"
        );
        assert_ne!(cell(&held), cell(&faded), "and not part faded");
        let t2 = t1 + Duration::from_secs(10);
        assert_eq!(left_alone(&mut a, once, t2), (0, 0), "and then nothing");

        told(&mut a, &crate::card::Command::Clear, t2);
        assert!(a.due(t2), "back from away, at once");
        a.frame(t2);
        let wait = t2 + Duration::from_millis(690);
        assert_eq!(left_alone(&mut a, t2, wait), (0, 0), "whole for the wait");
        let (_, frames) = left_alone(&mut a, wait, wait + Duration::from_secs(2));
        assert!(frames > 20, "and blinking after it: {frames} frames");
    }
}
