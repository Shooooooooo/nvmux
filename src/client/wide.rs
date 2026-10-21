//! A terminal wider than a window's text needs: what `[client] centre` does
//! with it (see [`crate::config::ClientSettings::centre`]).
//!
//! Neovim keeps the whole terminal, and a window wider than it wants, as the
//! measure left in the editor says ([`MEASURE_LUA`]), is asked for a grid
//! that wide (`nvim_ui_try_resize_grid`, which only a UI with `ext_multigrid`
//! can ask): Neovim draws its text to that width — wraps its lines there,
//! ends its cursor line there — and the client puts it in the middle of the
//! window ([`inset`]), a float anchored to it with it, as far as keeps the
//! float on the screen (see [`super::model::Model::inset`]). The status line
//! under it is still the window's width, as Neovim draws it, and has its
//! words moved in to stand over the text ([`realign`]). A window showing a
//! terminal wants the whole terminal, so it is not asked: its grid is as
//! wide as Neovim tells the program in it its terminal is. A grid asked for
//! is the window's for every UI of the session: as the client goes, it asks
//! for each window's own width back, and waits a moment for Neovim to have
//! done it ([`super::run`]).
//!
//! The client cuts its own copy of a grid to the width it asks for as it
//! asks, as Neovim cuts its own once it has the ask: a window split off
//! another has its text in its middle from the first frame of its flying in,
//! not a round trip later — over a slow link, once it had landed against the
//! window's edge — and Neovim's grid of that width, when it comes, moves
//! nothing, but for where long lines wrap. Text that moves in or out of a
//! window that stays where it is — the window's width judged again, or the
//! window wanting the whole terminal — slides there rather than jumping (see
//! [`super::anim::Animator::insets`]).
//!
//! A client killed outright — by nvmux, still there two seconds after it was
//! hung up, or by a crash — asks for nothing back, and Neovim keeps a grid
//! as it was asked for until a UI asks for the window whole. So a client
//! with windows of their own looks at each window once an attach, the first
//! time it knows how wide both the window and its grid are, and asks for one
//! on a grid not its width whole, unless it narrows that window itself. Two
//! clients attached at once share the grids: the one to attach last has the
//! windows as it wants them, and the other draws them as they are.
//!
//! What Neovim draws after a line's end — the text of a mark at its end, a
//! language server's diagnostic or a blame — it cuts at the grid's edge,
//! which on a grid narrower than its window is where the window has room for
//! more of it: a line about as long as the text is wide had all of its
//! diagnostic cut. So the editor says, as it draws, what each row of such a
//! window has after its line's end, and where Neovim put that ([`TAILS_LUA`]);
//! and the client draws what Neovim cut on in the margin past the grid's
//! edge, as far as the window goes ([`past`]), and with its row as that
//! moves. Where it is on the row is checked against what Neovim drew there,
//! and where the window conceals text, which moves a line's end where the
//! editor cannot see, it is found there ([`place`]).

use std::collections::BTreeMap;

use rmpv::ValueRef;
use unicode_width::UnicodeWidthChar;

use super::compose::Out;
use super::grid::{Cell, Text};
use super::model::syntax_hl;
use super::redraw;

/// What each window of the tab page in front wants, given how wide a
/// window's text is (the first argument) and whether to judge each by what
/// it shows (the second: `[client] centre = "smart"`, see
/// [`crate::config::ClientSettings::centre`]).
///
/// A window showing a terminal wants the whole terminal, more than any is
/// wide — the program in it is told its window's width. So does one whose
/// text is in its middle already, as a start screen's is: every line set in
/// from the left by about as much as the longest stops short of the
/// window's right edge — to within a couple of columns, and the gutter,
/// which the screen may or may not have left out. Such a screen laid itself
/// out for the window's whole width, which `nvim_win_get_width` gives it
/// whatever the grid; on a narrower grid it would be centred twice over,
/// and so off to the right. A window that keeps its width wants that. Any
/// other wants the width given — or, judged by what it shows, its buffer's
/// `'textwidth'` where it has one, and room for its long lines where they
/// are not wrapped: all of them with `'wrap'` off, and with it on those of a
/// Markdown table or code block, where tree-sitter has a parser to say which
/// those are. Enough room for nine in ten of them, the first few thousand
/// saying as much as the rest; and the gutter as well — the line numbers,
/// the signs and the folds.
///
/// Answered as `{ 'laststatus', windows }`, `windows` each window as
/// `{ window, wanted }`: `'laststatus'` says which windows have a status line
/// of their own, to stand over their text (see [`realign`]).
///
/// Given the client's channel as well, it stays in the editor and says it
/// again, as an `nvmux_client` notification (`measure`, and the same two),
/// whenever what the windows want or `'laststatus'` changes: a window made,
/// closed or resized, a buffer shown in a window, read, written or given a
/// filetype, a terminal opened, a tab page entered, or an option set that a
/// window's width or what it wants turns on. Not as its text is typed,
/// which would have it widen and narrow under the cursor. It goes when this
/// client's UI does, or the first time it finds the channel gone.
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
  local width = widths[math.ceil(#widths * 0.9)] or 0
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
      each[#each + 1] = { win, of(win) }
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

/// What takes [`MEASURE_LUA`] and [`TAILS_LUA`] back out of the editor,
/// given the client's channel: once nothing is centred, nothing is to be
/// measured, and no grid is narrower than its window.
pub const UNMEASURE_LUA: &str = "local chan = ...
local tails = 'nvmux_tails_' .. chan
pcall(vim.api.nvim_del_augroup_by_name, 'nvmux_centre_' .. chan)
pcall(vim.api.nvim_del_augroup_by_name, tails)
pcall(vim.api.nvim_set_decoration_provider, vim.api.nvim_create_namespace(tails), {})";

/// What tells the client, given its channel, what Neovim draws after the
/// end of the lines of each window whose grid is narrower than the window —
/// the text of the marks at their ends, which Neovim cuts at the grid's
/// edge: for each row with more of it than the grid has room for, the row,
/// the column the text starts in, as `screenpos()` has the line's end — past
/// the grid's width where none of it fits — and all of it, a list of pieces
/// of one highlight group each, the marks' texts in the order Neovim draws
/// them with a blank between each two. Said as an `nvmux_client`
/// notification (`tails`, the window, whether the window conceals no text,
/// the rows), each time a redraw of the window changes it; before it, the
/// attributes of each group the client has not been sent (`colours`, as the
/// agent sends them), and all of them again with a new colour scheme.
///
/// Said at the end of the redraw, from a decoration provider: ahead of the
/// `flush` that ends what Neovim sends of it, which the client takes it
/// with. Not the text of a line in a closed fold, which Neovim does not draw;
/// and of a line with text put in at its end (inline), from after that. It
/// goes when this client's UI does, or the first time it finds the channel
/// gone.
pub const TAILS_LUA: &str = "local chan = ...
local api = vim.api
local name = 'nvmux_tails_' .. chan
local ns = api.nvim_create_namespace(name)
local group = api.nvim_create_augroup(name, { clear = true })
-- The windows drawn since the last redraw ended, what was last said of
-- each, and the groups the client has the attributes of.
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
-- Text as Neovim draws it from column col: each tab out to the next stop.
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
-- Each row of a window whose text after its line's end runs past its grid's
-- edge, as { row, start, pieces }: the window's grid narrower than it. Where
-- a line could be no wider than the grid, text and all, it is left at that:
-- concealed text would only make it narrower.
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
  local function wide(chunks)
    local n = 0
    for _, c in ipairs(chunks) do
      n = n + vim.fn.strdisplaywidth(c[1])
    end
    return n
  end
  local info = vim.fn.getwininfo(win)[1]
  local left = vim.wo[win].wrap and 0 or info.leftcol
  local at = api.nvim_win_get_position(win)
  local ts = vim.bo[buf].tabstop
  local out = {}
  -- Whether line lnum, at p as screenpos() has it, is in a closed fold, as
  -- foldclosed() would say only of the current window. nvim_win_call() is
  -- not to be had here: with 'autochdir' set it marks every status line and
  -- winbar to be drawn again, and Neovim holds back win_viewport -- how far
  -- a window scrolled, which a scroll slides by -- while a window has
  -- anything to draw, so a call at the end of every redraw holds it back
  -- for good. screenpos() puts every line of a closed fold in the fold's
  -- row and the first column of its text, as it does a line of no more
  -- than one cell; only a fold's lines share a row with the line before or
  -- after them.
  local count = api.nvim_buf_line_count(buf)
  local function folded(lnum, p)
    if p.endcol ~= p.col or p.col - at[2] - 1 ~= info.textoff then
      return false
    end
    for _, n in ipairs({ lnum - 1, lnum + 1 }) do
      if n >= 1 and n <= count and vim.fn.screenpos(win, n, 1).row == p.row then
        return true
      end
    end
    return false
  end
  for r, l in pairs(lines) do
    local line = #l.ends > 0 and api.nvim_buf_get_lines(buf, r, r + 1, false)[1]
    -- What is put in at the line's end, which the text after it follows;
    -- and as wide as the line could be, text and all.
    local inline, most = 0, 0
    if line then
      for _, i in ipairs(l.inline) do
        if i[1] >= #line then
          inline = inline + wide(i[2])
        end
      end
      most = info.textoff + vim.fn.virtcol({ r + 1, '$' }, false, win) - 1 + inline - left + #l.ends
      for _, e in ipairs(l.ends) do
        most = most + wide(e[4])
      end
    end
    -- The line's end is the cell after its last character, as far as
    -- screenpos() sees, even past the window's edge where the line fills
    -- it; the text after it starts a cell on, past any put in there.
    local p = line and most > width and vim.fn.screenpos(win, r + 1, math.max(#line, 1))
    if p and p.row > 0 and not folded(r + 1, p) then
      local col = (line == '' and p.col - at[2] or p.endcol - at[2] + 1) + inline
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

/// How far into a window `slot` columns wide its grid `grid` columns wide is
/// drawn: in its middle, odd slack to the right; nowhere in for a grid as
/// wide as its window, or wider.
pub fn inset(slot: usize, grid: usize) -> usize {
    slot.saturating_sub(grid) / 2
}

/// A window's status line, `row` as Neovim drew it the window's width, made
/// to stand over its text, `text` columns of it `inset` columns in: its
/// widest run of blanks in one colour — the gap `%=` leaves between what it
/// says on the left and what on the right — closed up by as many columns as
/// the text is narrower than the window, which go either side of it, blank
/// in the gap's colour. Left as it is where the gap is not that wide.
pub fn realign(row: &mut [Out], inset: usize, text: usize) {
    realign_by(row, inset, text, |o| {
        (o.text == Text::Char(' ') && !o.wide).then_some(o.style)
    });
}

/// [`realign`], of a row of cells of any kind: `blank` says whether a cell
/// is a blank, and if so in what colour.
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

/// What Neovim draws after the end of the line a row of a window grid
/// shows — the text of the marks at its end — all of it, as the editor says
/// it (see [`TAILS_LUA`]): the grid has room for only so much of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tail {
    /// The column its first cell goes in, as the editor works it out: past
    /// the grid's width where none of it fits.
    pub start: usize,
    pub cells: Vec<Cell>,
    /// Whether `start` holds even where the row has only blanks of the tail
    /// to say so: not in a window that conceals text, which the editor does
    /// not see, and which puts the line's end left of where it says.
    pub sure: bool,
}

/// What a `tails` notification says of a window's rows (see [`TAILS_LUA`]),
/// `sure` as it says it of the window: each row's tail, by row.
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

/// Pieces of text, each in a highlight group of the editor's, as the cells
/// Neovim draws them in: each group laid over the default highlight, as the
/// margin they are drawn in has it (see [`syntax_hl`]); a character two
/// cells wide a cell and the half after it, a combining mark in with the
/// character before it, and a control character as Neovim shows one.
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

/// The column `tail` starts in on `row`, a row of a grid as Neovim drew it:
/// where the editor says, if the row has the tail's start there — or, sure
/// of it, if nothing but blanks of the tail are on the row to say; else the
/// rightmost column after a blank from which the row has more than blanks of
/// the tail's start, as it has where the window conceals text before it.
/// Past the row's end where none of it is on the row. `None` where neither
/// is: the row does not have the tail anywhere it can be told.
pub fn place(row: &[Cell], tail: &Tail) -> Option<usize> {
    let (w, v) = (row.len(), &tail.cells);
    // Whether the row has the tail's start from column `at`, and whether
    // more than blanks of it: all of it that fits, but a character two cells
    // wide in the row's last column, which Neovim has no room for there.
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

/// What of `tail` runs past the end of `row`, a row of a grid as Neovim
/// drew it (see [`place`]): the cells the row has no room for, and the
/// column the first goes in — past the row's end, and a character two cells
/// wide the row's last column cut drawn whole after it. Nothing where the
/// row has all of it, or does not have it where it can be told.
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

    /// The words of a status line close up at its gap to stand over the
    /// text, the columns either side blank in the gap's colour.
    #[test]
    fn a_status_line_closes_up_over_the_text() {
        let s = styled(3);
        // 31 wide, its gap 14: closed up by 11, to the 20 of the text.
        let mut r = row(" main.rs              rust 1:1 ", s);
        realign(&mut r, 5, 20);
        assert_eq!(text_of(&r), "      main.rs   rust 1:1       ");
        assert!(r.iter().all(|o| o.style == s));
    }

    /// Where its gap is too narrow to close up by that much, it is left as
    /// Neovim drew it; and the gap is the widest run of blanks in one colour.
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

    /// A row of a grid as Neovim drew it, a cell a character; or a tail's
    /// cells, in highlight group 7.
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

    /// What Neovim cut at the grid's edge runs on past it, from the first
    /// column past the edge; a tail all on the row has nothing past it; and
    /// one that starts past the row's end — the line filling the row — all
    /// of it, from where it starts.
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

    /// Where the row has only blanks of the tail, nothing says where it
    /// starts but the editor: taken as it says where it is sure, and not
    /// where the window conceals text.
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

    /// Text concealed before a line's end puts the end left of where the
    /// editor says: the tail is found where the row has it, more than blanks
    /// of it after a blank, and what runs past the edge runs on from there.
    /// A row with none of it anywhere has nothing past it.
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

    /// A character two cells wide that the row's last column had no room
    /// for runs on whole past the edge, where Neovim left a blank.
    #[test]
    fn a_wide_character_the_edge_cut_runs_on_whole() {
        let row = drawn("abcdef ■  ");
        assert_eq!(
            past_of(&row, &tail(7, "■ 日本", true)),
            Some((10, "日本".into()))
        );
    }

    /// The rows the editor says of, each as its tail's start and cells: each
    /// piece in its group laid over the default highlight, a character two
    /// cells wide a cell and a half, a combining mark in with the character
    /// before it, a control character as Neovim shows one.
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
