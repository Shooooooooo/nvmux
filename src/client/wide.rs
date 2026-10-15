//! A terminal wider than the editor's windows need: what `[client] centre`
//! does with it (see [`crate::config::Centre`]).
//!
//! # The editor centred
//!
//! Neovim is told the terminal is only as wide as the editor's windows side
//! by side want — each as wide as what it shows wants, or `window_width`
//! (see [`MEASURE_LUA`]), one that keeps its width (`'winfixwidth'`, as a
//! file tree does) as wide as it keeps, and a column between each two —
//! which the measure left in the editor works out from its layout, and says
//! again whenever that changes. The
//! frame its grids are composed into is that wide, and goes on the terminal
//! in its middle ([`place`]): the margins either side are blank in the
//! editor's own background, but for the rows the editor draws a bar across —
//! its status lines, the tab line — which run on through them to the
//! terminal's edges, in the colours they end in ([`run_on`]). Everything the
//! client draws over the editor — the cursor, the cards — is laid against the
//! editor where it now stands, and what the mouse says is taken back to it.
//!
//! A change of layout that changes what the windows want comes a moment after
//! the change itself: Neovim splits the window it has, at the width it has,
//! and the editor widens to give each its share once the measure says so. And
//! Neovim gives a wider screen to the windows on its right, so the client has
//! it give each window the width it wants again ([`SIZE_LUA`]), where
//! `'equalalways'` says it is to lay them out — as it does unless turned off.
//!
//! A window showing a terminal wants the whole terminal: Neovim tells the
//! program in it its window's width, and a shell or a program drawing a
//! screen of its own is given all of it. So with one on show the editor is
//! the whole terminal — that window all of it the others do not want — and
//! centred again once there is none.
//!
//! # Each window's text centred
//!
//! Neovim keeps the whole terminal, and a window wider than it wants, as the
//! measure says, is asked for a grid that wide (`nvim_ui_try_resize_grid`,
//! which only a UI with `ext_multigrid` can ask): Neovim draws its text to that width — wraps
//! its lines there, ends its cursor line there — and the client puts it in
//! the middle of the window ([`inset`]), a float anchored to it with it. The
//! status line under it is still the window's width, as Neovim draws it, and
//! has its words moved in to stand over the text ([`realign`]). A window
//! showing a terminal wants the whole terminal, so it is not asked: its grid
//! is as wide as Neovim tells the program in it its terminal is. A grid asked
//! for is the window's for every UI of the session: as the client goes, it
//! asks for each window's own width back, and waits a moment for Neovim to
//! have done it ([`super::run`]).
//!
//! A client killed outright — by nvmux, still there two seconds after it was
//! hung up, or by a crash — asks for nothing back, and Neovim keeps a grid
//! as it was asked for until a UI asks for the window whole. So a client
//! with windows of their own looks at each window once an attach, the first
//! time it knows how wide both the window and its grid are, and asks for one
//! on a grid not its width whole, unless it narrows that window itself. Two
//! clients attached at once share the grids: the one to attach last has the
//! windows as it wants them, and the other draws them as they are.

use super::compose::{Frame, Out};
use super::grid::Text;
use super::style::Style;

/// What each window of the tab page in front wants, and what they all want
/// side by side, given how wide a window's text is (the first argument) and
/// whether to judge each by what it shows (the second: see
/// [`crate::config::ClientSettings::smart_width`]).
///
/// A window showing a terminal wants the whole terminal, more than any is
/// wide — the program in it is told its window's width; a window that keeps
/// its width wants that. Any other wants the width given — or, judged by
/// what it shows, its buffer's `'textwidth'` where it has one, and room for
/// its long lines where they are not wrapped: all of them with `'wrap'` off,
/// and with it on those of a Markdown table or code block, where tree-sitter
/// has a parser to say which those are. Enough room for nine in ten of them,
/// the first few thousand saying as much as the rest; and the gutter as well
/// — the line numbers, the signs and the folds. Side by side, a row of
/// windows wants the sum with a column between each two, and a column of
/// them the most any one of them wants.
///
/// Answered as `{ wanted, 'laststatus', 'columns', windows }`: `'columns'`
/// so that an attach at another width knows the windows were laid out for
/// that one, and `windows` each window as `{ window, wanted }`, so that the
/// text of each can be given what it wants (see [`SIZE_LUA`]).
///
/// Given the client's channel as well, it stays in the editor and says it
/// again, as an `nvmux_client` notification (`measure`, and the same four),
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
local function terminal(win)
  return vim.bo[api.nvim_win_get_buf(win)].buftype == 'terminal'
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
  if not smart then
    return most
  end
  local buf = api.nvim_win_get_buf(win)
  local gutter = vim.fn.getwininfo(win)[1].textoff
  local tw = vim.bo[buf].textwidth
  local w = tw > 0 and tw + gutter or most
  return math.max(w, long(win, buf) + gutter)
end
local function measure()
  local each, by = {}, {}
  for _, win in ipairs(api.nvim_tabpage_list_wins(0)) do
    if api.nvim_win_get_config(win).relative == '' then
      by[win] = of(win)
      each[#each + 1] = { win, by[win] }
    end
  end
  local function want(node)
    local kind, v = node[1], node[2]
    if kind == 'leaf' then
      return by[v] or most
    end
    local n = 0
    for i, k in ipairs(v) do
      local w = want(k)
      if kind == 'row' then
        n = n + w + (i > 1 and 1 or 0)
      else
        n = math.max(n, w)
      end
    end
    return n
  end
  return { want(vim.fn.winlayout()), vim.o.laststatus, vim.o.columns, each }
end
local last = measure()
if chan then
  local group = api.nvim_create_augroup('nvmux_centre_' .. chan, { clear = true })
  local queued = false
  local function check()
    queued = false
    local now = measure()
    if now[1] ~= last[1] or now[2] ~= last[2] or not vim.deep_equal(now[4], last[4]) then
      last = now
      if not pcall(vim.rpcnotify, chan, 'nvmux_client', 'measure', now[1], now[2], now[3], now[4]) then
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

/// What Neovim is asked to do once the editor has been made wider or
/// narrower for its windows, where `'equalalways'` says it is to lay them
/// out: give each the width it wants, as the measure last said (the first
/// argument, `{ window, wanted }` each: see [`MEASURE_LUA`]). Along a row,
/// those that want a width are given it, and those that want the whole
/// terminal share what is left; a column of windows is as wide as the row
/// gives it. Where the row has less room than they want — the terminal
/// narrower than the editor would be — they are made the same width, as
/// Neovim makes them on a split.
pub const SIZE_LUA: &str = "local each = ...
if not vim.o.equalalways then
  return
end
local api = vim.api
local whole = 1073741824
local by = {}
for _, p in ipairs(each) do
  by[p[1]] = p[2]
end
local function want(node)
  local kind, v = node[1], node[2]
  if kind == 'leaf' then
    return by[v] or whole
  end
  local n = 0
  for i, k in ipairs(v) do
    local w = want(k)
    if kind == 'row' then
      n = n + w + (i > 1 and 1 or 0)
    else
      n = math.max(n, w)
    end
  end
  return n
end
local function leaf(node)
  while node[1] ~= 'leaf' do
    node = node[2][1]
  end
  return node[2]
end
local function size(node, width)
  local kind, v = node[1], node[2]
  if kind == 'leaf' then
    return true
  end
  if kind == 'col' then
    for _, k in ipairs(v) do
      if not size(k, width) then
        return false
      end
    end
    return true
  end
  local widths, fixed, open = {}, 0, {}
  for i, k in ipairs(v) do
    local w = want(k)
    if w >= whole then
      open[#open + 1] = i
    else
      widths[i] = w
      fixed = fixed + w
    end
  end
  local room = width - (#v - 1) - fixed
  if room < #open then
    return false
  end
  if #open > 0 then
    for j, i in ipairs(open) do
      widths[i] = math.floor(room / #open) + (j <= room % #open and 1 or 0)
    end
  else
    widths[#v] = widths[#v] + room
  end
  for i, k in ipairs(v) do
    if i < #v then
      pcall(api.nvim_win_set_width, leaf(k), widths[i])
    end
  end
  for i, k in ipairs(v) do
    if not size(k, widths[i]) then
      return false
    end
  end
  return true
end
if not size(vim.fn.winlayout(), vim.o.columns) then
  vim.cmd('horizontal wincmd =')
end";

/// Where the editor goes on a terminal `cols` wide, its windows wanting
/// `wanted` columns: its first column and how wide it is. The whole terminal
/// where they want as much or more, or where it is not known what they want.
pub fn editor(cols: usize, wanted: Option<usize>) -> (usize, usize) {
    match wanted {
        Some(w) if w > 0 && w < cols => ((cols - w) / 2, w),
        _ => (0, cols),
    }
}

/// `frame`, the editor as composed, on a frame `cols` wide with its first
/// column at `left`: the margins blank in `blank`, the editor's own
/// background. Odd slack falls to the right, as the hint bar's does.
pub fn place(frame: Frame, cols: usize, left: usize, blank: Style) -> Frame {
    if cols == frame.width && left == 0 {
        return frame;
    }
    let mut out = Frame::new(cols, frame.height, blank);
    for r in 0..frame.height {
        for c in 0..frame.width {
            let Some(at) = out.get_mut(r, left + c) else {
                break;
            };
            *at = frame.cells[r * frame.width + c].clone();
        }
    }
    // A wide character the terminal's edge cut is not one any more.
    out.mend();
    out
}

/// Run row `row` of `frame` on from the editor's columns `left..left +
/// width` to the terminal's edges: the margin on the `left` side blank in
/// the colours of the editor's first cell on it, the other in its last's —
/// a status line or the tab line going all the way across.
pub fn run_on(frame: &mut Frame, row: usize, left: usize, width: usize, sides: (bool, bool)) {
    if width == 0 {
        return;
    }
    let end = left + width;
    let style = |frame: &Frame, c: usize| frame.get(row, c).map(|o| o.style);
    if let (true, Some(s)) = (sides.0, style(frame, left)) {
        for c in 0..left {
            if let Some(o) = frame.get_mut(row, c) {
                *o = Out::blank(s);
            }
        }
    }
    if let (true, Some(s)) = (sides.1, style(frame, end - 1)) {
        for c in end..frame.width {
            if let Some(o) = frame.get_mut(row, c) {
                *o = Out::blank(s);
            }
        }
    }
}

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
    let w = row.len();
    let extra = w.saturating_sub(text);
    if extra == 0 || inset > extra {
        return;
    }
    let blank = |o: &Out| o.text == Text::Char(' ') && !o.wide;
    let (mut at, mut longest) = (0, 0);
    let mut i = 0;
    while i < w {
        if !blank(&row[i]) {
            i += 1;
            continue;
        }
        let style = row[i].style;
        let mut j = i;
        while j < w && blank(&row[j]) && row[j].style == style {
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
    let fill = Out::blank(row[at].style);
    let words: Vec<Out> = row[..at]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::style::Color;
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

    /// The editor is as wide as its windows want and in the middle, odd
    /// slack to the right; the whole terminal where they want that much or
    /// more, or nobody has said.
    #[test]
    fn the_editor_is_as_wide_as_its_windows_want_and_in_the_middle() {
        assert_eq!(editor(300, Some(120)), (90, 120));
        assert_eq!(editor(300, Some(241)), (29, 241));
        assert_eq!(editor(300, Some(300)), (0, 300));
        assert_eq!(editor(300, Some(483)), (0, 300));
        assert_eq!(editor(300, None), (0, 300));
        assert_eq!(
            editor(300, Some(0)),
            (0, 300),
            "nothing wanted is no answer"
        );
    }

    /// The editor goes in its columns, every cell as it was; the margins are
    /// blank in the background given.
    #[test]
    fn the_editor_is_placed_in_its_columns_on_blank_margins() {
        let blank = styled(9);
        let mut f = Frame::new(3, 2, Style::default());
        for (k, c) in "abcdef".chars().enumerate() {
            f.cells[k].text = Text::Char(c);
        }
        let out = place(f, 7, 2, blank);
        assert_eq!(out.text(), "  abc  \n  def  ");
        assert_eq!(out.get(0, 0).map(|o| o.style), Some(blank));
        assert_eq!(out.get(1, 6).map(|o| o.style), Some(blank));
        assert_eq!(out.get(0, 2).map(|o| o.style), Some(Style::default()));
    }

    /// A bar runs on to each edge it is asked to, in the colours of its own
    /// end on that side; the rest of the margin is left alone.
    #[test]
    fn a_bar_runs_on_to_the_edges_in_its_own_colours() {
        let (bar_l, bar_r, blank) = (styled(1), styled(2), styled(9));
        let mut f = Frame::new(9, 1, blank);
        for c in 3..6 {
            f.cells[c] = Out::blank(if c == 3 { bar_l } else { bar_r });
        }
        run_on(&mut f, 0, 3, 3, (true, false));
        let styles: Vec<Style> = f.cells.iter().map(|o| o.style).collect();
        assert_eq!(styles[..3], [bar_l; 3]);
        assert_eq!(styles[6..], [blank; 3], "not asked to run on to the right");
        run_on(&mut f, 0, 3, 3, (false, true));
        assert_eq!(
            f.cells.iter().map(|o| o.style).collect::<Vec<_>>()[6..],
            [bar_r; 3]
        );
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
}
