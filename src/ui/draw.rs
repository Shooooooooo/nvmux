//! Rendering. Pure functions of [`App`] state, so the whole screen can be
//! asserted against a `TestBackend` without a terminal.
//!
//! Nothing here sets a colour: every distinction is a modifier (reversed and
//! bold for the selection, dim for hints and numbers), so the picker uses the
//! terminal's palette and is `NO_COLOR`-correct by construction. Relying on
//! crossterm's `NO_COLOR` handling instead would break: it turns
//! `SetForegroundColor` into `ESC[m`, a full SGR reset that wipes bold,
//! reverse and dim mid-line.

use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::app::{App, Mode};
use super::backspace::Backspace;
use super::landing::{Landing, Side, Tier};
use super::note;
use super::starfield;
use super::swap::Dot;

/// The selected row's marker; other rows are indented to match, so names
/// stay in one column.
const MARKER: &str = "▸ ";
const INDENT: &str = "  ";

/// The marker on the row being moved; must be as wide as [`MARKER`]. `⇕`
/// (U+21D5), not `↕` (U+2195): the latter has the Emoji property, so some
/// terminals draw it two columns wide while `unicode-width` reports one.
const GRABBED: &str = "⇕ ";

/// The marker on the placeholder row for a new session (see
/// [`App::make_room`]); as wide as [`MARKER`].
const GHOST: &str = "+ ";

const CURSOR: &str = "▋";

/// A floor for when every name is empty; the block is otherwise sized to its
/// content. A wider minimum would push short names left of centre, since
/// names are left-aligned within the block.
const MIN_LIST_WIDTH: u16 = 4;
const MAX_LIST_WIDTH: u16 = 48;

const NUM_GAP: &str = "  ";

/// Room after the longest name so it does not end flush with the bar's edge.
/// The bar spans the full list width on every row, so its right edge does not
/// jump with each name's length.
const PAD: &str = " ";

/// Columns left of the list for a session's sign (see [`super::note`]),
/// leaving a blank column before the marker, like Neovim's sign column. Dim,
/// and outside the bar, since bold and dim share one reset.
const SIGN_OUT: u16 = 2;

/// 79 columns, to fit an 80-column terminal. `␣ order` and `, settings` are
/// listed because nobody would find them unaided; `?` (which shows the
/// `<prefix>` keys, not these) is not. Narrower, the row truncates from the
/// right, so `q quit` goes first (`Ctrl-c` also quits).
///
/// The row is lowercase, so keys are glyphs (`␣` is
/// [`crate::keys::SPACE_GLYPH`], `⏎`, `↑↓`) rather than `Space` or `Enter`,
/// as in the prefix's hint bar ([`crate::hint`]); a test keeps them agreeing.
const HINTS: &str =
    "↑↓ move  ⏎ attach  c new  r rename  x kill  ␣ order  / find  , settings  q quit";
const EMPTY: &str = "no sessions — press c to create one";

/// The hint row while a session is being moved; dim like the normal hints
/// (unlike the filter and kill confirm, which are being typed or asked).
/// Space also places, but only `⏎`, the usual confirm key, is shown.
const REORDER_HINTS: &str = "↑↓ move  ⏎ place  esc cancel";

pub fn draw(frame: &mut Frame, app: &App) {
    draw_with(frame, app, HINTS);
}

/// [`draw`] with other resting `hints`, for the floating picker's panel,
/// which is narrower than [`HINTS`] (see [`super::picker`]).
pub(super) fn draw_with(frame: &mut Frame, app: &App, hints: &str) {
    let (text, dim) = hint_line(app, hints);
    screen(frame, &text, dim, |frame, body| draw_list(frame, app, body));
}

/// Every full-screen view: a body, then a hint row on the last line; nothing
/// on an empty frame. One function so the zero-size guard and the split stay
/// together.
pub(super) fn screen(
    frame: &mut Frame,
    hint: &str,
    dim: bool,
    body: impl FnOnce(&mut Frame, Rect),
) {
    let area = frame.area();
    if area.height == 0 || area.width == 0 {
        return;
    }
    let (top, bottom) = split_hint_row(area);
    body(frame, top);
    draw_hint_row(frame, bottom, hint, dim);
}

/// Body above, hint row on the last line; shared so screens line up across
/// transitions.
pub(super) fn split_hint_row(area: Rect) -> (Rect, Rect) {
    let body = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    let bottom = Rect {
        y: area.y + area.height - 1,
        height: 1,
        ..area
    };
    (body, bottom)
}

/// The bottom row of every screen: centred, truncated rather than wrapped
/// (wrapping would push the body up), dim for hints. Also draws the empty
/// picker's message and the attaching status, on the row
/// [`centre_vertically`] picks.
pub(super) fn draw_hint_row(frame: &mut Frame, area: Rect, text: &str, dim: bool) {
    hint_row_on(frame.buffer_mut(), area, text, dim);
}

/// [`draw_hint_row`] on a buffer, for floating screens (see
/// [`super::float`]).
pub(super) fn hint_row_on(buf: &mut Buffer, area: Rect, text: &str, dim: bool) {
    let text = truncate(text, area.width as usize);
    let style = if dim {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    };
    Paragraph::new(Line::from(Span::styled(text, style)))
        .alignment(Alignment::Center)
        .render(area, buf);
}

fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    // Includes rows a filter just dropped, while they fade (see `effects`);
    // drawn plain, as the fade is a post-pass.
    let rows = app.rows();

    if rows.is_empty() {
        draw_hint_row(frame, centre_vertically(area, 1), EMPTY, true);
        return;
    }

    let Some(ListLayout {
        block,
        offset,
        num_width,
    }) = list_layout(app, area)
    else {
        return;
    };
    let height = block.height;

    // While a session is moved, the number column is blanked, not removed,
    // which would shift every name sideways. The digit keys are dead then,
    // and numbers changing owner under a moving row are noise. A session
    // being put down counts as moving until its trail is pulled in (see
    // `landing`).
    let landing = app.landing();
    let pull = landing.and_then(Landing::pulling);
    let reordering = matches!(app.mode(), Mode::Reorder { .. }) || pull.is_some();
    let selected_row = app.selected_row();
    let backspace = app.backspace();

    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(height as usize)
        .map(|(i, row)| {
            // A row being erased is drawn by its backspace alone, below.
            if backspace.is_some_and(|(id, _)| id == row.session.id) {
                return Line::default();
            }
            // The new-session placeholder: all dim, like a prompt
            // suggestion, and never selected.
            if row.ghost {
                let head = format!("{GHOST}{:>num_width$}{NUM_GAP}", row.num);
                let dim = Style::default().add_modifier(Modifier::DIM);
                return row_line(
                    &head,
                    &row.session.name,
                    0..0,
                    &[],
                    block.width as usize,
                    dim,
                );
            }
            let selected = i == selected_row && !row.leaving;
            let style = if selected {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
            };
            let marked = app.matched(&row.session.name);
            let head = head(row.num, selected, reordering, num_width);
            // The number is dim, except in the bar: bold and dim share one
            // reset, and most terminals cannot show both.
            let dim = if selected {
                0..0
            } else {
                MARKER.chars().count()..head.chars().count() - NUM_GAP.chars().count()
            };
            let name = if selected {
                let fill =
                    (block.width as usize).saturating_sub(head.width() + row.session.name.width());
                format!("{}{}", row.session.name, " ".repeat(fill))
            } else {
                row.session.name.clone()
            };
            row_line(&head, &name, dim, &marked, block.width as usize, style)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), block);

    // Rows trading places step aside by whole cells (see `swap`), after
    // drawing so the bar moves too, and before the trails, which start where
    // the bar now ends.
    for (line, row) in rows.iter().skip(offset).take(height as usize).enumerate() {
        let aside = app.swap().aside(&row.session.id);
        if aside != 0 {
            let at = Rect::new(block.x, block.y + line as u16, block.width, 1);
            step_aside(frame.buffer_mut(), at, aside);
        }
    }

    // Signs first, so trails and the landing bounce draw over them.
    draw_signs(frame, app, block, offset);
    draw_caption(frame, app, area, block);
    if let Some((id, backspace)) = backspace {
        draw_backspace(frame, app, block, offset, num_width, id, backspace);
    }
    if app.trailing() {
        draw_trails(frame, app, block, offset, 0.0);
    } else if let Some(pull) = pull {
        draw_trails(frame, app, block, offset, pull);
    }
    draw_sparks(frame, app, area, block, offset);
    if let Some(landing) = landing {
        draw_landing(frame, app, area, block, offset, landing);
    }
}

/// The landing's bounce and spray on the row just put down (see
/// [`super::landing`]): the bar overshoots at both ends and back
/// ([`super::landing::BOUNCE`]), and dust flies from both ends, some onto the
/// rows above and below. Both stay beside the list and on screen; dust also
/// stays within `area` (so off the hint row) and only on blank cells.
fn draw_landing(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    block: Rect,
    offset: usize,
    landing: &Landing,
) {
    let selected = app.selected_row();
    let Some(line) = selected
        .checked_sub(offset)
        .filter(|line| *line < block.height as usize)
    else {
        return;
    };
    let y = block.y + line as u16;
    // From the list's edges, not the name's, so dust never lands inside the
    // list.
    let left = block.x;
    let right = block.x + block.width;
    let screen = frame.area();
    let on_screen = |x: u16| x >= screen.x && x < screen.x + screen.width;
    let in_list = |y: u16| y >= area.y && y < area.y + area.height;
    let buf = frame.buffer_mut();

    for grain in landing.spray() {
        let at = match grain.tier {
            Tier::Above => y.checked_sub(1),
            Tier::Level => Some(y),
            Tier::Below => y.checked_add(1),
        };
        let Some(at) = at.filter(|at| in_list(*at)) else {
            continue;
        };
        let x = match grain.side {
            Side::After => right.checked_add(grain.offset),
            Side::Before => left.checked_sub(grain.offset + 1),
        };
        let Some(x) = x.filter(|x| on_screen(*x)) else {
            continue;
        };
        if buf[(x, at)].symbol() != " " {
            continue;
        }
        let style = if grain.dim {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default()
        };
        buf.set_string(x, at, grain.glyph.to_string(), style);
    }

    let widen = landing.widen();
    let bar = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);
    let before = (1..=widen).filter_map(|d| left.checked_sub(d));
    let after = (0..widen).map(|d| right + d);
    for x in before.chain(after).filter(|x| on_screen(*x)) {
        buf.set_string(x, y, " ", bar);
    }
}

/// Shift a drawn `row` by `aside` columns (negative is left), blanking the
/// cells it leaves and dropping what goes off screen (see [`super::swap`]).
fn step_aside(buf: &mut Buffer, row: Rect, aside: i16) {
    let row = row.intersection(buf.area);
    let cells: Vec<Cell> = (row.x..row.x + row.width)
        .map(|x| std::mem::take(&mut buf[(x, row.y)]))
        .collect();
    let screen = buf.area;
    for (x, cell) in (row.x..).zip(cells) {
        let Some(to) = x
            .checked_add_signed(aside)
            .filter(|to| *to >= screen.x && *to < screen.x + screen.width)
        else {
            continue;
        };
        buf[(to, row.y)] = cell;
    }
}

/// Each session's sign, [`SIGN_OUT`] columns left of its row (see
/// [`super::note`]), moving as the row steps aside. Skipped without room, on
/// a row being erased, and on the new-session placeholder.
fn draw_signs(frame: &mut Frame, app: &App, block: Rect, offset: usize) {
    let screen = frame.area();
    let dim = Style::default().add_modifier(Modifier::DIM);
    let spin = app.spin_frame();
    let erasing = app.backspace().map(|(id, _)| id);
    let rows = app.rows();
    for (line, row) in rows
        .iter()
        .skip(offset)
        .take(block.height as usize)
        .enumerate()
    {
        if row.ghost || erasing == Some(row.session.id.as_str()) {
            continue;
        }
        let Some(n) = app.note(&row.session.id) else {
            continue;
        };
        let Some(x) = block
            .x
            .checked_add_signed(app.swap().aside(&row.session.id))
            .and_then(|x| x.checked_sub(SIGN_OUT))
            .filter(|x| *x >= screen.x && *x < screen.x + screen.width)
        else {
            continue;
        };
        let mut glyph = [0u8; 4];
        frame.buffer_mut().set_string(
            x,
            block.y + line as u16,
            note::sign(n, spin).encode_utf8(&mut glyph),
            dim,
        );
    }
}

/// The selected session's full note, centred a blank row under the list.
/// Only while browsing or filtering, and only if there is a row for it.
fn draw_caption(frame: &mut Frame, app: &App, area: Rect, block: Rect) {
    if !matches!(app.mode(), Mode::Normal | Mode::Filter) {
        return;
    }
    let Some(n) = app.selected_row_id().and_then(|id| app.note(id)) else {
        return;
    };
    let y = block.y + block.height + 1;
    if y >= area.y + area.height {
        return;
    }
    let text = truncate(&note::caption(n), area.width as usize);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let para = Paragraph::new(Line::from(Span::styled(text, dim))).alignment(Alignment::Center);
    frame.render_widget(
        para,
        Rect {
            y,
            height: 1,
            ..area
        },
    );
}

/// Braille sparks from both ends of the list, along the seam where swapping
/// rows slid past each other (see [`super::swap`]). Drawn within the screen
/// and the list's `area` (so off the hint row), only on blank cells or a
/// trail's braille, whose dots they merge with; never on a bar.
fn draw_sparks(frame: &mut Frame, app: &App, area: Rect, block: Rect, offset: usize) {
    let dots = app.swap().sparks();
    if dots.is_empty() {
        return;
    }
    let screen = frame.area();
    let buf = frame.buffer_mut();
    for dot in dots {
        let Some((x, y, bit)) = spark_cell(&dot, block, offset) else {
            continue;
        };
        if x < screen.x || x >= screen.x + screen.width || y < area.y || y >= area.y + area.height {
            continue;
        }
        let cell = &mut buf[(x, y)];
        if cell.modifier.contains(Modifier::REVERSED) {
            continue;
        }
        let mut symbol = cell.symbol().chars();
        let lit = match (symbol.next(), symbol.next()) {
            (Some(' '), None) => 0,
            (Some(c), None)
                if (starfield::BRAILLE..=starfield::BRAILLE + 0xff).contains(&(c as u32)) =>
            {
                c as u32 - starfield::BRAILLE
            }
            _ => continue,
        };
        // A new cell is as bright as its spark; one already lit keeps its own
        // weight unless the spark is the brighter.
        if lit == 0 && dot.dim {
            cell.modifier.insert(Modifier::DIM);
        } else if !dot.dim {
            cell.modifier.remove(Modifier::DIM);
        }
        let glyph =
            char::from_u32(starfield::BRAILLE + (lit | bit)).expect("braille patterns are chars");
        cell.set_char(glyph);
    }
}

/// The cell and braille bit for a spark's dot, for a list drawn in `block`
/// scrolled to `offset`; `None` off the top or left of the screen.
fn spark_cell(dot: &Dot, block: Rect, offset: usize) -> Option<(u16, u16, u32)> {
    // In dots: the seam is its row's top edge, and x counts out from the dot
    // column just outside each end of the list.
    let seam = (i64::from(block.y) + dot.seam as i64 - offset as i64) * 4;
    let y = seam + i64::from(dot.level);
    let x = match dot.side {
        Side::After => i64::from(block.x + block.width) * 2 + i64::from(dot.out),
        Side::Before => i64::from(block.x) * 2 - 1 - i64::from(dot.out),
    };
    if x < 0 || y < 0 {
        return None;
    }
    let bit = starfield::DOTS[(y % 4) as usize][(x % 2) as usize];
    Some((u16::try_from(x / 2).ok()?, u16::try_from(y / 4).ok()?, bit))
}

/// A row's marker, right-aligned number (see [`super::app::Row::num`]) and
/// gap.
fn head(num: u32, selected: bool, reordering: bool, num_width: usize) -> String {
    let prefix = match (selected, reordering) {
        (true, true) => GRABBED,
        (true, false) => MARKER,
        (false, _) => INDENT,
    };
    let num = if reordering {
        String::new()
    } else {
        num.to_string()
    };
    format!("{prefix}{num:>num_width$}{NUM_GAP}")
}

/// One row cut to `width`, with the `char` indices in `dim` (the number
/// column) dimmed and the name's indices in `marked` (the filter's matches,
/// see [`App::matched`]) underlined, on top of `style`.
fn row_line(
    head: &str,
    name: &str,
    dim: std::ops::Range<usize>,
    marked: &[usize],
    width: usize,
    style: Style,
) -> Line<'static> {
    let text = truncate(&format!("{head}{name}"), width);
    let skip = head.chars().count();
    let style_at = |i: usize| {
        let mut style = style;
        if dim.contains(&i) {
            style = style.add_modifier(Modifier::DIM);
        }
        if i >= skip && marked.binary_search(&(i - skip)).is_ok() {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        style
    };
    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_style = style_at(0);
    for (i, c) in text.chars().enumerate() {
        let here = style_at(i);
        if here != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = here;
        run.push(c);
    }
    if !run.is_empty() || spans.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    Line::from(spans)
}

/// The row a kill is erasing: what is left of it and a cursor after it (see
/// [`super::backspace`]). Drawn as when the kill was confirmed, marker and
/// number included, but not reversed. The cursor may fall past the block's
/// end, but never off screen.
fn draw_backspace(
    frame: &mut Frame,
    app: &App,
    block: Rect,
    offset: usize,
    num_width: usize,
    id: &str,
    backspace: &Backspace,
) {
    let rows = app.rows();
    let Some(index) = rows.iter().position(|r| r.session.id == id) else {
        return;
    };
    let Some(line) = index
        .checked_sub(offset)
        .filter(|line| *line < block.height as usize)
    else {
        return;
    };
    let row = rows[index];
    let selected = index == app.selected_row();
    let text = truncate(
        &format!(
            "{}{}",
            head(row.num, selected, false, num_width),
            row.session.name
        ),
        block.width as usize,
    );
    let (kept, cursor) = backspace.render(&text);
    let area = frame.area();
    let y = block.y + line as u16;
    let buf = frame.buffer_mut();
    buf.set_string(block.x, y, kept, Style::default());
    let x = block.x + kept.width() as u16;
    if cursor && x < area.x + area.width {
        buf.set_string(x, y, CURSOR, Style::default());
    }
}

/// Star trails off both ends of a moving session's row: plain for the near
/// half, dim for the far half, never reversed.
///
/// Drawn outside the block: making room inside would widen it and shift
/// every name. On a narrow terminal a trail is cut at the screen's edge,
/// keeping the cells nearest the row. They follow a row stepped aside (see
/// [`super::swap`]).
///
/// `pull` is how far the trails are drawn back into a landing row; 0 while
/// still moving.
fn draw_trails(frame: &mut Frame, app: &App, block: Rect, offset: usize, pull: f32) {
    let selected = app.selected_row();
    let Some(line) = selected
        .checked_sub(offset)
        .filter(|line| *line < block.height as usize)
    else {
        return;
    };
    if selected >= app.rows().len() {
        return;
    }
    let y = block.y + line as u16;
    let area = frame.area();
    let near = starfield::TRAIL / 2;
    let dim = Style::default().add_modifier(Modifier::DIM);
    let aside = app.selected_row_id().map_or(0, |id| app.swap().aside(id));
    let left = block.x.saturating_add_signed(aside);

    // Right: from the column after the bar's last cell.
    let x = (block.x + block.width).saturating_add_signed(aside);
    let cells = starfield::TRAIL.min(usize::from((area.x + area.width).saturating_sub(x)));
    if cells > 0 {
        let stars: Vec<char> = app
            .trail_after()
            .render_pulled(cells, pull, false)
            .chars()
            .collect();
        let (bright, faint) = stars.split_at(near.min(cells));
        draw_trail(
            frame,
            Rect::new(x, y, cells as u16, 1),
            vec![
                Span::raw(bright.iter().collect::<String>()),
                Span::styled(faint.iter().collect::<String>(), dim),
            ],
        );
    }

    // Left: ending in the column before the marker, running the other way.
    let cells = starfield::TRAIL.min(usize::from(left.saturating_sub(area.x)));
    if cells > 0 {
        let stars: Vec<char> = app
            .trail_before()
            .render_pulled(cells, pull, true)
            .chars()
            .collect();
        let (faint, bright) = stars.split_at(cells - near.min(cells));
        draw_trail(
            frame,
            Rect::new(left - cells as u16, y, cells as u16, 1),
            vec![
                Span::styled(faint.iter().collect::<String>(), dim),
                Span::raw(bright.iter().collect::<String>()),
            ],
        );
    }
}

fn draw_trail(frame: &mut Frame, area: Rect, spans: Vec<Span<'static>>) {
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The hint row's text and whether it is dim: dim hints, or at full weight
/// whatever is being asked or typed.
fn hint_line(app: &App, hints: &str) -> (String, bool) {
    match app.mode() {
        Mode::Confirm { prompt, .. } | Mode::Leave { prompt, .. } => (prompt.clone(), false),
        Mode::Filter => (format!("/{}{CURSOR}", app.filter()), false),
        // Keep showing the query: a move of one visible row can carry a
        // session past several rows the filter hides.
        Mode::Reorder { .. } if !app.filter().is_empty() => {
            (format!("{REORDER_HINTS}  /{}", app.filter()), true)
        }
        Mode::Reorder { .. } => (REORDER_HINTS.to_string(), true),
        Mode::Normal => match app.message() {
            Some(msg) => (msg.to_string(), false),
            // A number waiting for another digit; otherwise the keystroke
            // would look ignored.
            None => match app.pending() {
                Some(n) => (format!("#{n}{CURSOR}"), false),
                None if !app.filter().is_empty() => {
                    // Otherwise the list looks silently shorter.
                    (format!("/{}", app.filter()), true)
                }
                None => (hints.to_string(), true),
            },
        },
    }
}

/// The centred, content-sized block the list is drawn in within the body
/// `area`, and the scroll offset keeping the selection in it; `None` for an
/// empty list or no room. Shared with [`row_at`] so clicks land on the row
/// drawn.
fn list_layout(app: &App, area: Rect) -> Option<ListLayout> {
    // Includes rows fading out of a filter: the block fits what is on
    // screen.
    let rows = app.rows();
    if area.height == 0 || rows.is_empty() {
        return None;
    }
    let (width, num_width) = list_columns(&rows);
    let width = width.min(area.width);

    let height = (rows.len() as u16).min(area.height);
    let mut block = centre(area, width, height);
    let gap = rows.iter().position(|r| r.ghost);
    // A gap pushes the rows below it down: the top stays where it was
    // without the gap, while there is room. Re-centring would lift the list a
    // line on every other height, as if the rows above made way.
    if gap.is_some() {
        let before = centre(area, width, height.saturating_sub(1));
        if before.y + height <= area.y + area.height {
            block.y = before.y;
        }
    }
    // Keep the gap in view; the selection is the row above it.
    let focus = gap.unwrap_or_else(|| app.selected_row());
    let offset = scroll_offset(focus, rows.len(), height as usize);
    Some(ListLayout {
        block,
        offset,
        num_width,
    })
}

/// The list's unconstrained width (longest name plus marker, number, gap and
/// pad, clamped to the limits) and its number column's width.
fn list_columns(rows: &[super::app::Row]) -> (u16, usize) {
    let widest = rows
        .iter()
        .map(|r| r.session.name.width())
        .max()
        .unwrap_or(0) as u16;
    // As wide as the longest number, so names stay aligned past nine.
    let num_width = rows
        .iter()
        .map(|r| r.num.to_string().len())
        .max()
        .unwrap_or(1);
    let width = (widest
        + MARKER.width() as u16
        + num_width as u16
        + NUM_GAP.width() as u16
        + PAD.width() as u16)
        .clamp(MIN_LIST_WIDTH, MAX_LIST_WIDTH);
    (width, num_width)
}

/// The list's width given room; sizes the floating picker's panel (see
/// [`super::picker`]).
pub(super) fn list_width(app: &App) -> u16 {
    list_columns(&app.rows()).0
}

/// See [`list_layout`].
struct ListLayout {
    block: Rect,
    /// The visible row on the block's first line.
    offset: usize,
    num_width: usize,
}

/// The visible row at (`column`, `row`), `area` being the frame last drawn.
/// A row spans the terminal's full width, not just the block: a short name
/// is a poor touchpad target.
pub(super) fn row_at(app: &App, area: Rect, column: u16, row: u16) -> Option<usize> {
    // As in `screen`: an empty frame has no hint row to split off.
    if area.height == 0 || area.width == 0 {
        return None;
    }
    let (body, _) = split_hint_row(area);
    let ListLayout { block, offset, .. } = list_layout(app, body)?;
    if column < area.x || column >= area.x + area.width {
        return None;
    }
    if row < block.y || row >= block.y + block.height {
        return None;
    }
    // Convert from drawn rows, which include fading rows and the gap, to
    // visible rows, which do not.
    let index = offset + usize::from(row - block.y);
    let rows = app.rows();
    let drawn = rows.get(index)?;
    if drawn.leaving || drawn.ghost {
        return None;
    }
    Some(
        rows[..index]
            .iter()
            .filter(|r| !r.leaving && !r.ghost)
            .count(),
    )
}

/// Where session `id`'s row is drawn, as wide as the bar (see [`PAD`]), or
/// `None` off screen; `area` is the whole frame. What the effects paint over
/// (see [`super::effects`]).
pub(super) fn row_rect(app: &App, area: Rect, id: &str) -> Option<Rect> {
    if area.height == 0 || area.width == 0 {
        return None;
    }
    let (body, _) = split_hint_row(area);
    let ListLayout { block, offset, .. } = list_layout(app, body)?;
    let index = app.rows().iter().position(|r| r.session.id == id)?;
    let line = index
        .checked_sub(offset)
        .filter(|line| *line < block.height as usize)?;
    Some(Rect::new(block.x, block.y + line as u16, block.width, 1))
}

/// Where session `id`'s name is drawn within its [`row_rect`], excluding the
/// bar's trailing blanks; `None` if none of it is on screen.
pub(super) fn name_rect(app: &App, area: Rect, id: &str) -> Option<Rect> {
    let row = row_rect(app, area, id)?;
    let (body, _) = split_hint_row(area);
    let num_width = list_layout(app, body)?.num_width;
    let head = (MARKER.width() + num_width + NUM_GAP.width()) as u16;
    let name = app
        .rows()
        .iter()
        .find(|r| r.session.id == id)?
        .session
        .name
        .width() as u16;
    let width = row.width.saturating_sub(head).min(name);
    (width > 0).then(|| Rect::new(row.x + head, row.y, width, 1))
}

/// Whether (`column`, `row`) is part of a drawn row (marker to end of name,
/// inner blanks included), so effects never land in the space in a name like
/// `session 3`.
pub(super) fn on_a_row(app: &App, area: Rect, column: u16, row: u16) -> bool {
    if area.height == 0 || area.width == 0 {
        return false;
    }
    let (body, _) = split_hint_row(area);
    let Some(ListLayout {
        block,
        offset,
        num_width,
    }) = list_layout(app, body)
    else {
        return false;
    };
    if row < block.y || row >= block.y + block.height || column < block.x {
        return false;
    }
    let rows = app.rows();
    let Some(drawn) = rows.get(offset + usize::from(row - block.y)) else {
        return false;
    };
    let width = (MARKER.width() + num_width + NUM_GAP.width() + drawn.session.name.width())
        .min(block.width as usize);
    usize::from(column - block.x) < width
}

/// Keep `selected` visible within a window of `height` rows.
pub(super) fn scroll_offset(selected: usize, total: usize, height: usize) -> usize {
    if height == 0 || total <= height {
        return 0;
    }
    if selected < height {
        return 0;
    }
    (selected + 1 - height).min(total - height)
}

/// Centre a `width` x `height` block inside `area`, both axes.
pub(super) fn centre(area: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

/// Centre a `height`-row block vertically at full width, leaving horizontal
/// centring to the paragraph's `Alignment::Center` (unlike [`centre`], which
/// sizes the rect to the content).
pub(super) fn centre_vertically(area: Rect, height: u16) -> Rect {
    Rect {
        y: area.y + (area.height.saturating_sub(height)) / 2,
        height: height.min(area.height),
        ..area
    }
}

/// Truncate to a display width, not bytes or `char`s, so CJK names and emoji
/// do not overflow.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.to_string().width();
        if w + cw > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::app::Key;
    use super::super::landing;
    use super::super::swap;
    use super::super::test_support::{self, filtering, picker as app};
    use super::*;
    use std::time::Duration;

    fn render(app: &App, w: u16, h: u16) -> Vec<String> {
        test_support::render(w, h, |f| draw(f, app))
    }

    #[test]
    fn the_whole_screen_is_a_centred_list_and_one_hint_line() {
        // 80 columns, as `HINTS` is 79: a truncated row would weaken the check.
        let lines = render(
            &app(&["api-server", "dotfiles", "scratch", "notes"]),
            80,
            11,
        );

        assert!(
            lines[10].contains("attach") && lines[10].contains("quit"),
            "hints should be on the last row, got {:?}",
            lines[10]
        );

        let content: Vec<&String> = lines[..10].iter().filter(|l| !l.is_empty()).collect();
        assert_eq!(content.len(), 4, "expected 4 rows, got {content:?}");

        test_support::assert_no_borders(&lines);
    }

    #[test]
    fn the_selection_is_marked_and_others_are_indented_to_match() {
        let lines = render(&app(&["aaa", "bbb"]), 40, 6);
        let rows: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("aa") || l.contains("bb"))
            .collect();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("▸ 1  aaa"), "selected row: {:?}", rows[0]);
        assert!(
            rows[1].contains("  2  bbb"),
            "unselected row: {:?}",
            rows[1]
        );

        // Display columns, not bytes: "▸" is three bytes but one column.
        let col = |l: &str, name: &str| {
            let byte = l.find(name).expect("name present");
            l[..byte].width()
        };
        assert_eq!(col(rows[0], "aaa"), col(rows[1], "bbb"));
    }

    #[test]
    fn the_list_is_centred_on_both_axes() {
        let lines = render(&app(&["one", "two"]), 60, 21);
        let occupied: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(i, l)| *i < 20 && !l.is_empty())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(occupied.len(), 2);

        assert_eq!(occupied, vec![9, 10], "list is not vertically centred");

        let row = &lines[9];
        let (left, right) = test_support::padding(row, 60);
        assert!(
            left.abs_diff(right) <= 2,
            "not horizontally centred: {left} left, {right} right, row {row:?}"
        );
    }

    #[test]
    fn every_row_carries_its_session_number() {
        let lines = render(&app(&["api-server", "dotfiles", "notes"]), 40, 6);
        for want in ["▸ 1  api-server", "  2  dotfiles", "  3  notes"] {
            assert!(
                lines.iter().any(|l| l.contains(want)),
                "missing {want:?} in {lines:#?}"
            );
        }
    }

    /// Otherwise names would be cut by the number's width.
    #[test]
    fn the_list_block_grows_to_fit_the_number_column() {
        let lines = render(&app(&["exactly-this-long"]), 40, 6);
        let row = lines
            .iter()
            .find(|l| l.contains("exactly"))
            .expect("the row");
        assert!(
            row.contains("▸ 1  exactly-this-long"),
            "the name was truncated to make room: {row:?}"
        );
    }

    #[test]
    fn a_pending_number_is_shown_on_the_hint_row() {
        let names: Vec<String> = (0..12).map(|i| format!("s{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut a = app(&refs);
        a.on_key(Key::Char('1'));

        let lines = render(&a, 40, 14);
        assert!(
            lines[13].contains("#1"),
            "the pending number is not on the hint row: {:?}",
            lines[13]
        );
    }

    /// A session picked up for reordering, with the cursor on `row`.
    fn reordering(names: &[&str], row: usize) -> App {
        let mut a = app(names);
        for _ in 0..row {
            a.on_key(Key::Char('j'));
        }
        a.on_key(Key::Char(' '));
        assert!(matches!(a.mode(), Mode::Reorder { .. }), "the grab took");
        a
    }

    #[test]
    fn the_numbers_are_hidden_while_a_session_is_being_moved() {
        let lines = render(&reordering(&["api-server", "dotfiles", "notes"], 0), 40, 6);
        for line in lines.iter().filter(|l| l.contains("dotfiles")) {
            assert!(
                !line.chars().any(|c| c.is_ascii_digit()),
                "a number survived into reorder mode: {line:?}"
            );
        }
        assert!(lines.iter().any(|l| l.contains("api-server")), "{lines:#?}");
    }

    /// The number column is blanked, not removed.
    #[test]
    fn the_names_do_not_move_when_a_session_is_picked_up() {
        let column = |a: &App| -> Vec<usize> {
            render(a, 40, 6)
                .iter()
                .filter(|l| l.contains("dotfiles") || l.contains("notes"))
                .map(|l| l[..l.find(char::is_alphabetic).expect("a name")].width())
                .collect()
        };
        let before = column(&app(&["dotfiles", "notes"]));
        let after = column(&reordering(&["dotfiles", "notes"], 0));
        assert_eq!(before.len(), 2);
        assert_eq!(
            before, after,
            "the names shifted when the row was picked up"
        );
    }

    #[test]
    fn the_grabbed_row_is_marked_differently_from_the_cursor() {
        assert_eq!(
            GRABBED.width(),
            MARKER.width(),
            "a marker of a different width shifts the whole name column"
        );
        assert_eq!(MARKER.width(), INDENT.width());

        let lines = render(&reordering(&["aaa", "bbb"], 1), 40, 6);
        let row = lines.iter().find(|l| l.contains("bbb")).expect("the row");
        assert!(row.contains(GRABBED), "grabbed row: {row:?}");
        assert!(
            !lines.iter().any(|l| l.contains(MARKER)),
            "the plain cursor marker is still on screen: {lines:#?}"
        );
    }

    #[test]
    fn the_reorder_hints_name_enter_and_not_the_space_bar() {
        let lines = render(&reordering(&["one", "two"], 0), 60, 6);
        let hints = &lines[5];
        assert!(hints.contains("⏎ place"), "got {hints:?}");
        assert!(
            !hints.contains('␣'),
            "the space bar is advertised: {hints:?}"
        );
    }

    /// The prefix's hint bar ([`crate::hint`]) uses the same glyph,
    /// [`crate::keys::SPACE_GLYPH`]; this pins the literal to it.
    #[test]
    fn the_space_bar_is_spelled_the_way_every_hint_row_spells_it() {
        assert!(
            HINTS.contains(&format!("{} order", crate::keys::SPACE_GLYPH)),
            "the picker's row has drifted from the shared glyph: {HINTS:?}"
        );
        assert!(
            !HINTS.contains("space") && !HINTS.contains("Space"),
            "the space bar is spelled out as well: {HINTS:?}"
        );
    }

    #[test]
    fn an_applied_filter_is_still_shown_while_reordering() {
        let mut a = app(&["api-server", "dotfiles"]);
        a.on_key(Key::Char('/'));
        for c in "dot".chars() {
            a.on_key(Key::Char(c));
        }
        a.on_key(Key::Enter); // filter applied, normal mode
        a.on_key(Key::Char(' '));

        let lines = render(&a, 60, 6);
        assert!(lines[5].contains("place"), "got {:?}", lines[5]);
        assert!(
            lines[5].contains("/dot"),
            "the query vanished: {:?}",
            lines[5]
        );
    }

    #[test]
    fn the_empty_state_is_one_dimmed_line_with_hints_still_below() {
        let lines = render(&app(&[]), 80, 9);
        let content: Vec<&String> = lines[..8].iter().filter(|l| !l.is_empty()).collect();
        assert_eq!(content.len(), 1, "expected one line, got {content:?}");
        assert!(content[0].contains("no sessions"), "got {:?}", content[0]);
        assert!(content[0].contains("press c to create one"));
        assert!(lines[8].contains("quit"), "hints should still be present");
    }

    /// The filter and kill confirm use the hint row: no popup and no shift in
    /// the list. Naming uses the full-screen prompt instead.
    #[test]
    fn prompts_replace_the_hint_line_in_place() {
        for (open_with, typed, expected) in
            [('/', "dot", "/dot▋"), ('x', "", "kill \"dotfiles\"? [y/N]")]
        {
            let mut a = app(&["dotfiles"]);
            a.on_key(Key::Char(open_with));
            for c in typed.chars() {
                a.on_key(Key::Char(c));
            }
            // Wide enough for all of `HINTS`, or the `quit` check below would
            // pass regardless.
            let lines = render(&a, 70, 8);
            assert!(
                lines[7].contains(expected),
                "prompt should be on the bottom row: {:?}",
                lines[7]
            );
            assert!(
                !lines[7].contains("quit"),
                "hints should be replaced, not appended"
            );
            assert!(lines[..7].iter().any(|l| l.contains("dotfiles")));
        }
    }

    #[test]
    fn the_filter_prompt_shows_the_query_with_a_cursor() {
        let mut a = app(&["api-server", "dotfiles"]);
        a.on_key(Key::Char('/'));
        for c in "dot".chars() {
            a.on_key(Key::Char(c));
        }
        let lines = render(&a, 60, 6);
        assert!(lines[5].contains("/dot▋"), "got {:?}", lines[5]);
        assert!(lines[..5].iter().any(|l| l.contains("dotfiles")));
        assert!(!lines[..5].iter().any(|l| l.contains("api-server")));
    }

    #[test]
    fn the_hint_row_gives_up_quit_first_when_it_does_not_fit() {
        let lines = render(&app(&["one"]), 62, 6);
        let row = &lines[5];
        assert!(row.contains("order"), "the new entry was cut: {row:?}");
        assert!(row.contains("attach"), "{row:?}");
        assert!(
            !row.contains("quit"),
            "something else was cut first: {row:?}"
        );
        assert!(row.width() <= 62, "the row overflowed: {row:?}");
    }

    /// The README's picker art (detected by [`MARKER`], which appears nowhere
    /// else in it) must carry this exact row, and vice versa. The art itself
    /// is optional.
    #[test]
    fn the_readme_shows_the_hint_row_the_picker_actually_prints() {
        let readme = include_str!("../../README.md");
        assert_eq!(
            readme.contains(MARKER),
            readme.contains(HINTS),
            "the README's picker art is stale — it should carry this row \
             verbatim, under a `{MARKER}` selection marker, or carry neither:\n{HINTS}"
        );
    }

    #[test]
    fn a_narrow_terminal_truncates_the_hints_rather_than_wrapping() {
        let lines = render(&app(&["one"]), 20, 5);
        assert_eq!(lines.len(), 5);
        // Row 3 must stay part of the list area, not spillover from the hints.
        assert!(
            lines[4].width() <= 20,
            "hint row overflowed: {:?}",
            lines[4]
        );
        assert!(
            !lines[3].contains("quit"),
            "hints wrapped upward: {:?}",
            lines[3]
        );
    }

    #[test]
    fn long_names_are_truncated_to_the_block() {
        let long = "a-really-quite-extraordinarily-long-session-name-that-goes-on";
        let lines = render(&app(&[long]), 40, 5);
        for line in &lines {
            assert!(line.width() <= 40, "line overflowed the terminal: {line:?}");
        }
    }

    #[test]
    fn wide_characters_do_not_overflow() {
        // CJK is two columns per character; byte or char widths would overflow.
        let lines = render(&app(&["日本語のセッション名前", "notes"]), 30, 6);
        for line in &lines {
            assert!(
                line.width() <= 30,
                "overflowed: {line:?} ({} cols)",
                line.width()
            );
        }
    }

    #[test]
    fn a_list_longer_than_the_screen_scrolls_to_keep_the_selection_visible() {
        let names: Vec<String> = (0..30).map(|i| format!("session-{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut a = app(&refs);
        a.on_key(Key::Char('G')); // last entry

        let lines = render(&a, 40, 10);
        assert!(
            lines.iter().any(|l| l.contains("▸ 30  session-29")),
            "the selection scrolled out of view: {lines:#?}"
        );
        // Thirty sessions: two-digit numbers, and the names still line up.
        let name_columns: Vec<usize> = lines
            .iter()
            .filter(|l| l.contains("session-"))
            .map(|l| l[..l.find("session-").expect("a name")].width())
            .collect();
        assert_eq!(name_columns.len(), 9, "expected a full window: {lines:#?}");
        assert!(
            name_columns.windows(2).all(|w| w[0] == w[1]),
            "names are not in one column: {lines:#?}"
        );
    }

    // --- hit-testing -------------------------------------------------------

    fn line_of(lines: &[String], name: &str) -> u16 {
        lines
            .iter()
            .position(|l| l.contains(name))
            .unwrap_or_else(|| panic!("{name} not on screen: {lines:#?}")) as u16
    }

    /// A click in any column; checked against the renderer, not a copy of its
    /// arithmetic.
    #[test]
    fn a_click_on_a_rendered_row_names_that_row() {
        let a = app(&["api-server", "dotfiles", "notes"]);
        let (w, h) = (40, 9);
        let area = Rect::new(0, 0, w, h);
        let lines = render(&a, w, h);
        for (i, name) in ["api-server", "dotfiles", "notes"].iter().enumerate() {
            let y = line_of(&lines, name);
            for x in [0, w / 2, w - 1] {
                assert_eq!(row_at(&a, area, x, y), Some(i), "({x},{y}) for {name}");
            }
        }
    }

    #[test]
    fn a_click_off_the_list_names_nothing() {
        let a = app(&["one", "two"]);
        let (w, h) = (40, 9);
        let area = Rect::new(0, 0, w, h);
        let lines = render(&a, w, h);
        let first = line_of(&lines, "one");
        let last = line_of(&lines, "two");
        assert_eq!(row_at(&a, area, 5, first - 1), None, "the blank above");
        assert_eq!(row_at(&a, area, 5, last + 1), None, "the blank below");
        assert_eq!(row_at(&a, area, 5, h - 1), None, "the hint row");
        assert_eq!(row_at(&a, area, w, first), None, "past the right edge");
        assert_eq!(row_at(&a, area, 5, h), None, "past the bottom");
    }

    #[test]
    fn a_click_on_the_empty_state_names_nothing() {
        let a = app(&[]);
        let area = Rect::new(0, 0, 40, 9);
        for y in 0..9 {
            assert_eq!(row_at(&a, area, 10, y), None, "row {y}");
        }
    }

    #[test]
    fn a_click_on_a_scrolled_list_accounts_for_the_offset() {
        let names: Vec<String> = (0..30).map(|i| format!("session-{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut a = app(&refs);
        a.on_key(Key::Char('G'));
        let (w, h) = (40, 10);
        let area = Rect::new(0, 0, w, h);
        let lines = render(&a, w, h);
        let y = line_of(&lines, "session-29");
        assert_eq!(row_at(&a, area, 5, y), Some(29));
        let y = line_of(&lines, "session-21");
        assert_eq!(row_at(&a, area, 5, y), Some(21));
        assert_eq!(row_at(&a, area, 5, 0), Some(21), "the first line on screen");
    }

    #[test]
    fn a_click_on_a_list_taller_than_the_screen_stops_at_the_hint_row() {
        let names: Vec<String> = (0..30).map(|i| format!("s{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let a = app(&refs);
        let area = Rect::new(0, 0, 20, 6);
        assert_eq!(row_at(&a, area, 1, 0), Some(0));
        assert_eq!(row_at(&a, area, 1, 4), Some(4));
        assert_eq!(row_at(&a, area, 1, 5), None, "the hint row");
    }

    #[test]
    fn hit_testing_tiny_terminals_does_not_panic() {
        for &(w, h) in test_support::TINY_SIZES {
            let area = Rect::new(0, 0, w, h);
            for a in [app(&["one", "two"]), app(&[])] {
                for y in 0..=h {
                    for x in 0..=w {
                        let _ = row_at(&a, area, x, y);
                    }
                }
            }
        }
    }

    #[test]
    fn scroll_offset_keeps_the_selection_in_the_window() {
        assert_eq!(scroll_offset(0, 30, 10), 0);
        assert_eq!(scroll_offset(5, 30, 10), 0, "no scroll while it still fits");
        assert_eq!(scroll_offset(9, 30, 10), 0);
        assert_eq!(scroll_offset(10, 30, 10), 1);
        assert_eq!(scroll_offset(29, 30, 10), 20, "last item, last window");
        assert_eq!(scroll_offset(5, 5, 10), 0, "shorter than the window");
        assert_eq!(scroll_offset(0, 0, 0), 0, "degenerate case must not panic");
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        for &(w, h) in test_support::TINY_SIZES {
            let _ = render(&app(&["one", "two"]), w.max(1), h.max(1));
            let _ = render(&app(&[]), w.max(1), h.max(1));
            let _ = render(&reordering(&["one", "two"], 1), w.max(1), h.max(1));
        }
    }

    /// So it is correct under `NO_COLOR` by construction.
    #[test]
    fn nothing_sets_a_colour() {
        let mut a = app(&["one", "two", "three"]);
        a.on_key(Key::Char('j'));
        test_support::assert_no_colour(50, 8, |f| draw(f, &a));

        let b = reordering(&["one", "two", "three"], 1);
        test_support::assert_no_colour(50, 8, |f| draw(f, &b));
    }

    /// The prompts (filter, kill confirm) are not dim: they are being typed or
    /// asked.
    #[test]
    fn the_reorder_hints_are_dim_like_the_ordinary_ones() {
        let mut with_filter = app(&["api-server", "dotfiles"]);
        with_filter.on_key(Key::Char('/'));
        with_filter.on_key(Key::Char('d'));
        with_filter.on_key(Key::Enter);
        with_filter.on_key(Key::Char(' '));
        assert!(
            matches!(with_filter.mode(), Mode::Reorder { .. }),
            "the grab took"
        );

        for (what, a, want_dim) in [
            ("plain hints", app(&["one", "two"]), true),
            ("reorder hints", reordering(&["one", "two"], 0), true),
            ("reorder hints with a filter", with_filter, true),
            ("the filter prompt", filtering(&["one", "two"], "on"), false),
        ] {
            let buf = test_support::buffer(60, 6, |f| draw(f, &a));
            let row = buf.area.height - 1;
            let cell = (0..buf.area.width)
                .map(|x| &buf[(x, row)])
                .find(|c| c.symbol().trim() != "")
                .expect("something on the hint row");
            assert_eq!(
                cell.modifier.contains(Modifier::DIM),
                want_dim,
                "{what}: wanted dim={want_dim}"
            );
        }
    }

    #[test]
    fn the_selected_row_is_reversed_and_bold() {
        let a = app(&["one", "two"]);
        let buf = test_support::buffer(40, 6, |f| draw(f, &a));
        let marked = (0..buf.area.height)
            .find(|y| (0..buf.area.width).any(|x| buf[(x, *y)].symbol() == "▸"))
            .expect("a marked row");
        let cell = (0..buf.area.width)
            .map(|x| &buf[(x, marked)])
            .find(|c| c.symbol() == "▸")
            .expect("marker cell");
        assert!(cell.modifier.contains(Modifier::REVERSED));
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn truncate_counts_display_width() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 3), "hel");
        assert_eq!(truncate("日本語", 4), "日本", "two columns per character");
        assert_eq!(truncate("日本語", 3), "日", "must not split a wide char");
        assert_eq!(truncate("", 0), "");
    }

    // --- the trail behind a session in flight --------------------------------

    fn is_braille(symbol: &str) -> bool {
        symbol
            .chars()
            .any(|c| (0x2801..=0x28ff).contains(&(c as u32)))
    }

    /// [`reordering`] with the trails on whatever the config says.
    fn trailing(names: &[&str], row: usize) -> App {
        let mut a = reordering(names, row);
        a.set_trail(true);
        a
    }

    /// The trail starts after the bar (the list's edge, however short the
    /// name) and runs `TRAIL` cells, plain then dim, not reversed.
    #[test]
    fn the_grabbed_row_trails_stars_off_the_end_of_its_bar() {
        let a = trailing(&["api-server", "docs"], 1);
        let buf = test_support::buffer(44, 8, |f| draw(f, &a));
        let row =
            |y: u16| -> String { (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect() };
        let end_of = |line: &str, name: &str| {
            line[..line.find(name).expect("the name") + name.len()].width() as u16
        };
        let y = (0..buf.area.height)
            .find(|y| row(*y).contains("docs"))
            .expect("the grabbed row");
        let name_end = end_of(&row(y), "docs");
        let end = end_of(&row(y - 1), "api-server") + PAD.width() as u16;

        for x in name_end - 1..end {
            assert!(
                buf[(x, y)].modifier.contains(Modifier::REVERSED),
                "the name's last character and the blanks past it to the \
                 list's edge are in the bar, at {x}"
            );
        }
        for i in 0..starfield::TRAIL as u16 {
            let cell = &buf[(end + i, y)];
            assert!(
                cell.symbol() == " " || is_braille(cell.symbol()),
                "{:?} at {i} is not a star or a blank",
                cell.symbol()
            );
            assert!(
                !cell.modifier.contains(Modifier::REVERSED),
                "the trail is not part of the bar"
            );
            assert_eq!(
                cell.modifier.contains(Modifier::DIM),
                usize::from(i) >= starfield::TRAIL / 2,
                "cell {i} of the trail has the wrong weight"
            );
        }
        let past = &buf[(end + starfield::TRAIL as u16, y)];
        assert_eq!(past.symbol(), " ", "the trail ran past its length");
        assert!(past.modifier.is_empty());
    }

    #[test]
    fn only_a_session_in_flight_leaves_a_trail() {
        let buf = test_support::buffer(44, 8, |f| draw(f, &app(&["api-server", "docs"])));
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                assert!(!is_braille(buf[(x, y)].symbol()), "a star at ({x},{y})");
            }
        }

        let a = trailing(&["api-server", "docs"], 1);
        let lines = render(&a, 44, 8);
        for line in lines.iter().filter(|l| !l.contains("docs")) {
            assert!(
                !line.chars().any(|c| is_braille(&c.to_string())),
                "{line:?}"
            );
        }
    }

    #[test]
    fn the_trail_stops_at_the_edge_of_the_terminal() {
        let a = trailing(&["api-server"], 0);
        let narrow = render(&a, 40, 6);
        let row = narrow
            .iter()
            .find(|l| l.contains("api-server"))
            .expect("the row");
        let end = row[..row.find("api-server").expect("the name") + "api-server".len()].width();
        let w = (end + 2) as u16;
        for line in render(&a, w, 6) {
            assert!(line.width() <= usize::from(w), "{line:?} is wider than {w}");
        }
    }

    /// Mirrored: it ends before the marker and runs `TRAIL` cells leftwards.
    #[test]
    fn the_grabbed_row_trails_stars_off_its_other_end_too() {
        let a = trailing(&["api-server", "docs"], 1);
        let buf = test_support::buffer(44, 8, |f| draw(f, &a));
        let (x0, y) = (0..buf.area.height)
            .find_map(|y| {
                (0..buf.area.width)
                    .find(|x| buf[(*x, y)].symbol() == GRABBED.trim_end())
                    .map(|x| (x, y))
            })
            .expect("the grabbed row's marker");
        let trail = starfield::TRAIL as u16;
        assert!(x0 >= trail, "no room for the trail in this test's layout");
        for i in 1..=trail {
            let cell = &buf[(x0 - i, y)];
            assert!(
                cell.symbol() == " " || is_braille(cell.symbol()),
                "{:?} at {i} before the marker is not a star or a blank",
                cell.symbol()
            );
            assert!(!cell.modifier.contains(Modifier::REVERSED));
            assert_eq!(
                cell.modifier.contains(Modifier::DIM),
                usize::from(i) > starfield::TRAIL / 2,
                "cell {i} before the marker has the wrong weight"
            );
        }
        let past = &buf[(x0 - trail - 1, y)];
        assert_eq!(past.symbol(), " ", "the trail ran past its length");
        assert!(past.modifier.is_empty());
    }

    /// `[effects.move] enabled = false`: still picked up, marked and moved.
    #[test]
    fn with_the_trail_off_a_session_in_flight_leaves_none() {
        let mut a = reordering(&["api-server", "docs"], 1);
        a.set_trail(false);
        let buf = test_support::buffer(44, 8, |f| draw(f, &a));
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                assert!(!is_braille(buf[(x, y)].symbol()), "a star at ({x},{y})");
            }
        }
        assert!(
            render(&a, 44, 8)
                .iter()
                .any(|l| l.contains(GRABBED.trim_end())),
            "the session is still marked as moving"
        );
    }

    // --- the filter's underline -------------------------------------------

    /// The cells of row `y` whose symbol is in `name`'s place, left to right.
    fn name_cells<'a>(
        buf: &'a ratatui::buffer::Buffer,
        y: u16,
        name: &str,
    ) -> Vec<&'a ratatui::buffer::Cell> {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        let at = row.find(name).expect("the name is on the row");
        let x0 = row[..at].chars().count() as u16;
        (x0..x0 + name.chars().count() as u16)
            .map(|x| &buf[(x, y)])
            .collect()
    }

    /// Fuzzy: `asv` marks three spread-out letters of `api-server`.
    #[test]
    fn the_letters_a_filter_matched_are_underlined() {
        let a = filtering(&["api-server", "dotfiles"], "asv");
        let buf = test_support::buffer(40, 6, |f| draw(f, &a));
        let lines = render(&a, 40, 6);
        let y = line_of(&lines, "api-server");
        let marks: String = name_cells(&buf, y, "api-server")
            .iter()
            .map(|c| {
                if c.modifier.contains(Modifier::UNDERLINED) {
                    '^'
                } else {
                    ' '
                }
            })
            .collect();
        // a-p-i---s-e-r-v-e-r
        assert_eq!(marks, "^   ^  ^  ");
        test_support::assert_no_colour(40, 6, |f| draw(f, &a));
    }

    /// The underline is not an effect; with no query there is none.
    #[test]
    fn the_underline_stays_with_the_effects_off() {
        let mut a = filtering(&["api-server", "dotfiles"], "asv");
        a.set_effects(false);
        let buf = test_support::buffer(40, 6, |f| draw(f, &a));
        assert!(buf
            .content
            .iter()
            .any(|c| c.modifier.contains(Modifier::UNDERLINED)));

        let plain = app(&["api-server", "dotfiles"]);
        let buf = test_support::buffer(40, 6, |f| draw(f, &plain));
        assert!(buf
            .content
            .iter()
            .all(|c| !c.modifier.contains(Modifier::UNDERLINED)));
    }

    /// The bar runs from the marker to one cell past the longest name,
    /// whatever the selected name. No other row is reversed.
    #[test]
    fn the_selection_bar_is_as_wide_as_the_list_on_every_row() {
        let names = ["api-server", "docs", "dotfiles"];
        let mut a = app(&names);
        a.set_effects(false);
        let lines = render(&a, 40, 6);
        let line = &lines[line_of(&lines, "api-server") as usize];
        let start = line[..line.find('▸').unwrap()].width() as u16;
        let end = (line[..line.find("api-server").unwrap() + "api-server".len()].width()
            + PAD.width()) as u16;

        for selected in names {
            let buf = test_support::buffer(40, 6, |f| draw(f, &a));
            for name in names {
                let y = line_of(&lines, name);
                let bar: Vec<u16> = (0..buf.area.width)
                    .filter(|x| buf[(*x, y)].modifier.contains(Modifier::REVERSED))
                    .collect();
                if name == selected {
                    assert_eq!(
                        bar,
                        (start..end).collect::<Vec<_>>(),
                        "{name}'s bar when it is selected"
                    );
                } else {
                    assert!(
                        bar.is_empty(),
                        "{name} reversed while {selected} is selected"
                    );
                }
            }
            a.on_key(Key::Char('j'));
        }
    }

    #[test]
    fn the_pad_does_not_move_the_list() {
        let a = app(&["api-server", "docs"]);
        let mut b = app(&["api-server", "docs"]);
        b.on_key(Key::Char('j'));
        let (la, lb) = (render(&a, 40, 6), render(&b, 40, 6));
        for name in ["api-server", "docs"] {
            let at = |lines: &[String]| {
                let line = &lines[line_of(lines, name) as usize];
                line[..line.find(name).unwrap()].width()
            };
            assert_eq!(at(&la), at(&lb), "{name} moved");
        }
    }

    /// The bar's trailing blanks belong to the row, not the name, so effects
    /// strike or hand off only the name.
    #[test]
    fn a_name_is_measured_without_the_pad() {
        let mut a = app(&["api-server", "docs"]);
        let area = Rect::new(0, 0, 40, 6);
        let lines = render(&a, 40, 6);
        let x = |name: &str| {
            let line = &lines[line_of(&lines, name) as usize];
            line[..line.find(name).unwrap()].width() as u16
        };
        let end = x("api-server") + 10 + PAD.width() as u16;
        for (name, width) in [("api-server", 10), ("docs", 4)] {
            let id = a.selected_row_id().unwrap().to_string();
            let y = line_of(&lines, name);
            assert_eq!(
                name_rect(&a, area, &id),
                Some(Rect::new(x(name), y, width, 1))
            );
            let row = row_rect(&a, area, &id).unwrap();
            assert_eq!(row.x + row.width, end, "{name}'s row");
            a.on_key(Key::Char('j'));
        }
    }

    /// Names are never dimmed.
    #[test]
    fn the_numbers_are_dim_except_in_the_selection() {
        let mut a = app(&["api-server", "dotfiles"]);
        a.set_effects(false);
        let buf = test_support::buffer(40, 6, |f| draw(f, &a));
        let lines = render(&a, 40, 6);
        for (name, num, dim) in [("api-server", "1", false), ("dotfiles", "2", true)] {
            let y = line_of(&lines, name);
            let line = &lines[y as usize];
            let x = line[..line.find(name).unwrap()]
                .rfind(num)
                .map(|at| line[..at].chars().count() as u16)
                .expect("the number is on the row");
            assert_eq!(buf[(x, y)].modifier.contains(Modifier::DIM), dim, "{name}");
            assert!(name_cells(&buf, y, name)
                .iter()
                .all(|c| !c.modifier.contains(Modifier::DIM)));
        }
        test_support::assert_no_colour(40, 6, |f| draw(f, &a));
    }

    // --- rows fading out of a filter ----------------------------------------

    /// A click on it names nothing, and a click below it names the visible
    /// row drawn there.
    #[test]
    fn a_row_fading_out_of_a_filter_cannot_be_clicked() {
        let a = filtering(&["api-server", "dotfiles", "notes"], "n");
        assert!(a.leaving().is_some(), "the keystroke dropped rows");
        let area = Rect::new(0, 0, 40, 8);
        let lines = render(&a, 40, 8);
        assert!(lines.iter().any(|l| l.contains("dotfiles")), "{lines:#?}");

        let dotfiles = line_of(&lines, "dotfiles");
        let notes = line_of(&lines, "notes");
        assert_eq!(row_at(&a, area, 20, dotfiles), None);
        assert_eq!(row_at(&a, area, 20, notes), Some(0), "the only visible row");
    }

    // --- the backspace a kill erases with --------------------------------------

    /// [`picker`] with the second row selected and its kill confirmed, the
    /// backspace `ms` milliseconds along.
    fn erasing(ms: u64) -> App {
        let mut a = app(&["api-server", "dotfiles", "notes"]);
        a.on_key(Key::Char('j'));
        a.on_key(Key::Char('x'));
        assert!(matches!(
            a.on_key(Key::Char('y')),
            super::super::app::Request::Kill(_)
        ));
        assert!(a.start_backspace("id000001"));
        a.tick(std::time::Duration::from_millis(ms));
        a
    }

    /// First the whole row and a cursor, not reversed; midway its start and
    /// the cursor; at the end nothing, with the rows around it unmoved.
    #[test]
    fn a_killed_row_is_erased_from_its_end_and_leaves_its_place_empty() {
        let start = erasing(0);
        let lines = render(&start, 44, 8);
        let y = line_of(&lines, "dotfiles");
        assert!(
            lines[y as usize].ends_with(&format!("dotfiles{CURSOR}")),
            "{lines:#?}"
        );
        let buf = test_support::buffer(44, 8, |f| draw(f, &start));
        assert!(
            (0..buf.area.width).all(|x| !buf[(x, y)].modifier.contains(Modifier::REVERSED)),
            "no longer the selection"
        );

        let mid = erasing(120);
        let line = render(&mid, 44, 8)[y as usize].clone();
        let kept = line.trim_start().trim_end_matches(CURSOR);
        assert!(line.ends_with(CURSOR), "{line:?}");
        assert!(
            !kept.is_empty() && "▸ 2  dotfiles".starts_with(kept) && kept != "▸ 2  dotfiles",
            "part of the row, from its start: {line:?}"
        );

        let end = erasing(1_000);
        assert!(!end.erasing());
        let after = render(&end, 44, 8);
        assert_eq!(after[y as usize], "", "the row is empty: {after:#?}");
        assert_eq!(line_of(&after, "api-server"), y - 1, "the rows stay put");
        assert_eq!(line_of(&after, "notes"), y + 1);
        test_support::assert_no_colour(44, 8, |f| draw(f, &mid));
    }

    /// The erased row is gone, and one whose kill failed comes back whole.
    #[test]
    fn a_fresh_listing_ends_the_backspace() {
        let mut a = erasing(1_000);
        let same = a.sessions().to_vec();
        a.set_sessions(same);
        assert!(a.backspace().is_none());
        assert!(render(&a, 44, 8).iter().any(|l| l.contains("dotfiles")));
    }

    #[test]
    fn with_the_kill_effect_off_there_is_no_backspace() {
        let mut a = app(&["api-server", "dotfiles"]);
        a.set_effects(false);
        assert!(!a.start_backspace("id000001"));
        assert!(!a.animating());
    }

    // --- the swap -----------------------------------------------------------

    fn column_of(lines: &[String], name: &str) -> usize {
        let line = &lines[usize::from(line_of(lines, name))];
        line[..line.find(name).expect("drawn")].width()
    }

    /// The columns of row `y` in the reversed bar.
    fn bar(buf: &Buffer, y: u16) -> Vec<u16> {
        (0..buf.area.width)
            .filter(|x| buf[(*x, y)].modifier.contains(Modifier::REVERSED))
            .collect()
    }

    /// On the key, the moved session is [`swap::ASIDE`] cells right and the
    /// one it passed as far left; other rows stay put; afterwards all are back.
    /// No colour is set.
    #[test]
    fn a_move_steps_both_rows_aside_and_back() {
        let names = ["api-server", "docs", "notes"];
        let column = column_of(&render(&trailing(&names, 0), 40, 6), "notes");
        let aside = usize::from(swap::ASIDE);

        let mut a = trailing(&names, 0);
        a.on_key(Key::Char('j'));
        let lines = render(&a, 40, 6);
        assert!(
            line_of(&lines, "docs") < line_of(&lines, "api-server"),
            "the rows traded places: {lines:#?}"
        );
        assert_eq!(
            column_of(&lines, "api-server"),
            column + aside,
            "{lines:#?}"
        );
        assert_eq!(column_of(&lines, "docs"), column - aside, "{lines:#?}");
        assert_eq!(column_of(&lines, "notes"), column, "{lines:#?}");
        test_support::assert_no_colour(40, 6, |f| draw(f, &a));

        a.tick(swap::SIDESTEP);
        let lines = render(&a, 40, 6);
        for name in names {
            assert_eq!(column_of(&lines, name), column, "{name}: {lines:#?}");
        }
    }

    /// The bar moves whole; its trails leave from where it now ends, not from
    /// the list's edge.
    #[test]
    fn the_bar_steps_aside_with_its_trails() {
        let names = ["api-server", "docs", "notes"];
        let still = trailing(&names, 0);
        let buf = test_support::buffer(44, 8, |f| draw(f, &still));
        let y = line_of(&render(&still, 44, 8), "api-server");
        let was = bar(&buf, y);

        let mut a = trailing(&names, 0);
        a.on_key(Key::Char('j'));
        let buf = test_support::buffer(44, 8, |f| draw(f, &a));
        let y = line_of(&render(&a, 44, 8), "api-server");
        let now = bar(&buf, y);
        let shifted: Vec<u16> = was.iter().map(|x| x + swap::ASIDE).collect();
        assert_eq!(now, shifted, "the bar, whole, ASIDE cells right");

        let (first, end) = (now[0], now[now.len() - 1] + 1);
        let near = starfield::TRAIL as u16 / 2;
        for i in 0..starfield::TRAIL as u16 {
            for (x, far) in [(end + i, i >= near), (first - 1 - i, i >= near)] {
                let cell = &buf[(x, y)];
                assert!(
                    cell.symbol() == " " || is_braille(cell.symbol()),
                    "{:?} at {x} is not a star or a blank",
                    cell.symbol()
                );
                assert_eq!(
                    cell.modifier.contains(Modifier::DIM),
                    far,
                    "trail weight at {x}"
                );
            }
        }
    }

    /// Both rows jump, with no sidestep or sparks.
    #[test]
    fn with_the_trail_off_a_move_steps_nothing_aside() {
        let names = ["api-server", "docs", "notes"];
        let column = column_of(&render(&trailing(&names, 0), 40, 6), "notes");
        let mut a = trailing(&names, 0);
        a.set_trail(false);
        a.on_key(Key::Char('j'));
        let lines = render(&a, 40, 6);
        for name in names {
            assert_eq!(column_of(&lines, name), column, "{name}: {lines:#?}");
        }
        assert!(
            !lines
                .iter()
                .any(|l| l.chars().any(|c| is_braille(&c.to_string()))),
            "{lines:#?}"
        );
    }

    /// The row over the seam has no trail, so its dots are all sparks: beside
    /// the list at both ends, never on text, gone afterwards. Rows further off
    /// get none.
    #[test]
    fn a_move_throws_sparks_off_the_seam() {
        let names = ["api-server", "docs", "notes", "web"];
        let still = trailing(&names, 1);
        let buf = test_support::buffer(50, 10, |f| draw(f, &still));
        let was = bar(&buf, line_of(&render(&still, 50, 10), "docs"));
        let (first, end) = (was[0], was[was.len() - 1] + 1);

        // Down a row: the seam is the top edge of the row it lands on.
        let mut a = trailing(&names, 1);
        a.on_key(Key::Char('j'));
        let lines = render(&a, 50, 10);
        let y = line_of(&lines, "docs");
        assert_eq!(line_of(&lines, "notes"), y - 1);

        let mut seen = [false; 2];
        let mut t = Duration::ZERO;
        while t < swap::SPARKING {
            let buf = test_support::buffer(50, 10, |f| draw(f, &a));
            for x in (0..buf.area.width).filter(|x| is_braille(buf[(*x, y - 1)].symbol())) {
                assert!(x < first || x >= end, "a spark on the row at {x}, {t:?} in");
                seen[usize::from(x >= end)] = true;
            }
            for other in (0..buf.area.height).filter(|r| *r + 1 < y || *r > y) {
                assert!(
                    !(0..buf.area.width).any(|x| is_braille(buf[(x, other)].symbol())),
                    "a spark on row {other}, {t:?} in"
                );
            }
            test_support::assert_no_colour(50, 10, |f| draw(f, &a));
            a.tick(Duration::from_millis(10));
            t += Duration::from_millis(10);
        }
        assert_eq!(seen, [true; 2], "sparks off both ends");
        let buf = test_support::buffer(50, 10, |f| draw(f, &a));
        assert!(
            !(0..buf.area.width).any(|x| is_braille(buf[(x, y - 1)].symbol())),
            "the sparks are out"
        );
    }

    /// Whatever the scroll. A seam scrolled off screen draws nothing rather
    /// than panicking.
    #[test]
    fn the_sparks_stay_off_the_hint_row() {
        let names: Vec<String> = (0..12).map(|i| format!("session-{i}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        for key in [
            Key::Char('j'),
            Key::Char('k'),
            Key::Char('G'),
            Key::Char('g'),
        ] {
            let mut a = trailing(&names, 0);
            a.on_key(Key::Char('G'));
            a.on_key(key);
            for _ in 0..(swap::SPARKING.as_millis() / 10) {
                let buf = test_support::buffer(40, 6, |f| draw(f, &a));
                let hint = buf.area.height - 1;
                assert!(
                    !(0..buf.area.width).any(|x| is_braille(buf[(x, hint)].symbol())),
                    "a spark on the hint row after {key:?}"
                );
                a.tick(Duration::from_millis(10));
            }
        }
    }

    // --- the landing --------------------------------------------------------

    /// The third of four sessions carried up a row and put down `ms` ago, with
    /// the trail on, so it lands between two rows.
    fn landed(ms: u64) -> App {
        let mut a = trailing(&["api-server", "docs", "notes", "nvmux"], 2);
        a.tick(std::time::Duration::from_millis(200));
        a.on_key(Key::Char('k'));
        // Let the move's sparks and sidestep finish, so every dot is the
        // landing's.
        a.tick(swap::SPARKING);
        assert!(!a.swap().moving());
        a.on_key(Key::Enter);
        assert!(a.landing().is_some(), "putting it down lands it");
        a.tick(std::time::Duration::from_millis(ms));
        a
    }

    fn line(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    /// The landed bar's start and end on row `y`; the end follows the longest
    /// name, `api-server`, not the landed `notes`.
    fn bar_ends(buf: &ratatui::buffer::Buffer, y: u16) -> (u16, u16) {
        let text = line(buf, y);
        let first = text
            .find('▸')
            .map(|b| text[..b].chars().count() as u16)
            .unwrap();
        let drawn = "▸ 2  api-server";
        (
            first,
            first + drawn.chars().count() as u16 + PAD.width() as u16,
        )
    }

    /// How many reversed cells the bar overshoots on row `y`, before its start
    /// and from its end.
    fn reach(a: &App, y: u16) -> (u16, u16) {
        let buf = test_support::buffer(50, 8, |f| draw(f, a));
        let (first, end) = bar_ends(&buf, y);
        let reversed = |x: u16| buf[(x, y)].modifier.contains(Modifier::REVERSED);
        let before = (1..=first).take_while(|d| reversed(first - d)).count() as u16;
        let after = (end..buf.area.width).take_while(|x| reversed(*x)).count() as u16;
        (before, after)
    }

    /// On impact the numbers return and the bar runs two cells further at both
    /// ends; the rows beside it are untouched.
    #[test]
    fn a_session_put_down_looks_in_flight_until_it_lands() {
        let pulling = landed(30);
        let lines = render(&pulling, 50, 8);
        let y = line_of(&lines, "notes");
        assert!(lines[y as usize].contains(GRABBED.trim_end()), "{lines:#?}");
        assert!(!lines.iter().any(|l| l.contains("1  ")), "numbers hidden");

        let impact = landed(landing::PULL.as_millis() as u64 + 10);
        let buf = test_support::buffer(50, 8, |f| draw(f, &impact));
        let lines = render(&impact, 50, 8);
        assert_eq!(line_of(&lines, "notes"), y, "it lands where it was put");
        assert!(lines[y as usize].contains("▸ 2  notes"), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("3  docs")));

        let (first, end) = bar_ends(&buf, y);
        for x in (first - 2..first).chain(end..end + 2) {
            let cell = &buf[(x, y)];
            assert_eq!(cell.symbol(), " ", "the bar widens with blanks at {x}");
            assert!(cell.modifier.contains(Modifier::REVERSED), "at {x}");
        }
        assert_eq!(reach(&impact, y), (2, 2), "and no further");

        for neighbour in [y - 1, y + 1] {
            for x in 0..buf.area.width {
                let cell = &buf[(x, neighbour)];
                if cell.symbol().trim() == "" || is_braille(cell.symbol()) {
                    continue;
                }
                let number = cell.symbol().chars().all(|c| c.is_ascii_digit());
                assert_eq!(
                    cell.modifier.contains(Modifier::DIM),
                    number,
                    "row {neighbour} at {x}: only its number is dim"
                );
            }
        }
    }

    /// The bar bounces: two cells out at each end, back, one out, and still.
    #[test]
    fn a_landed_bar_bounces_once() {
        let pull = landing::PULL.as_millis() as u64;
        let y = line_of(&render(&landed(pull + 10), 50, 8), "notes");
        let mut at = 0;
        for (lasts, cells) in landing::BOUNCE {
            let mid = at + lasts.as_millis() as u64 / 2;
            assert_eq!(reach(&landed(pull + mid), y), (cells, cells), "{mid} ms in");
            at += lasts.as_millis() as u64;
        }
        assert_eq!(reach(&landed(pull + at + 20), y), (0, 0), "at rest");
    }

    /// Dust flies from both ends, in braille lit only in the middle dot rows;
    /// no colour is set.
    #[test]
    fn a_landed_session_throws_dust_level_with_its_name() {
        const MIDDLE: u32 = 0x02 | 0x04 | 0x10 | 0x20;
        let a = landed(landing::PULL.as_millis() as u64 + 80);
        let buf = test_support::buffer(50, 8, |f| draw(f, &a));
        let y = line_of(&render(&a, 50, 8), "notes");
        let (first, end) = bar_ends(&buf, y);
        let text = line(&buf, y);

        let mut before = 0;
        let mut after = 0;
        for x in 0..buf.area.width {
            let symbol = buf[(x, y)].symbol();
            if !is_braille(symbol) {
                continue;
            }
            let bits = symbol.chars().next().unwrap() as u32 - 0x2800;
            assert_eq!(
                bits & !MIDDLE,
                0,
                "{symbol:?} at {x} is off the middle rows"
            );
            if x < first {
                before += 1;
            } else if x >= end {
                after += 1;
            } else {
                panic!("dust on the row itself at {x}");
            }
        }
        assert!(before > 0 && after > 0, "both ends: {text:?}");
        for other in (0..buf.area.height).filter(|r| r.abs_diff(y) > 1) {
            assert!(
                !line(&buf, other)
                    .chars()
                    .any(|c| is_braille(&c.to_string())),
                "dust on row {other}, more than a row from the landing"
            );
        }
        test_support::assert_no_colour(50, 8, |f| draw(f, &a));
    }

    /// The splash lands beside the list past the bar's ends, never on a
    /// neighbour's text, in the dots nearest the landed row.
    #[test]
    fn a_landed_session_splashes_the_rows_beside_it() {
        let pull = landing::PULL.as_millis() as u64;
        let first_frame = landed(pull);
        let y = line_of(&render(&first_frame, 50, 8), "notes");
        let mut seen = [[false; 2]; 2];
        for ms in (pull..=landing::LENGTH.as_millis() as u64).step_by(5) {
            let a = landed(ms);
            let buf = test_support::buffer(50, 8, |f| draw(f, &a));
            let (first, end) = bar_ends(&buf, y);
            for (i, (row, name, edge)) in [
                (y - 1, "api-server", 0x40 | 0x80),
                (y + 1, "docs", 0x01 | 0x08),
            ]
            .into_iter()
            .enumerate()
            {
                for x in (0..buf.area.width).filter(|x| is_braille(buf[(*x, row)].symbol())) {
                    assert!(x < first || x >= end, "on {name}'s row at {x}, {ms} ms");
                    let bits = buf[(x, row)].symbol().chars().next().unwrap() as u32 - 0x2800;
                    assert_eq!(bits & !edge, 0, "{x} on row {row} is off its edge");
                    seen[i][usize::from(x >= end)] = true;
                }
            }
            test_support::assert_no_colour(50, 8, |f| draw(f, &a));
        }
        assert_eq!(seen, [[true; 2]; 2], "both ends of both rows");
    }

    /// The landed row is right above the hint row, whose short message leaves
    /// room a grain could land on.
    #[test]
    fn the_splash_stays_off_the_hint_row() {
        for ms in (0..=landing::LENGTH.as_millis() as u64).step_by(10) {
            let mut a = trailing(&["api-server", "docs", "notes"], 1);
            a.tick(std::time::Duration::from_millis(200));
            a.on_key(Key::Char('j'));
            a.on_key(Key::Enter);
            a.set_message("ok");
            a.tick(std::time::Duration::from_millis(ms));
            let buf = test_support::buffer(40, 4, |f| draw(f, &a));
            assert!(line(&buf, 2).contains("docs"), "landed right above it");
            assert!(
                !line(&buf, 3).chars().any(|c| is_braille(&c.to_string())),
                "dust on the hint row at {ms} ms: {:?}",
                line(&buf, 3)
            );
        }
    }

    /// The picker also stops asking for frames.
    #[test]
    fn a_landing_settles_to_the_plain_list() {
        let a = landed(1_000);
        assert!(a.landing().is_none());
        assert!(!a.animating());
        let buf = test_support::buffer(50, 8, |f| draw(f, &a));
        assert!(!buf.content.iter().any(|c| is_braille(c.symbol())));
        assert_eq!(
            render(&a, 50, 8),
            render(&placed(&["api-server", "notes", "docs", "nvmux"], 1), 50, 8),
            "exactly the list as drawn without any of this"
        );
    }

    /// A picker holding `names` with row `row` selected and nothing passing.
    fn placed(names: &[&str], row: usize) -> App {
        let mut a = app(names);
        for _ in 0..row {
            a.on_key(Key::Char('j'));
        }
        a.tick(std::time::Duration::from_secs(1));
        a
    }

    /// A cancelled move lands nowhere, nor does any move with the trail off.
    #[test]
    fn only_a_session_put_down_lands_and_only_when_on() {
        let mut cancelled = trailing(&["api-server", "docs"], 1);
        cancelled.on_key(Key::Esc);
        assert!(cancelled.landing().is_none());

        let mut off = trailing(&["api-server", "docs"], 1);
        off.set_trail(false);
        off.on_key(Key::Enter);
        assert!(off.landing().is_none());
    }

    // --- the gap a new session opens -----------------------------------------

    /// [`picker`] with the second row selected and a gap made after it for
    /// `session 4`.
    fn making_room() -> App {
        let mut a = app(&["api-server", "dotfiles", "notes"]);
        a.on_key(Key::Char('j'));
        assert!(a.make_room("session 4"));
        a
    }

    /// The placeholder has a `+` marker, its future number and the prompt's
    /// suggested name, aligned with the other rows and all dim; the row below
    /// takes the next number.
    #[test]
    fn the_new_session_stands_in_a_gap_under_the_highlight() {
        let before = render(&app(&["api-server", "dotfiles", "notes"]), 44, 9);
        let a = making_room();
        let lines = render(&a, 44, 9);
        let y = line_of(&lines, "session 4");
        assert_eq!(line_of(&lines, "dotfiles") + 1, y, "{lines:#?}");
        assert_eq!(line_of(&lines, "notes"), y + 1);
        assert!(lines[y as usize].contains("+ 3  session 4"), "{lines:#?}");
        assert!(lines[y as usize + 1].contains("  4  notes"), "{lines:#?}");
        let column = |line: &str, name: &str| line[..line.find(name).expect("drawn")].width();
        assert_eq!(
            column(&lines[y as usize], "session 4"),
            column(&lines[y as usize - 1], "dotfiles"),
            "the names are in one column"
        );
        assert_eq!(
            line_of(&lines, "api-server"),
            line_of(&before, "api-server"),
            "four rows centre where three did, so the rows above stay put"
        );

        let buf = test_support::buffer(44, 9, |f| draw(f, &a));
        let row: Vec<&ratatui::buffer::Cell> = (0..buf.area.width)
            .map(|x| &buf[(x, y)])
            .filter(|c| c.symbol() != " ")
            .collect();
        assert!(
            row.iter().all(|c| c.modifier == Modifier::DIM),
            "dim, and nothing else: {row:?}"
        );
        test_support::assert_no_colour(44, 9, |f| draw(f, &a));
    }

    #[test]
    fn the_rows_above_the_gap_stay_put_on_any_height() {
        for h in 7..=12 {
            let before = render(&app(&["api-server", "dotfiles", "notes"]), 44, h);
            let lines = render(&making_room(), 44, h);
            assert_eq!(
                line_of(&lines, "api-server"),
                line_of(&before, "api-server"),
                "{h} rows: {lines:#?}"
            );
            assert_eq!(line_of(&lines, "dotfiles"), line_of(&before, "dotfiles"));
            assert_eq!(line_of(&lines, "notes"), line_of(&before, "notes") + 1);
        }
    }

    /// So the last row is not pushed off the screen.
    #[test]
    fn a_gap_with_no_room_below_takes_it_from_above() {
        // Five rows of body for a list of four with the gap: the list fills it.
        let lines = render(
            &{
                let mut a = app(&["one", "two", "three", "four"]);
                a.on_key(Key::Char('j'));
                assert!(a.make_room("session 1"));
                a
            },
            30,
            6,
        );
        for name in ["one", "two", "session 1", "three", "four"] {
            line_of(&lines, name);
        }
    }

    /// The rows around it keep their indices.
    #[test]
    fn the_new_session_cannot_be_clicked() {
        let a = making_room();
        let area = Rect::new(0, 0, 44, 9);
        let lines = render(&a, 44, 9);
        assert_eq!(row_at(&a, area, 20, line_of(&lines, "session 4")), None);
        assert_eq!(row_at(&a, area, 20, line_of(&lines, "dotfiles")), Some(1));
        assert_eq!(row_at(&a, area, 20, line_of(&lines, "notes")), Some(2));
    }

    /// Here the highlight is on the last row in view.
    #[test]
    fn a_gap_below_the_window_is_scrolled_into_view() {
        let names: Vec<String> = (0..30).map(|i| format!("session-{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut a = app(&refs);
        for _ in 0..8 {
            a.on_key(Key::Char('j'));
        }
        let lines = render(&a, 40, 10);
        assert!(
            lines[8].contains("session-08"),
            "the highlight on the last line of the window: {lines:#?}"
        );
        assert!(a.make_room("session 31"));
        let lines = render(&a, 40, 10);
        let y = line_of(&lines, "session 31");
        assert_eq!(line_of(&lines, "session-08") + 1, y, "{lines:#?}");
        assert!(
            lines[y as usize].contains("+ 10  session 31"),
            "the number session-09 had: {lines:#?}"
        );
    }

    #[test]
    fn the_number_column_makes_room_for_the_last_number_too() {
        let names: Vec<String> = (1..=9).map(|i| format!("s{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut a = app(&refs);
        assert!(a.make_room("session 10"));
        let lines = render(&a, 40, 14);
        assert!(
            lines[line_of(&lines, "s9") as usize].contains(" 10  s9"),
            "{lines:#?}"
        );
        assert!(
            lines[line_of(&lines, "s1") as usize].contains("▸  1  s1"),
            "{lines:#?}"
        );
        assert!(lines[line_of(&lines, "session 10") as usize].contains("+  2  session 10"));
    }

    #[test]
    fn a_closed_gap_leaves_the_list_as_it_was() {
        let plain = render(&app(&["api-server", "dotfiles", "notes"]), 44, 9);
        let mut a = making_room();
        a.close_room();
        let mut back = app(&["api-server", "dotfiles", "notes"]);
        back.on_key(Key::Char('j'));
        back.tick(std::time::Duration::from_secs(1));
        assert_eq!(render(&a, 44, 9), render(&back, 44, 9));
        assert_ne!(
            render(&a, 44, 9),
            plain,
            "the highlight is still on dotfiles"
        );
    }

    // --- notes ---------------------------------------------------------------

    use crate::notes::{Kind, Note};

    fn note(kind: Kind, title: &str, text: &str) -> Note {
        Note {
            kind,
            title: title.to_string(),
            text: text.to_string(),
            age: Duration::from_secs(120),
            read_at: std::time::Instant::now(),
        }
    }

    /// A picker of `names` with these notes, by name.
    fn noted(names: &[&str], notes: Vec<(&str, Note)>) -> App {
        let mut a = app(names);
        let by_id = notes
            .into_iter()
            .map(|(name, n)| {
                let s = a.sessions().iter().find(|s| s.name == name).expect("named");
                (s.id.clone(), n)
            })
            .collect();
        a.set_notes(by_id);
        a
    }

    /// A busy session, one with a notification, and one with nothing to say.
    fn three() -> App {
        noted(
            &["api-server", "blog", "dotfiles"],
            vec![
                ("api-server", note(Kind::Busy(None), "claude", "working")),
                (
                    "blog",
                    note(
                        Kind::Notified,
                        "Claude Code",
                        "Claude needs your permission",
                    ),
                ),
            ],
        )
    }

    /// One blank column left of the marker column, dim, with no colour.
    #[test]
    fn a_sign_stands_left_of_its_row_dim() {
        let a = three();
        let (w, h) = (60, 12);
        let lines = render(&a, w, h);
        let buf = test_support::buffer(w, h, |f| draw(f, &a));
        let (busy, notified, plain) = (
            line_of(&lines, "api-server"),
            line_of(&lines, "blog"),
            line_of(&lines, "dotfiles"),
        );
        let start = bar(&buf, busy)[0];
        assert_eq!(buf[(start, busy)].symbol(), MARKER.trim_end());
        let x = start - SIGN_OUT;
        assert_eq!(buf[(x + 1, busy)].symbol(), " ", "a blank column between");
        assert_eq!(
            buf[(x, busy)].symbol(),
            "⠋",
            "the spinner, at its first frame"
        );
        assert_eq!(buf[(x, notified)].symbol(), "∗");
        assert_eq!(buf[(x, plain)].symbol(), " ", "nothing to say, no sign");
        for y in [busy, notified] {
            assert!(buf[(x, y)].modifier.contains(Modifier::DIM), "row {y}");
            assert!(!buf[(x, y)].modifier.contains(Modifier::BOLD), "row {y}");
        }
        test_support::assert_no_colour(w, h, |f| draw(f, &a));
    }

    /// A frame every [`note::SPIN`], with redraws asked at that pace while it
    /// is on screen.
    #[test]
    fn the_spinner_turns_with_the_clock() {
        let mut a = three();
        assert!(a.spinning());
        a.tick(note::SPIN * 2);
        let (w, h) = (60, 12);
        let lines = render(&a, w, h);
        let buf = test_support::buffer(w, h, |f| draw(f, &a));
        let y = line_of(&lines, "api-server");
        assert_eq!(buf[(bar(&buf, y)[0] - SIGN_OUT, y)].symbol(), "⠹");

        let still = noted(
            &["api-server"],
            vec![("api-server", note(Kind::Busy(Some(42)), "lua_ls", ""))],
        );
        assert!(!still.spinning(), "a gauge does not turn");
        assert!(!app(&["api-server"]).spinning(), "nor does nothing");
    }

    /// The caption follows the selection; a session without a note has none.
    #[test]
    fn the_selected_sessions_note_is_spelt_out_under_the_list() {
        let mut a = three();
        let (w, h) = (60, 12);
        let lines = render(&a, w, h);
        let last = line_of(&lines, "dotfiles") as usize;
        assert_eq!(lines[last + 1], "", "a blank row first");
        assert_eq!(lines[last + 2].trim(), "claude: working · 2m");
        let (left, right) = test_support::padding(&lines[last + 2], w as usize);
        assert!(left.abs_diff(right) <= 1, "{:?}", lines[last + 2]);
        let buf = test_support::buffer(w, h, |f| draw(f, &a));
        let x = left as u16;
        assert!(buf[(x, last as u16 + 2)].modifier.contains(Modifier::DIM));

        a.on_key(Key::Char('j'));
        let lines = render(&a, w, h);
        assert_eq!(
            lines[last + 2].trim(),
            "Claude Code · Claude needs your permission · 2m"
        );
        a.on_key(Key::Char('j'));
        let lines = render(&a, w, h);
        assert_eq!(lines[last + 2], "", "dotfiles has nothing to say");
    }

    /// No caption while asking `[y/N]` or moving a session; signs stay.
    #[test]
    fn the_caption_keeps_out_of_a_question() {
        let mut a = three();
        a.on_key(Key::Char('x'));
        let lines = render(&a, 60, 12);
        let last = line_of(&lines, "dotfiles") as usize;
        assert_eq!(lines[last + 2], "", "{lines:#?}");
        assert!(lines[line_of(&lines, "blog") as usize].contains('∗'));
    }

    /// Without a spare row or column they are skipped, never drawn over
    /// anything.
    #[test]
    fn signs_and_the_caption_need_room() {
        let a = three();
        // Three rows of list, and the hint row: no row under the list.
        let lines = render(&a, 60, 4);
        assert!(!lines.iter().any(|l| l.contains("working")), "{lines:#?}");
        // The list is sixteen wide; at sixteen columns there is no gutter.
        let lines = render(&a, 16, 8);
        assert!(!lines.iter().any(|l| l.contains('∗')), "{lines:#?}");
        for (w, h) in test_support::TINY_SIZES {
            render(&a, *w, *h);
        }
    }
}
