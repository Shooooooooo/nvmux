//! A terminal wider than the editor's windows need: what `[client] centre`
//! does with it (see [`crate::config::Centre`]).
//!
//! # The editor centred
//!
//! Neovim is told the terminal is only as wide as the editor's windows side
//! by side want — each up to `window_width`, one that keeps its width
//! (`'winfixwidth'`, as a file tree does) as wide as it keeps, and a column
//! between each two — which the measure left in the editor works out from
//! its layout, and says again whenever that changes ([`MEASURE_LUA`]). The
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
//! it make them the same width again, as `'equalalways'` makes them on a
//! split — where that is on, as it is unless turned off.
//!
//! A window showing a terminal wants the whole terminal: Neovim tells the
//! program in it its window's width, and a shell or a program drawing a
//! screen of its own is given all of it. So with one on show the editor is
//! the whole terminal, and centred again once there is none.
//!
//! # Each window's text centred
//!
//! Neovim keeps the whole terminal, and a window wider than `window_width` is
//! asked for a grid that wide (`nvim_ui_try_resize_grid`, which only a UI
//! with `ext_multigrid` can ask): Neovim draws its text to that width — wraps
//! its lines there, ends its cursor line there — and the client puts it in
//! the middle of the window ([`inset`]), a float anchored to it with it. The
//! status line under it is still the window's width, as Neovim draws it, and
//! has its words moved in to stand over the text ([`realign`]). A window
//! showing a terminal, as the measure says which are, is not asked: its grid
//! is as wide as Neovim tells the program in it its terminal is, and that
//! would be held to `window_width` with it.

use super::compose::{Frame, Out};
use super::grid::Text;
use super::style::Style;

/// What the windows of the tab page in front want, side by side, given the
/// most columns a window's text gets (the first argument): a window showing
/// a terminal wants the whole terminal, more than any is wide — the program
/// in it is told its window's width; a window that keeps its width wants
/// that, any other the most; a row of them the sum with a column between
/// each two, and a column of them the most any one of them wants. Answered
/// as `{ wanted, 'laststatus', 'columns', terminals }`: `'columns'` so that
/// an attach at another width knows the windows were laid out for that one,
/// and the windows showing a terminal, so that the text of none of them is
/// centred in it.
///
/// Given the client's channel as well, it stays in the editor and says it
/// again, as an `nvmux_client` notification (`measure`, and the same four),
/// whenever what the windows want, `'laststatus'` or the windows showing a
/// terminal change: a window made, closed or resized, a buffer shown in a
/// window, a terminal opened, a tab page entered, `'winfixwidth'` or
/// `'laststatus'` set. It goes when this client's UI does, or the first time
/// it finds the channel gone.
pub const MEASURE_LUA: &str = "local most, chan = ...
local api = vim.api
local whole = 1073741824
local function terminal(win)
  return vim.bo[api.nvim_win_get_buf(win)].buftype == 'terminal'
end
local function want(node)
  local kind, v = node[1], node[2]
  if kind == 'leaf' then
    if terminal(v) then
      return whole
    end
    if vim.wo[v].winfixwidth then
      return api.nvim_win_get_width(v)
    end
    return most
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
local function terminals()
  local wins = {}
  for _, win in ipairs(api.nvim_tabpage_list_wins(0)) do
    if terminal(win) then
      table.insert(wins, win)
    end
  end
  return wins
end
local function measure()
  return { want(vim.fn.winlayout()), vim.o.laststatus, vim.o.columns, terminals() }
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
  }, { group = group, callback = soon })
  api.nvim_create_autocmd('OptionSet',
    { group = group, pattern = { 'winfixwidth', 'laststatus' }, callback = soon })
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
/// narrower for its windows: make them the same width again, as it does on a
/// split, where `'equalalways'` says it is to.
pub const EQUALISE_LUA: &str = "if vim.o.equalalways then
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
