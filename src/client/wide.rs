//! A terminal wider than a window's text needs: what `[client] centre` does
//! with it (see [`crate::config::ClientSettings::centre`]).
//!
//! Neovim keeps the whole terminal, and a window wider than it wants, as the
//! measure left in the editor says ([`MEASURE_LUA`]), is asked for a grid
//! that wide (`nvim_ui_try_resize_grid`, which only a UI with `ext_multigrid`
//! can ask): Neovim draws its text to that width — wraps its lines there,
//! ends its cursor line there — and the client puts it in the middle of the
//! window ([`inset`]), a float anchored to it with it. The status line under
//! it is still the window's width, as Neovim draws it, and has its words
//! moved in to stand over the text ([`realign`]). A window showing a terminal
//! wants the whole terminal, so it is not asked: its grid is as wide as
//! Neovim tells the program in it its terminal is. A grid asked for is the
//! window's for every UI of the session: as the client goes, it asks for each
//! window's own width back, and waits a moment for Neovim to have done it
//! ([`super::run`]).
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

use super::compose::Out;
use super::grid::Text;

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

/// What takes [`MEASURE_LUA`] back out of the editor, given the client's
/// channel: once nothing is centred, nothing is to be measured.
pub const UNMEASURE_LUA: &str =
    "pcall(vim.api.nvim_del_augroup_by_name, 'nvmux_centre_' .. select(1, ...))";

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
}
