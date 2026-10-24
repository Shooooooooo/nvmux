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
//! Nor while nobody sees it: a client nvmux has put aside — parked, or
//! started ahead of its session's first visit — is told so
//! ([`crate::card::Command::Away`]), and until it is back in front it
//! animates nothing and draws what changes at most once every
//! [`AWAY_FRAME_GAP`], however busy its session is.

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
/// and not saved, and whether its session is on another machine, which the
/// file never has (see [`crate::config::Drawing`]).
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

/// The longest a lone `ESC` is held for the rest of a sequence, in
/// milliseconds, whatever `'ttimeoutlen'` says (see [`input`]). The client
/// runs under nvmux's relay, which has held it for as long already, for the
/// same reason (`crate::pty`'s `SEQUENCE_TIMEOUT`): the bytes of one key are
/// split only by a buffer boundary, which is machine time, not human time.
/// Held for `'ttimeoutlen'` on top, in a terminal that sends the Escape key
/// as a bare `ESC`, every `<Esc>` that leaves Insert mode would wait 60 ms by
/// Neovim's default before Neovim heard of it.
const RELAYED_ESCAPE: u64 = 10;

/// How soon to ask who is attached again, having found a UI only passing
/// through grid 1: another nvmux client's refresh, or one attaching again.
const RECHECK: Duration = Duration::from_millis(30);

/// How long what the editor says runs past the grids' edges waits for the
/// `flush` it was said ahead of (see [`wide::TAILS_LUA`]), before it is
/// taken without one: a redraw that changed nothing on the screen but that
/// sends none.
const TAILS_WAIT: Duration = Duration::from_millis(25);

/// How long a refresh is given to be answered before it is given up, and how
/// long before one that came to nothing is tried again.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(2);

/// What the client asks after the round trip with, while the user is at
/// work (see [`predict::PING_EVERY`]): a call Neovim answers as it reads
/// it, at work on keys typed ahead or not, as it does `nvim_get_api_info`
/// at the attach — parsing an expression, which changes nothing. Not
/// `nvim_get_mode`: Neovim holds that till it has taken every key typed
/// ahead (neovim#6247), so it timed those as well as the link — 1.35 s for
/// a link of 0.5 s, with pages typed ahead of a Neovim at work on them —
/// and a scroll made ahead of Neovim waited the longer for it, where how
/// long keys wait for Neovim is counted otherwise: by the agent's word
/// that Neovim is still at work on the scrolls typed (`behind`), and where
/// it says what Neovim made of each key, by how long one takes to be
/// answered (see [`predict::Predictor::deadline`]). Nor `nvim_input` with
/// no keys, which looks through what is typed ahead for `<C-c>` again.
const PING: &str = "nvim_parse_expression";

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
/// time — and the view's own (`lines`, window, buffer, `b:changedtick`,
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
/// the window's buffer, and, told to say what Neovim made of each key, what
/// decides where a motion takes the cursor besides — those mapped looked up
/// again then a second after Neovim last went to wait for a key, for one
/// mapped since; a view moved sideways,
/// as Neovim moves it (`leftcol`, window, its first column); the display
/// column the cursor wants in the window that has it, Neovim's `curswant`,
/// when that or the window changes (`want`, window, column) — or, told by
/// its sixth argument to say what Neovim made of each key, that with the
/// rest of it, as Neovim goes to wait for a key with every key typed taken,
/// when any of it changes (`ack`, the keys Neovim has taken, counted, the
/// last of them, its mode, and the window, buffer, `b:changedtick`, line
/// count, cursor, `curswant` and view they left, and how wide Neovim lays
/// the cursor's line out: see [`predict::Ack`]), the edits they made sent
/// first and, in Normal, Visual and Insert mode, the frame that draws them
/// flushed as the next key is taken, where Neovim takes it without waiting
/// for it and so without flushing that frame itself — and what would make a
/// key typed in Insert mode in the buffer of the window that has the cursor
/// do other than what typing it plainly does, as Insert mode is entered
/// there, the buffer is, or an option that bears on it is set, where any of
/// it changed (`insert`, see [`typing::Facts`]); and
/// each time Neovim moves a view while it is behind on the keys typed
/// (`behind`), which it may send no frame for till it has caught up — what
/// the client scrolled ahead of it waits for it meanwhile (see
/// [`Predictor::deadline`]). The keys are counted in `_G.nvmux_taken`, by
/// channel, for a fence to read (see [`predict::Sync`]).
const AGENT_LUA: &str = "local chan, predict, keys, ikeys, reach, acks, typing = ...
local api = vim.api
local group = api.nvim_create_augroup('nvmux_client_' .. chan, { clear = true })
-- Whether this client has gone: what is left of the agent stops. And what
-- lets go of its count of the keys, where it keeps one.
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
  -- The keys typed that Neovim has taken, counted as the client counts the
  -- ones it sends (see predict::Sync): each key of Neovim's notation one, a
  -- character or a name in <…>, as it comes off the typeahead before a
  -- mapping makes anything else of it; a click or a turn of the wheel one;
  -- a paste none. Nought from the moment the agent is left, so that a fence
  -- that reads it before the first key is not taken for no agent at all;
  -- with the last few keys, for the client to check its count by.
  local taken, tail = 0, {}
  -- Whether the frame for the keys last said to have been taken may not
  -- have gone out yet (see the SafeState handler below), till the next key
  -- typed is taken; and the modes the client judges by what is said of the
  -- keys, which that frame is flushed in (see predict::Rest).
  local unflushed = false
  local judged = { n = true, v = true, V = true, ['\\22'] = true, i = true }
  -- Where <C-w> and <C-u> stop once in Insert mode, as Neovim has it, for
  -- the client to stop them there too: Neovim's Insstart_orig (orig),
  -- which goes back before each key to where Insert mode began, Insstart
  -- (began) — the cursor as it is entered, or the line's start for o and O
  -- — but stays put (sticks) from the <C-g>u that Neovim's own mappings of
  -- <C-w> and <C-u> type on, where it began then, and from there goes back
  -- only as a key takes out what is before it. Characters typed, <CR> and
  -- <Tab> move neither; any other key typed there — an arrow, a click,
  -- completion — and a line's break taken out, or its indent done again,
  -- may, and they are not known again till Insert mode is entered again
  -- (began false). With the key last taken in Normal mode, which entered
  -- it; and what of the last key in Insert mode is seen only once Neovim
  -- has taken it: whether it took characters out (rubbed), or was a
  -- character typed past the line's indent (typed, the line and where its
  -- indent ended), and whether it was the <C-g> of a pair.
  local began, orig, sticks, entering = false, false, false, nil
  local rubbed, typed_at, ctrl_g = false, nil, false
  local function indent_of()
    return #api.nvim_get_current_line():match('^[ \\t]*')
  end
  -- What the key before did, now that Neovim has taken it.
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
      -- Neovim drew what the keys said to have been taken made, and then
      -- took this one: if it took it without waiting for it, it flushed
      -- none of that, and the frame goes out now, before this key changes
      -- anything. If it did wait, it flushed it then, and nothing is left.
      -- With the cursor put back where Neovim draws it: it puts it there
      -- before it flushes, and may have drawn elsewhere since, as in
      -- Insert mode the mode's own message, before taking a key.
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
  -- The keys something maps, or starts a mapping with, in the window's
  -- buffer: looked up again a second after they last were. And where the
  -- client is told what Neovim made of each key, how a key that moves the
  -- cursor a line down or up is mapped, for the client to know its own
  -- (see predict::Idiom): to what, whether as an expression, whether what
  -- it maps to is mapped again, and whether no longer mapping starts with
  -- it, which Neovim would wait to tell apart.
  local function mapped(buf)
    local m = maps[buf]
    if m and vim.uv.now() - m.at < 1000 then
      return m.keys, m.ikeys, m.idioms
    end
    local function look(list, modes)
      local out = {}
      for _, key in ipairs(list) do
        -- The keys that enter Insert mode are followed in Normal mode
        -- alone: in Visual mode, i and a start text objects, which plugins
        -- map there.
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
  -- The display columns 'colorcolumn' colours in a window, from nought: a
  -- column counted from 'textwidth' only where that is set.
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
  -- What 'iskeyword' makes of each character under 256 in a buffer, a
  -- digit a character: 0 for a blank, 1 for punctuation and 2 for one of a
  -- word, as charclass() has it there (see predict::Stepping::classes).
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
    -- Where the client is told what Neovim made of each key, a window with
    -- 'vartabstop' is not followed: its tabs are not 'tabstop' wide.
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
    -- 'listchars' but its tab, as predict::Listchars::read has them; and
    -- whether Neovim looks at every character of a line for highlighting of
    -- its own, and so at its blanks (see predict::Blanks).
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
    -- Where the client is told what Neovim made of each key, what decides
    -- where a motion takes the cursor besides, and what it moves with it
    -- (see predict::Stepping): 'cursorlineopt' as one for the number, two
    -- for the line and four for the screen line; whether the window has
    -- highlights of its own, or conceals what the cursor's line shows; the
    -- columns 'colorcolumn' colours; 'scrolljump'; whether it may conceal
    -- text on any line; what 'iskeyword' makes of each character; whether
    -- :lmap mappings take the character typed after f and its kind for
    -- another, 'iminsert' 1; 'sections' and 'paragraphs'; and whether a view
    -- Neovim centres on the cursor is centred by rows, 'smoothscroll' in a
    -- window that wraps.
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
  -- What of a window's options decides how the cursor's line is drawn — its
  -- numbers, its bar, what is concealed, the columns coloured — and how far
  -- a motion past 'scrolloff' scrolls the view, as they were when its shape
  -- was last said, where the client is told what Neovim made of each key;
  -- how many lines its buffer has, which a key that adds or takes out lines
  -- changes, for the keys typed after it to be worked out from the shape
  -- that goes with Neovim's word on it; the buffer's 'iskeyword', which a
  -- filetype's plugin sets as the buffer is loaded; and its 'iminsert',
  -- which Insert mode's <C-^> turns on and off. Set by an autocommand of
  -- another event, as configurations turn 'relativenumber' and
  -- 'cursorline' off in Insert mode and on again out of it, they fire no
  -- OptionSet; nor does a highlight namespace given the window, nor <C-^>.
  -- So they are read again as Neovim goes to wait for a key, and the shapes
  -- said again ahead of the ack where they changed.
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
  -- caught up. Nor need it send a frame till then, and a frame is what
  -- lands the scrolls the client made ahead of it: so the client is told,
  -- with each scroll, that Neovim is still at work on them.
  au('WinScrolled', {}, function()
    if scrolled then
      send('behind')
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
  -- The column the cursor wants in the window that has it: Neovim's
  -- curswant, which a scroll that takes the cursor to another line lands it
  -- on, wherever on its own line it is now. Said when it or the window
  -- changes, as Neovim goes to wait for a key — once it has taken those
  -- typed ahead, and before the frame that shows what they did.
  -- Where the client is to be told what Neovim made of each key, it is said
  -- with the rest of that instead: the keys taken, the last of them, and
  -- where they left the cursor, the view and the mode, when any of it has
  -- changed — Neovim says SafeState again after every request — the edits
  -- they made sent first, so that the client's lines are the buffer's as of
  -- the b:changedtick this says (see predict::Predictor::fresh); and how wide
  -- Neovim lays the cursor's line out, what it draws between the characters
  -- and all, short of a line longer than the client keeps — and so each line
  -- in view that ends in blanks, among which blank virtual text shows
  -- nothing of itself on the screen. A second after, the keys mapped are
  -- looked up afresh: one mapped since, by a plugin or by hand, has the
  -- window's shape said again.
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
      if #l <= cap and l:find('%s$') then
        blanks[#blanks + 1] = { view.topline + i - 2, vim.fn.virtcol({ view.topline + i - 1, #l }) }
      end
    end
    -- The lines about the view, as far either side of it as the lines sent
    -- go (see around), that Neovim draws with more than their text in its
    -- colours, which the client draws a line a scroll ahead of Neovim
    -- brings into view in: virtual text — among their characters, after
    -- them, over them, at a column — or a sign, in its column or over the
    -- number, or a sign's highlight across the line or its number. Such a
    -- line is left to Neovim (see predict::Predictor::decorated). Said as
    -- runs of lines, each from its first to the one after its last, and the
    -- lines looked over for them: short of the band past as many marks of a
    -- kind as are looked through, to the line of the last of them. Said
    -- ahead of the ack, where they changed.
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
    -- Where the client types ahead of Neovim, the lines in view with virtual
    -- text that Neovim draws in a column of the window's — right-aligned, at
    -- a column given, over the text — or inline after the last character,
    -- staying before what is typed there: what the client would otherwise
    -- move with the line's end (see predict::Predictor::pinned). Said ahead
    -- of the ack, where they changed.
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
    -- Said just ahead of the frame Neovim flushes as it waits for the next
    -- key, which the client takes it with. But where that key has come in
    -- already, Neovim goes on to it without waiting, and so without
    -- flushing: the next frame the client would have is one for keys after
    -- these. So the frame is flushed as the next key is taken, once Neovim
    -- has drawn it all (see on_key above) — not here, before Neovim draws
    -- its ruler and statusline, its folds and diffs; nor only where a key
    -- is found to have come in, as looking for one takes in a <C-c> typed
    -- ahead, which Neovim then never sees. In the modes the client judges
    -- by the word (see predict::Rest).
    if judged[mode] then
      unflushed = true
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
  -- How far the client may scroll ahead, again, as the round trip moves:
  -- and where it is told what Neovim made of the keys, told it afresh.
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
  -- What would make a key typed in Insert mode in a buffer do other than
  -- what typing it plainly does (see typing::Facts), where the client types
  -- ahead of Neovim: the keys mappings start with, the abbreviations,
  -- whether a plugin's autocommand may do anything to what is typed, or as
  -- Insert mode is entered or left, and the options that bear on it — said
  -- for the buffer in the window that has the cursor as Insert mode is
  -- entered, once what that set off has been, a plugin loaded on the way
  -- in and the mappings it makes among it; as a buffer is entered, and an
  -- option among those is set; and only where any of it changed. A prompt
  -- buffer's text goes in after its prompt, wherever Insert mode is
  -- entered, and none of the prompt comes out: none goes in as typed.
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
    local function hooks(buf)
      return { unseen(buf, { 'InsertCharPre', 'TextChangedI', 'CursorMovedI' }),
        unseen(buf, { 'InsertEnter', 'ModeChanged' }),
        unseen(buf, { 'InsertLeavePre', 'InsertLeave', 'ModeChanged' }) }
    end
    local function facts()
      if gone then
        return
      end
      local buf, win = api.nvim_get_current_buf(), api.nvim_get_current_win()
      local bo, wo = vim.bo[buf], vim.wo[win]
      local firsts = {}
      -- Neovim's own mappings of <C-W> and <C-U>, which have them break
      -- the undo sequence first: followed as they are, not as mappings.
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
      -- Neovim's comp_textwidth: with 'wrapmargin', the window's width less
      -- it and the fold and sign columns, and eight for a number column
      -- whatever it takes; the whole gutter and eight here, if anything
      -- narrower.
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
    -- A plugin may make an autocommand of these at any time, loaded late or
    -- set up by hand: whether one does is read again as Neovim goes to wait
    -- for a key, and what is said of the buffer said again ahead of the ack
    -- where it changed.
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
    /// The display column it lands on, and the one it wants from then on.
    vcol: usize,
    want: usize,
    /// The first display column Neovim then shows the view from, having
    /// moved it to show the cursor if that was out of view or too near an
    /// edge (see [`predict::showing`]); `None` where the client cannot say,
    /// the cursor at the end of a line sent cut short, which goes on.
    left: Option<usize>,
}

impl Landed {
    /// The grid column the cursor is in, in a view from first display
    /// column `left` of a window `width` columns wide: `None` out of it.
    fn col(&self, left: usize, shape: &predict::Shape, width: usize) -> Option<usize> {
        self.vcol
            .checked_sub(left)
            .map(|c| shape.textoff + c)
            .filter(|&col| col < width)
    }
}

/// Where a scroll ahead lands the cursor in window `win`, `width` columns
/// wide: from display column `want`, on a line the client has been sent.
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

/// Whether a key that scrolls can be predicted from what the client has of
/// the editor: from Neovim's last word on the keys, and those sent since,
/// where the agent says what Neovim made of each (see
/// [`Predictor::known`]); where not, once a fence asked after every key it
/// could not follow has been answered (see [`predict::Typed::sure`]).
fn sure(p: &Predictor) -> bool {
    if p.acks() {
        p.known()
    } else {
        p.typed.sure()
    }
}

/// Whether the client has what it takes to draw window `win`'s view of
/// lines `shown` from first display column `left`, `width` columns wide,
/// the cursor on line `line`: every line, and those sent cut short reaching
/// past the view's right edge (see [`predict::known`]).
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

/// Buffer line `line` of window `win`, which Neovim's view does not show,
/// with the predictions `p`, drawn as the client draws a line a scroll ahead
/// of Neovim brings into view (see [`Predictor::row`]) in the colours `look`
/// has, the cursor drawn on line `cursor`, in a window `h` rows tall and
/// `width` columns wide: its number as Neovim's gutter has it there, in the
/// group its place takes of `ids` (see [`predict::numbers`]); its sign and
/// fold columns, with `'cursorline'` numbering the cursor's line, in that
/// line's groups on it; and the bar of `'cursorline'` laid under it if it
/// is the cursor's, made in `model` (see [`model::Derive`]) — the whole row,
/// from the window's first column. `None` where the client cannot draw it
/// so: a line it has not, a bar it cannot lay. Only for a window whose
/// gutter and bar the client can draw afresh at all (see [`App::afresh`]).
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

/// Where a step of the cursor moved ahead of Neovim in a window goes on
/// from, as [`App::onward`] finds it: the window, its view as Neovim has
/// it, its text rectangle in its grid, the window, gutter and first display
/// column the steps are worked out with, where the cursor is, and the
/// newest step.
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
    /// Whether Neovim's view of the window shows buffer line `l`.
    fn neovims(&self, l: i64) -> bool {
        let h = (self.rect.1 - self.rect.0) as i64;
        (self.view.topline..self.view.topline + h).contains(&l)
    }

    /// The text of buffer line `l` a step may go onto, with the window's
    /// predictions `p`: as the steps typed it, or as the agent sent it,
    /// whole; stepped over as Neovim does ([`predict::ordinary`]) and —
    /// where Neovim may take a character for another width than the
    /// client does (see `stepping`) — all ASCII.
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
    /// [`PING`], for the round trip it takes: see [`predict`].
    Ping,
    /// A request asked after keys, whose answer says Neovim has taken them
    /// and drawn what they did: see [`predict::Typed`]. With the count of
    /// inputs it was asked after, and the fence as [`predict::Sync`] knows
    /// it.
    Fence {
        sent: u64,
        fenced: predict::Fenced,
    },
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

/// The input a scroll the client predicts is made of: typed or turned `by`,
/// the client's number for it (see [`predict::Sync`]), and when it was
/// sent — which is when the time Neovim is given to make it runs from (see
/// [`Predictor::deadline`]), made out again from an ack or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cause {
    by: By,
    seq: u64,
    at: Instant,
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
    /// The steps being made are made again, a frame having let go of them
    /// (see [`App::remake`]): they put the cursor back where it was drawn,
    /// not travelling there.
    remaking: bool,
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
    /// What the agent said Neovim made of the keys, for the frame it was
    /// said ahead of: the client's number for the last key it covers, and
    /// the ack (see [`App::heard_ack`]).
    ack: Option<(u64, predict::Ack)>,
    /// The client's number for the last key before one Neovim may count as
    /// two, where the agent has said it counted past it, for the frame that
    /// was said ahead of: Neovim's word that it has taken every key to it,
    /// and no more of them (see [`predict::Sync::passed`]).
    passed: Option<u64>,
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
    /// a terminal wider than it needs, its width judged by what it shows or
    /// `window_width` for all of them (see [`wide`]).
    centre: Centre,
    /// `[client] window_width`: how many columns a window's text gets with
    /// it centred; 0 for no limit.
    window_width: usize,
    /// The measure has been left in the editor, this attach.
    measuring: bool,
    /// The window grids asked to be narrower than their windows, and how
    /// wide: see [`wide`].
    narrowed: HashMap<u64, usize>,
    /// The window grids the client has cut to the width it asked for, which
    /// Neovim has not resized since, and how wide Neovim last had each: see
    /// [`App::narrow_windows`].
    cut: HashMap<u64, usize>,
    /// The widths asked of each window grid that Neovim has not resized it
    /// to since, oldest first: what tells an answer to an ask asked again
    /// since from the answer to the last (see [`App::grid_resized`]).
    unanswered: HashMap<u64, Vec<usize>>,
    /// What each window of the tab page in front wants, by window, as the
    /// measure last said (see [`wide::MEASURE_LUA`]).
    wants: BTreeMap<i64, usize>,
    /// What runs past the grid's edge of each window the editor has said it
    /// of since the last `flush`, by window, and since when: taken with the
    /// `flush` it was said ahead of (see [`wide::TAILS_LUA`]).
    tails: Vec<(i64, BTreeMap<usize, wide::Tail>)>,
    tails_since: Option<Instant>,
    /// The window grids this attach has looked at for a width another
    /// client asked for, each once: see [`App::narrow_windows`].
    looked: HashSet<u64>,
    /// What a frame put together from only the rows that changed needs to
    /// know of the last (see [`App::frame`]): none only for a test's client,
    /// made to put every frame together whole.
    partial: Option<Partial>,
}

/// What a frame put together from only the rows that changed needs to know
/// of the frame before it (see [`App::frame`]).
#[derive(Debug, Default)]
struct Partial {
    /// When the last frame was put together, by the grids' clock (see
    /// [`grid::tick`]): what changed after it is the next frame's.
    at: u64,
    /// Whether the last frame was the editor's grids where the editor put
    /// them and nothing more (see [`Animator::still`]), and no card over it.
    still: bool,
    /// The rows of this frame to put together.
    rows: Vec<bool>,
    /// How many frames have been put together from only the rows that
    /// changed: for the tests to know it was done at all.
    #[cfg(test)]
    partly: usize,
}

impl App {
    /// A client drawing with `effects`, predicting if `predict` (see
    /// [`predict`]) — the scrolls alone till told otherwise (see
    /// [`App::with_predictions`]) — on a terminal of `size`.
    pub fn new(term: Palette, effects: Effects, predict: bool, size: (usize, usize)) -> Self {
        Self {
            model: Model::new(term),
            screen: Screen::moving_rows(),
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

    /// This client, with each window's text in the middle of the window as
    /// `centre` says: `window_width` wide, or as wide as what it shows wants
    /// (see [`wide`]).
    pub fn with_centre(self, centre: Centre, window_width: usize) -> Self {
        let mut app = Self {
            centre,
            window_width,
            ..self
        };
        // A grid narrower than its window is a window's: windows of their
        // own are wanted.
        app.multigrid_wanted |= app.centring();
        app
    }

    /// This client, working out `predictions` ahead of Neovim, where it
    /// predicts at all (see [`crate::config::Predictions`]).
    pub fn with_predictions(mut self, predictions: Predictions) -> Self {
        if let Some(p) = self.predict.as_mut() {
            p.set_predictions(predictions);
        }
        self
    }

    /// This client, having the terminal move the rows a frame moved if `on`,
    /// as every client does, or writing them again if not: for a test to
    /// hold the one up against the other (see [`region`]).
    #[cfg(test)]
    pub fn with_scroll_regions(mut self, on: bool) -> Self {
        self.screen.set_regions(on);
        self
    }

    /// This client, putting each frame together from only the rows that
    /// changed where it can if `on`, as every client does, or every row of
    /// each if not: for a test to hold the one up against the other (see
    /// [`App::frame`]).
    #[cfg(test)]
    pub fn with_partial_frames(mut self, on: bool) -> Self {
        if on != self.partial.is_some() {
            self.partial = on.then(Partial::default);
        }
        self
    }

    /// Whether each window's text is centred, as it stands: not with no
    /// limit on a window's width.
    fn centring(&self) -> bool {
        self.centre != Centre::Off && self.window_width != 0
    }

    /// Say who this client is, and attach — once it is known who else is
    /// attached, where that decides how.
    pub fn start(&mut self) {
        let id = self.outbox.request("nvim_get_api_info", &[]);
        self.asked.insert(id, Ask::ApiInfo);
        // Answered at once, busy or not, so a measure of the link alone.
        self.asked_at.insert(id, Instant::now());
        if let Some(p) = self.predict.as_mut() {
            p.attached();
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

    /// Leave the measure in the editor for this attach, once the client knows
    /// its own channel; and with it, what says what runs past the edges of
    /// the grids it narrows (see [`wide::TAILS_LUA`]).
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
                self.model.set_laststatus(Some(l));
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
                // What its rows ran on with past its edge was said of the
                // width it had; it is said again as Neovim draws it at this.
                self.model.forget_tails(grid);
            }
            // A height of nought asks for none: the window's own.
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

    /// Neovim resizing `grid` to `width`, about to be taken into the model:
    /// the grid is as wide as Neovim has it again, whatever the client cut
    /// it to (see [`App::narrow_windows`]) — but for an answer to an ask the
    /// client has asked again of it since, which would have the client's
    /// copy wider than the client cut it, and is left as wide as it is.
    ///
    /// A window split off another is asked for `window_width` before the
    /// measure has said what it wants, and asked again, for less, once it
    /// has; taking the second ask after redrawing for the first, Neovim
    /// answers both. It makes its grid of the first width from the one it
    /// had, and draws nothing of what it kept again, while the client has
    /// already cut its copy to the second width: taken whole, the copy would
    /// be wider than it was ever drawn, and the client would wait on a
    /// refresh for the columns between — holding its frames, and with them
    /// the split's flying in — that Neovim's answer to the second ask cuts
    /// off again.
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
                // As wide as Neovim has it now, for the asks that wait on
                // Neovim's resizing a grid the client cut.
                self.cut.insert(grid, *width);
                *width = copy;
            }
            _ => {
                self.cut.remove(&grid);
            }
        }
    }

    /// How wide asking `width` of the window on `grid` has Neovim make its
    /// grid: nought is the window's own width.
    fn asked_width(&self, grid: u64, width: usize) -> usize {
        match self.model.layout.get(&grid).map(|p| &p.place) {
            Some(Place::Window { width: whole, .. }) if width == 0 => *whole,
            _ => width,
        }
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
        self.unanswered.clear();
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
        self.unanswered.clear();
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
        // Told to type ahead of Neovim, it is to say what bears on that as
        // well; not told, it is left as it was before there was any.
        if typing {
            args.push(Arg::Bool(true));
        }
        let id = self
            .outbox
            .request("nvim_exec_lua", &[Arg::str(AGENT_LUA), Arg::Array(args)]);
        self.asked.insert(id, Ask::Agent);
    }

    /// Tell the agent how far either side of a view to keep lines, should
    /// the round trip have moved it: see [`Predictor::reach`].
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
            // Said as Neovim draws what it says of, ahead of the `flush`:
            // taken with that frame.
            Some("tails") => {
                if let Some(win) = int(1) {
                    let sure = !matches!(params.get(2), Some(ValueRef::Boolean(false)));
                    self.tails.push((win, wide::tails(params.get(3), sure)));
                    self.tails_since.get_or_insert(now);
                }
            }
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
            Some("ack") => self.heard_ack(&params[1..], now),
            // Neovim is still at work on keys typed ahead of it, and may
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
                if what == "shape" {
                    if let Some(win) = int(1) {
                        self.reshaped(win, now);
                    }
                }
                // What may let the keys typed since Neovim's last word go in
                // ahead of it, where they are typed into its buffer.
                let base = self.predict.as_ref().and_then(|p| p.base.as_ref());
                if what == "insert" && base.is_some_and(|b| Some(b.ack.buf) == int(1)) {
                    self.remake(now);
                }
                // What a scroll ahead was waiting for, perhaps.
                if matches!(what, "lines" | "splice") && self.flushed {
                    self.fill_in(now);
                }
            }
            None => {}
        }
    }

    /// The agent said window `win`'s shape again: a window left to Neovim
    /// for drawing the cursor elsewhere may draw it as the client does now,
    /// and the cursor moved ahead with a gutter of another width than it
    /// has now is a column out.
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

    /// Gone to the back (see [`crate::card::Command::Away`]): what moves
    /// stops where it is going, and nothing starts to until the client is
    /// back in front. What is left to draw, or changes from here on, is drawn
    /// as [`AWAY_FRAME_GAP`] allows.
    fn go_away(&mut self, now: Instant) {
        self.away = true;
        self.anim.reset();
        self.forget_dropped();
        self.anim.hold_blink(true, now);
        self.dirty = true;
    }

    /// Back in front (see [`crate::card::Command::Clear`]). From away, a
    /// frame at once — whatever changed since the last, up to a second's
    /// worth, and the cursor put where it is even where nothing did — which
    /// nvmux waits for, to bring the session back in as it is now and not as
    /// it was last drawn (see `crate::pty::relay`).
    fn come_back(&mut self, now: Instant) {
        if std::mem::take(&mut self.away) {
            self.screen.forget_cursor();
            self.dirty = true;
        }
        self.anim.hold_blink(false, now);
    }

    /// Draw with `drawing` from here on, as nvmux has changed its settings
    /// (see [`crate::card::Command::Settings`]).
    ///
    /// The effects change at once: what is moving moves on to its end, and
    /// nothing turned off starts again. The rest are things this attach has
    /// told Neovim, and are told it again: whether the windows have grids of
    /// their own, and what the agent left in the editor predicts, by
    /// attaching again — as the client does when the UIs attached change —
    /// and how wide each window's text is to be, by measuring again.
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
            // Windows of their own wanted now: whether they can be had is
            // who else is attached, which the answer decides as it does at
            // the start.
            Attach::Attached { multigrid: false } if flipped && wanted => {
                self.ask_uis();
                false
            }
            // Not wanted any more; or the agent to be told it predicts, or
            // that it does not — or that it is to say what Neovim made of
            // each key, or not, and what bears on typing ahead of it.
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
        // An attach to come leaves the agent and the measure as they now
        // are; this one is told here.
        if again {
            return;
        }
        self.leave_agent();
        if centred != (self.centre, self.window_width) {
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
        // Nearly every frame: read from its bytes (see `redraw::decode_bytes`).
        if let Some(params) = redraw::redraw_params(frame) {
            // Between a detach and its answer, what is drawn is the old
            // attach's, and the next one starts from nothing.
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
                // A `redraw` never gets here: it was read from its bytes above.
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
                    Some(Ask::Fence { sent, fenced }) => {
                        // What the agent said ahead of a frame that never
                        // came — a key that changed nothing on the screen —
                        // before what the fence says after it.
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

    /// Take a batch into the model, and set off what it starts — if
    /// `animate`, or simply put everything where it now is. `switched` are
    /// the window grids the batch draws another buffer in.
    fn apply(&mut self, mut batch: Vec<Event>, animate: bool, switched: &[u64], now: Instant) {
        let multigrid = self.multigrid();
        let before = self.anim.before(&self.model, &batch, multigrid, switched);
        // What Neovim's resizes leave of the grids the client cut.
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
    /// after the last of it came, and no more than [`RELAYED_ESCAPE`] — or
    /// [`REPLY_WAIT`] for a terminal's answer already part way through (see
    /// [`input`]).
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
                    let mode = Mode::of(&self.model.mode_name);
                    if let Some(p) = self.predict.as_mut() {
                        // Neovim takes a paste as keys typed on its command
                        // line, and as none in Normal, Visual and Insert
                        // mode: where the client cannot be sure it is in
                        // one of those, it may be keys.
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

    /// Keys typed, as the predictions follow them: a scroll among them is
    /// made at once where it can be. See [`predict::Typed`].
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

    /// Ask after a fence, if the predictions want one: see
    /// [`predict::Typed`]. Queued after every key there is to queue, so
    /// Neovim answers it once it has taken them all — with, where the agent
    /// counts them, how many it has (see [`predict::Sync`]): -1 before there
    /// is an agent to count them.
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
        // Numbered as the agent counts it, one key, before a scroll it makes
        // ahead of Neovim is: see `predict::Sync`.
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

    /// What a turn of the wheel over grid `grid`, `way`, with modifiers
    /// `mods`, makes (see [`App::predict_wheel`]), and by what — in Insert
    /// mode where `insert` says: none where the scrolls are not predicted,
    /// or something maps it, or the client cannot say where Neovim is.
    fn wheel_command(
        &self,
        grid: u64,
        way: &str,
        mods: &str,
        insert: bool,
    ) -> Option<(Command, By)> {
        let p = self.predict.as_ref()?;
        // A turn of the wheel is a scroll, and left to Neovim with the rest
        // where the scrolls are not predicted. And where the agent says what
        // Neovim made of each key, left to it as a key that scrolls is where
        // the counts are not tied, or the client is blind from an input
        // before it — a key it could not make: a turn drawn from where
        // Neovim has the view and the cursor would leave out what that
        // input did. Like those keys, it is made with Neovim's word on the
        // input (see [`App::replay`]).
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
        }
    }

    /// A scroll, or a motion of the cursor, Neovim will make in window grid
    /// `grid` of input `cause`: made at once, where the client can say what
    /// Neovim will make of it. One that is made — or that has nothing to
    /// make — leaves the screen as Neovim will have it, and is not to be
    /// made again (see [`predict::Sent::made`]); one that is not is one the
    /// predictions after it wait for Neovim to have drawn. See [`predict`].
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

    /// [`App::predict_command`], and nothing more: says whether it was made.
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

    /// Whether the cursor may be drawn ahead of Neovim in window grid `grid`
    /// `now`, as far as what the client has drawn goes: attached with
    /// windows of their own, drawn, not waiting for what an attach lacks,
    /// and the grid a window shown that has the cursor.
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

    /// A motion of the cursor in Normal mode Neovim will make in window grid
    /// `grid`, `motion` counted `count` — made by a key mapped to go a row
    /// of the screen at a time if `screen` (see [`predict::Idiom`]) — of key
    /// `cause`: drawn at once, where the client can say exactly where Neovim
    /// will put the cursor and draw it (see [`anim::chain`]) — along its
    /// line, or to another in the view, or past `'scrolloff'`, the view
    /// scrolled ahead of Neovim with it as far as Neovim scrolls it (see
    /// [`moves::keep_in_view`], [`App::scroll_ahead`]) — and what else of the
    /// window the cursor's line decides (see [`App::repaint`]). Says whether
    /// it was.
    ///
    /// Only from what Neovim last said of the keys and those followed since,
    /// in a window as [`App::onward`] has it; over lines it has as the
    /// buffer has them now, steps over as Neovim does, and finds Neovim
    /// drawing (see [`App::explained`]) and, the line Neovim's word was said
    /// on, measuring (see [`predict::Ack::width`]) as it would — or, for a
    /// line out of Neovim's view, one Neovim draws with nothing the client
    /// does not draw: no virtual text nor sign, as the agent says (see
    /// [`Predictor::decorated`]), nor text concealed (see
    /// [`predict::Stepping::concealing`]); to another line only where what
    /// the cursor's line decides can be drawn as Neovim draws it (see
    /// [`App::may_leave`]); and not in a buffer proving itself, nor at all
    /// once the client has drawn too many wrong (see [`predict::Trust`]) —
    /// where it is worked out, judged and not drawn, and no scroll ahead
    /// made of it.
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
        // The line the steps typed into, as they typed it: the client's lines
        // may have the edits for Neovim's word on one of their keys ahead of
        // the word, and Neovim's rows are not yet what they will be.
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
        // A row of the screen at a time, on lines a row each, is a line at a
        // time — but in a window that wraps, Neovim's `nv_screengo` lands
        // the cursor otherwise where the column it wants is past the row's
        // end, which it brings back into the row; or in the row's right
        // half, on a tab over it short of the tab's end, where the cursor is
        // drawn — on the character before the tab, rather. The view it
        // scrolls is the one `j` and `k` scroll.
        let text_width = width.saturating_sub(shape.textoff);
        if let (true, true, Want::Col(c)) = (screen, shape.wrap, from.want) {
            let sits = moves::drawn(line, shape, to.byte);
            if c >= text_width || (to.byte > 0 && sits > c && c > text_width / 2) {
                return false;
            }
        }
        // Laid out as Neovim lays them out, the lines moved from and to and,
        // where a move goes on past a line's end, every one it passes (see
        // [`App::laid_out`]); and where the view moves, every line it brings
        // into view (see [`App::brought`]), which Neovim has yet to draw.
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
        // A motion that leaves the cursor on the character it was on leaves
        // it drawn where it was: Neovim works out where to draw it only once
        // it moves, and on a tab may draw it as it was drawn in another mode
        // — at the tab's start, as Visual mode leaves it — for as long as it
        // does not.
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

    /// Where the next step of the cursor moved ahead of Neovim in window
    /// grid `grid` goes on from `now` (see [`anim::chain`]), where it may
    /// take one at all: where the cursor may be drawn ahead of Neovim (see
    /// [`App::steps_in`]), from what Neovim last said of the keys and those
    /// followed since ([`Predictor::known`]); in a window whose rows and
    /// lines are one to one, its options all such as the client follows
    /// ([`predict::Stepping::plain`]), nothing ahead of Neovim in it but
    /// what the steps made — a view scrolled ahead otherwise, up or down or
    /// sideways, lands the cursor where its scroll says — and not left to
    /// Neovim a while (see [`predict::Trust`]); from the newest step, in
    /// Normal mode, worked out with this window, gutter and view — one worked
    /// out with others is let go of as the client hears of them, and nothing
    /// goes on from it meanwhile — or, with none, from Neovim's word, in the
    /// view the screen shows: one Neovim has moved since of its own accord
    /// is not the view a step from that word would be drawn in, nor the one
    /// the next will say. And over the lines as the word has them — or as the
    /// steps typed them.
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

    /// Whether Neovim lays line `l` out, whose text the client has as `text`
    /// has it, as the client lays it out, for a step worked out `on` window
    /// grid `grid` (see [`App::onward`]) to go onto: as the steps typed it;
    /// in Neovim's view, by its own drawing of it and, the line its word was
    /// said on and those in view that end in blanks, by its own measure —
    /// what the screen shows may not show what is between the characters,
    /// if blank, and Neovim's measure counts it (see [`App::explained`]);
    /// out of it, as [`App::brought`] has it.
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

    /// Whether line `l` — whose text the client has as `text` has it — a
    /// view scrolled ahead of Neovim shows, worked out `on` the window as
    /// [`App::onward`] has it, is one the client lays out as Neovim does:
    /// one in Neovim's view, Neovim's to draw; out of it, one the client has
    /// whole, which Neovim draws with nothing the client does not draw — no
    /// virtual text, among its characters or about them, nor sign, as the
    /// agent says (see [`Predictor::decorated`]), nor text concealed (see
    /// [`predict::Stepping::concealing`]).
    fn brought<'a>(&self, on: &Onward, l: i64, text: &dyn Fn(i64) -> Option<&'a str>) -> bool {
        let Some((p, _, _, stepping)) = self.stepped_by(on.win) else {
            return false;
        };
        on.neovims(l) || (text(l).is_some() && !p.decorated(on.win, l) && !stepping.concealing)
    }

    /// What the steps in window `win` are worked out with: the predictions,
    /// Neovim's last word on the keys, the window's shape and what decides
    /// a motion in it.
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

    /// The display column the cursor is drawn in, in window grid `grid`,
    /// where a step goes on from Neovim's word and leaves the cursor on the
    /// character it was on: where Neovim draws it, where its frame has the
    /// cursor where its word does.
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

    /// Whether the cursor may be drawn ahead of Neovim on line `to` of window
    /// grid `grid` rather than on `from`, as far as what the cursor's line
    /// decides of the window goes: the cursor's line in a gutter drawn as
    /// Neovim's rules have it, and a bar across the line moved off and onto
    /// that the client can lay and take off as Neovim does — across the
    /// line, not `'screenline'`'s across the cursor's row of it, which is
    /// not followed. Not where what the cursor's line decides is drawn in
    /// groups of the window's own, which the client has no word of; nor
    /// where the line left may be drawn concealed once it is, which the
    /// client cannot draw. What is drawn over the rows (see
    /// [`App::repaint`]) goes with them as they slide by.
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

    /// Whether what the cursor's line decides of window grid `grid` can be
    /// drawn over its rows as Neovim draws it with the cursor on line `to`
    /// rather than on Neovim's `from` (see [`App::repaint`]), where it shows
    /// lines Neovim's view does not as well: as [`App::may_leave`] has it,
    /// and where the cursor's line decides any of it, rows of the lines out
    /// of Neovim's view the client can draw afresh (see [`App::afresh`]). A
    /// window whose options the agent has said nothing of past the shape it
    /// says of every window — what the cursor's line decides among them —
    /// is one the client draws nothing over, as one it works out no motions
    /// in.
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

    /// Move the cursor in window grid `grid` ahead of Neovim as `step`
    /// says, worked out `on` the window as it was (see [`App::onward`]),
    /// for key `cause`: the line it goes onto as `drawn` (see
    /// [`chain::Seen`]); and where it moves the view, the view scrolled ahead
    /// of Neovim with it (see [`App::scroll_ahead`]) — not for a step only
    /// worked out to be judged, which draws nothing. Says whether it was.
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
            // Where the cursor lands, where it goes to another line: as a
            // scroll alone says it (see [`anim::Ahead::col`]).
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

    /// Scroll window grid `grid` ahead of Neovim from the view at `from` to
    /// the one at `to`, for key `cause`, as a step of the cursor moved
    /// ahead of Neovim says (see [`App::step`]): the rows it brings into view
    /// drawn from the agent's lines (see [`anim::Animator::predict`]), at
    /// once, the cursor drawn by the step — and landed, as far as the
    /// scroll can say, in grid column and wanting display column `col`.
    /// One that cannot be drawn at once, a line it brings in not to be had,
    /// is let go of, and every scroll ahead in the window and every step
    /// with it, to be made again from Neovim's next word. Says whether it
    /// was made.
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
        // What a scroll before it could not have, first.
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

    /// What a step onto buffer line `line` of window grid `grid` notes of
    /// it (see [`chain::Seen`]): the row Neovim draws it on, past the
    /// gutter; or, out of Neovim's view, a line a scroll ahead of it brings
    /// in, its text `text` as far as it goes, as the client lays it out —
    /// `None` where the client cannot.
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

    /// A key typed in Insert mode that edits the cursor's line, `e`, of key
    /// `cause`, Neovim will make in window grid `grid`: drawn at once —
    /// Neovim's row for the line laid out afresh around the text it leaves,
    /// and the cursor where it leaves it (see [`typing`]) — where the client
    /// can say what it does, and how Neovim draws it. Says whether it was.
    ///
    /// Only where the client types ahead of Neovim at all
    /// (`[client] predict_typing`), from Neovim's last word on the keys and
    /// those followed since ([`Predictor::known`]), Neovim resting in Insert
    /// mode itself — not completing, nor in Replace mode; in a window whose
    /// rows and lines are one to one, not sliding by (see
    /// [`anim::Animator::gliding`]), nothing else ahead of Neovim in it, and
    /// its options all such as the client follows
    /// ([`predict::Stepping::plain`]); where nothing the agent says of the
    /// buffer's Insert mode bears on the key ([`typing::refused`]); over a
    /// line the client has whole as the buffer has it — as Neovim's word
    /// has it, or as the steps before typed it — that it steps over as
    /// Neovim does, and that Neovim draws and measures as the client would
    /// lay it out (see [`App::explained`]), with no virtual text that stays
    /// where it is as the line's end moves (see [`Predictor::pinned`]), on
    /// a row that is the line's alone: not one whose line, centred, runs on
    /// past the grid's edge (see [`wide`]). And only where what it leaves is
    /// drawn on that row as the line was (see [`typing::relayout`]). In a
    /// buffer proving what is typed there, it is worked out, judged and not
    /// drawn (see [`predict::Trust`]).
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
        // Nor over rows sliding by (see [`anim::Animator::gliding`]), which
        // the line typed into, drawn over its row, would not slide with.
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
        // The line as the steps before typed it; or as Neovim's word has it,
        // where the client has the buffer as it was then — not where the
        // edits for a word yet to be taken have come ahead of it — whole
        // and stepped over as Neovim does.
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
        // Where Insert mode began, which `<C-w>` and `<C-u>` stop at once,
        // as the step before has it, or Neovim's word: not knowing it, they
        // are left to Neovim.
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
        // The row as the steps before laid it out; or as Neovim draws it,
        // where it draws and measures the line as the client lays it out.
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
        // A glyph of `'list'`'s new to the row in the colours Neovim draws
        // one in on it: across the bar of `'cursorline'`, its group laid
        // over the bar, as the client lays the bar under a cell — and none
        // where it cannot (see [`model::Model::bar_derivable`]).
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

    /// `i`, `a`, `A` or `I` entering Insert mode as `enter` says — or, where
    /// `None`, `<Esc>` leaving it — of key `cause`, Neovim will make in
    /// window grid `grid`: drawn at once, the cursor where it goes and in
    /// the shape of the mode it goes into (see [`anim::Animator`]), where
    /// the client can say where Neovim puts it (see [`typing::entered`] and
    /// [`typing::left`]). Says whether it was. What Neovim draws of the
    /// mode besides — `-- INSERT --` — is left as Neovim drew it.
    ///
    /// Only where the client types ahead of Neovim, from Neovim's last word
    /// on the keys and those followed since ([`Predictor::known`]), in
    /// Normal mode to enter Insert mode and in Insert mode itself to leave
    /// it — not completing — in a window whose rows and lines are one to
    /// one, nothing else ahead of Neovim in it, and its options all such
    /// as the client follows ([`predict::Stepping::plain`]), and that does
    /// not conceal text on the cursor's line in either mode; over a line the
    /// client has whole as the buffer has it, that Neovim draws and measures
    /// as the client lays it out (see [`App::explained`]) or the steps typed
    /// it — and then not over rows sliding by (see [`App::predict_echo`]);
    /// in a buffer the agent has said text can go in, to enter (see
    /// [`typing::Facts::editable`]) — at the line's end, not past virtual
    /// text that may stay before the cursor there (see
    /// [`Predictor::pinned`]) — and where `<Esc>` does no more than put the
    /// cursor back a character, to leave (see [`typing::unleft`]); and where
    /// the cursor stays on its line's row, and the view where it is.
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
        // The line as the steps typed it; or as Neovim's word has it, where
        // the client has the buffer as it was then, and Neovim draws and
        // measures it as the client lays it out.
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
        // Not over rows sliding by, where the line typed into is drawn over
        // its row (see [`App::predict_echo`]).
        let Some(text) = text.filter(|_| echo.is_none() || !self.anim.gliding(grid)) else {
            return false;
        };
        let (byte, vcol, proof) = match enter {
            Some(e) => {
                if !facts.editable {
                    return false;
                }
                let (byte, vcol) = typing::entered(e, text, shape, from.byte);
                // At the line's end, not where the cursor may go after
                // virtual text there (see [`Predictor::pinned`]).
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
        // Where a plugin's autocommand of the mode entered or left may move
        // the cursor or edit the buffer — a trimmer of the blanks lines end
        // in, whose `:%s` leaves the cursor on the last line it trimmed —
        // worked out and judged, and not drawn, till enough have been judged
        // right; as are the keys after it, which go on from it. Leaving it
        // is taken for that, too, where what the agent says of the buffer
        // was said before Insert mode was first entered in it, and a plugin
        // loaded on the way in had yet to make its own.
        let what = match proof {
            Proof::Enter => Unseen::Entered,
            _ => Unseen::Left,
        };
        let unseen = self.unseen(ack.buf, proof) && !p.trust.proven(ack.buf, what);
        // On the line's row: not past the end of a row its line fills, in a
        // window that wraps; and in one that does not, where the view shows
        // it as it is.
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

    /// Draw over each window the cursor has been moved ahead of Neovim in
    /// (see [`anim::chain`]) — and, where the client works out the motions,
    /// each scrolled ahead of Neovim or sliding, the cursor drawn where the
    /// scrolls take it — what Neovim will draw of its rows besides the
    /// cursor, once it hears of the keys: the line typed into, as typed;
    /// where the cursor is drawn on another line than Neovim's, the gutter
    /// as Neovim draws it with the cursor there (see [`predict::regutter`]),
    /// and the bar of `'cursorline'` moved with it, each row worked out
    /// again from what Neovim's last frame drew — and a row the client
    /// cannot say of, as that frame drew it. Rows a scroll ahead of Neovim,
    /// or a slide, shows of lines Neovim's view does not, drawn afresh with
    /// the cursor where it is drawn. Before every frame: Neovim's frames,
    /// the steps, the view and its slide all move what is to be drawn.
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

    /// What [`App::repaint`] draws over window grid `grid`'s rows, by row of
    /// its grid: `None` for nothing.
    ///
    /// Each row shows a line: the one its grid has, or another, the rows
    /// scrolled ahead of Neovim or sliding (see [`anim::Animator::rows_shown`]);
    /// and the cursor is drawn on one, as the steps or the scrolls ahead
    /// have it (see [`anim::Animator::cursor_line`]).
    /// Of a row of a line Neovim drew — its grid's, or one its view has left
    /// that the window's scroll keeps (see [`anim::scroll::Kept`]) — Neovim's
    /// drawing is drawn over: the row of the line typed into ahead of
    /// Neovim, as the steps laid it out (see [`App::predict_echo`]); and
    /// where the cursor is drawn on another line than Neovim's was as it drew
    /// the row, the gutter, where the cursor's line decides any of it; and
    /// where `'cursorline'` draws a bar across the cursor's line, the text of
    /// the rows of the line the cursor was on and of the one it is drawn on
    /// — as Neovim drew them, or as typed into — the bar taken off the one
    /// and laid under the other (see [`model::Derive`]): a row whose
    /// highlights can no longer be made so left as Neovim drew it. A row of a
    /// line the client brought into view ahead of Neovim, in a window where
    /// the cursor's line decides any of it, is drawn afresh, with its gutter
    /// and bar as Neovim draws them with the cursor where it is drawn (see
    /// [`drawn_afresh`]). What the cursor's line decides is drawn over no row
    /// where the client cannot draw it as Neovim does (see
    /// [`App::redrawn_with`]): it is left as Neovim drew it, for its next
    /// frame to put right.
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
        // What the rows drawn afresh are drawn with, once for them all: where
        // there can be any, rows shown ahead of Neovim's view, in a window
        // where the cursor's line decides any of them.
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
            // Neovim's drawing of the line, from the window's first column,
            // and the line its cursor was on as it drew it; or the client's.
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

    /// Whether the client can draw a row of window grid `grid` afresh with
    /// what the cursor's line decides of it as Neovim draws it (see
    /// [`drawn_afresh`]): a gutter of Neovim's own, in the editor's
    /// groups — not the window's own, nor with signs drawn over numbers —
    /// all of them named, and a bar of `'cursorline'`, if any, across the
    /// line, which the client can lay under it as Neovim does.
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

    /// Whether Neovim's gutter in window grid `grid`, as its frame has it,
    /// can be drawn as Neovim will draw it with the cursor on line `to` (see
    /// [`predict::regutter`]): a gutter of Neovim's own — not the
    /// `'statuscolumn'`'s, which may draw anything the cursor's line
    /// decides — and, where the cursor's line decides any of it, laid out
    /// row for row as the client lays it out, with no sign drawn over a
    /// number.
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

    /// Whether every row of window grid `grid`'s gutter that shows a line,
    /// as Neovim's frame has it, can be drawn as Neovim draws it with the
    /// cursor on line `to` instead (see [`predict::regutter`]). `None` for a
    /// window whose gutter the client has no shape or groups for, or that
    /// the `'statuscolumn'` draws.
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

    /// Whether Neovim draws line `line` of window grid `grid`, whose text the
    /// client has as `text`, as the client would: the cells of its text, from
    /// the gutter to the line's end or the window's, being those the client
    /// lays it out in. Where they are not, the client's text is not Neovim's
    /// — an edit it has yet to hear of — or Neovim draws something between
    /// the characters the client does not know of: virtual text, text
    /// concealed, a character taken for another width. `None` for a line
    /// not in view, or one too long for a window that wraps. The text alone
    /// is compared, so the colours it is laid out in are none in particular.
    ///
    /// Blank virtual text among blanks shows nothing of itself: where
    /// Neovim's own measure of the line is not known to agree with the
    /// client's (see [`predict::Ack::blanks`]) — not `measured` — a line
    /// whose last cell the client draws blank is not taken as drawn as the
    /// client would. One with nothing to draw is.
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
        // Neovim takes it as 'scroll' from now on, whatever is predicted.
        if let (Command::Half { count: Some(n), .. }, Some(p)) = (command, self.predict.as_mut()) {
            p.set_scroll(win, n.min(h), cause.seq);
        }
        if self.predict.is_none() || !self.multigrid() || !self.flushed || self.holding(now) {
            return false;
        }
        // A page back from a view whose first line is the buffer's last, or
        // the one before, keeps fewer of the lines the view showed than
        // anywhere else — Neovim's `get_scroll_overlap` runs out of lines to
        // reckon them over — which [`predict::outcome`] does not follow:
        // where Neovim's word on each key judges what the client drew of
        // it, it is left to Neovim. Under "scrolls" it is drawn as it was.
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
        // A key that scrolls in Normal mode, where the client works out the
        // motions, or a turn of the wheel over steps of the cursor's: one of
        // them, as far as the cursor goes.
        if let Some(made) = self.scroll_step(grid, command, cause, now) {
            return made;
        }
        // The cursor moved ahead of Neovim in the window: a scroll alone is
        // worked out from where Neovim has it, which it is not, and waits for
        // Neovim's word on the steps before it.
        if self.anim.chain(grid).is_some() {
            return false;
        }
        let Some(p) = self.predict.as_ref() else {
            return false;
        };
        // The modes a key or the wheel scrolls a window in as predicted:
        // keys and pages in normal and visual mode, lines in insert mode as
        // well.
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
        // Where the cursor lands out of view, or too near an edge, Neovim
        // moves the view sideways to show it. That is drawn too, for a scroll
        // alone ahead of Neovim whose lines are all known: what it shows,
        // moved (see `side_rows`). Any other is let go, for Neovim to draw.
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
            // Under rows moved sideways, nothing slides.
            self.anim.settle(grid);
            self.anim
                .side(&self.model, grid, rows, (left, until), Some(cursor), now);
        }
        true
    }

    /// A key that scrolls window grid `grid` in Normal mode, `command`, of
    /// key `cause`, where the client works out where the motions take the
    /// cursor: made as they are, a step of the cursor's (see
    /// [`anim::chain`]) — from where the steps before it leave the cursor
    /// and the view, or Neovim's word with none — with the view scrolled
    /// ahead of Neovim as far as Neovim scrolls it (see [`predict::outcome`],
    /// [`App::scroll_ahead`]): its landing worked out at once, as a motion
    /// down or up lands (see [`moves::onto`]), and judged by Neovim's word
    /// on the key as the motions are. So the motions after it go on from
    /// it, and it from them. Says whether it was made: `None` where it is
    /// made as a scroll alone, as it was before the client worked out
    /// motions (see [`App::predict_scroll`]) — one it cannot make as a step,
    /// or that would not be drawn, or makes nothing, where nothing but
    /// scrolls alone are ahead of Neovim in the window (see
    /// [`App::alone`]).
    ///
    /// So is a turn of the wheel over a window the cursor was moved ahead
    /// in, which Neovim makes in Normal mode as it makes `<C-e>`, `<C-y>` or
    /// a page: a scroll alone from Neovim's view and cursor would take the
    /// view back over the steps' scrolls, and count the lines the cursor
    /// goes again from where Neovim has it, as its frames for the steps
    /// come. Where nothing ahead of Neovim in it but the cursor, a turn is a
    /// scroll alone, as it always was.
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

    /// The step a key that scrolls, `command`, of key `cause`, makes in
    /// window grid `grid`, as [`App::scroll_step`] makes it — where the
    /// client can make one: where a motion can go on (see [`App::onward`]);
    /// onto a line the client lays out as Neovim does (see
    /// [`App::laid_out`]), past whatever the cursor's line decides (see
    /// [`App::may_leave`]); in a view whose lines out of Neovim's the client
    /// has, lays out as Neovim does (see [`App::brought`]), and a line to
    /// a row. With no step before it, not one only worked out to be judged,
    /// nor one that makes nothing: those are for a scroll alone.
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

    /// A key that scrolls window grid `grid`, which cannot be made as a
    /// step of the cursor's (see [`App::scroll_step`]): made as a scroll
    /// alone (`None`), as it was before the client worked out motions,
    /// where nothing but scrolls are ahead of Neovim in the window — none
    /// at all, or steps every one of which scrolled (see
    /// [`anim::Animator::scrolls_alone`]), let go of as steps and kept as
    /// the scrolls they made, which say as much of the cursor as a scroll
    /// alone does; and left to Neovim where a motion is (`Some(false)`),
    /// which a scroll alone would not go on from.
    fn alone(&mut self, grid: u64, now: Instant) -> Option<bool> {
        self.anim.chain(grid)?;
        if !self.anim.scrolls_alone(grid) {
            return Some(false);
        }
        self.anim.unchain(&self.model, grid, now);
        None
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

    /// The display column the cursor wants in window `win`: Neovim's
    /// `curswant`, as its agent last said (see [`Predictor::want`]) — or,
    /// where it has not, the one the cursor is in.
    fn wants(&self, win: i64, shape: &predict::Shape) -> usize {
        self.predict
            .as_ref()
            .and_then(|p| p.want(win))
            .unwrap_or_else(|| {
                (self.model.cursor.col + shape.leftcol).saturating_sub(shape.textoff)
            })
    }

    /// Draw what windows scrolled ahead of Neovim could not show before, now
    /// that more of their lines have come or Neovim has drawn again; or, in
    /// a window no longer one to predict a scroll in, give them up, and
    /// predict nothing more till Neovim has caught up.
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

    /// Stop scrolling window grid `grid` ahead of Neovim: it shows Neovim's
    /// view again, and the inputs the scrolls were made of are to be made
    /// again from the next ack (see [`App::replay`]).
    fn give_up(&mut self, grid: u64) {
        let seqs = self.anim.give_up(grid);
        if let Some(p) = self.predict.as_mut() {
            p.given_up(&seqs);
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
        let wants = self.wants(win, shape);
        // One held back that lands the cursor where Neovim moves the view
        // sideways is let go, and those after it: the keys after them wait
        // for Neovim, as after any other it did not predict.
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

    /// A scroll sideways Neovim will make of window grid `grid`, typed or
    /// turned `by`: made at once, where the client can say what Neovim will
    /// make of it. See [`predict_command`](Self::predict_command).
    fn predict_side(&mut self, grid: u64, side: Side, cause: Cause, now: Instant) -> bool {
        if self.predict.is_none() || !self.multigrid() || !self.flushed || self.holding(now) {
            return false;
        }
        // As for a scroll up or down (see `App::predict_scroll`): the cursor
        // moved ahead of Neovim is not where one from Neovim's word finds it,
        // and no scroll sideways is a step of it.
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
        let until = p.deadline(cause.at);
        self.anim
            .side(&self.model, grid, rows, (to.left, until), cursor, now);
        self.dirty = true;
        true
    }

    /// The rows of window grid `grid` as it shows them — as Neovim drew
    /// them, or scrolled up or down ahead of Neovim — moved to show its text
    /// from display column `to`. See [`predict::shifted`].
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

    /// What the editor has said runs past the edges of the windows' grids,
    /// into the model: with the batch it was said ahead of, or once it has
    /// waited [`TAILS_WAIT`] for one. Said of a window gone, it is dropped.
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

    /// What the agent says Neovim made of the keys (see [`predict::Ack`]),
    /// where the counts can be trusted: said just ahead of the frame that
    /// draws it, and taken with that frame — Neovim's frame is the last word
    /// on the screen, and under a loss on the link it comes as much as a
    /// lost segment's time after — or at once, where there is nothing left
    /// for a frame to draw: not where the keys changed the buffer, which
    /// the frame draws, whatever they did to the cursor; nor where the
    /// client could not make out what one of the keys does (see
    /// [`predict::Sent::lost`]), which may have changed what a window shows
    /// and left its cursor and view as they were; nor where a scroll it
    /// covers was made ahead of Neovim in a window other than the one with
    /// the cursor, whose view it does not say. The newest wins.
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
            // Past a key Neovim may count as two: the steps for the keys
            // before it are let go of with the frame that shows them taken,
            // and at once where Neovim has drawn it already, as an ack is.
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

    /// Whether what Neovim has drawn shows what `ack` says already: the
    /// cursor in its window, on its line and column, the view from its line
    /// and column, in its mode. A key that leaves things as they were — `l`
    /// at the end of a line — has an ack and no frame.
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

    /// Take what the agent said Neovim made of the keys, once the frame it
    /// was said ahead of has been, for Neovim's last word on them (see
    /// [`Predictor::judge`]). A scroll made ahead of Neovim of a key it
    /// covers that its frame did not make is one Neovim did not, or made
    /// otherwise: the view goes back to Neovim's at once, rather than at the
    /// scroll's deadline, which on a lossy link may be a lost segment's
    /// time after the frame. And what the keys sent since make is made out
    /// again from there (see [`App::replay`]).
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
        // Neovim's frame for the keys, or one it drew after more of them,
        // having gone on to those before it could flush: only from its own
        // can a scroll be made of the keys after them.
        let fresh = self.shows(&ack);
        self.judge_steps(k, &ack, fresh, now);
        // Steps that went on from a scroll given up: worked out over a view
        // the screen no longer shows.
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

    /// Judge where the cursor was moved ahead of Neovim (see
    /// [`anim::chain`]), and what was typed ahead of it, by `ack`, which
    /// covers the client's keys up to `k`, Neovim's frame for them on the
    /// screen if `fresh`.
    ///
    /// - The step for `k` is where Neovim says the key left the cursor and
    ///   the view, over the same buffer as it was — or, where the steps typed
    ///   into a line, with the line's text as Neovim's word has it, which the
    ///   agent sends the edits for ahead of it (see
    ///   [`Predictor::fresh`]); or, for a key that went into Insert mode or
    ///   out of it, over what an autocommand of the mode left of the buffer,
    ///   the steps after it worked out again over that where it changed —
    ///   or, where a plugin's may do anything and Neovim's word does not
    ///   say, may have (see [`App::edited`]): *made*. One that changed it
    ///   proves itself in the buffer before it is drawn there again, as one
    ///   wrong would (see [`predict::Trust::edited`]). It is let go of, and
    ///   those before it, the cursor already where Neovim's frame has it;
    ///   and that frame, its own, is checked to have drawn it in the cell
    ///   the client did, and the line typed into as the client laid it out
    ///   — where not, the client lays the line out otherwise than Neovim,
    ///   and the window is left to Neovim a while. A line Neovim draws
    ///   otherwise past its end only — virtual text there changed — has the
    ///   steps after let go of, to be laid out again over it (see
    ///   [`App::replay`]).
    /// - The step for `k` is not: *wrong*. The cursor goes to where Neovim
    ///   has it at once, every step in the window let go of, and what it
    ///   was of proves itself in the buffer before it is drawn there again
    ///   (see [`predict::Trust`]).
    /// - There is no step for `k` — a key the client did not predict, or the
    ///   agent saying something moved the cursor without a key — or the
    ///   buffer changed under them, or the client has not the text the line
    ///   typed into has: nothing to judge, and the steps are let go of.
    ///
    /// Either of the last two has the keys after `k` made out again from the
    /// ack (see [`App::replay`]).
    fn judge_steps(&mut self, k: u64, ack: &predict::Ack, fresh: bool, now: Instant) {
        for grid in self.anim.chained() {
            let Some(chain) = self.anim.chain(grid) else {
                continue;
            };
            let (here, textoff) = (chain.win == ack.pos.win, chain.textoff);
            // Over the buffer as it was worked out over — or, where the steps
            // typed into it, or the key went into Insert mode or out of it,
            // over what the key leaves of it: an autocommand of `InsertLeave`
            // that takes out the blanks a line ends in, a plugin's, has the
            // key do more than it does, and is for the judge to find.
            let step = chain.step_for(k).cloned().filter(|s| {
                let mode = matches!(s.proof, Proof::Enter | Proof::Leave);
                here && (s.echo.is_some() || mode || s.tick == ack.tick)
            });
            let Some(p) = self.predict.as_ref() else {
                return;
            };
            // The line typed into, as Neovim's word has it: `None` where the
            // client has not that text.
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
                    // The buffer changed under the steps after it — by an
                    // autocommand of the mode it went into or out of — or,
                    // where a plugin's may have and nothing says, may have:
                    // they are worked out again over it.
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
                    // The scrolls ahead with them: a hidden step made none,
                    // nor any after it.
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

    /// Whether key `k`, into Insert mode or out of it, found by Neovim's
    /// word on it, `ack`, made as step `s` worked it out, edited the buffer
    /// besides — which the key itself does not, but an autocommand of the
    /// mode may: by the `b:changedtick` the step was worked out over; or,
    /// after text typed ahead of Neovim, by its word on the key before, or
    /// on any before with the buffer as it is now. `None` where nothing
    /// says: Neovim's word on the text typed and the key at once.
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

    /// Whether, as the agent says of buffer `buf`, a plugin's autocommand
    /// may do anything as a key of kind `proof` enters Insert mode, or
    /// leaves it (see [`App::predict_mode`]).
    fn unseen(&self, buf: i64, proof: Proof) -> bool {
        let facts = self.predict.as_ref().and_then(|p| p.facts(buf));
        facts.is_some_and(|f| match proof {
            Proof::Enter => f.unseen_entered,
            _ => f.unseen_left || !f.from_insert,
        })
    }

    /// Whether Neovim's frame draws the line `e` typed into in window grid
    /// `grid` as the client laid it out: its text, and the row whole, past
    /// the line's end as well — `(false, false)` where it is not in view.
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

    /// Whether Neovim's frame has the cursor where step `s` in window grid
    /// `grid` drew it, its gutter `textoff` wide and the step drawn in
    /// display column `vcol`: `at`.
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

    /// Let go of the cursor moved ahead of Neovim in window grid `grid`,
    /// unjudged: it goes to where Neovim has it, and the view with it — the
    /// scrolls ahead of Neovim the steps made, which are all there are in a
    /// window with steps (see [`App::onward`]) — and the keys of the steps
    /// are to be made again from the next ack, the client blind from them
    /// on till then (see [`predict::Typed::blinded`]).
    fn drop_steps(&mut self, grid: u64, now: Instant) {
        if self.anim.chain(grid).is_some() {
            self.give_up(grid);
        }
        let seqs = self.anim.drop_chain(&self.model, grid, now);
        self.forget(&seqs);
        self.dirty = true;
    }

    /// The steps for the keys `seqs` were let go of unjudged: see
    /// [`App::drop_steps`].
    fn forget(&mut self, seqs: &[u64]) {
        if let (Some(p), false) = (self.predict.as_mut(), seqs.is_empty()) {
            p.given_up(seqs);
            p.trust.unjudged(seqs.len() as u64);
            p.typed.blinded();
        }
    }

    /// Let go of every step the animations let go of on their own (see
    /// [`anim::Animator::take_dropped`]), as [`App::drop_steps`] does: says
    /// whether there were any.
    fn forget_dropped(&mut self) -> bool {
        let seqs = self.anim.take_dropped();
        self.forget(&seqs);
        !seqs.is_empty()
    }

    /// Work the steps a frame of Neovim's let go of out again at once (see
    /// [`anim::chain`]), from Neovim's last word on the keys, over the
    /// window as that frame left it (see [`App::replay`]): where what it
    /// changed leaves the line laid out as before — the window a size
    /// other than it was, something drawn after the line's end — the cursor
    /// stays where it was drawn, rather than going back to Neovim's for a
    /// round trip; where not, they wait for Neovim's next word.
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

    /// Where the counts of the keys Neovim has taken are in doubt, no ack is
    /// taken for a key of the client's (see [`predict::Sync`]), and no step
    /// can be judged: every one is let go of. From a key Neovim may count as
    /// two on only, the steps for the keys before it are judged by what the
    /// agent says of them as before, and drawn till then — or till its frame
    /// once it counts past them (see [`App::floor`]): let go of now, they
    /// would take the screen back to Neovim's last frame, behind them, for a
    /// key that leaves them as they were. Not where a paste only holds
    /// the acks back till a fence after it — Neovim taking it in Normal
    /// mode as no key, the steps for the keys before it are where Neovim
    /// puts the cursor still, till its frame for the paste says otherwise.
    fn forget_unsure(&mut self, now: Instant) {
        let Some(from) = self
            .predict
            .as_ref()
            .filter(|p| p.acks())
            .and_then(|p| p.sync.doubted_from())
        else {
            return;
        };
        // A step for a key before the first the counts are in doubt from is
        // judged by what the agent says of it all the same: it stays.
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

    /// A fence answered, Neovim having taken the client's keys up to `upto`
    /// — or an ack or a fence that counted past a key Neovim may count as
    /// two, `upto` the key before it: a step for one of them still drawn is
    /// one Neovim took without going to wait for a key between — an
    /// operator pending, completion, the key it may count as two read with
    /// it — and said nothing of, its frame the last word. Every step is let
    /// go of, and the client is blind from the first key after Neovim's
    /// last word till it says another.
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

    /// Make out again the inputs sent since Neovim's last word on them,
    /// from where it says Neovim is (see [`predict::Typed::rebase`]), and
    /// make what the client can of those it has not made: a scroll typed
    /// after a key it was blind to, or one it gave up on, a motion whose
    /// step was let go of — once each, a scroll made already being ahead of
    /// Neovim still, or made by Neovim since. A key that cannot be made, or
    /// a click, leaves it blind from that input on, as it would typed now.
    /// So does a scroll not made, where the frame the word came with is not
    /// Neovim's for those keys but for more (`fresh`): Neovim, going on to
    /// keys it had to hand, did not flush between them, and that frame may
    /// have made it already. A motion is worked out from the ack, not the
    /// frame, and is made all the same.
    fn replay(&mut self, fresh: bool, now: Instant) {
        let Some(p) = self.predict.as_ref() else {
            return;
        };
        let queue: Vec<predict::Sent> = p.queue.iter().cloned().collect();
        let grid = self.model.cursor.grid;
        let win = self.model.wins.get(&grid).copied();
        for sent in queue {
            // A turn of the wheel, made out again over its own window, and
            // by the wheel.
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
            // Insert mode entered by a key whose step is not made, nor ahead
            // of Neovim still, is not known to be entered as the key alone
            // enters it (see [`predict::Typed::unentered`]).
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
                // Nothing to make: a count, or the first key of a `z`.
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

    /// Neovim has drawn: a window it moved sideways lands the scrolls ahead
    /// up to it, and one still ahead is drawn again from Neovim's rows.
    fn landed_sideways(&mut self, now: Instant) {
        for (win, left) in std::mem::take(&mut self.lefts) {
            if let Some(p) = self.predict.as_mut() {
                p.set_leftcol(win, left);
            }
            if let Some((&grid, _)) = self.model.wins.iter().find(|(_, w)| **w == win) {
                self.anim.landed(grid, left);
                // The cursor moved ahead in a view Neovim has moved since.
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
        self.forget_dropped();
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
    ///
    /// A frame is put together from only the rows that changed where it can
    /// be (see [`App::changed_rows`]): over
    /// the frame shown, the rows of the editor's grids drawn or scrolled
    /// since are put together again, and compared with the terminal's alone.
    pub fn frame(&mut self, now: Instant) -> Vec<u8> {
        // Steps Neovim has said nothing of in time: let go of, as a scroll
        // ahead is, and the client blind from them till it does.
        for (grid, seqs) in self.anim.lapsed(&self.model, now) {
            self.give_up(grid);
            self.forget(&seqs);
        }
        self.anim.advance(now);
        self.repaint();
        let (w, h) = self.size;
        self.cards.expire(now);
        // The frame shown, to put the rows that changed together over, where
        // they are all that can differ (see [`App::changed_rows`]).
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
                // Still: nothing is painted over it, and this is the cursor.
                let cursor = self.anim.paint(&mut frame, &self.model, now);
                (frame, cursor)
            }
            None => {
                // Put together in the buffer of a frame done with; and
                // mended once, last — and before what is painted over it
                // too, where that reads the cells under it: a card, or the
                // cursor's smear, sparks or blink.
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
        self.frame_before = self.last_frame.replace(now);
        out
    }

    /// Whether this frame is to be put together from only the rows that
    /// changed, which it marks in [`Partial::rows`]: where this frame and
    /// the last are the editor's
    /// grids and nothing more — nothing moving, held or painted over either,
    /// no card over either — and nothing has changed since the last but the
    /// cells of rows of grids on show. A row of the frame is changed where a
    /// row of a grid drawn there is. Notes this frame for the next either
    /// way.
    fn changed_rows(&mut self, height: usize) -> bool {
        let Some(partial) = self.partial.as_mut() else {
            return false;
        };
        let since = std::mem::replace(&mut partial.at, grid::tick());
        let still = self.anim.still() && self.cards.is_empty();
        let was_still = std::mem::replace(&mut partial.still, still);
        if !(still && was_still && self.model.unchanged_since(since)) {
            return false;
        }
        let rows = &mut partial.rows;
        rows.clear();
        rows.resize(height, false);
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
    // Read from the file only by a client nvmux did not start, which is told
    // nothing of where its session is either: a session on this machine, as
    // far as it knows.
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
        // What the keys ask of Neovim goes now, ahead of the frame they may
        // have predicted: composed first, that frame would hold them back by
        // as long as it took — a millisecond a key, on its way to a link
        // that is slow already, or no key would have been predicted.
        if let Err(e) = app.outbox.send(&mut stream) {
            break Err(anyhow::Error::from(e).context("writing to the session"));
        }
        if app.due(now) {
            // Nothing at all, for a frame that changes nothing on the screen
            // and had nothing waiting to go ahead of it: see
            // `Screen::draw_into`.
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
                // What else comes is passed over unread: a whole screen's
                // redraw, for a refresh.
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
    use crate::config::Predict;
    use rmpv::Value;

    type Call = (Option<u64>, String, Vec<Value>);

    /// The motions as well as the scrolls, as `predict_typing = "off"` has
    /// them; and the typing as well, as `[client]` has it by default.
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
        d.client.predict_scrolls = Predict::Off;
        d.client.predict_motions = Predict::Off;
        d.client.predict_typing = Predict::Off;
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

    /// Told `"auto"`, a client predicts for a session on another machine,
    /// and attaches again for the agent; for a session on this one it
    /// predicts nothing, and attaches as it was — unless told `"on"`, when
    /// it predicts for that one too.
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

    /// Whether the measure is to judge each window by what it shows: its
    /// second argument, in `calls`.
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

    /// Told to centre each window's text, a client measures the windows, by
    /// what each shows; told to give each the same width, it measures them
    /// again, by nothing; told not to centre any more, it takes the measure
    /// back out of the editor. None of it attaches again.
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

    /// A grid narrower than its window is a window's, so a client centring
    /// each window's text wants windows of their own, with no effect and
    /// nothing predicted wanting them: started so, it asks after the UIs
    /// before it attaches; told so, it attaches again with them, and told
    /// not to centre any more, without.
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

    /// A client writes each frame lean: the next of two rows changed one
    /// under the other reached by CR LF, not by a CUP.
    #[test]
    fn a_client_writes_lean_frames() {
        let t0 = Instant::now();
        let mut a = attached(false, t0);
        a.frame(t0);
        redraw(&mut a, vec![line(1, 1, "ab"), line(1, 2, "cd")], t0);
        let text = String::from_utf8_lossy(&a.frame(t0)).into_owned();
        assert!(text.contains("ab\r\ncd"), "{text:?}");
    }

    /// A client has the terminal move the rows Neovim scrolled, and writes
    /// only the line the scroll uncovered; one made to write them again, as
    /// a test holds the one up against the other, writes the rows.
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
        // Of the rows the scroll moved, how many are written again: each at
        // the number that tells it from the row that was there.
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

    /// A client puts the frame after a line drawn together from that line's
    /// row alone, once it has two frames to go on: the frame shown and one
    /// done with. A window placed has the next put together whole; after
    /// that, a frame with nothing changed puts no row together, and the frame
    /// after a line drawn in the window that line's row alone.
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

    /// A small random walk for the tests: xorshift, the same walk for the
    /// same seed.
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

    /// A client on a terminal 30 by 12, drawing with `effects`, attached with
    /// windows of their own; having the terminal move the rows a frame moved
    /// if `regions`, and putting frames together from only the rows that
    /// changed if `partial`.
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

    /// The effects a walk of `seed` is drawn with, by turns: windows moving
    /// alone; scrolls sliding too; the cursor smeared as well; and a cursor
    /// that blinks.
    fn walk_effects(seed: u64) -> Effects {
        let mut s = crate::config::Settings::default();
        let turn = seed % 4;
        s.effects.scroll.enabled = turn != 1;
        s.effects.smear.enabled = turn == 3;
        s.effects.blink.enabled = turn == 0;
        Effects::from_settings(&s)
    }

    /// The grids of [`random_batch`]'s editor: grid 1, two windows, a float
    /// and the message area, each as tall and wide as it starts.
    const WALK_GRIDS: [(u64, u64, u64); 5] =
        [(1, 30, 12), (2, 30, 5), (3, 30, 4), (4, 10, 3), (5, 30, 2)];

    /// A `grid_line` of `text` in highlight `hl` from column `col`.
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

    /// The editor of [`random_batch`] as it starts: every grid placed and
    /// drawn whole, a few highlights, and a cursor that blinks in normal
    /// mode, a bar in insert mode.
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

    /// One batch of what an editor does, at random: lines drawn, regions
    /// and windows scrolled, grids cleared, windows and floats moved or
    /// hidden and placed again, highlights and the default colours defined,
    /// the message area moved, the cursor and the mode moved, an option set.
    fn random_batch(w: &mut Walk) -> Vec<(&'static str, Vec<Value>)> {
        let mut events = Vec::new();
        for _ in 0..1 + w.below(3) {
            let (grid, width, height) = WALK_GRIDS[w.below(5) as usize];
            match w.below(28) {
                0..=1 if width > 2 => {
                    // A character two cells wide, which a float's edge may
                    // cut in two.
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
                // Now and then over the edge of grid 1, on a terminal
                // wider than it.
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
                    // A window scrolled as Neovim scrolls one: its view
                    // moved, its rows with it, and the rows it scrolled to
                    // drawn.
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

    /// Whatever the editor does, a client putting each frame together from
    /// only the rows that changed writes what one putting every row together
    /// writes, frame for frame: over a random walk of what an editor does
    /// (see [`random_batch`]), with frames drawn as often as not between the
    /// batches and time going on as unevenly — through windows moving,
    /// scrolls sliding and the cursor smeared or blinking, and still between
    /// — the terminal resized now and then, to sizes grid 1 covers and one
    /// it does not, and cards coming and going over it; with the terminal
    /// moving the rows a frame moved, and without. Hundreds of the frames
    /// are put together in part, with each effect but the blink, which
    /// paints over every frame.
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
                    // A card over the screen, for a moment. (One that leaves
                    // its mark stays over every frame after, all of them put
                    // together whole.)
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

    /// The same `events`, and then the same `keys`, to a client putting
    /// every frame together whole and to one putting frames together from
    /// only the rows that changed, and the same frame of each: how many of
    /// the second's have been put together in part.
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

    /// A view the client has moved ahead of Neovim, down or sideways, is not
    /// the editor's grids where the editor put them, though nothing moves:
    /// a client puts every frame together whole while it is ahead — and
    /// writes what one made to put every frame together whole writes — and
    /// in part again once Neovim has caught up, or the view has gone back.
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

    /// Settings come in the card's way, in the client's input, and go to the
    /// client rather than to the cards.
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

    /// Back in front from away, a client draws at once, and says something
    /// even where nothing has changed since its last frame — the cursor, put
    /// where it is: a frame nvmux waits for, to bring the session back as it
    /// is now (see `crate::pty::relay`). Back with what it had yet to draw,
    /// it draws that. In front all along, a Clear with nothing to take down
    /// draws nothing.
    #[test]
    fn a_client_back_from_away_draws_at_once_whatever_changed() {
        use crate::card::Command::{Away, Clear};
        let t0 = Instant::now();
        let mut a = attached(true, t0);
        redraw(&mut a, vec![line(1, 0, "here"), goto(0, 2)], t0);
        a.frame(t0);
        // Gone, and drawn as it went: nothing left to draw.
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

        // Gone again, and the session drew on after the frame it went with:
        // back sooner than the crawl would have drawn it, that is drawn.
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

        // In front, a Clear with no card to take down draws nothing.
        let later = back + Duration::from_secs(2);
        told(&mut a, &Clear, later);
        assert!(!a.due(later), "in front");
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
        d.client.predict_scrolls = Predict::Auto;
        d.remote = true;
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
        predicting_as(Predictions::default(), rtt).0
    }

    /// [`predicting`], working out `predictions` ahead of Neovim; with the
    /// agent's request, unanswered.
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

    /// A scroll that takes the cursor to another line lands it in the
    /// column the agent says it wants, Neovim's `curswant` — not the one it
    /// is in, at the end of a line too short for it; where the agent has not
    /// said, in the one it is in.
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
            // The wheel, three lines down: the cursor goes with the top.
            a.keys(b"\x1b[<65;3;2M", t1);
            assert_eq!(top_row(&a), "line 3 and on and on");
            a.anim.cursor_at(&a.model)
        };
        assert_eq!(land(Some(12)), (0, 12), "where it wants to be");
        assert_eq!(land(Some(2_147_483_647)), (0, 19), "as `$` has it");
        assert_eq!(land(None), (0, 5), "where it is, as far as is known");
    }

    /// In a window that does not wrap, shown from column 8, a scroll that
    /// lands the cursor out of view is drawn as Neovim draws it, the view
    /// moved sideways to show the cursor — alone ahead of Neovim; behind
    /// another, it is left to Neovim. And Neovim's own frame, when it comes,
    /// moves nothing.
    #[test]
    fn a_scroll_landing_out_of_view_moves_the_view_sideways() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let from = |left: usize, text: &[String]| -> Vec<String> {
            text.iter()
                .map(|t| t.chars().skip(left).collect())
                .collect()
        };
        // Lines 0 to 19, `abc` where `short` says, the cursor on line 0 in
        // column 10, wanting to be there.
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

        // Down three, to `abc`: its `c` is six columns left of the view, which
        // moves as far.
        let (mut a, text) = sideways(3);
        wheel(&mut a);
        assert_eq!(a.anim.ahead(2), (3, 3));
        assert_eq!(a.anim.side_left(2), Some(2));
        assert_eq!(top_row(&a), "c");
        assert_eq!(a.anim.cursor_at(&a.model), (0, 0), "on its `c`");
        wheel(&mut a);
        assert_eq!(a.anim.ahead(2), (3, 3), "the next waits for Neovim");
        // A frame of Neovim's before the scroll's, drawing nothing of it:
        // what is drawn again is the scroll's rows, moved, not Neovim's.
        redraw(&mut a, vec![], t1);
        assert_eq!(top_row(&a), "c");
        // Neovim's frame: it moved the view as far, said just before.
        let left = vec!["leftcol".into(), 1000.into(), 2.into()];
        notify(&mut a, "nvmux_client", left, t1);
        let mut events = vec![at(3, 3, 3)];
        events.extend(whole(2, 20, 5, &from(2, &text[3..8])));
        events.push(("grid_cursor_goto", vec![2.into(), 0.into(), 0.into()]));
        redraw(&mut a, events, t1);
        assert_eq!((a.anim.ahead(2), a.anim.side_left(2)), ((0, 0), None));
        assert_eq!(top_row(&a), "c");
        assert_eq!(a.anim.cursor_at(&a.model), (0, 0));

        // Down three, in view; three more, to `abc`, behind it: let go.
        let (mut a, _) = sideways(6);
        wheel(&mut a);
        wheel(&mut a);
        assert_eq!(a.anim.ahead(2), (3, 3));
        assert_eq!(a.anim.side_left(2), None);
        assert_eq!(top_row(&a), "and on and on and on", "line 3, from column 8");
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

    /// With a window the agent says is not to be predicted, a turn of the
    /// wheel waits for Neovim.
    #[test]
    fn a_turn_of_the_wheel_waits_where_it_cannot_be_predicted() {
        let t1 = Instant::now() + Duration::from_secs(1);
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
        // The link gets faster: a millisecond, every time it is asked.
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
        // A second, smoothed to 300 ms: fifteen pages.
        assert_eq!(measure(&mut a, Duration::from_secs(1)), [17]);
        // Faster again, down to a link no page outruns: the page in view, the
        // next, and two over — never none, however fast.
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

    /// Each kind is drawn ahead over a link as fast as it gets, the agent
    /// keeping lines for it from the first: a turn of the wheel scrolls the
    /// window at once, `l` moves the cursor, and what is typed goes in.
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

    /// A link grown fast stops nothing: the agent goes on sending what the
    /// predictions are made from, as far as the link pages, and the client
    /// keeps what it has.
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

    /// [`predicting`], the agent telling the client what Neovim made of
    /// each key: its request answered, the counts tied by the attach's fence
    /// answered with nothing after it, and its word on none of the keys
    /// taken — the cursor at the start of `line 0` — taken at once.
    fn acking(rtt: Duration) -> App {
        acking_as(MOTIONS, rtt)
    }

    /// [`acking`], working out `predictions` ahead of Neovim.
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

    /// The agent's `ack` for [`predicting`]'s window, buffer 1 at
    /// `b:changedtick` 1 and 50 lines long: `count` keys taken, the last of
    /// them `tail`, Neovim in `mode`, and the cursor on `line`, from nought,
    /// at byte `col` wanting display column `want` (-1 for the end of the
    /// line), the view from line `top` — and that line as wide as the client
    /// lays it out, where it has it, as Neovim lays out a line with nothing
    /// drawn between its characters.
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

    /// Neovim's frame for the cursor moved to `line` of [`predicting`]'s
    /// window, from nought, at byte `col`, the view from line 0.
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

    /// The agent is left counting the keys Neovim takes, and a fence reads
    /// the count, only where the client works out more than the scrolls:
    /// otherwise it is left and asked as it ever was.
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

    /// Told to have the agent say what Neovim made of each key, or told not
    /// to any more, a client predicting attaches again, for the agent it
    /// leaves to count the keys or not; told what it was told before, it
    /// stays as it is.
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

    /// A fence asked before the agent was left finds no count, and ties
    /// nothing; the client asks again once the agent is there, rather than
    /// leave the counts untied till the user pauses after a key it does not
    /// predict.
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

    /// What the agent says Neovim made of a key is said ahead of the frame
    /// that draws it, and taken with that frame, not before: Neovim's
    /// frame is the last word on the screen.
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

    /// A key that leaves the screen as it was has an ack and no frame: one
    /// that says what Neovim has drawn already is taken as it comes.
    #[test]
    fn an_ack_that_changes_nothing_on_the_screen_is_taken_at_once() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = acking(Duration::from_millis(200));
        a.keys(b"k", t1);
        ack(&mut a, 1, "k", "n", (0, 0, 0, 0), t1);
        assert_eq!(base(&a).map(|b| b.seq), Some(1));
        // One that says Neovim is in another mode than it drew waits for
        // the frame that draws it.
        a.keys(b"v", t1);
        ack(&mut a, 2, "v", "v", (0, 0, 0, 0), t1);
        assert_eq!(base(&a).map(|b| b.seq), Some(1));
    }

    /// An ack whose frame never came is taken at the fence asked after its
    /// key, before what the fence says.
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

    /// Where the agent says what Neovim made of each key, the column the
    /// cursor wants comes with that, and a scroll that takes the cursor to
    /// another line lands it there: the twin of
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

    /// Where the client would have put the cursor for the motions typed,
    /// had it drawn them, is judged against where Neovim says it put it:
    /// `jjl` as Neovim made it, `j` that Neovim made otherwise, and a key the
    /// client does not follow.
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

    /// An ack that covers a scroll made ahead of Neovim, with a frame that
    /// does not make it, is Neovim's word that it did not: the view goes back
    /// to Neovim's then and there, not at the scroll's deadline — and it is
    /// counted.
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
        // One its frame made only part of is not the one Neovim made.
        a.keys(b"2\x05", t1);
        assert_eq!(a.anim.ahead(2), (2, 2));
        ack(&mut a, 3, "2<C-E>", "n", (1, 0, 0, 1), t1);
        let mut events = vec![at(1, 1, 1)];
        events.extend((0..5).map(|r| line(2, r, &format!("line {}", r + 1))));
        redraw(&mut a, events, t1);
        assert_eq!(a.anim.ahead(2), (0, 0), "a line of the two");
        assert_eq!(top_row(&a), "line 1");
    }

    /// A turn of the wheel after a key the client does not follow is not
    /// drawn either, as a scroll typed then is not: drawn from where Neovim
    /// has the view and the cursor, it would leave out what that key did —
    /// here the cursor four lines down, which the turn leaves in view, the
    /// client's a line short of Neovim's. It is made once the agent says
    /// where the key left the cursor, with the frame that draws it, as a
    /// scroll typed then is (see
    /// [`a_scroll_after_a_key_left_to_neovim_lands_with_its_ack`]); and the
    /// turns after it at once.
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

    /// [`a_turn_of_the_wheel_after_a_key_left_to_neovim_lands_with_its_ack`],
    /// the counts in doubt by the time the ack's frame comes — `<C-c>`,
    /// which may throw the turn away: not made then, as a scroll typed
    /// then would not be.
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

    /// The agent's word on a turn of the wheel over a window other than the
    /// one with the cursor says nothing of that window's view: it waits for
    /// the frame that draws the turn, rather than giving the scroll up at
    /// once, which would take the window back a turn — and make the turns
    /// after it again from a view without it.
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
        // Over the window on the right, the cursor in the one on the left.
        a.keys(b"\x1b[<65;16;2M", t1);
        assert_eq!(a.anim.ahead(4).0, 3, "scrolled ahead");
        ack(&mut a, 1, "<ScrollWheelDown>", "n", (0, 0, 0, 0), t1);
        assert_eq!(a.anim.ahead(4).0, 3, "till its frame");
        let mut events = vec![viewport(3, 3)];
        events.extend((0..5).map(|r| line(4, r, &format!("line {}", r + 3))));
        redraw(&mut a, events, t1);
        assert_eq!(a.anim.ahead(4).0, 0, "made by it");
    }

    /// A scroll typed after a key the client does not follow is made once
    /// the agent says where that key left the cursor — with the frame that
    /// draws it — not once Neovim has caught up with every key typed.
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
        // And Neovim's own, when it comes, lands it.
        ack(&mut a, 2, "j<C-E>", "n", (1, 0, 0, 1), t1);
        let mut events = vec![at(1, 1, 1)];
        events.extend((0..5).map(|r| line(2, r, &format!("line {}", r + 1))));
        redraw(&mut a, events, t1);
        assert_eq!(a.anim.ahead(2), (0, 0));
        assert_eq!(top_row(&a), "line 1");
    }

    /// A settings card that leaves the keys the client works out as they
    /// were — any change of the settings sends one — leaves it blind to a
    /// key it could not follow, as it was: a scroll typed after that key
    /// waits for its ack still, which made from Neovim's word before it
    /// would scroll from the wrong line.
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

    /// Made out again from an ack, a scroll still ahead of Neovim is one
    /// made already, and is not made a second time: here the agent says
    /// something moved the cursor before Neovim took the key.
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

    /// An ack whose own frame never came — Neovim went on to the next key
    /// before it flushed — is taken with the next frame, which draws more
    /// than the ack covers: what that frame made already is not made again
    /// on top of it, and the client is blind till an ack comes with its own.
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

    /// Where the agent says nothing of a key — the link stalled, or Neovim
    /// at work on something else — a scroll made of it is given up at its
    /// deadline, as it is without acks.
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

    /// [`acking`], the agent having said the window's shape as it does
    /// where it says what Neovim made of each key — `'whichwrap'` and the
    /// rest of [`predict::Stepping`] after the rest — `mapped` among the keys
    /// it says something maps: the cursor moved ahead of Neovim along its
    /// line, `line 0` of `line 0` to `line 19`.
    fn stepping_with(rtt: Duration, mapped: &[&str]) -> App {
        stepping_as(MOTIONS, rtt, mapped)
    }

    /// [`stepping_with`], working out `predictions` ahead of Neovim.
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

    /// The shape the agent says of [`stepping`]'s window where it says what
    /// Neovim made of each key: its gutter `textoff` wide, `'number'` and
    /// `'relativenumber'` as `numbers` says, and the window drawn by
    /// `'statuscolumn'` where not `plain`; `'wrap'`; `'cursorline'` as
    /// `cul` has it (see [`predict::Cursorline`]); and `mapped` among the
    /// keys something maps, `idioms` how (see [`predict::Idiom`]).
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

    /// The agent says window 1000's shape is `shape`.
    fn reshape(a: &mut App, shape: Vec<Value>, now: Instant) {
        let params = vec!["shape".into(), 1000.into(), Value::Array(shape)];
        notify(a, "nvmux_client", params, now);
    }

    /// The groups Neovim draws a gutter in, as `hl_group_set` names them
    /// for these tests.
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

    /// What Neovim draws of [`numbered`]'s window with the cursor on line
    /// `cursor` at byte `byte`: each row's number, `'number'` and
    /// `'relativenumber'` both on, in the group its place takes — the
    /// cursor's in `CursorLineNr` if `cul` — but for the rows `numhl` says,
    /// in a sign's colours, 20; the rows' text after it; the cursor, and
    /// the view.
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

    /// [`stepping`]'s window with `'number'` and `'relativenumber'` on, its
    /// gutter four columns wide, `'cursorline'` as `cul` has it, and rows
    /// `numhl` with a sign's number in its colours: as Neovim draws it with
    /// the cursor on line 0, the groups named.
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

    /// The rows of the screen as the next frame puts them together, a row to
    /// a line.
    fn composed(a: &mut App) -> Vec<String> {
        let f = composed_frame(a);
        f.text().lines().map(|l| l.trim_end().to_string()).collect()
    }

    /// The frame the next frame puts together.
    fn composed_frame(a: &mut App) -> compose::Frame {
        a.repaint();
        let (cols, rows) = a.size();
        compose::compose(&a.model, &a.anim, cols, rows)
    }

    /// The groups the gutter is drawn over in on each row of grid 2, as the
    /// cursor moved ahead of Neovim has it: none for a row drawn as Neovim
    /// drew it.
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

    /// `j` over a slow link moves the cursor a line down at once, and with
    /// it every number of `'relativenumber'`, the cursor's line's now the
    /// line's own, aligned left: each in the group Neovim draws it in. Its
    /// frame, when it comes with the agent's word that it made it so, is
    /// the same; and so for the rest of the motions to another line.
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
        // Each of the motions to another line.
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

    /// A number Neovim draws in a sign's colours, not its own group's, is
    /// the sign's to keep: the number on its row moves on, in its colours.
    /// Where `'cursorline'` numbers the cursor's line, Neovim lays them over
    /// that line's own, which the client has no id for: a move onto such a
    /// line, or off it, is left to Neovim.
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

    /// A frame of Neovim's that draws a line the cursor was moved onto
    /// otherwise — between its characters — lets go of the steps; one that
    /// draws another line, or numbers the gutter afresh, as Neovim's frames
    /// for the keys before the last do, leaves them drawn.
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

    /// `CursorLine` as these tests have Neovim define it, its id `id`: a
    /// background alone, or with a colour in front as well.
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

    /// The text of row `row` of [`numbered`]'s window, `line N`, under
    /// `'cursorline'`'s bar, `CursorLine` laid over nothing: id 21.
    fn barred(row: u64) -> (&'static str, Vec<Value>) {
        line_hl(2, row, 4, &format!("{:16}", format!("line {row}")), 21)
    }

    /// `'cursorline'` drawing a bar across the cursor's line, a background
    /// alone, and numbering it: `j` takes the bar off the line the cursor
    /// leaves and lays it under the one it goes to, each row's text as
    /// Neovim drew it, and the cursor's number goes with it — every cell of
    /// the window as Neovim's frame for the key has it, in its colours.
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

    /// A bar the client cannot lay under a line and take off as Neovim
    /// does — one with a colour in front of its own, which Neovim lays over
    /// the line instead; one the editor has not named; one across the row
    /// of the line the cursor is on alone — leaves the motions to another
    /// line to Neovim; those along it are drawn. So does a `'statuscolumn'`,
    /// which may draw anything the cursor's line decides.
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

    /// A motion that would take the cursor out of the view, which Neovim
    /// scrolls to keep it in, is left to Neovim where the agent has not said
    /// how far `'scrolljump'` scrolls it (see [`scrolling`]); one to the
    /// view's last row is drawn.
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

    /// [`stepping`], the agent having said all it says of the window where
    /// it says what Neovim made of each key — `'scrolljump'` one, no text
    /// concealed on any line — and that no line about the view has virtual
    /// text among its characters: a motion past the edge of the view
    /// scrolls it ahead of Neovim, and a key that scrolls is a step.
    fn scrolling(rtt: Duration) -> App {
        scrolling_as(
            rtt,
            shaped(0, (false, false, true), (false, 0), &[], vec![]),
        )
    }

    /// [`scrolling`], the window shaped as `shape` says (see [`shaped`]).
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

    /// The agent says of [`stepping`]'s window that of its lines from 0 to
    /// 20, `lines` have virtual text or signs.
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

    /// Neovim's frame for [`stepping`]'s view of `line N`s scrolled `by`
    /// rows, fewer than five, from line `from`: the rows it brings in drawn,
    /// the cursor on line `cursor` at byte `col`.
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

    /// `j` on the view's last row scrolls the view a line ahead of Neovim
    /// with the cursor: one scroll ahead, and one step, which goes on from
    /// it. Neovim's frame for it, scrolled as far, with its word that it
    /// left the cursor there, makes them both, and the screen stays as it
    /// was drawn.
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

    /// A page after motions goes from where they left the cursor, not from
    /// Neovim's: the view half a window on from where it is, the cursor as
    /// far down from the line the motions took it to. And Neovim's frames
    /// for the motions, then for the page, make each in turn.
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

    /// Motions after a page go on from where it left the cursor, in the
    /// view it scrolled ahead of Neovim: as Neovim makes them once its
    /// frame for the page has come, and with it.
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

    /// The motions without the scrolls: a motion is drawn at once, and a key
    /// that scrolls, or a turn of the wheel, waits for Neovim, as any other
    /// key the client does not follow. The scrolls without the motions —
    /// the typing on, for the agent to say what Neovim made of each key —
    /// the other way about.
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

    /// A page past the lines the client has is made, and held till they
    /// come, as it always was: with no step before it, a scroll alone; after
    /// pages made as steps, those let go of as steps and kept as the
    /// scrolls they made, which go on drawing the cursor where they did. A
    /// motion after it waits for Neovim.
    #[test]
    fn a_held_page_with_no_chain_still_waits_for_its_lines() {
        let t1 = Instant::now() + Duration::from_secs(1);
        // Of the lines up to 20 the client has, pages of three rows up to
        // the 15th's are all there.
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

    /// A count given `<C-d>` is the window's `'scroll'` from then on: a shape
    /// the agent said before Neovim took the key, which has the `'scroll'`
    /// Neovim had before it, does not undo that for the pages after it —
    /// till Neovim's word on the key, from which the shapes are Neovim's.
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

    /// A page back from a view whose first line is the buffer's last, or the
    /// one before — which Neovim pages back from further than from
    /// elsewhere, keeping fewer of the lines it showed — is left to Neovim;
    /// one from a line further up is made, as ever.
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

    /// A line the scroll ahead would bring into view that the agent says
    /// has virtual text or a sign, which the client would draw without it,
    /// leaves the motion to Neovim: the view stays as Neovim has it, and the
    /// cursor on its last row.
    #[test]
    fn a_line_with_virtual_text_or_a_sign_is_not_scrolled_into_view() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = scrolling(Duration::from_millis(200));
        decorated(&mut a, &[3, 5], t1);
        a.keys(b"4jj", t1);
        assert_eq!((top_row(&a), drawn(&a)), ("line 0".into(), (4, 0)));
        assert_eq!(a.anim.ahead(2), (0, 0));
        // Nor past the lines the agent looked over for it.
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

    /// A frame of Neovim's that moves the view otherwise than the scroll
    /// ahead of it the steps made — further — lets go of the steps and the
    /// scroll: the view and the cursor are Neovim's at once.
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

    /// Motions past the edge of the view worked out while the motions prove
    /// themselves in the buffer — judged, and not drawn, nor any scroll made
    /// ahead of Neovim — are judged by Neovim's word on each as those in
    /// view are, its frame for the key having moved the view as the step
    /// would: eight found right, and they are drawn again, the view
    /// scrolled ahead with them.
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

    /// With `'number'` and `'relativenumber'` on, `j` past the bottom of the
    /// view numbers every row as Neovim will once it has scrolled: the rows
    /// Neovim's view has renumbered, and the line brought into view drawn
    /// with its own, the cursor's, in the groups their places take.
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

    /// A line Neovim scrolled out of view as it made a scroll ahead of it,
    /// whose row the window's scroll keeps while another is ahead, brought
    /// back into view by `k` past the top ahead of Neovim, is drawn as
    /// Neovim drew it — in its own colours, which the client's lines have
    /// nothing of — but for its gutter, renumbered with the cursor where
    /// the steps draw it, from where Neovim had it then.
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
        // Line 0 in colours of its own, as `:syntax` gives it.
        events.push(line_hl(2, 0, 4, &format!("{:16}", "line 0"), 5));
        redraw(&mut a, events, t1);
        ack(&mut a, 0, "", "n", (4, 0, 0, 0), t1);
        a.keys(b"jj", t1);
        assert_eq!(a.anim.ahead(2), (2, 2));
        // Neovim's frame for the first: line 0 out of its view.
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

    /// A turn of the wheel that scrolls the cursor's line out of view, which
    /// Neovim moves the cursor down for, has the gutter renumbered with the
    /// cursor where it is drawn: on Neovim's rows that stay in view and on
    /// those the scroll brings in, each number in the group its place
    /// takes.
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

    /// A scroll that draws the cursor on another line than Neovim's in a
    /// window whose gutter the client cannot lay out as Neovim does — in
    /// groups of the window's own, or a `'statuscolumn'`'s — or where the
    /// line it leaves may be drawn concealed, is drawn all the same, as a
    /// scroll was before the client drew what the cursor's line decides:
    /// Neovim's rows numbered as Neovim numbered them, for its own frame to
    /// put right. So is one that leaves the cursor on its line, which
    /// changes nothing of that.
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

    /// A row Neovim drew that its view has left, which a slide still shows,
    /// is drawn as Neovim drew it — its virtual text and all, which the
    /// client's own lines have nothing of — its gutter renumbered with the
    /// cursor where it now is: from where Neovim had it as it drew the row.
    #[test]
    fn a_row_neovim_drew_that_a_slide_still_shows_is_renumbered_as_it_was() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = numbered(Duration::from_millis(200), 0, &[]);
        let mut s = crate::config::Settings::default();
        s.effects.scroll.enabled = true;
        s.effects.windows.enabled = false;
        a.anim.set_effects(Effects::from_settings(&s));
        redraw(&mut a, vec![line_hl(2, 1, 11, "VT", 0)], t1);
        // Neovim scrolls the view two lines down, the cursor to line 3.
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

    /// `j` and `k` mapped as LazyVim maps them — to `gj` and `gk` with no
    /// count, which on lines a row each go where `j` and `k` go — are
    /// followed as the keys themselves; a mapping anything else is not, nor
    /// is `gj` where the column the cursor wants is past the row's end,
    /// which `gj` brings back into it.
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

    /// The agent's lines of [`stepping`]'s window are `text`, and `line N`
    /// after them, and Neovim draws the first five as `drawn` has them.
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

    /// `j` mapped the LazyVim way, onto a tab over the column the cursor
    /// wants in the right half of a row that wraps, short of the tab's end
    /// where `j` itself puts the cursor: `gj` steps back off it, onto the
    /// character before, as Neovim's `nv_screengo` has it — left to Neovim.
    /// In the row's left half, or `j` unmapped, it goes where `j` goes.
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

    /// A key the client cannot make out — `zc`, closing a fold — may change
    /// what the window shows and leave its cursor and view as they were:
    /// Neovim's word on it waits for the frame that shows what it did, and
    /// the motion typed behind it is made out over that, not over the rows
    /// as they were.
    #[test]
    fn a_word_on_a_key_the_client_cannot_make_out_waits_for_its_frame() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"zcj", t1);
        assert_eq!(drawn(&a), (0, 0), "blind after `zc`");
        ack(&mut a, 2, "zc", "n", (0, 0, 0, 0), t1);
        assert!(a.anim.chain(2).is_none(), "not over the rows as they were");
        // Lines 0 to 3 folded into the first row.
        let mut fold = vec![line(2, 0, "+--  4 lines: line 0")];
        fold.extend((1..5).map(|r| line(2, r, &format!("line {:<15}", r + 3))));
        fold.extend(cursor_to(0, 0));
        redraw(&mut a, fold, t1);
        assert!(a.anim.chain(2).is_none(), "nor over the fold");
        assert_eq!(drawn(&a), (0, 0));
    }

    /// A line the client draws ending in blanks may have blank virtual text
    /// among them that nothing on the screen shows: a motion onto it, or
    /// along it, is drawn where Neovim's own measure of it — which the
    /// agent sends of each such line in view, as of the cursor's — agrees
    /// with the client's, and left to Neovim where it measures it wider, or
    /// has not measured it.
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

    /// What the cursor's line decides of a window drawn in highlights of
    /// its own — `'winhighlight'`, or a namespace of the window's — is
    /// drawn in groups the client has no word of: the motions to another
    /// line there are left to Neovim where the line decides any of it, and
    /// drawn where it decides none.
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

    /// Text concealed off the cursor's line and shown on it: the line the
    /// cursor leaves is drawn concealed once it has, which the client
    /// cannot draw — the motions to another line are left to Neovim, those
    /// along it drawn.
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

    /// A motion to another line while the window's rows slide by, a scroll
    /// of Neovim's gliding, is drawn, and what is drawn over the rows — the
    /// gutter renumbered — slides with them: the line the slide still shows
    /// that Neovim's view no longer does drawn afresh, numbered as the rest.
    /// Once they have landed, it is drawn as it always was.
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

    /// Where the client draws the cursor, in its window's grid: grid 2 is
    /// at the top left of the screen.
    fn drawn(a: &App) -> (i64, i64) {
        a.anim.cursor_at(&a.model)
    }

    /// [`stepping`]'s window's shape, as [`shaped`] says it, with the rest of
    /// what the agent says where it says what Neovim made of each key — and
    /// what `'iskeyword'` at its default makes of each character under 256
    /// (see [`predict::Stepping::classes`]).
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

    /// The motions by words over a slow link move the cursor at once, before
    /// Neovim has heard of them — along the line, on to the next and back,
    /// with counts — where the agent says what `'iskeyword'` makes of the
    /// characters; where it does not, they wait for Neovim.
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

    /// `f`, `t`, `F` and `T` over a slow link move the cursor at once,
    /// before Neovim has heard of them, the cursor waiting for nothing but
    /// the character typed after them; where the agent says `:lmap`
    /// mappings may take that character for another, they wait for Neovim.
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

    /// A motion along the line over a slow link moves the cursor at once,
    /// before Neovim has heard of it; Neovim's frame, when it comes with the
    /// agent's word that it made it so, lets go of it, and the cursor stays.
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
        // Each of the motions along the line.
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

    /// Motions typed faster than the agent answers them stay drawn where the
    /// last of them goes: Neovim's frames for the keys before it, and one
    /// it draws part way through them with no word of where it is, move
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

    /// A key the client cannot follow leaves the motions after it to Neovim
    /// till the agent says where that key left the cursor: then they are
    /// worked out from there, and drawn at once.
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

    /// A motion Neovim made otherwise sends the cursor to where Neovim has
    /// it at once; and the buffer proves itself before the client draws
    /// another there: eight in a row worked out right, each moving the
    /// cursor, judged and not drawn.
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
        // Proving: worked out and judged, the cursor left where Neovim draws
        // it — right, and then wrong again.
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

    /// Eight motions that each move the cursor, judged right while hidden,
    /// and the next is drawn.
    #[test]
    fn a_buffer_drawn_wrong_is_drawn_again_once_proved() {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping(Duration::from_millis(200));
        a.keys(b"l", t1);
        ack(&mut a, 1, "l", "n", (0, 2, 2, 0), t1);
        redraw(&mut a, cursor_to(0, 2), t1);
        // Back and forth, each moving the cursor.
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

    /// Neovim drawing the cursor somewhere else than the client did, on a
    /// motion it worked out right — a character it takes for another width,
    /// say — leaves the window's motions to Neovim a while.
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
        // Once the while is over, drawn again.
        let later = t1 + predict::DISTRUST;
        ack(&mut a, 2, "ll", "n", (0, 2, 2, 0), later);
        redraw(&mut a, cursor_to(0, 2), later);
        a.keys(b"l", later);
        assert_eq!(drawn(&a), (0, 3));
    }

    /// The agent sends the edits ahead of the ack that follows them: an ack
    /// for a buffer as it is at a change the client's lines are not at
    /// leaves the motions to Neovim.
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
        // The edit, as the agent says it, and the next ack with it.
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

    /// Neovim saying nothing of a motion in time, the cursor goes back to
    /// where Neovim has it, as a scroll ahead does, and the client waits
    /// for its word before drawing another.
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

    /// A motion something maps is the mapping's to make: Neovim's.
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

    /// `g_`, its `g` a prefix of Neovim's own mappings, is followed whole.
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

    /// A fence answered for keys Neovim took without a word of where they
    /// left the cursor — an operator pending, say — has the client let go
    /// of what it drew for them, and wait for Neovim's next word.
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

    /// `<C-c>`, which Neovim counts twice, puts the counts in doubt: what
    /// was drawn ahead goes, and nothing is till a fence ties the counts
    /// again.
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
        // Neovim's word, and the fence after `<C-c>`, a key typed since:
        // nothing tied yet. The next, with nothing after it, ties them.
        ack(&mut a, 4, "l<C-C><C-C>l", "n", (0, 2, 2, 0), t1);
        redraw(&mut a, cursor_to(0, 2), t1);
        answer(&mut a, asked, Value::from(3), t1);
        a.tick(t1);
        let fence = request(&calls(&mut a), "nvim_eval", "luaeval");
        answer(&mut a, fence, Value::from(4), t1);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 3), "tied again");
    }

    /// A key with Alt, which Neovim may take as `<Esc>` and the key — as a
    /// terminal's `<Esc>` and the key after it read together are — puts the
    /// counts in doubt from it on and not before: the step for a key typed
    /// before it stays drawn, and is judged by Neovim's word on its key as
    /// ever; nothing is drawn from the Alt key on till a fence ties the
    /// counts again. Let go of, the step would take the screen back behind
    /// a key Neovim had taken.
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
        // Neovim's word on the keys from the Alt key on, which it took as
        // one, and the fence asked with a key after it: nothing tied yet.
        // The next, with nothing after it, ties them.
        ack(&mut a, 3, "l<M-h>l", "n", (0, 1, 1, 0), t1);
        redraw(&mut a, cursor_to(0, 1), t1);
        answer(&mut a, asked, Value::from(2), t1);
        a.tick(t1);
        let fence = request(&calls(&mut a), "nvim_eval", "luaeval");
        answer(&mut a, fence, Value::from(3), t1);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 2), "tied again");
    }

    /// Neovim's word on a key with Alt read with the key before it, which
    /// says nothing of that key alone but that Neovim has taken it: the step
    /// for it is let go of with the frame the word was said ahead of, which
    /// shows Neovim past it — not before, which would take the screen back
    /// behind it, nor never, which would leave it drawn over what Neovim
    /// draws after it.
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

    /// [`a_count_past_a_key_with_alt_lets_go_of_the_steps_before_it_with_its_frame`],
    /// where Neovim's last frame shows what the word says already: the step
    /// is let go of at once, as an ack is — but not where the keys changed
    /// the buffer, which the frame for them draws, and the step with them
    /// till then.
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

    /// [`a_count_past_a_key_with_alt_lets_go_of_the_steps_before_it_with_its_frame`],
    /// said by a fence with keys after it, and no ack: Neovim has drawn its
    /// frame by the time it answers one, and the step is let go of at once.
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

    /// A frame of Neovim's that changes what the window shows elsewhere than
    /// on the line the cursor moved along — a plugin's virtual text on
    /// another, a diagnostic, a timer's redraw — leaves the steps drawn. One
    /// that changes that line's row is not what they were worked out over:
    /// they are let go of, and worked out again at once from Neovim's word,
    /// where the line is laid out as before — something drawn after its end
    /// — and left to Neovim where not, the cursor put back where Neovim has
    /// it and not travelling there.
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
        // Neovim's word, with the line as it draws it now: still laid out
        // otherwise, and left to Neovim.
        ack(&mut a, 3, "lll", "n", (0, 3, 5, 0), t1);
        a.keys(b"l", t1);
        assert_eq!(drawn(&a), (0, 0));
    }

    /// A step that moves only the cursor leaves the frame the editor's
    /// grids and nothing more: it is put together from the rows that
    /// changed, which are none, and the cursor.
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

    /// The agent's word on line 0 of [`stepping`]'s buffer edited, from
    /// `b:changedtick` `from` to `to`: `text` for it.
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

    /// [`ack`] for line 0 of [`stepping`]'s buffer at `b:changedtick` `tick`,
    /// in Normal mode, the cursor at byte `col` and the line laid out by
    /// Neovim `width` columns wide.
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

    /// A line Neovim lays out wider than the client does — blank virtual
    /// text between its characters, which nothing on the screen shows
    /// before blanks — has its motions left to Neovim, by Neovim's own
    /// measure of it.
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

    /// A motion that leaves the cursor on the character it was on leaves it
    /// drawn where Neovim drew it, which on a tab may be the tab's start:
    /// Neovim works out where to draw the cursor only once it moves, and
    /// leaving Visual mode on a tab leaves it where Visual mode drew it.
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
        // Neovim's frame for a key that moved nothing is one it drew before
        // that key: the cursor where it was.
        let mut a = stepping(Duration::from_millis(200));
        redraw(&mut a, cursor_to(0, 3), t1);
        a.keys(b"0", t1);
        assert_eq!(drawn(&a), (0, 3), "Neovim's word and its frame disagree");
    }

    /// An ack for a key that edited the line — `x` in a run of one
    /// character, which leaves the cursor where it was — comes ahead of the
    /// frame that draws the edit, as every ack does: the motions after it
    /// are drawn with that frame, not over the line as it was before it.
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

    /// A paste, in Normal mode no key to Neovim, holds the acks back till a
    /// fence after it is answered, and leaves the client blind after it;
    /// but the motions drawn before it are where Neovim puts the cursor
    /// still, and stay drawn till its frame for the paste says otherwise.
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

    /// A turn of the wheel over the window the cursor was moved ahead in is
    /// drawn at once, as it is with no motion ahead — but from where the
    /// motions leave the cursor, a step of its own after theirs: the view
    /// scrolled ahead of Neovim, and the cursor pushed down from the line
    /// they took it to, not from Neovim's. One that cannot be made so — a
    /// line it brings into view the agent has said nothing of — waits for
    /// Neovim, the motions drawn as they were and the client blind after
    /// it; and a turn sideways always does.
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

    /// Steps a frame lets go of put the cursor back where Neovim has it at
    /// once, smear or no smear: travelling back, it would draw a move of its
    /// own that no key made.
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
        // Worked out again at once: neither back nor forth.
        redraw(&mut a, vec![line(2, 0, "line 0  E")], t2);
        assert_eq!(drawn(&a), (0, 5));
        assert!(a.anim.still(), "not travelling back and forth");
        redraw(&mut a, vec![line(2, 0, "li: ne 0")], t2);
        assert_eq!(drawn(&a), (0, 0));
        assert!(a.anim.still(), "not travelling back");
    }

    /// What `'iskeyword'` makes of the characters under 256 at its default,
    /// as the agent sends it (see [`typing::Facts::classes`]).
    fn classes() -> String {
        (0u8..=255)
            .map(|b| match b {
                0 | b'\t' | b' ' | 0xa0 => '0',
                b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'_' | 192..=255 => '2',
                _ => '1',
            })
            .collect()
    }

    /// The agent's word on buffer 1's Insert mode, entered — nothing in it
    /// to type otherwise, as Neovim's defaults have it — as `change` makes
    /// it, by its place in the notification (see [`typing::Facts::read`]),
    /// the method's name first; and that no line in view of its window has
    /// virtual text that stays where it is as text is typed (see
    /// [`Predictor::pinned`]).
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

    /// [`ack`] for line 0 of [`stepping`]'s buffer at `b:changedtick`
    /// `tick`, in Insert mode, the cursor at byte `col` — as far into a line
    /// of ASCII, and wanting as far — and the line laid out by Neovim as the
    /// client lays out its text.
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

    /// Neovim's frame for line 0 of [`stepping`]'s window as `text`, the
    /// cursor at byte `col` of it in Insert mode.
    fn typed_frame(text: &str, col: i64) -> Vec<(&'static str, Vec<Value>)> {
        let mut events = vec![
            ("mode_change", vec!["insert".into(), 1.into()]),
            line(2, 0, &format!("{text:20}")),
        ];
        events.extend(cursor_to(0, col));
        events
    }

    /// [`stepping`]'s window, typing ahead of Neovim as well, Neovim in
    /// Insert mode at the start of `line 0` by its word and its frame, and
    /// nothing in the buffer's Insert mode to type otherwise.
    fn typing(rtt: Duration) -> App {
        let t1 = Instant::now() + Duration::from_secs(1);
        let mut a = stepping_as(TYPING, rtt, &[]);
        facts(&mut a, |_| {}, t1);
        ack_i(&mut a, 0, "", 0, 1, t1);
        redraw(&mut a, typed_frame("line 0", 0), t1);
        a
    }

    /// Neovim's word on each of `keys` typed into `line 0` of [`typing`]'s
    /// window from its start, after `from` typed before them, the line's
    /// text `texts` after each, and its frame for it: the agent's edit, its
    /// ack, the frame.
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

    /// Characters typed in Insert mode over a slow link go in at once, the
    /// cursor going on past each — before Neovim has heard of them — and
    /// Neovim's word on each, with its frame, finds them made and lets them
    /// go, its own row the same.
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
        // And taken out again.
        a.keys(b"\x7f\x7f", t1);
        assert_eq!(composed(&mut a)[0], "aline 0");
        assert_eq!(drawn(&a), (0, 1));
    }

    /// Characters typed while an `i` the client leaves to Neovim is on its
    /// way — in a buffer the agent has yet to say anything of, as before
    /// Insert mode is first entered there — wait for Neovim's word on it,
    /// and appear together once the agent has said what it does once
    /// entered, which comes after that word.
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

    /// The edits for Neovim's word on a key come ahead of the word, and the
    /// word ahead of its frame — each may come apart from the next over a
    /// lossy link: a key typed in between goes on from what the keys before
    /// it typed, not from the client's lines.
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

    /// Virtual text Neovim draws anew past the end of the line typed into —
    /// a diagnostic, as it is typed — with its word on a key: the keys
    /// after it are laid out again over Neovim's row as it now is, and what
    /// it draws there goes with the end. Where it draws the typed text
    /// itself otherwise than the client laid it out, the window is left to
    /// it a while.
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
        // Drawn otherwise: a character as wide as the client did not take
        // it for, say.
        let mut a = typing(Duration::from_millis(200));
        a.keys(b"ab", t1);
        splice0(&mut a, (1, 2), "aline 0", t1);
        ack_i(&mut a, 1, "a", 1, 2, t1);
        redraw(&mut a, typed_frame("a line 0", 1), t1);
        let p = a.predict.as_ref().expect("predicting");
        assert!(p.trust.refused(1000, t1), "left to Neovim");
        assert_eq!(composed(&mut a)[0], "a line 0");
    }

    /// Text typed while the window's rows slide by, a scroll of Neovim's
    /// gliding, is left to Neovim: the line typed into, drawn over its row,
    /// would not slide with them. Once they have landed, it is drawn.
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

    /// A line with virtual text Neovim draws where the client would not move
    /// it with the line's end — right-aligned, in a column of the window's,
    /// inline after the last character and staying before what is typed
    /// there — as the agent says of the lines in view, is left to Neovim to
    /// type into, and to enter Insert mode at the end of; elsewhere on it,
    /// Insert mode is entered at once.
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

    /// A glyph of `'list'`'s new to the line typed into — `trail`, after a
    /// blank typed at its end — across the bar of `'cursorline'` is drawn in
    /// its group laid over the bar, as Neovim draws it, not in its group
    /// alone, which would leave a hole in the bar.
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

    /// Told to work out the motions alone, the client types nothing ahead
    /// of Neovim.
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

    /// A key a mapping in Insert mode starts with is left to Neovim, and
    /// those after it with it, till Neovim says what it made of it.
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

    /// An autopair Neovim's mappings did not have when the agent last said
    /// them makes `(` into `()`: the `(` drawn ahead is judged wrong, and the
    /// row and the cursor are Neovim's at once. What else is typed is drawn
    /// as ever; `(` is worked out and judged, and not drawn, till it has
    /// proved itself.
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

    /// In a buffer where a plugin's `InsertCharPre` — or `TextChangedI` —
    /// may do anything to what is typed, it is worked out and judged, and
    /// not drawn, till eight characters other than blanks have been judged
    /// right: from then on, it is drawn.
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

    /// A key Neovim takes without going to wait for another — `<C-n>`,
    /// completing — and says nothing of: the fence asked after it finds
    /// Neovim has taken the characters typed before it, and Neovim's frame
    /// is the last word on them. What was typed ahead goes, and the keys
    /// after wait for Neovim.
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

    /// [`stepping`]'s window, typing ahead of Neovim as well, in Normal mode
    /// by Neovim's word, the buffer's Insert mode with nothing in it to type
    /// otherwise, and the cursor a block in Normal mode and a bar in Insert
    /// mode, as `'guicursor'` has them by default.
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

    /// The terminal's cursor as the next frame leaves it: where, and in what
    /// shape, as `DECSCUSR` numbers them.
    fn terminal_cursor(a: &mut App) -> ((usize, usize), u8) {
        let mut frame = composed_frame(a);
        let c = a.anim.paint(&mut frame, &a.model, Instant::now());
        ((c.row, c.col), c.shape)
    }

    /// `i`, `a`, `A` and `I` over a slow link put the cursor where Neovim
    /// puts it at once, and draw it as Insert mode draws it — a bar, where
    /// Normal mode draws a block — before Neovim has heard of them; Neovim's
    /// word on the key, with its frame, finds it made.
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

    /// `<Esc>` after text typed ahead of Neovim puts the cursor back a
    /// character, a block again, the text still drawn as typed — and the
    /// motions after it go on from there, over the line as typed.
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

    /// `<Esc>` on a line all blanks — an indent nothing was typed after,
    /// which Neovim takes out — or after an abbreviation, which it expands,
    /// waits for Neovim, the cursor a bar till then.
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

    /// A plugin's `InsertLeave` that takes out the blanks a line ends in
    /// has `<Esc>` do more than put the cursor back: judged wrong by the
    /// line's text as Neovim's word on it has it, the line is Neovim's at
    /// once, and `<Esc>` proves itself in the buffer before it is drawn
    /// there again.
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

    /// A plugin's `InsertEnter` that edits the line and moves the cursor:
    /// the `i` drawn ahead is judged wrong by Neovim's word on it, whatever
    /// the edit did to the buffer, and the cursor and its shape are
    /// Neovim's at once; `i` proves itself in the buffer before it is drawn
    /// there again.
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

    /// [`moding`], the agent saying a plugin's autocommand may do anything
    /// in the buffer as Insert mode is entered, if `entered`, and as it is
    /// left, if `left`.
    fn unseen_moding(entered: bool, left: bool, now: Instant) -> App {
        let mut a = moding(Duration::from_millis(200));
        facts(&mut a, |v| v.extend([entered.into(), left.into()]), now);
        a
    }

    /// The last four of `keys`, as the agent says them with its word on
    /// them.
    fn tail(keys: &[&str]) -> String {
        keys[keys.len().saturating_sub(4)..].concat()
    }

    /// Neovim's frame for [`moding`]'s window in Normal mode, the cursor at
    /// byte `col` of line `line`.
    fn normal_frame(line: i64, col: i64) -> Vec<(&'static str, Vec<Value>)> {
        let mut events = vec![("mode_change", vec!["normal".into(), 0.into()])];
        events.extend(cursor_to(line, col));
        events
    }

    /// A plugin's `InsertLeave` — or `InsertLeavePre`, or `ModeChanged` —
    /// may move the cursor or edit the buffer, as a trimmer of the blanks
    /// lines end in does, its `:%s` leaving the cursor on the last line it
    /// trimmed: in its buffer, `<Esc>` is worked out and judged, and not
    /// drawn, nor the keys after it, which go on from it, till eight have
    /// been judged right, each leaving the buffer as it was; from then on,
    /// it is drawn. Entering Insert mode, which nothing of the plugin's sets
    /// off, is drawn all along.
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

    /// A plugin's `InsertLeave` the agent says of only once `<Esc>` has been
    /// drawn ahead eight times and more — made by hand, or by a plugin
    /// loaded late — finds nothing proved of what it does: `<Esc>` is worked
    /// out and judged, and not drawn, as where it was there all along.
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

    /// The same of a plugin's `InsertEnter` — or `ModeChanged` — and the
    /// keys that enter Insert mode: `i` is worked out and judged, and not
    /// drawn, nor what is typed after it, till eight have been judged
    /// right; `<Esc>`, which nothing of the plugin's sets off, is drawn all
    /// along.
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

    /// `<Esc>` drawn ahead of Neovim — in a buffer whose leaving Insert mode
    /// the agent knows nothing to set off, or has proved itself — that
    /// Neovim makes as drawn, the line typed into as typed, but with another
    /// line edited besides, as Neovim's word on the key before it and on it
    /// say: an autocommand of the mode, a plugin's, that the agent did not
    /// know of or that had done nothing till then. The motion after it was
    /// worked out over the buffer as it was, and is let go of, and worked
    /// out again over it as it is; and `<Esc>` proves itself in the buffer
    /// before it is drawn there again — though not drawn wrong, for that,
    /// nor counted so.
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

    /// Neovim's word on text typed and on the `<Esc>` after it at once says
    /// nothing of what leaving Insert mode did to the buffer, which the text
    /// changed as well: where a plugin's autocommand of it may do anything,
    /// that `<Esc>` proves nothing, and the motion after it, worked out over
    /// the buffer as it was, is worked out again over it as it is.
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

    /// A count before `i` has `<Esc>` type the text again as many times
    /// over: the `i` and the `<Esc>` are left to Neovim, the text typed
    /// between drawn once Neovim's word on the `i` has come.
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

    /// An Insert mode entered with a count, and another entered with none
    /// after it, both before Neovim's word on the first: at that word, the
    /// keys after it are made out again from there, its `<Esc>` — which
    /// types the text again — left to Neovim, whatever was made of the
    /// keys after it as they were typed.
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

    /// `I` in Visual block mode inserts on every line of the block at
    /// `<Esc>`, which puts the cursor where the block starts: the client
    /// enters no Insert mode from Visual mode, and so leaves that `<Esc>`
    /// to Neovim, though what is typed on the block's first line before it
    /// is drawn.
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

    /// An escape held is let go `'ttimeoutlen'` after it came, and no later
    /// than the relay's own wait, whatever else wakes the loop in between.
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

    /// A `'ttimeoutlen'` shorter than the relay's wait is kept, and none at
    /// all lets an escape go at once.
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

    /// A float anchored to a window whose text is centred goes in with the
    /// text, but no further than keeps it on the screen: Neovim fitted it to
    /// the screen where it put it, and one near the right of the screen
    /// would otherwise be drawn past its edge.
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
        // Grid 3, a float 8 wide on the window's grid, at column `col`.
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

    /// A `tails` notification for window 1000's row `row`: its tail, `text`
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

    /// A window narrowed to 8 of its 20 columns, its text 6 in, with
    /// `text` on its first row.
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

    /// What the editor says runs past a narrowed grid's edge is drawn on in
    /// the window's margin, as far as the window goes: with the flush the
    /// editor said it ahead of, not before it.
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

    /// Said where no flush follows — what changed past the edge changed
    /// nothing on the screen — it is drawn once it has waited for one.
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

    /// What runs past a row's end slides with its row as the window
    /// scrolls, and lands with it.
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

    /// A grid the client cuts narrower lets go of what its rows ran on with:
    /// that was said of the width it had.
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

    /// The measure's word on window 1000: it wants `width`.
    fn wanting(width: i64) -> Vec<Value> {
        let each = vec![Value::Array(vec![1000.into(), width.into()])];
        vec!["measure".into(), 2.into(), Value::Array(each)]
    }

    /// [`wide_app`] with one window, the whole terminal wide and drawn on,
    /// asked for `window_width` — the measure has said nothing of it — and
    /// then, the measure having said it wants 6, for that: as a window split
    /// off another is asked for. Neither ask answered yet.
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

    /// Neovim's answer to an ask the client has asked again since, for less,
    /// coming on its own: the client's copy stays as it cut it. Neovim made
    /// its grid of that width from the one it had and draws none of it
    /// again, so taken whole the copy would be wider than it was ever drawn,
    /// and the client would hold its frames — a split's flying in with them
    /// — on a refresh for the columns between, which Neovim's answer to the
    /// second ask cuts off again.
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

    /// Neovim's answer to the last ask is taken whatever was asked before
    /// it: taking both asks before it redraws, Neovim answers only the last.
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

        // What the resize adds Neovim draws.
        let mut events = vec![resize(2, 12, 5)];
        events.extend((0..5).map(|r| line_at(2, r, 8, "    ")));
        redraw(&mut a, events, t0);
        assert_eq!(a.model.grids[&2].width(), 12, "taken");
        assert!(a.cut.is_empty() && a.unanswered.is_empty());
        assert!(!a.take_refresh(), "drawn whole");
    }

    /// A window split off another, asked for `window_width` and then for
    /// what the measure says it wants, flies in whatever order Neovim's
    /// answers come in: its answer to the first ask coming on its own, and
    /// then the windows placed again where they were going, the split is
    /// still on its way — not held for a refresh, which would have it land
    /// at once.
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

        // `:vsplit`: the window 10 wide, and one 9 wide right of it.
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
        // Where the line between the two windows is on the top row.
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

    /// An ask back to the width of an answer not taken waits for Neovim's
    /// answer to the ask after it: Neovim has its grid that wide, and taking
    /// both asks before it redraws would find it as wide as it was and say
    /// nothing, leaving the client's copy cut, with what is past the cut
    /// never sent again (see
    /// [`a_grid_cut_is_asked_back_whole_once_neovim_has_cut_it`]).
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

    /// With nothing centred, nothing is measured, and nothing left in the
    /// editor to measure it.
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

    /// A client with a blinking block cursor, as `[effects.blink]` and a
    /// `'guicursor'` of `block-blinkwait700-blinkoff400-blinkon250` make it,
    /// drawn once and then left alone.
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

    /// `run`'s loop with nothing coming in, from `from` to `until`: wait for
    /// [`App::wake_at`], rounded up to the millisecond as `run` rounds it,
    /// and draw if [`App::due`]. How many times it woke, and how many frames
    /// it drew.
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

    /// A blinking cursor left alone fades out and back in, a frame at a time
    /// while it fades — every wake a frame, rather than wakes that find
    /// nothing due — and holds without a wake in between.
    #[test]
    fn a_blinking_cursor_is_drawn_as_it_fades() {
        let t0 = Instant::now();
        let mut a = blinking(false, true, t0);
        let (wakes, frames) = left_alone(&mut a, t0, t0 + Duration::from_secs(10));
        // Fades are 40% of each half of a 650 ms blink: 15 or so frames a
        // blink, 15 blinks after the wait.
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

    /// A blinking cursor does not keep the animations' clock from starting
    /// afresh: a smear after a quiet spell moves as it does with the blink
    /// off, rather than all the way in its first step.
    #[test]
    fn a_smear_after_a_quiet_spell_moves_with_the_blink_on() {
        let frames_after_jump = |blink: bool| {
            let t0 = Instant::now();
            let mut a = blinking(true, blink, t0);
            let quiet = t0 + Duration::from_secs(2);
            left_alone(&mut a, t0, quiet);
            redraw(&mut a, vec![goto(5, 15)], quiet);
            // Frames while the smear moves, before the blink's wait is over.
            left_alone(&mut a, quiet, quiet + Duration::from_millis(300)).1
        };
        let still = frames_after_jump(false);
        assert!(still > 3, "the smear moves over {still} frames");
        assert_eq!(frames_after_jump(true), still);
    }

    /// Away, a blinking cursor holds still and whole: the client is drawn
    /// once more, as an away client draws, within [`AWAY_FRAME_GAP`] of its
    /// last frame, with the cursor whole, and then sleeps. Back in front, it
    /// draws at once, and blinks again from the start of its wait.
    #[test]
    fn a_client_away_holds_its_blink_whole() {
        let t0 = Instant::now();
        let mut a = blinking(false, true, t0);
        // In the middle of the first fade out.
        let t1 = t0 + Duration::from_millis(760);
        left_alone(&mut a, t0, t1);
        told(&mut a, &crate::card::Command::Away, t1);
        let once = a.wake_at(t1).expect("drawn once more");
        assert!(once <= t1 + AWAY_FRAME_GAP, "drawn at {once:?}");
        assert!(a.due(once));
        let held = String::from_utf8_lossy(&a.frame(once)).into_owned();
        // The cursor's cell drawn as it is at the start, whole, rather than
        // as it is at this moment if it goes on blinking, part faded: the SGR
        // its `h` is written in.
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
