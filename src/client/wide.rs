//! `[client] centre`: centring a window's text when the terminal is wider
//! than it needs (see [`crate::config::ClientSettings::centre`]).
//!
//! With `ext_multigrid`, the client asks Neovim (`nvim_ui_try_resize_grid`)
//! for a grid narrower than its window, at the width [`MEASURE_LUA`] reports,
//! so Neovim wraps text there. The client draws the grid in the window's
//! middle ([`inset`]), moving anchored floats with it as far as they stay on
//! screen (see [`super::model::Model::inset`]), and moves the full-width
//! status line's words over the text ([`realign`]). Terminal windows are
//! never narrowed. Grid sizes are shared by every UI of the session, so on
//! exit the client asks for each window's width back and briefly waits for it
//! ([`super::run`]).
//!
//! A window that does not wrap gets a grid from where its centred text starts
//! to its right edge ([`run_on`]), so a line wider than the text runs on
//! rather than being cut where the text ends, the window empty beside it.
//! Past the text a row is drawn only as far as it has text ([`ink_end`]).
//!
//! The client cuts its own grid copy as soon as it asks, so a new split is
//! centred from its first frame rather than a round trip later. If a grid is
//! asked for again, narrower, before Neovim answers (a split gets
//! `window_width` before the measure, then the measured width), the copy
//! keeps the narrower width (see `App::grid_resized`). Inset changes slide
//! ([`super::anim::Animator::insets`]).
//!
//! A killed client restores nothing, and Neovim keeps a grid as asked until
//! some UI asks for the full width. So once per attach the client asks for
//! the full width of each window whose grid is not the window's width, unless
//! it narrows that window itself. With two clients attached, the last to
//! attach wins.
//!
//! Neovim cuts end-of-line virtual text (diagnostics, blame) at the grid's
//! edge. [`TAILS_LUA`] reports each such row's full tail and where it starts;
//! the client draws the cut part in the margin ([`past`]), locating it on the
//! drawn row ([`place`]), since concealed text shifts the line's end.

use std::collections::BTreeMap;

use rmpv::ValueRef;
use unicode_width::UnicodeWidthChar;

use super::compose::Out;
use super::grid::{Cell, Text};
use super::model::syntax_hl;
use super::redraw;

/// Lua returning the width each window of the current tab page wants, given
/// the configured text width, whether `[client] centre = "smart"`, and
/// optionally the client's channel.
///
/// - Terminal windows want the whole terminal.
/// - So do windows already centred, like a start screen: every line indented
///   about as much as the longest stops short of the right edge, within 2
///   columns plus the gutter. They lay out for `nvim_win_get_width`, so on a
///   narrower grid they would be centred twice.
/// - `'winfixwidth'` windows want their width.
/// - Others want the given width. With `smart`: `'textwidth'` if set, and at
///   least enough for their unwrapped lines among the first 4000, plus the
///   gutter: with `'nowrap'` the widest, otherwise 90% of Markdown tables and
///   code blocks (via tree-sitter).
///
/// Returns `{ 'laststatus', { { window, wanted, nowrap }, ... } }`;
/// `'laststatus'` tells which windows have their own status line
/// ([`realign`]), `nowrap` which run their lines on ([`run_on`]).
///
/// With a channel, it stays installed and sends `measure` on `nvmux_client`
/// with the same two values whenever they change (window, buffer, tab and
/// relevant option events), but not on text changes, which would make the
/// width jump while typing. It removes itself on this UI's `UILeave` or once
/// the channel is gone.
pub const MEASURE_LUA: &str = "local most, smart, chan = ...
local api = vim.api
local whole = 1073741824
local blocks
local seen = {}
local edges = {}
local function terminal(win)
  return vim.bo[api.nvim_win_get_buf(win)].buftype == 'terminal'
end
local function centred(win, buf)
  local tick = vim.b[buf].changedtick
  local e = edges[buf]
  if not e or e.tick ~= tick then
    e = { tick = tick }
    local n = math.min(api.nvim_buf_line_count(buf), 4000)
    for _, line in ipairs(api.nvim_buf_get_lines(buf, 0, n, false)) do
      local ink = line:match('^.*%S')
      if ink then
        local lead = ink:match('^%s*')
        if lead == '' then
          e.left = nil
          break
        end
        e.left = math.min(e.left or whole, vim.fn.strdisplaywidth(lead))
        e.right = math.max(e.right or 0, vim.fn.strdisplaywidth(ink))
      end
    end
    edges[buf] = e
  end
  if not e.left then
    return false
  end
  local slack = api.nvim_win_get_width(win) - e.right - e.left
  return slack >= -2 and slack <= vim.fn.getwininfo(win)[1].textoff + 2
end
local function blocked(buf, n)
  if blocks == nil then
    local ok, q = pcall(vim.treesitter.query.parse, 'markdown',
      '[(pipe_table) (fenced_code_block) (indented_code_block)] @block')
    blocks = ok and q or false
  end
  local ok, parser = pcall(vim.treesitter.get_parser, buf, 'markdown')
  if not blocks or not ok or not parser then
    return nil
  end
  local tree = parser:parse()[1]
  if not tree then
    return nil
  end
  local rows = {}
  for _, node in blocks:iter_captures(tree:root(), buf, 0, n) do
    local first, _, last, col = node:range()
    if col == 0 then
      last = last - 1
    end
    for r = first, math.min(last, n - 1) do
      rows[r + 1] = true
    end
  end
  return rows
end
local function long(win, buf)
  local wrap = vim.wo[win].wrap
  local key = table.concat({ vim.b[buf].changedtick, tostring(wrap), vim.bo[buf].filetype }, ':')
  if seen[buf] and seen[buf].key == key then
    return seen[buf].width
  end
  local n = math.min(api.nvim_buf_line_count(buf), 4000)
  local rows = true
  if wrap then
    rows = vim.bo[buf].filetype == 'markdown' and blocked(buf, n)
  end
  local widths = {}
  if rows then
    for i, line in ipairs(api.nvim_buf_get_lines(buf, 0, n, false)) do
      if (rows == true or rows[i]) and line ~= '' then
        widths[#widths + 1] = vim.fn.strdisplaywidth(line)
      end
    end
  end
  table.sort(widths)
  -- Every unwrapped line gets room, else one may run on past the window's
  -- edge; a Markdown block wider than nine in ten only wraps.
  local width = widths[wrap and math.ceil(#widths * 0.9) or #widths] or 0
  seen[buf] = { key = key, width = width }
  return width
end
local function of(win)
  if terminal(win) then
    return whole
  end
  if vim.wo[win].winfixwidth then
    return api.nvim_win_get_width(win)
  end
  local buf = api.nvim_win_get_buf(win)
  if centred(win, buf) then
    return whole
  end
  if not smart then
    return most
  end
  local gutter = vim.fn.getwininfo(win)[1].textoff
  local tw = vim.bo[buf].textwidth
  local w = tw > 0 and tw + gutter or most
  return math.max(w, long(win, buf) + gutter)
end
local function measure()
  local each = {}
  for _, win in ipairs(api.nvim_tabpage_list_wins(0)) do
    if api.nvim_win_get_config(win).relative == '' then
      each[#each + 1] = { win, of(win), not vim.wo[win].wrap }
    end
  end
  return { vim.o.laststatus, each }
end
local last = measure()
if chan then
  local group = api.nvim_create_augroup('nvmux_centre_' .. chan, { clear = true })
  local queued = false
  local function check()
    queued = false
    local now = measure()
    if now[1] ~= last[1] or not vim.deep_equal(now[2], last[2]) then
      last = now
      if not pcall(vim.rpcnotify, chan, 'nvmux_client', 'measure', now[1], now[2]) then
        pcall(api.nvim_del_augroup_by_id, group)
      end
    end
  end
  local function soon()
    if not queued then
      queued = true
      vim.schedule(check)
    end
  end
  api.nvim_create_autocmd({
    'WinNew', 'WinClosed', 'WinResized', 'TabEnter', 'VimResized', 'BufWinEnter', 'TermOpen',
    'FileType', 'BufReadPost', 'BufWritePost',
  }, { group = group, callback = soon })
  api.nvim_create_autocmd('OptionSet', {
    group = group,
    pattern = {
      'winfixwidth', 'laststatus', 'textwidth', 'wrap', 'number', 'relativenumber',
      'numberwidth', 'signcolumn', 'foldcolumn', 'statuscolumn',
    },
    callback = soon,
  })
  api.nvim_create_autocmd('UILeave', {
    group = group,
    callback = function()
      if vim.v.event.chan == chan then
        pcall(api.nvim_del_augroup_by_id, group)
      end
    end,
  })
end
return last";

/// Lua removing [`MEASURE_LUA`] and [`TAILS_LUA`] for a channel, once
/// nothing is centred.
pub const UNMEASURE_LUA: &str = "local chan = ...
local tails = 'nvmux_tails_' .. chan
pcall(vim.api.nvim_del_augroup_by_name, 'nvmux_centre_' .. chan)
pcall(vim.api.nvim_del_augroup_by_name, tails)
pcall(vim.api.nvim_set_decoration_provider, vim.api.nvim_create_namespace(tails), {})";

/// Lua that, given the client's channel, reports the end-of-line virtual
/// text Neovim cuts at the edge of grids narrower than their window: per row
/// that overflows, the start column (from `screenpos()`; past the grid's
/// width if none fits) and the full text as single-highlight pieces, marks in
/// draw order separated by a blank. Sent as `nvmux_client` `tails` (window,
/// whether it conceals nothing, rows) when a redraw changes it, after
/// `colours` with new groups' attributes in the agent's format (all re-sent
/// on `ColorScheme`).
///
/// Sent from a decoration provider at the end of the redraw, so it arrives
/// before the `flush` the client applies it with. Lines in closed folds are
/// skipped; the tail starts after any inline virtual text at the line's end.
/// Removed on this UI's `UILeave` or once the channel is gone.
///
/// Never makes a window current (as `nvim_win_call` would for
/// `foldclosed()`): with `'autochdir'` that causes a redraw, and Neovim
/// withholds `win_viewport` (scroll amounts) while one is pending. Done every
/// redraw, scroll amounts would never arrive, breaking scroll animation and
/// the client's predicted scrolls.
pub const TAILS_LUA: &str = "local chan = ...
local api = vim.api
local name = 'nvmux_tails_' .. chan
local ns = api.nvim_create_namespace(name)
local group = api.nvim_create_augroup(name, { clear = true })
-- Windows drawn since the last redraw, the last report per window, and the
-- groups whose attributes were sent.
local drawn, said, sent = {}, {}, {}
local function stop()
  pcall(api.nvim_set_decoration_provider, ns, {})
  pcall(api.nvim_del_augroup_by_id, group)
end
local function send(...)
  if not pcall(vim.rpcnotify, chan, 'nvmux_client', ...) then
    stop()
  end
end
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
  return { gid, rgb, cterm }
end
-- Expand tabs as drawn from column col.
local function untab(text, col, ts)
  if not text:find('\\t', 1, true) then
    return text
  end
  local out = {}
  for piece, tab in text:gmatch('([^\\t]*)(\\t?)') do
    out[#out + 1] = piece
    col = col + vim.fn.strdisplaywidth(piece)
    if tab ~= '' then
      out[#out + 1] = string.rep(' ', ts - col % ts)
      col = col + ts - col % ts
    end
  end
  return table.concat(out)
end
-- { row, start, pieces } for each row whose end-of-line text runs past a
-- narrowed grid's edge. Lines that fit even unconcealed are skipped:
-- conceal only makes them narrower.
local function tails(win, fresh)
  local width = vim.fn.winwidth(win)
  if api.nvim_win_get_config(win).relative ~= '' or width >= api.nvim_win_get_width(win) then
    return {}
  end
  local buf = api.nvim_win_get_buf(win)
  local lines = {}
  local marks = api.nvim_buf_get_extmarks(buf, -1, { vim.fn.line('w0', win) - 1, 0 },
    { vim.fn.line('w$', win) - 1, -1 }, { details = true, type = 'virt_text' })
  for i, m in ipairs(marks) do
    local d = m[4]
    if not d.invalid and (d.virt_text_pos == 'eol' or d.virt_text_pos == 'inline') then
      local l = lines[m[2]] or { ends = {}, inline = {} }
      lines[m[2]] = l
      if d.virt_text_pos == 'eol' then
        l.ends[#l.ends + 1] = { d.priority or 4096, m[3], i, d.virt_text }
      else
        l.inline[#l.inline + 1] = { m[3], d.virt_text }
      end
    end
  end
  if next(lines) == nil then
    return {}
  end
  local info = vim.fn.getwininfo(win)[1]
  local left = vim.wo[win].wrap and 0 or info.leftcol
  local at = api.nvim_win_get_position(win)
  local ts = vim.bo[buf].tabstop
  local count = api.nvim_buf_line_count(buf)
  -- Display width of chunks, tabs expanded.
  local function wide(chunks)
    local n = 0
    for _, c in ipairs(chunks) do
      n = n + vim.fn.strdisplaywidth(untab(c[1], 0, ts))
    end
    return n
  end
  -- Screen rows lines a..b take; a closed fold takes one.
  local function rows(a, b)
    return api.nvim_win_text_height(win, { start_row = a, end_row = b }).all
  end
  -- Whether line r (its end at screenpos() p) is in a closed fold, which is
  -- drawn without tails. In a fold every column maps to the fold's start,
  -- even past the end; an empty line is folded if it and a neighbour take
  -- one row together but two apart.
  local function folded(r, line, p)
    if p.endcol ~= info.wincol + info.textoff then
      return false
    elseif line ~= '' then
      local q = vim.fn.screenpos(win, r + 1, #line + 1)
      return q.row == p.row and q.col == p.col
    end
    return r > 0 and rows(r - 1, r) < rows(r - 1, r - 1) + rows(r, r)
      or r + 1 < count and rows(r, r + 1) < rows(r, r) + rows(r + 1, r + 1)
  end
  local out = {}
  for r, l in pairs(lines) do
    local line = #l.ends > 0 and api.nvim_buf_get_lines(buf, r, r + 1, false)[1]
    -- Width of inline text at the line's end (the tail follows it), and the
    -- widest the line could be, tail included.
    local inline, most = 0, 0
    if line then
      for _, i in ipairs(l.inline) do
        if i[1] >= #line then
          inline = inline + wide(i[2])
        end
      end
      most = info.textoff + vim.fn.strdisplaywidth(untab(line, 0, ts)) + inline - left + #l.ends
      for _, e in ipairs(l.ends) do
        most = most + wide(e[4])
      end
    end
    if line and most > width then
      -- The line's end is the cell after its last character as screenpos()
      -- sees it, even past the window's edge; the tail starts one cell on,
      -- after any inline text.
      local p = vim.fn.screenpos(win, r + 1, math.max(#line, 1))
      local col = (line == '' and p.col - at[2] or p.endcol - at[2] + 1) + inline
      if p.row > 0 and not folded(r, line, p) then
        table.sort(l.ends, function(x, y)
          if x[1] ~= y[1] then
            return x[1] < y[1]
          elseif x[2] ~= y[2] then
            return x[2] < y[2]
          end
          return x[3] < y[3]
        end)
        local start, pieces = col, {}
        for k, e in ipairs(l.ends) do
          if k > 1 then
            pieces[#pieces + 1] = { ' ', 0 }
            col = col + 1
          end
          for _, c in ipairs(e[4]) do
            local hl = c[2]
            if type(hl) == 'table' then
              hl = hl[#hl]
            end
            local gid = hl and vim.fn.hlID(hl) or 0
            if gid > 0 and not sent[gid] then
              sent[gid] = true
              fresh[#fresh + 1] = attrs(gid)
            end
            local text = untab(c[1], col, ts)
            pieces[#pieces + 1] = { text, gid }
            col = col + vim.fn.strdisplaywidth(text)
          end
        end
        if col > width then
          out[#out + 1] = { p.row - 1 - at[1], start, pieces }
        end
      end
    end
  end
  return out
end
local function report()
  local wins = drawn
  drawn = {}
  local fresh, told = {}, {}
  for win in pairs(wins) do
    if api.nvim_win_is_valid(win) then
      local ok, rows = pcall(tails, win, fresh)
      local now = { vim.wo[win].conceallevel == 0, ok and rows or {} }
      if not vim.deep_equal(now, said[win] or { true, {} }) then
        said[win] = now
        told[#told + 1] = { win, now[1], now[2] }
      end
    else
      said[win] = nil
    end
  end
  if #fresh > 0 then
    send('colours', fresh)
  end
  for _, t in ipairs(told) do
    send('tails', t[1], t[2], t[3])
  end
end
api.nvim_set_decoration_provider(ns, {
  on_win = function(_, win)
    drawn[win] = true
  end,
  on_end = function()
    if next(drawn) ~= nil then
      pcall(report)
    end
  end,
})
api.nvim_create_autocmd('WinClosed', {
  group = group,
  callback = function(ev)
    said[tonumber(ev.match)] = nil
  end,
})
api.nvim_create_autocmd('ColorScheme', {
  group = group,
  callback = function()
    local again = {}
    for gid in pairs(sent) do
      again[#again + 1] = attrs(gid)
    end
    if #again > 0 then
      send('colours', again)
    end
  end,
})
api.nvim_create_autocmd('UILeave', {
  group = group,
  callback = function()
    if vim.v.event.chan == chan then
      stop()
    end
  end,
})
for _, win in ipairs(api.nvim_tabpage_list_wins(0)) do
  drawn[win] = true
end
pcall(report)";

/// Left offset centring `text` columns in a `slot`-wide window; odd slack
/// goes to the right.
pub fn inset(slot: usize, text: usize) -> usize {
    slot.saturating_sub(text) / 2
}

/// What [`MEASURE_LUA`] says a window wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Want {
    pub width: usize,
    /// `'nowrap'`: its lines run on past its text ([`run_on`]).
    pub nowrap: bool,
}

/// Width of a grid whose `text` columns are centred in a `slot`-wide window
/// and whose lines run on past them to the window's right edge.
pub fn run_on(slot: usize, text: usize) -> usize {
    slot - inset(slot, text)
}

/// Where a row of a grid running on past `text` columns stops being drawn:
/// after the last cell past them for which `ink` holds (not blank). Blanks
/// past the text are left to the margin, so a highlighted line
/// (`'cursorline'`) spans the text as on a grid that wide.
pub fn ink_end(text: usize, width: usize, ink: impl Fn(usize) -> bool) -> usize {
    (text..width)
        .rev()
        .find(|&c| ink(c))
        .map_or(text, |c| c + 1)
}

/// Move a full-width status line's words over the text (`text` columns,
/// `inset` in): its widest single-colour run of blanks (the `%=` gap) shrinks
/// by the window's extra width, which is added on either side as blanks in
/// the gap's colour. Unchanged if the gap is too narrow.
pub fn realign(row: &mut [Out], inset: usize, text: usize) {
    realign_by(row, inset, text, |o| {
        (o.text == Text::Char(' ') && !o.wide).then_some(o.style)
    });
}

/// [`realign`] for any cell type; `blank` returns a blank cell's colour.
pub fn realign_by<T: Clone, C: PartialEq>(
    row: &mut [T],
    inset: usize,
    text: usize,
    blank: impl Fn(&T) -> Option<C>,
) {
    let w = row.len();
    let extra = w.saturating_sub(text);
    if extra == 0 || inset > extra {
        return;
    }
    let (mut at, mut longest) = (0, 0);
    let mut i = 0;
    while i < w {
        let Some(colour) = blank(&row[i]) else {
            i += 1;
            continue;
        };
        let mut j = i;
        while j < w && blank(&row[j]).as_ref() == Some(&colour) {
            j += 1;
        }
        if j - i > longest {
            (at, longest) = (i, j - i);
        }
        i = j;
    }
    if longest < extra {
        return;
    }
    let fill = row[at].clone();
    let words: Vec<T> = row[..at]
        .iter()
        .chain(&row[at + extra..])
        .cloned()
        .collect();
    for (k, o) in row.iter_mut().enumerate() {
        *o = if k < inset || k >= inset + text {
            fill.clone()
        } else {
            words[k - inset].clone()
        };
    }
}

/// A grid row's full end-of-line virtual text, as [`TAILS_LUA`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tail {
    /// Column of the first cell, per the editor; past the grid's width if
    /// none of it fits.
    pub start: usize,
    pub cells: Vec<Cell>,
    /// Whether `start` can be trusted when the row shows only blanks of the
    /// tail. False in a window that conceals text, which moves the line's end
    /// left of where the editor says.
    pub sure: bool,
}

/// Parse a `tails` notification's rows, keyed by row; `sure` is per window.
pub fn tails(rows: Option<&ValueRef<'_>>, sure: bool) -> BTreeMap<usize, Tail> {
    let rows = rows
        .and_then(ValueRef::as_array)
        .map_or(&[][..], Vec::as_slice);
    rows.iter()
        .filter_map(|row| {
            let row = row.as_array()?;
            let at = |i: usize| usize::try_from(redraw::int(row.get(i)?)?).ok();
            let pieces = row.get(2)?.as_array()?.iter().filter_map(|piece| {
                let piece = piece.as_array()?;
                let group = redraw::int(piece.get(1)?).and_then(|g| u32::try_from(g).ok());
                Some((redraw::as_str(piece.first()?)?, group.unwrap_or(0)))
            });
            let tail = Tail {
                start: at(1)?,
                cells: cells(pieces),
                sure,
            };
            Some((at(0)?, tail))
        })
        .collect()
}

/// Lay out `(text, group)` pieces as Neovim draws them: each group over the
/// default highlight ([`syntax_hl`]), wide characters as a cell plus a
/// `Half`, combining marks joined to the previous cell, control characters
/// as `^X` or `<xx>`.
fn cells<'a>(pieces: impl Iterator<Item = (&'a str, u32)>) -> Vec<Cell> {
    let mut out: Vec<Cell> = Vec::new();
    for (text, group) in pieces {
        let hl = syntax_hl(0, group);
        for c in text.chars() {
            match c {
                '\0'..='\x1f' | '\x7f' => {
                    out.push(Cell {
                        text: Text::Char('^'),
                        hl,
                    });
                    out.push(Cell {
                        text: Text::Char((c as u8 ^ 0x40) as char),
                        hl,
                    });
                }
                _ => match c.width() {
                    Some(0) => {
                        if let Some(cell) = out.iter_mut().rev().find(|c| c.text != Text::Half) {
                            let mut b = Vec::new();
                            cell.text.push_to(&mut b);
                            let mut s = String::from_utf8(b).unwrap_or_default();
                            s.push(c);
                            cell.text = Text::Cluster(s.into());
                        }
                    }
                    Some(w) => {
                        out.push(Cell {
                            text: Text::Char(c),
                            hl,
                        });
                        for _ in 1..w {
                            out.push(Cell {
                                text: Text::Half,
                                hl,
                            });
                        }
                    }
                    None => out.extend(format!("<{:02x}>", c as u32).chars().map(|ch| Cell {
                        text: Text::Char(ch),
                        hl,
                    })),
                },
            }
        }
    }
    out
}

/// The column `tail` starts at on a drawn `row`: `tail.start` if the row
/// matches there with non-blank text (or only blanks, or nothing, when
/// `sure`); else the rightmost column after a blank where the row matches
/// with non-blank text, as when concealed text shifted it. `None` if neither.
pub fn place(row: &[Cell], tail: &Tail) -> Option<usize> {
    let (w, v) = (row.len(), &tail.cells);
    // Whether the row matches the tail from `at` (as much as fits, except a
    // wide character in the last column, which Neovim has no room for), and
    // if so whether any matched cell is non-blank.
    let has = |at: usize| -> Option<bool> {
        let n = v.len().min(w.saturating_sub(at));
        let mut ink = false;
        for (i, c) in v[..n].iter().enumerate() {
            if at + i + 1 == w && v.get(i + 1).is_some_and(|n| n.text == Text::Half) {
                break;
            }
            if row[at + i].text != c.text {
                return None;
            }
            ink |= !c.text.is_blank();
        }
        Some(ink)
    };
    if v.is_empty() {
        return None;
    }
    if has(tail.start).is_some_and(|ink| ink || tail.sure) {
        return Some(tail.start);
    }
    (1..w)
        .rev()
        .find(|&at| row[at - 1].text.is_blank() && has(at) == Some(true))
}

/// The cells of `tail` cut off at the end of `row`, and the column the first
/// goes in (at least the row's end). A wide character cut at the last column
/// is included whole. `None` if nothing is cut or [`place`] fails.
pub fn past<'a>(row: &[Cell], tail: &'a Tail) -> Option<(usize, &'a [Cell])> {
    let start = place(row, tail)?;
    let w = row.len();
    let mut on = w.saturating_sub(start).min(tail.cells.len());
    if tail.cells.get(on).is_some_and(|c| c.text == Text::Half) {
        on = on.saturating_sub(1);
    }
    let rest = &tail.cells[on..];
    (!rest.is_empty()).then_some(((start + on).max(w), rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::style::{Color, Style};
    use crate::palette::Rgb;

    fn styled(bg: u8) -> Style {
        Style {
            bg: Color::Rgb(Rgb(bg, bg, bg)),
            ..Style::default()
        }
    }

    fn row(text: &str, style: Style) -> Vec<Out> {
        text.chars()
            .map(|c| Out {
                text: Text::Char(c),
                style,
                wide: false,
            })
            .collect()
    }

    fn text_of(row: &[Out]) -> String {
        let mut out = Vec::new();
        for o in row {
            o.text.push_to(&mut out);
        }
        String::from_utf8(out).expect("utf-8")
    }

    #[test]
    fn a_narrower_grid_goes_in_the_middle_of_its_window() {
        assert_eq!(inset(150, 120), 15);
        assert_eq!(inset(149, 120), 14, "odd slack to the right");
        assert_eq!(inset(120, 120), 0);
        assert_eq!(
            inset(100, 120),
            0,
            "a grid wider than its window is not inset"
        );
    }

    #[test]
    fn a_grid_running_on_reaches_the_windows_edge() {
        assert_eq!(run_on(160, 140), 150);
        assert_eq!(run_on(161, 140), 151, "odd slack to the right");
        assert_eq!(run_on(100, 120), 100, "text wider than the window");
    }

    #[test]
    fn a_row_running_on_ends_after_its_last_non_blank() {
        let end = |text: &str| {
            let row = drawn(text);
            ink_end(4, row.len(), |c| !row[c].text.is_blank())
        };
        assert_eq!(end("abcdef  gh  "), 10);
        assert_eq!(end("ab          "), 4, "nothing past the text");
        assert_eq!(end("abcd日  "), 6, "a wide character whole");
    }

    #[test]
    fn a_status_line_closes_up_over_the_text() {
        let s = styled(3);
        // 31 wide, its gap 14: closed up by 11, to the 20 of the text.
        let mut r = row(" main.rs              rust 1:1 ", s);
        realign(&mut r, 5, 20);
        assert_eq!(text_of(&r), "      main.rs   rust 1:1       ");
        assert!(r.iter().all(|o| o.style == s));
    }

    /// The gap is the widest run of blanks in one colour.
    #[test]
    fn a_status_line_with_too_narrow_a_gap_is_left_alone() {
        let s = styled(3);
        let mut r = row(" a long name  rust ", s);
        let before = text_of(&r);
        realign(&mut r, 3, 10);
        assert_eq!(text_of(&r), before);

        let (a, b) = (styled(1), styled(2));
        let mut r = row("x", a);
        r.extend(row("     ", b));
        r.extend(row("   y", a));
        realign(&mut r, 1, 6);
        assert_eq!(text_of(&r), " x    y   ");
        let styles: Vec<Style> = r[2..6].iter().map(|o| o.style).collect();
        assert_eq!(
            styles,
            [b, a, a, a],
            "the five in one colour closed up, not the three"
        );
    }

    fn drawn(text: &str) -> Vec<Cell> {
        cells(std::iter::once((text, 0)))
    }

    fn tail(start: usize, text: &str, sure: bool) -> Tail {
        Tail {
            start,
            cells: cells(std::iter::once((text, 7))),
            sure,
        }
    }

    fn said(cells: &[Cell]) -> String {
        let mut out = Vec::new();
        for c in cells {
            c.text.push_to(&mut out);
        }
        String::from_utf8(out).expect("utf-8")
    }

    fn past_of(row: &[Cell], tail: &Tail) -> Option<(usize, String)> {
        past(row, tail).map(|(at, cells)| (at, said(cells)))
    }

    #[test]
    fn what_neovim_cut_at_the_edge_runs_on_past_it() {
        let msg = "    ■ mismatched";
        let row = drawn("let x = 1;     ■ mis");
        assert_eq!(
            past_of(&row, &tail(11, msg, true)),
            Some((20, "matched".into()))
        );
        let row = drawn("let x = 1;     ■ mismatched    ");
        assert_eq!(past_of(&row, &tail(11, msg, true)), None, "all on it");
        let row = drawn("let x = 10;");
        assert_eq!(
            past_of(&row, &tail(12, msg, true)),
            Some((12, msg.into())),
            "past the line's end"
        );
    }

    #[test]
    fn a_tail_shown_only_in_blanks_is_placed_only_where_the_editor_is_sure() {
        let row = drawn("let x = 1;   ");
        assert_eq!(
            past_of(&row, &tail(11, "    ■ x", true)),
            Some((13, "  ■ x".into()))
        );
        assert_eq!(past_of(&row, &tail(11, "    ■ x", false)), None);
        let full = drawn("let x = 12;");
        assert_eq!(past_of(&full, &tail(12, "■ x", false)), None);
    }

    /// Concealed text puts the line's end left of where the editor says.
    #[test]
    fn a_tail_not_where_the_editor_says_is_found_where_the_row_has_it() {
        // `see [the link](https://…)` concealed down to `see the link`.
        let row = drawn("see the link     ■ broke");
        assert_eq!(
            past_of(&row, &tail(40, "    ■ broken link", false)),
            Some((24, "n link".into()))
        );
        assert_eq!(
            past_of(&row, &tail(40, "    ■ broken link", true)),
            Some((40, "    ■ broken link".into())),
            "sure, the row's blanks there say nothing against it"
        );
        let row = drawn("see the link     ■ fixed");
        assert_eq!(past_of(&row, &tail(13, "    ■ broken link", true)), None);
    }

    /// Neovim leaves the last column blank when a wide character does not fit.
    #[test]
    fn a_wide_character_the_edge_cut_runs_on_whole() {
        let row = drawn("abcdef ■  ");
        assert_eq!(
            past_of(&row, &tail(7, "■ 日本", true)),
            Some((10, "日本".into()))
        );
    }

    #[test]
    fn tails_are_read_as_the_editor_says_them() {
        use rmpv::Value;
        let piece = |text: &str, group: i64| Value::Array(vec![text.into(), group.into()]);
        let rows = Value::Array(vec![
            Value::Array(vec![
                3.into(),
                12.into(),
                Value::Array(vec![piece("  ", 0), piece("■ x", 41)]),
            ]),
            Value::Array(vec![
                5.into(),
                20.into(),
                Value::Array(vec![piece("日a\u{301}", 7), piece("\u{1}", 0)]),
            ]),
        ]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &rows).expect("encode");
        let v = rmpv::decode::read_value_ref(&mut &bytes[..]).expect("decode");
        let got = tails(Some(&v), false);
        assert_eq!(got.keys().copied().collect::<Vec<_>>(), [3, 5]);
        let three = &got[&3];
        assert_eq!(
            (three.start, three.sure, said(&three.cells)),
            (12, false, "  ■ x".into())
        );
        assert_eq!(
            three.cells.iter().map(|c| c.hl).collect::<Vec<_>>(),
            [0, 0, syntax_hl(0, 41), syntax_hl(0, 41), syntax_hl(0, 41)]
        );
        let texts: Vec<Text> = got[&5].cells.iter().map(|c| c.text.clone()).collect();
        assert_eq!(
            texts,
            [
                Text::Char('日'),
                Text::Half,
                Text::Cluster("a\u{301}".into()),
                Text::Char('^'),
                Text::Char('A'),
            ]
        );
    }
}
