//! Rendering. Pure functions of [`App`] state, so the whole screen can be
//! asserted against a `TestBackend` without a terminal.
//!
//! # Colour
//!
//! Nothing here sets a foreground or background colour. Every distinction is
//! carried by a *modifier* — reversed and bold for the selection, dim for the
//! hint line and the row numbers — which means the picker uses the terminal's
//! own palette, inherits its background, and is `NO_COLOR`-correct by
//! construction rather than by remembering to check a flag at each call site.
//!
//! This matters more than it looks. crossterm's own `NO_COLOR` handling turns
//! `SetForegroundColor(c)` into a bare `ESC[m`, which is a *full SGR reset*: it
//! wipes bold, reverse and dim mid-line. Any design that sets colours and then
//! relies on that flag renders incorrectly for exactly the users who asked for
//! no colour.

use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::app::{App, Mode};
use super::backspace::Backspace;
use super::landing::{Landing, Side, Tier};
use super::starfield;
use super::swap::Dot;

/// The marker on the selected row. Unselected rows are indented to match, so
/// names stay in one column and nothing shifts as the selection moves.
const MARKER: &str = "▸ ";
const INDENT: &str = "  ";

/// The marker on the row being moved.
///
/// Must be exactly as wide as [`MARKER`], or every name jumps a column at the
/// moment the eye is following one of them. `⇕` (U+21D5) rather than the more
/// obvious `↕` (U+2195): the latter carries the Emoji property, so a terminal
/// applying emoji presentation paints it two columns wide while `unicode-width`
/// still reports one — the very defect that blanking the number column, rather
/// than collapsing it, is there to avoid.
const GRABBED: &str = "⇕ ";

/// The marker on the row standing in a gap made for a new session (see
/// [`App::make_room`]): a session that is not there yet.
///
/// Exactly as wide as [`MARKER`], for the reason [`GRABBED`] is.
const GHOST: &str = "+ ";

/// The mark on the row of the session `Esc` goes back to — the one
/// `<prefix> Space` came back to the picker from (see [`App::back_to`]).
///
/// In a gutter left of the list, one blank column out from the marker column,
/// rather than in it: the picker opens with the cursor on that very row, so
/// its marker column already holds [`MARKER`], and a mark there would be gone
/// the one time it is most wanted. Out there it is in the same column on every
/// row, the way Neovim's sign column is, and never on a row.
///
/// A dot rather than anything that points: beside [`MARKER`] on arrival, an
/// arrow would read as a second cursor. The bullet operator, U+2219, rather
/// than the bullet or the middle dot, which are of ambiguous width and drawn
/// two columns wide by a terminal set up for East Asian text; this one is
/// one column everywhere [`MARKER`] is.
///
/// Dim, like the numbers: a reminder, not a second selection. And still,
/// drawn here rather than painted over the frame by [`super::effects`], so the
/// fade that brings the picker up has it from the first frame.
const BACK: &str = "∙";

/// How many columns left of the list [`BACK`] goes: far enough that a blank
/// column keeps it off the marker.
const BACK_OUT: u16 = 2;

/// The prompt cursor.
const CURSOR: &str = "▋";

/// Only a floor for the degenerate case of every name being empty; the block is
/// otherwise sized to its content. A minimum wider than the content would push
/// short names visibly left of centre, because names are left-aligned *within*
/// the block so they line up with each other.
const MIN_LIST_WIDTH: u16 = 4;
const MAX_LIST_WIDTH: u16 = 48;

/// Columns between the number and the name.
const NUM_GAP: &str = "  ";

/// What the list's width allows past the end of its longest name, so that name
/// does not end flush against the edge of the selection's bar — the marker
/// already gives it room on the left.
///
/// The bar itself runs the whole width of the list on every row, the pad
/// included, rather than stopping past whichever name it is on: the same bar
/// whatever is selected, so moving the selection moves only the bar, and its
/// right edge does not jump in and out with the length of each name.
const PAD: &str = " ";

/// Sixty-nine columns, and it used to be sixty exactly — the widest row that
/// still fits a small terminal without truncation. `␣ order` is what that budget
/// was spent on: reordering is the one picker command nobody would find unaided,
/// since it leaves no trace on screen until it is used and `?` shows the
/// `<prefix>` keys rather than these.
///
/// So below sixty-nine columns this row now truncates from the right, where
/// before it never did. `q quit` is therefore the first entry to go, which is
/// the order to want: quitting is the one thing every user of a full-screen
/// program tries unprompted, and `Ctrl-c` quits as well.
///
/// The space bar is `␣` — [`crate::keys::SPACE_GLYPH`], where that decision now
/// lives — rather than the `Space` that [`crate::keys::key_label`] spells it in
/// the help screen's key column and the README, because this row is uniformly
/// lowercase (`esc`, never `Esc`) and a glyph is neither. The same reason `⏎`
/// and `↑↓` stand for Enter and the arrows here and are written out in the
/// README — and the reason the hint bar the prefix puts up ([`crate::hint`])
/// spells its keys the same way. A test below keeps the two rows agreeing.
///
/// `?` is still not listed, for the reason above.
const HINTS: &str = "↑↓ move  ⏎ attach  c new  r rename  x kill  ␣ order  / filter  q quit";
const EMPTY: &str = "no sessions — press c to create one";

/// What the hint row says while a session is in flight. Dim, like the hints it
/// stands in for: it is the same kind of thing — a reminder of which keys are
/// live — and the row that says an edit is open is the one wearing the marker,
/// not this one. The filter and the kill confirm stay undimmed because they are
/// something being asked or typed, which this is not.
///
/// Only `⏎` is offered for placing, though Space places too and keeps doing so:
/// the key that picked the session up still puts it down, and a user who
/// reached for it once will reach for it again. Naming both spends columns to
/// teach a choice nobody has to make — one row, one way to say "done", and the
/// one to name is the key that already means confirm everywhere else a mode is
/// open. Space stays the undocumented half of the pair, harmless to find by
/// habit and never needed by anyone reading the row.
const REORDER_HINTS: &str = "↑↓ move  ⏎ place  esc cancel";

pub fn draw(frame: &mut Frame, app: &App) {
    let (text, dim) = hint_line(app);
    screen(frame, &text, dim, |frame, body| draw_list(frame, app, body));
}

/// Every full-screen view is the same shape: a body, then one hint row on the
/// last line — and nothing at all on a frame with no rows, since there is no
/// last line to take off it. One function so the zero-size guard and the split
/// cannot be separated at a call site.
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

/// The layout every screen shares: the body above, one hint row on the last
/// line. Same split everywhere, so the screens line up across a transition.
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

/// The one-row line at the bottom of every screen: centred, truncated rather
/// than wrapped (the row owns exactly one line, and wrapping would push the
/// body up and make the layout jump), dim when it is a hint rather than
/// something the user is being told.
///
/// Two lines that are not hint rows are drawn through it as well, being the
/// same kind of thing — one dim sentence, centred, cut to fit: the empty
/// picker's "no sessions", and the attaching screen's status. Each stands in
/// for a list that is not there, on the row [`centre_vertically`] picks.
pub(super) fn draw_hint_row(frame: &mut Frame, area: Rect, text: &str, dim: bool) {
    let text = truncate(text, area.width as usize);
    let style = if dim {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    };
    let para = Paragraph::new(Line::from(Span::styled(text, style))).alignment(Alignment::Center);
    frame.render_widget(para, area);
}

fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    // The rows a filter keystroke just dropped are drawn too, where they stood,
    // for as long as they fade (see `effects`). Drawn plain: the fade is the
    // post-pass's, so this function still sets no colour.
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

    // The number column is kept but left blank while a session is being moved.
    // Dropping it outright would narrow the centred block by `num_width` plus
    // the gap and slide every name sideways exactly as one of them is being
    // watched — the same thing the matching indent on unselected rows refuses.
    // Hiding the digits is a display choice and not a correctness one: the
    // numbers stay where they are on screen throughout a move, and it is the
    // sessions that travel between them. They go away because the digit keys are
    // dead in this mode, and a column of numbers changing owner under a moving
    // row is noise.
    //
    // A session being put down still looks in flight while its trail is pulled
    // in: the numbers come back on the frame it lands (see `landing`).
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
            // A session not made yet, standing in the gap made for it: dim
            // from end to end, as a suggestion is on the prompt it is about
            // to be named in, and never the selection.
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
            // The number is dim beside the name it stands for, except in the
            // selection's bar: bold and dim share one reset, and most
            // terminals cannot show both, so dimming it there would break the
            // bar rather than quieten the number.
            let num = if selected {
                0..0
            } else {
                MARKER.chars().count()..head.chars().count() - NUM_GAP.chars().count()
            };
            // The bar is the width of the list, whatever the name it is on.
            let name = if selected {
                let fill =
                    (block.width as usize).saturating_sub(head.width() + row.session.name.width());
                format!("{}{}", row.session.name, " ".repeat(fill))
            } else {
                row.session.name.clone()
            };
            row_line(&head, &name, num, &marked, block.width as usize, style)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), block);

    // Rows trading places step aside, whole cells at a time (see `swap`):
    // moved once drawn, so a row goes as it was drawn, bar and all, and before
    // the trails, which leave from wherever the bar now ends.
    for (line, row) in rows.iter().skip(offset).take(height as usize).enumerate() {
        let aside = app.swap().aside(&row.session.id);
        if aside != 0 {
            let at = Rect::new(block.x, block.y + line as u16, block.width, 1);
            step_aside(frame.buffer_mut(), at, aside);
        }
    }

    // Before what passes out past the ends of the rows, so a trail or a
    // landing's bounce runs over the mark while it is there, and leaves it
    // where it was.
    if let Some(id) = app.back_to() {
        draw_back(frame, app, block, offset, id);
    }
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

/// The landing's bounce and its spray, on the row just put down (see
/// [`super::landing`]): the reversed bar runs further at each end and back
/// again ([`super::landing::BOUNCE`]), and dust flies out of both ends, level with
/// the text, with a little kicked onto the rows above and below. Like the
/// trails, all of it goes over the terminal's own background either side of
/// the list, never over a row, and stops at the edge of the screen; the splash
/// stops at the edge of the list's `area` too, so it never reaches the hint
/// row. Dust only lands on blank cells.
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
    // The bar's ends are the list's, so a row away the splash flies from them
    // too: it lands beside the list however long the names, and never in the
    // gap inside one.
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

/// Draw `row` `aside` columns to the right of where it was drawn, or to the
/// left for a negative `aside`, leaving blank the cells it moves off: a row
/// stepping aside (see [`super::swap`]). Whatever would go past the edge of the
/// screen is dropped.
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

/// The mark on the row of session `id` ([`BACK`]): [`BACK_OUT`] columns left
/// of the list, stepping aside with the row, and not at all where there is no
/// room left of the list or on a row that is not on screen. None on a row
/// being erased, which is a session on its way out rather than one to go back
/// to.
fn draw_back(frame: &mut Frame, app: &App, block: Rect, offset: usize, id: &str) {
    if app.backspace().is_some_and(|(erasing, _)| erasing == id) {
        return;
    }
    let Some(line) = app
        .rows()
        .iter()
        .position(|r| r.session.id == id)
        .and_then(|index| index.checked_sub(offset))
        .filter(|line| *line < block.height as usize)
    else {
        return;
    };
    let screen = frame.area();
    let Some(x) = block
        .x
        .checked_add_signed(app.swap().aside(id))
        .and_then(|x| x.checked_sub(BACK_OUT))
        .filter(|x| *x >= screen.x && *x < screen.x + screen.width)
    else {
        return;
    };
    let dim = Style::default().add_modifier(Modifier::DIM);
    frame
        .buffer_mut()
        .set_string(x, block.y + line as u16, BACK, dim);
}

/// The sparks off the seams the rows of a session in flight slid past each
/// other on (see [`super::swap`]): braille, out of both ends of the list, along
/// the rows of dots either side of a seam. Like the trails they go over the
/// terminal's own background, never over a row, and stop at the edge of the
/// screen and of the list's `area`, so never on the hint row. A spark that
/// lands on a cell of the trail shares it, its dot added to the stars'; on
/// anything else, the bar of a session stepped aside included, it is not drawn.
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

/// The cell a spark's dot is in, on the screen of a list drawn in `block`
/// scrolled to `offset`, and the bit that lights it there: `None` when it is
/// off the top or the left of the screen.
fn spark_cell(dot: &Dot, block: Rect, offset: usize) -> Option<(u16, u16, u32)> {
    // In dots: the seam is the top edge of its row, and the list's ends are
    // the column of dots either side of it.
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

/// What comes before a row's name: its marker, its number (see
/// [`super::app::Row::num`]) right-aligned in the number column, and the gap.
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

/// One row, cut to `width`, with the characters at the `char` indices in
/// `dim` dimmed — the number column — and the letters of the name at the
/// `char` indices in `marked` underlined — the letters the filter matched (see
/// [`App::matched`]). Dim and underline are modifiers, so this still sets no
/// colour, and both sit on top of the row's own style.
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

/// The row a kill is erasing: as much of it as is left, and the cursor after
/// it, until both have gone (see [`super::backspace`]). The row is drawn as it
/// was the moment the kill was confirmed, marker and number included, but not
/// reversed: it is a row being cleared now, no longer the selection.
///
/// The cursor goes in the column after what is left, which for a row as wide
/// as the block is past its end, on the terminal's own background — and not
/// at all past the edge of the screen.
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

/// The trails behind a session in flight: stars flying off both ends of its
/// row — rightwards off the end of its bar, and leftwards off the marker — at
/// full weight where they leave it and dim where they trail away.
///
/// Drawn after the list and outside its block. The block is sized to the names
/// and centred on them, so making room for the trails inside it would widen it
/// and slide every name sideways the moment one was picked up — the very thing
/// the blank number column is kept to prevent. Past either end of a row there
/// is only the terminal's own background, so a trail can simply be laid over
/// it; on a terminal too narrow for all of one, it stops at the edge, keeping
/// the cells nearest the row.
///
/// Two weights, because these screens have two below bold: plain for the half
/// nearest the row, so the stars read as coming off it, and dim for the far
/// half. Modifiers, like everything else here, and never the grabbed row's own
/// reversed bar: that marks the session, and the trails are only behind it.
///
/// `pull` is how far both trails have been drawn back into the row, for a
/// session landing: 0 for a session still in flight.
///
/// A session stepped aside (see [`super::swap`]) takes its trails with it:
/// they leave from the ends of the bar where it is drawn, not of the list.
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

    // Off the end of the bar, which is the end of the list: the trail starts
    // in the column after its last cell.
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

    // Off the other end: the marker is the row's first column, so this trail
    // ends in the column before it, and runs the other way.
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

/// What the hint row says, and whether it is dim: a hint when nothing is being
/// asked or typed, and otherwise the thing being asked or typed, at full weight.
fn hint_line(app: &App) -> (String, bool) {
    match app.mode() {
        Mode::Confirm { prompt, .. } => (prompt.clone(), false),
        Mode::Filter => (format!("/{}{CURSOR}", app.filter()), false),
        // The query comes too, when there is one. A move of one visible row can
        // carry a session past several the filter is hiding, and that is the one
        // moment the screen must not also stop saying a filter is applied — the
        // reason the `/query` in normal mode exists at all.
        Mode::Reorder { .. } if !app.filter().is_empty() => {
            (format!("{REORDER_HINTS}  /{}", app.filter()), true)
        }
        Mode::Reorder { .. } => (REORDER_HINTS.to_string(), true),
        Mode::Normal => match app.message() {
            Some(msg) => (msg.to_string(), false),
            // A number waiting on another digit; without this the picker would
            // look like it had ignored the keystroke.
            None => match app.pending() {
                Some(n) => (format!("#{n}{CURSOR}"), false),
                None if !app.filter().is_empty() => {
                    // Or the list silently looks shorter than it is.
                    (format!("/{}", app.filter()), true)
                }
                None => (HINTS.to_string(), true),
            },
        },
    }
}

/// Where the list is drawn within the body `area`, and which visible row its
/// first line shows: the centred block, sized to its content, and the scroll
/// offset that keeps the selection inside it. `None` when there is nothing to
/// draw — an empty list, or no room.
///
/// One function for the renderer and for [`row_at`], so a click lands on the
/// row the eye sees rather than on a second opinion about where the rows are.
fn list_layout(app: &App, area: Rect) -> Option<ListLayout> {
    // Every row drawn, the ones fading out of a filter included: the block is
    // sized to what is on screen, and settles once they have gone.
    let rows = app.rows();
    if area.height == 0 || rows.is_empty() {
        return None;
    }
    let widest = rows
        .iter()
        .map(|r| r.session.name.width())
        .max()
        .unwrap_or(0) as u16;
    // Right-aligned in a column as wide as the longest number, so the names stay
    // in one column once the list runs past nine.
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
        .clamp(MIN_LIST_WIDTH, MAX_LIST_WIDTH)
        .min(area.width);

    let height = (rows.len() as u16).min(area.height);
    let mut block = centre(area, width, height);
    let gap = rows.iter().position(|r| r.ghost);
    // A gap pushes the rows under it down and leaves the rows over it where
    // they stood. Centred afresh on one row more, the list would rise a line
    // on every other terminal height instead, the highlight with it, and the
    // gap would read as the rows above making way rather than those below.
    // So the top stays where the list had it without the gap, while there is
    // room under it for the extra row.
    if gap.is_some() {
        let before = centre(area, width, height.saturating_sub(1));
        if before.y + height <= area.y + area.height {
            block.y = before.y;
        }
    }
    // A gap made for a new session is what the screen is about while it is
    // open, and it is the row after the selection: kept in view, the
    // selection above it comes with it on any list more than a row high.
    let focus = gap.unwrap_or_else(|| app.selected_row());
    let offset = scroll_offset(focus, rows.len(), height as usize);
    Some(ListLayout {
        block,
        offset,
        num_width,
    })
}

/// See [`list_layout`].
struct ListLayout {
    /// The centred block the rows are drawn in.
    block: Rect,
    /// The visible row on the block's first line.
    offset: usize,
    /// Columns the number column takes.
    num_width: usize,
}

/// The visible row drawn on terminal cell (`column`, `row`), if any, with
/// `area` the whole frame as it was last drawn.
///
/// A row is the full width of the terminal, not only the centred block: the
/// picker has one column, so nothing else can be meant by a click level with a
/// row, and a target the width of a short name is a poor one for a touchpad.
/// The hint row and the empty space above and below the list are nothing.
pub(super) fn row_at(app: &App, area: Rect, column: u16, row: u16) -> Option<usize> {
    // As `screen` guards before splitting: a frame with no rows has no hint row
    // to take off it.
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
    // An index into the rows drawn, which a row fading out of a filter or one
    // standing in a gap is; the answer is an index into the visible ones,
    // which neither is.
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

/// Where the row of session `id` is drawn, as wide as the selection's bar is
/// on it — the whole width of the list, whatever the length of the name (see
/// [`PAD`]) — with `area` the whole frame — or `None` when it is not on
/// screen. What the effects' post-pass paints over (see [`super::effects`]),
/// measured by the same layout the rows were drawn with.
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

/// Where the name of session `id` is drawn: its row ([`row_rect`]) after the
/// marker, the number and the gap, and short of the blanks the bar runs on
/// past it, cut where the row is cut. `None` when the row is not on screen, or
/// so narrow that none of the name is.
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

/// Whether terminal cell (`column`, `row`) is part of a row as drawn — its
/// marker, number, gap or name, the blanks among them included — rather than
/// the terminal's own background around the list. What lets the effects lay
/// glyphs beside rows without one landing in the space of a name like
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

/// Centre a `height`-row block vertically, leaving it the full width.
///
/// The primitive for one line of centred text: the paragraph keeps the whole
/// width and its own `Alignment::Center` does the rest, which is what
/// [`centre`] cannot do — that sizes the rect to the content. Two callers, and
/// both are a single dim line standing in for a list: the empty picker's "no
/// sessions", and the attaching screen's spinner.
pub(super) fn centre_vertically(area: Rect, height: u16) -> Rect {
    Rect {
        y: area.y + (area.height.saturating_sub(height)) / 2,
        height: height.min(area.height),
        ..area
    }
}

/// Truncate to a display width, counting grapheme width rather than bytes or
/// `char`s so CJK names and emoji do not overflow the block they were measured
/// into.
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
        // Seventy columns because `HINTS` is sixty-nine: narrower and the row
        // is truncated, and "the hints are on the last row" would be asserted
        // against whatever happened to survive.
        let lines = render(
            &app(&["api-server", "dotfiles", "scratch", "notes"]),
            70,
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

    /// The block is sized to fit the number column, or the names it was
    /// measured for would be truncated by exactly the width of the number.
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

    /// A half-typed number is shown, or the picker looks like it swallowed the
    /// keystroke.
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

    /// The numbers go away while a session is in flight, as asked: the digit
    /// keys are dead in this mode and a column of numbers changing owner under a
    /// moving row is noise.
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

    /// The column is blanked, not removed. Collapsing it would narrow the
    /// centred block and slide every name sideways at the exact moment the eye
    /// is following one of them.
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

    /// Space places too, but the row names only `⏎`: one way to say "done" is
    /// enough to teach, and the space bar is there for the hand that picked the
    /// session up with it rather than for anyone reading this row.
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

    /// Two hint rows now name keys: this one, and the bar the prefix puts up over
    /// an attached session ([`crate::hint`]). They are the same row on different
    /// screens, and a user who learns `␣` on one must not meet `space` on the
    /// other — so the glyph is [`crate::keys::SPACE_GLYPH`]'s to decide, and this
    /// is the literal above agreeing with it.
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

    /// One visible row of movement can carry a session past several the filter
    /// is hiding. That is the one moment the screen must not also stop saying a
    /// filter is applied.
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
        let lines = render(&app(&[]), 70, 9);
        let content: Vec<&String> = lines[..8].iter().filter(|l| !l.is_empty()).collect();
        assert_eq!(content.len(), 1, "expected one line, got {content:?}");
        assert!(content[0].contains("no sessions"), "got {:?}", content[0]);
        assert!(content[0].contains("press c to create one"));
        assert!(lines[8].contains("quit"), "hints should still be present");
    }

    /// The picker's own prompts — the filter and the kill confirm — stay on the
    /// hint row: no popup, no border, no shift in the list above. Naming is the
    /// exception, and hands off to the full-screen prompt.
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
            // Wide enough for the whole of `HINTS`: at sixty columns `quit` no
            // longer fits either, and the assertion below would pass whether or
            // not the prompt had replaced anything.
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

    /// `HINTS` no longer fits every terminal, so which entry goes first is a
    /// decision rather than an accident. `q quit` is the one to lose: it is what
    /// every user of a full-screen program tries unprompted, and `Ctrl-c` quits
    /// as well — whereas `␣ order` is the entry the row grew to carry.
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

    /// The README's picture of the picker reproduces this row byte for byte, and
    /// nothing else ties the two together — so without this the art quietly
    /// describes a program that no longer exists. `keys.rs` pins its own README
    /// table the same way.
    ///
    /// Whether to carry the art at all is the README's own call — it dropped it
    /// once, for the recorded demo, which shows the picker moving and cannot go
    /// stale — so what is pinned is the copy and not its presence. [`MARKER`] is
    /// how the picture announces itself: it is drawn by the same screen and
    /// appears nowhere else in the prose. Equality rather than an implication,
    /// because either one alone is the thing this test is for: a hint row under
    /// a selection marker nvmux no longer draws is as stale as the reverse.
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
        // CJK names are two columns per character; a byte- or char-based width
        // calculation would overflow the terminal here.
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
        // Thirty sessions means two-digit numbers. Whatever is on screen, the
        // names line up in one column: the marker and the number column are
        // both fixed width, so nothing shifts as the selection moves.
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

    /// The line each name was rendered on.
    fn line_of(lines: &[String], name: &str) -> u16 {
        lines
            .iter()
            .position(|l| l.contains(name))
            .unwrap_or_else(|| panic!("{name} not on screen: {lines:#?}")) as u16
    }

    /// A click on the line a row is drawn on names that row, whatever column it
    /// lands in — measured against the renderer rather than against a copy of
    /// its arithmetic.
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

    /// With the list scrolled, the first line on screen is not row zero.
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

    /// A list longer than the body fills every line of it; the hint row is
    /// still not part of the list.
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

    /// The picker must never emit an SGR colour, so it inherits the terminal's
    /// palette and background and is correct under NO_COLOR without needing to
    /// test for it.
    #[test]
    fn nothing_sets_a_colour() {
        let mut a = app(&["one", "two", "three"]);
        a.on_key(Key::Char('j'));
        test_support::assert_no_colour(50, 8, |f| draw(f, &a));

        let b = reordering(&["one", "two", "three"], 1);
        test_support::assert_no_colour(50, 8, |f| draw(f, &b));
    }

    /// Both hint rows are hints, so both are dim. The prompts are not — the
    /// filter and the kill confirm are being typed or asked, and would recede
    /// into the wallpaper if they were.
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

    /// The trail starts in the column after the grabbed row's bar — the
    /// list's own edge, past the pad after its longest name, however short the
    /// grabbed one — runs `TRAIL` cells, plain for the near half and dim for
    /// the far half, and is not part of the reversed bar.
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

    /// Stars only ever appear behind the session in flight: not on its
    /// neighbours, and not at all when nothing is picked up.
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

    /// On a terminal that ends before the trail does, it is cut off at the
    /// edge rather than drawn past it.
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

    /// The other trail mirrors the first: it ends in the column before the
    /// marker, runs `TRAIL` cells leftwards, plain for the half nearest the
    /// row and dim for the far half.
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

    /// `[effects.move] enabled = false`: the session is still picked up, marked and
    /// moved, with nothing streaming off either end of it.
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

    /// The letters the query matched are underlined and the rest are not —
    /// fuzzily, so `asv` marks three letters of `api-server` spread apart.
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

    /// The underline is not an effect: with the picker's effects off it is
    /// still there, and with no query there is nothing to underline.
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

    /// The selection's bar is the same on every row: from the marker to one
    /// blank cell past the end of the longest name, however short the name it
    /// is on, and no further. No other row is reversed anywhere.
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

    /// The pad is allowed for in every row's width, so the block stays put
    /// as the selection moves onto the longest name.
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

    /// The name's own cells stop where the name does: the blanks the bar
    /// runs on past it are the row's, not the name's, so what is struck or
    /// handed off is the name. The row runs as far as the bar does, which is
    /// the same on every row.
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

    /// The number is dim beside its name on every row but the selected one,
    /// whose bar stays whole; the names themselves are never dimmed.
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

    // --- the mark on the session come back from ------------------------------

    /// `names` in the picker, opened back over the second row the way
    /// `<prefix> Space` opens it.
    fn back_over(names: &[&str]) -> App {
        let mut a = app(names);
        a.select_session("id000001");
        a.set_came_from("id000001");
        a
    }

    /// Every cell the mark is in, as (column, line).
    fn marks(buf: &Buffer) -> Vec<(u16, u16)> {
        let area = buf.area;
        (area.y..area.y + area.height)
            .flat_map(|y| (area.x..area.x + area.width).map(move |x| (x, y)))
            .filter(|(x, y)| buf[(*x, *y)].symbol() == BACK)
            .collect()
    }

    /// Back from a session, its row is marked left of the list, one blank
    /// column out from the cursor's marker, dim, and in no colour; no other
    /// row is.
    #[test]
    fn the_session_come_back_from_is_marked_left_of_its_row() {
        let a = back_over(&["api-server", "dotfiles", "notes"]);
        let buf = test_support::buffer(40, 8, |f| draw(f, &a));
        let lines = render(&a, 40, 8);
        let y = line_of(&lines, "dotfiles");
        let start = bar(&buf, y)[0];
        assert_eq!(buf[(start, y)].symbol(), MARKER.trim_end());
        let x = start - BACK_OUT;
        assert_eq!(marks(&buf), [(x, y)], "{lines:#?}");
        assert_eq!(buf[(x + 1, y)].symbol(), " ", "a blank between");
        assert!(buf[(x, y)].modifier.contains(Modifier::DIM));
        test_support::assert_no_colour(40, 8, |f| draw(f, &a));
    }

    /// The mark stays on its row when the cursor leaves it, in the same
    /// column: the gutter left of the list, whatever the row.
    #[test]
    fn the_mark_stays_on_its_row_when_the_cursor_leaves() {
        let mut a = back_over(&["api-server", "dotfiles", "notes"]);
        let before = marks(&test_support::buffer(40, 8, |f| draw(f, &a)));
        a.on_key(Key::Char('j'));
        let buf = test_support::buffer(40, 8, |f| draw(f, &a));
        assert_eq!(marks(&buf), before);
        let y = line_of(&render(&a, 40, 8), "notes");
        assert!(!bar(&buf, y).is_empty(), "the cursor is on notes");
    }

    /// The mark is still: it asks for no frames, and time changes nothing
    /// about it.
    #[test]
    fn the_mark_is_still() {
        let mut a = back_over(&["api-server", "dotfiles", "notes"]);
        assert!(!a.animating());
        let first = test_support::buffer(40, 8, |f| draw(f, &a));
        a.tick(Duration::from_secs(10));
        assert_eq!(test_support::buffer(40, 8, |f| draw(f, &a)), first);
    }

    /// Nothing is marked when the picker came back from no session, from one
    /// that is not listed, or with `[effects.back]` off — and once a fresh
    /// listing no longer has the session, its mark goes with its row.
    #[test]
    fn only_a_listed_session_come_back_from_is_marked() {
        let names = ["api-server", "dotfiles", "notes"];
        let none = |a: &App| marks(&test_support::buffer(40, 8, |f| draw(f, a))).is_empty();
        assert!(none(&app(&names)), "not opened from a session");

        let mut gone = app(&names);
        gone.set_came_from("gone");
        assert!(none(&gone), "not listed");

        let mut off = app(&names);
        off.set_effects(false);
        off.set_came_from("id000001");
        assert!(none(&off), "switched off");

        let mut killed = back_over(&names);
        assert!(!none(&killed));
        let listed = killed.sessions().to_vec();
        killed.set_sessions(vec![listed[0].clone(), listed[2].clone()]);
        assert!(none(&killed), "listed no longer");
    }

    /// A row being erased is no longer one to go back to, and loses its mark
    /// as the backspace starts.
    #[test]
    fn a_row_being_erased_is_not_marked() {
        let mut a = erasing(0);
        a.set_came_from("id000001");
        assert!(marks(&test_support::buffer(44, 8, |f| draw(f, &a))).is_empty());
    }

    /// The session come back from, picked up, trails stars over its mark;
    /// put down, the mark is back where it was.
    #[test]
    fn a_trail_runs_over_the_mark_and_leaves_it() {
        let mut a = back_over(&["api-server", "dotfiles", "notes"]);
        a.set_trail(true);
        let before = marks(&test_support::buffer(40, 8, |f| draw(f, &a)));
        a.on_key(Key::Char(' '));
        assert!(a.trailing());
        assert!(marks(&test_support::buffer(40, 8, |f| draw(f, &a))).is_empty());
        a.on_key(Key::Enter);
        a.tick(Duration::from_secs(5));
        assert!(!a.animating());
        assert_eq!(marks(&test_support::buffer(40, 8, |f| draw(f, &a))), before);
    }

    /// Where the list runs to the edge of the screen there is no gutter left of
    /// it, and no mark; and no size of terminal makes the mark panic.
    #[test]
    fn the_mark_stops_at_the_edge_of_the_screen() {
        let a = back_over(&["a-session-with-a-long-name", "another-long-one"]);
        assert!(marks(&test_support::buffer(30, 6, |f| draw(f, &a))).is_empty());
        for &(w, h) in test_support::TINY_SIZES {
            test_support::buffer(w, h, |f| draw(f, &a));
        }
    }

    // --- rows fading out of a filter ----------------------------------------

    /// A row a filter keystroke just dropped is drawn where it stood, but it is
    /// not there to click: a click on it names nothing, and a click below it
    /// names the visible row it is drawn over.
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

    /// At first the row is all there with the cursor after it, though no
    /// longer the reversed bar; midway it is the start of the row and the
    /// cursor; at the end there is nothing left of it, and the rows around it
    /// have not moved.
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

    /// A listing after the kill is the truth: the erased row is gone with it,
    /// and a row whose kill failed comes back whole.
    #[test]
    fn a_fresh_listing_ends_the_backspace() {
        let mut a = erasing(1_000);
        let same = a.sessions().to_vec();
        a.set_sessions(same);
        assert!(a.backspace().is_none());
        assert!(render(&a, 44, 8).iter().any(|l| l.contains("dotfiles")));
    }

    /// `[effects.kill] enabled = false`: a confirmed kill erases nothing.
    #[test]
    fn with_the_kill_effect_off_there_is_no_backspace() {
        let mut a = app(&["api-server", "dotfiles"]);
        a.set_effects(false);
        assert!(!a.start_backspace("id000001"));
        assert!(!a.animating());
    }

    // --- the swap -----------------------------------------------------------

    /// The column `name` starts at on its row, in cells.
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

    /// On the key, the session carried is drawn [`swap::ASIDE`] cells right
    /// of the column and the one it passed as far left; a row the move did not
    /// pass stays put; once the sidestep is over, every name is back in the
    /// column. None of it sets a colour.
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

    /// The bar steps aside whole, still the width of the list, and its trails
    /// go with it: they leave from where the bar now ends, plain and then dim,
    /// rather than from the list's edge.
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

    /// With the trail off, a move is drawn as it always was: both rows jump,
    /// and nothing steps aside or sparks.
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

    /// A move's sparks fly off the seam the two rows slid past each other on.
    /// The row over the seam has no trail of its own, so every dot there is a
    /// spark: beside the list at both ends, never on the row's text, gone once
    /// the sparks are. The rows further off get none.
    #[test]
    fn a_move_throws_sparks_off_the_seam() {
        let names = ["api-server", "docs", "notes", "web"];
        let still = trailing(&names, 1);
        let buf = test_support::buffer(50, 10, |f| draw(f, &still));
        let was = bar(&buf, line_of(&render(&still, 50, 10), "docs"));
        let (first, end) = (was[0], was[was.len() - 1] + 1);

        // Down a row: the seam is the top edge of the row it lands on, under
        // the row of the session it passed.
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

    /// Sparks stay within the list's area, whatever the list's scroll and
    /// wherever the seam: none reaches the hint row, and a seam scrolled off
    /// the screen draws nothing rather than panicking.
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

    /// The third session of four picked up, carried up a row and put down,
    /// `ms` milliseconds ago, with the trail and its landing on — so it lands
    /// with a row on either side of it.
    fn landed(ms: u64) -> App {
        let mut a = trailing(&["api-server", "docs", "notes", "nvmux"], 2);
        a.tick(std::time::Duration::from_millis(200));
        a.on_key(Key::Char('k'));
        // The move's own sparks and sidestep over first, so every dot here is
        // the landing's.
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

    /// Where the landed bar starts and ends on row `y`: the marker's column,
    /// and the column past the pad after the list's longest name,
    /// `api-server`, not the landed `notes`.
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

    /// The bar's own reach on row `y` either side of it at `ms` after the
    /// landing: how many reversed cells it runs to before `first` and from
    /// `end`.
    fn reach(a: &App, y: u16) -> (u16, u16) {
        let buf = test_support::buffer(50, 8, |f| draw(f, a));
        let (first, end) = bar_ends(&buf, y);
        let reversed = |x: u16| buf[(x, y)].modifier.contains(Modifier::REVERSED);
        let before = (1..=first).take_while(|d| reversed(first - d)).count() as u16;
        let after = (end..buf.area.width).take_while(|x| reversed(*x)).count() as u16;
        (before, after)
    }

    /// While the trail is pulled in the row still looks in flight; on impact
    /// the numbers come back and the bar runs two cells further at both ends,
    /// with blanks, and the rows beside it are left as they are.
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

    /// After the impact, dust flies out of both ends on the row's own line,
    /// in braille lit only in the middle rows of dots — and none of it sets a
    /// colour.
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

    /// The splash lands on the rows either side, from both ends: beside the
    /// list, past the end of the landed bar — which is past every name — never
    /// on a neighbour's text, and in the dots nearest the landed row.
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

    /// The splash stays within the list's area: none of it reaches the hint
    /// row, though the landed row sits right above it. A short message there
    /// leaves the row blank enough for a grain to land on, if one could.
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

    /// Once it has settled there is nothing left of it, and the picker stops
    /// asking for frames.
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

    /// A cancelled move lands nowhere, and with the trail off there is no
    /// landing either: it is how the trail ends.
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

    /// The rows under the highlight step down a line, and the new session
    /// stands in the gap with a `+` for its marker, the number it will have and
    /// the name the prompt will offer — in the same columns as every other
    /// row, and dim from end to end. The row below has taken the next number.
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

    /// The rows above the gap stay where they stood on any terminal height,
    /// and the rows below step down: centring four rows where three were would
    /// otherwise lift the list a line on every other height.
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

    /// With no row to spare under the list, the gap takes the one it needs
    /// from above instead, rather than pushing the last row off the screen.
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

    /// The row in the gap is not a row anything can be done to: a click on it
    /// names nothing, and the rows around it are still the rows they were.
    #[test]
    fn the_new_session_cannot_be_clicked() {
        let a = making_room();
        let area = Rect::new(0, 0, 44, 9);
        let lines = render(&a, 44, 9);
        assert_eq!(row_at(&a, area, 20, line_of(&lines, "session 4")), None);
        assert_eq!(row_at(&a, area, 20, line_of(&lines, "dotfiles")), Some(1));
        assert_eq!(row_at(&a, area, 20, line_of(&lines, "notes")), Some(2));
    }

    /// On a list longer than the screen, with the highlight on the last row
    /// in view, the list scrolls so the gap is in view too.
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

    /// The number column is as wide as the widest number shown, the one the
    /// last row takes once it has moved down included.
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

    /// Closed, the rows are back where they were, under their own numbers.
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
}
