//! Decodes `redraw` notifications into events for the client's model.
//!
//! A notification carries batches, each a name and one or more argument
//! lists: `["grid_line", [1, 0, 0, …], [1, 1, 0, …]]`. Each list becomes one
//! [`Event`]. Neovim adds parameters over time (`win_float_pos` has gained
//! four), so extra arguments are ignored; a list too short or
//! malformed, or an unknown event, is dropped alone rather than with its
//! batch. The client asks for its event set (see `super::App`), so unknown
//! events come from a newer Neovim.
//!
//! Decoded from borrowed values ([`rmpv::ValueRef`]) so cell text is copied
//! once; `grid_line`, nearly all of the traffic, is read straight from the
//! bytes without a value tree ([`decode_bytes`]).

use rmpv::ValueRef;

use super::grid::Text;
use super::style::{Attrs, DefaultColors, Underline};

/// One thing a `redraw` notification says.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    GridResize {
        grid: u64,
        width: usize,
        height: usize,
    },
    DefaultColors(DefaultColors),
    HlAttr {
        id: u32,
        rgb: Attrs,
        cterm: Attrs,
    },
    HlGroup {
        name: String,
        id: u32,
    },
    GridLine {
        grid: u64,
        row: usize,
        col: usize,
        cells: Vec<LineCell>,
    },
    GridClear {
        grid: u64,
    },
    GridDestroy {
        grid: u64,
    },
    GridCursor {
        grid: u64,
        row: usize,
        col: usize,
    },
    /// `rows` above zero moves the region's text up, below zero down.
    GridScroll {
        grid: u64,
        top: usize,
        bot: usize,
        left: usize,
        right: usize,
        rows: i64,
    },
    WinPos {
        grid: u64,
        win: Option<i64>,
        row: usize,
        col: usize,
        width: usize,
        height: usize,
    },
    /// Where a float is, on the screen: Neovim works out the anchoring itself
    /// and says where that put it (`screen_row`, `screen_col`).
    WinFloatPos {
        grid: u64,
        win: Option<i64>,
        mouse: bool,
        zindex: i64,
        compindex: i64,
        row: i64,
        col: i64,
        /// The grid it is anchored to, whose text it moves with.
        anchor: Option<u64>,
    },
    WinHide {
        grid: u64,
    },
    WinClose {
        grid: u64,
    },
    MsgSetPos {
        grid: u64,
        row: usize,
        scrolled: bool,
        sep: String,
        zindex: i64,
        compindex: i64,
    },
    WinViewport {
        grid: u64,
        win: Option<i64>,
        topline: i64,
        botline: i64,
        curline: i64,
        curcol: i64,
        line_count: i64,
        scroll_delta: i64,
    },
    WinViewportMargins {
        grid: u64,
        top: usize,
        bottom: usize,
        left: usize,
        right: usize,
    },
    ModeInfoSet {
        enabled: bool,
        modes: Vec<ModeInfo>,
    },
    ModeChange {
        name: String,
        index: usize,
    },
    OptionSet {
        name: String,
        value: OptionValue,
    },
    BusyStart,
    BusyStop,
    MouseOn,
    MouseOff,
    Bell,
    VisualBell,
    Flush,
    SetTitle(String),
    /// Raw bytes for the terminal, Neovim's way of reaching it through a UI:
    /// an OSC 52 copy, or a query of its own (0.12 on).
    UiSend(Vec<u8>),
    /// The server is exiting with this status.
    ErrorExit(i64),
}

/// One run of cells in a `grid_line`: a text, the highlight it is drawn in if
/// that changes here, and how many cells in a row it fills.
#[derive(Debug, Clone, PartialEq)]
pub struct LineCell {
    pub text: Text,
    /// `None` is the previous run's highlight, as the protocol leaves it.
    pub hl: Option<u32>,
    pub repeat: usize,
}

/// A cursor's shape in one mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Block,
    Horizontal,
    Vertical,
}

/// How the cursor looks in one of Neovim's modes, from `'guicursor'`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModeInfo {
    pub name: String,
    /// `None` for the modes that have no cursor of their own — hovering a
    /// status line, dragging a separator.
    pub shape: Option<Shape>,
    /// How much of the cell a bar or an underline covers, in percent.
    pub percentage: u8,
    /// In milliseconds; any of the three at zero is a cursor that does not
    /// blink.
    pub blinkwait: u64,
    pub blinkon: u64,
    pub blinkoff: u64,
    /// The highlight the cursor is drawn in, or 0 for the terminal's own.
    pub attr_id: u32,
}

impl ModeInfo {
    pub fn blinks(&self) -> bool {
        self.blinkwait > 0 && self.blinkon > 0 && self.blinkoff > 0
    }
}

/// An option's value, as far as the client reads one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionValue {
    Bool(bool),
    Int(i64),
    Str(String),
    Other,
}

impl OptionValue {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            OptionValue::Bool(b) => Some(*b),
            OptionValue::Int(n) => Some(*n != 0),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            OptionValue::Int(n) => Some(*n),
            _ => None,
        }
    }
}

/// Every event in a `redraw` notification's parameters, in order. Unreadable
/// events are dropped and counted, never fatal.
pub fn decode(params: &ValueRef<'_>, into: &mut Vec<Event>) -> usize {
    let mut dropped = 0;
    let Some(batches) = params.as_array() else {
        return 1;
    };
    for batch in batches {
        let Some(batch) = batch.as_array() else {
            dropped += 1;
            continue;
        };
        let Some(name) = batch.first().and_then(as_str) else {
            dropped += 1;
            continue;
        };
        for args in &batch[1..] {
            let Some(args) = args.as_array() else {
                dropped += 1;
                continue;
            };
            match event(name, args) {
                Some(Some(event)) => into.push(event),
                // Known, and nothing the client acts on.
                Some(None) => {}
                None => {
                    tracing::debug!(name, "dropped a redraw event the client could not read");
                    dropped += 1;
                }
            }
        }
    }
    dropped
}

/// [`decode`] straight from the bytes: the same events and drops, without a
/// value tree for the whole notification. `grid_line` is read from the bytes
/// directly; every other event is decoded from its own argument list's bytes
/// and read by [`event`]. They differ only on a notification nested deeper
/// than rmpv follows, which [`decode`] never sees: this reads what it can.
pub fn decode_bytes(params: &[u8], into: &mut Vec<Event>) -> usize {
    let mut rd = Cursor { buf: params, at: 0 };
    let Some(batches) = rd.array_len() else {
        return slow(params, into);
    };
    let mut dropped = 0;
    for _ in 0..batches {
        let start = rd.at;
        let Some(len) = rd.array_len() else {
            dropped += 1;
            if rd.skip_from(start).is_none() {
                return dropped;
            }
            continue;
        };
        let name = match (len, rd.str_value()) {
            (1.., Some(Ok(name))) => name,
            _ => {
                dropped += 1;
                if rd.skip_from(start).is_none() {
                    return dropped;
                }
                continue;
            }
        };
        let line = name == "grid_line";
        for _ in 1..len {
            let start = rd.at;
            if line {
                match rd.grid_line() {
                    Some(Some(event)) => {
                        into.push(event);
                        continue;
                    }
                    // Not an array at all: dropped as `decode` drops it.
                    Some(None) => {
                        dropped += 1;
                        if rd.skip_from(start).is_none() {
                            return dropped;
                        }
                        continue;
                    }
                    // Anything else odd: the slow path says what it is.
                    None => rd.at = start,
                }
            }
            let Some(end) = rd.skip_from(start) else {
                return dropped + 1;
            };
            let mut sub = &params[start..end];
            let Ok(args) = rmpv::decode::read_value_ref(&mut sub) else {
                dropped += 1;
                continue;
            };
            let Some(args) = args.as_array() else {
                dropped += 1;
                continue;
            };
            match event(name, args) {
                Some(Some(event)) => into.push(event),
                Some(None) => {}
                None => {
                    tracing::debug!(name, "dropped a redraw event the client could not read");
                    dropped += 1;
                }
            }
        }
    }
    dropped
}

/// The bytes of a `redraw` notification's parameters, if `frame` is one:
/// `[2, "redraw", params, …]`, read as `super::App::message` reads it.
pub fn redraw_params(frame: &[u8]) -> Option<&[u8]> {
    let mut rd = Cursor { buf: frame, at: 0 };
    if rd.array_len()? < 3 || rd.uint()? != super::wire::NOTIFICATION {
        return None;
    }
    match rd.str_value()? {
        Ok("redraw") => Some(&frame[rd.at..]),
        _ => None,
    }
}

/// The id of the request `frame` answers, if it is a response: `[1, id, …]`,
/// with the kind and the id read as values, however they are written.
pub fn response_id(frame: &[u8]) -> Option<u64> {
    let mut rd = Cursor { buf: frame, at: 0 };
    if rd.array_len()? < 2 || rd.uint()? != super::wire::RESPONSE {
        return None;
    }
    rd.uint()
}

fn slow(params: &[u8], into: &mut Vec<Event>) -> usize {
    let mut rd = params;
    match rmpv::decode::read_value_ref(&mut rd) {
        Ok(v) => decode(&v, into),
        Err(_) => 1,
    }
}

/// A place in a msgpack value's bytes.
struct Cursor<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn byte(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.at)?;
        self.at += 1;
        Some(b)
    }

    fn be(&mut self, width: usize) -> Option<u64> {
        let bytes = self.buf.get(self.at..self.at + width)?;
        self.at += width;
        Some(bytes.iter().fold(0u64, |n, &b| (n << 8) | u64::from(b)))
    }

    /// An array's length, the cursor past its header.
    fn array_len(&mut self) -> Option<usize> {
        let save = self.at;
        let n = match self.byte()? {
            m @ 0x90..=0x9f => Some(usize::from(m & 0x0f)),
            0xdc => self.be(2).map(|n| n as usize),
            0xdd => self.be(4).map(|n| n as usize),
            _ => None,
        };
        if n.is_none() {
            self.at = save;
        }
        n
    }

    /// A `str`: its text, or its bytes when they are not UTF-8. `None`, and
    /// the cursor where it was, for anything else.
    fn str_value(&mut self) -> Option<Result<&'a str, &'a [u8]>> {
        let save = self.at;
        let len = match self.byte()? {
            m @ 0xa0..=0xbf => Some(usize::from(m & 0x1f)),
            0xd9 => self.be(1).map(|n| n as usize),
            0xda => self.be(2).map(|n| n as usize),
            0xdb => self.be(4).map(|n| n as usize),
            _ => None,
        };
        let Some(bytes) = len.and_then(|len| self.buf.get(self.at..self.at.checked_add(len)?))
        else {
            self.at = save;
            return None;
        };
        self.at += bytes.len();
        Some(std::str::from_utf8(bytes).map_err(|_| bytes))
    }

    /// An integer as [`ValueRef::as_u64`] reads one: any non-negative one,
    /// however it is written. `None`, and the cursor where it was, otherwise.
    fn uint(&mut self) -> Option<u64> {
        let save = self.at;
        let n = match self.byte()? {
            m @ 0x00..=0x7f => Some(u64::from(m)),
            0xcc => self.be(1),
            0xcd => self.be(2),
            0xce => self.be(4),
            0xcf => self.be(8),
            0xd0 => self
                .be(1)
                .map(|n| n as u8 as i8 as i64)
                .and_then(|n| u64::try_from(n).ok()),
            0xd1 => self
                .be(2)
                .map(|n| n as u16 as i16 as i64)
                .and_then(|n| u64::try_from(n).ok()),
            0xd2 => self
                .be(4)
                .map(|n| n as u32 as i32 as i64)
                .and_then(|n| u64::try_from(n).ok()),
            0xd3 => self
                .be(8)
                .map(|n| n as i64)
                .and_then(|n| u64::try_from(n).ok()),
            _ => None,
        };
        if n.is_none() {
            self.at = save;
        }
        n
    }

    /// Past the value that starts at `start`, wherever the cursor is now.
    fn skip_from(&mut self, start: usize) -> Option<usize> {
        let n = super::wire::frame_len(&self.buf[start..]).ok()??;
        self.at = start + n;
        Some(self.at)
    }

    /// Past `n` values from here.
    fn skip_n(&mut self, n: usize) -> Option<()> {
        for _ in 0..n {
            self.skip_from(self.at)?;
        }
        Some(())
    }

    /// One `grid_line` argument list: `Some(Some(_))` read, `Some(None)` not
    /// an array, `None` for anything else this does not read the way
    /// [`event`] would (it is then read by [`event`]).
    fn grid_line(&mut self) -> Option<Option<Event>> {
        let Some(len) = self.array_len() else {
            return Some(None);
        };
        if len < 4 {
            return None;
        }
        let grid = self.uint()?;
        let row = usize::try_from(self.uint()?).ok()?;
        let col = usize::try_from(self.uint()?).ok()?;
        let runs = self.array_len()?;
        let mut cells = Vec::with_capacity(runs);
        for _ in 0..runs {
            let n = self.array_len()?;
            if n == 0 {
                return None;
            }
            // Nearly every cell: one ASCII character, `fixstr` of one byte.
            let text = match self.buf.get(self.at..self.at + 2) {
                Some(&[0xa1, c]) if c < 0x80 => {
                    self.at += 2;
                    Text::Char(char::from(c))
                }
                _ => match self.str_value()? {
                    Ok(s) => Text::new(s),
                    Err(b) => Text::new(&String::from_utf8_lossy(b)),
                },
            };
            let hl = if n >= 2 {
                Some(u32::try_from(self.uint()?).ok()?)
            } else {
                None
            };
            let repeat = if n >= 3 {
                usize::try_from(self.uint()?).ok()?
            } else {
                1
            };
            if n > 3 {
                self.skip_n(n - 3)?;
            }
            cells.push(LineCell { text, hl, repeat });
        }
        self.skip_n(len - 4)?;
        Some(Some(Event::GridLine {
            grid,
            row,
            col,
            cells,
        }))
    }
}

/// One event: `None` for one that could not be read, `Some(None)` for one
/// read and ignored.
fn event(name: &str, a: &[ValueRef<'_>]) -> Option<Option<Event>> {
    Some(Some(match name {
        "grid_resize" => Event::GridResize {
            grid: uint(a.first()?)?,
            width: size(a.get(1)?)?,
            height: size(a.get(2)?)?,
        },
        "default_colors_set" => Event::DefaultColors(DefaultColors {
            rgb_fg: colour(a.first()?),
            rgb_bg: colour(a.get(1)?),
            rgb_sp: colour(a.get(2)?),
            // One-based, with 0 for none: unlike every other cterm colour
            // the protocol carries.
            cterm_fg: a
                .get(3)
                .and_then(int)
                .filter(|n| (1..=256).contains(n))
                .map(|n| (n - 1) as u8),
            cterm_bg: a
                .get(4)
                .and_then(int)
                .filter(|n| (1..=256).contains(n))
                .map(|n| (n - 1) as u8),
        }),
        "hl_attr_define" => Event::HlAttr {
            id: u32::try_from(uint(a.first()?)?).ok()?,
            rgb: attrs(a.get(1)?)?,
            cterm: attrs(a.get(2)?)?,
        },
        "hl_group_set" => Event::HlGroup {
            name: as_str(a.first()?)?.to_string(),
            id: u32::try_from(uint(a.get(1)?)?).ok()?,
        },
        "grid_line" => Event::GridLine {
            grid: uint(a.first()?)?,
            row: size(a.get(1)?)?,
            col: size(a.get(2)?)?,
            cells: line_cells(a.get(3)?)?,
        },
        "grid_clear" => Event::GridClear {
            grid: uint(a.first()?)?,
        },
        "grid_destroy" => Event::GridDestroy {
            grid: uint(a.first()?)?,
        },
        "grid_cursor_goto" => Event::GridCursor {
            grid: uint(a.first()?)?,
            row: size(a.get(1)?)?,
            col: size(a.get(2)?)?,
        },
        "grid_scroll" => Event::GridScroll {
            grid: uint(a.first()?)?,
            top: size(a.get(1)?)?,
            bot: size(a.get(2)?)?,
            left: size(a.get(3)?)?,
            right: size(a.get(4)?)?,
            rows: int(a.get(5)?)?,
        },
        "win_pos" => Event::WinPos {
            grid: uint(a.first()?)?,
            win: a.get(1).and_then(handle),
            row: size(a.get(2)?)?,
            col: size(a.get(3)?)?,
            width: size(a.get(4)?)?,
            height: size(a.get(5)?)?,
        },
        // `[grid, win, anchor, anchor_grid, anchor_row, anchor_col,
        // mouse_enabled, zindex, compindex, screen_row, screen_col]`. The last
        // four came later; only the screen position is needed, as Neovim
        // resolves the anchoring. The anchor grid is kept because a window the
        // client draws further in takes its floats along (see `super::wide`).
        "win_float_pos" => Event::WinFloatPos {
            grid: uint(a.first()?)?,
            win: a.get(1).and_then(handle),
            mouse: a.get(6).and_then(boolean).unwrap_or(true),
            zindex: a.get(7).and_then(int).unwrap_or(50),
            compindex: a.get(8).and_then(int).unwrap_or(0),
            row: int(a.get(9)?)?,
            col: int(a.get(10)?)?,
            anchor: a.get(3).and_then(uint),
        },
        "win_hide" => Event::WinHide {
            grid: uint(a.first()?)?,
        },
        "win_close" => Event::WinClose {
            grid: uint(a.first()?)?,
        },
        "msg_set_pos" => Event::MsgSetPos {
            grid: uint(a.first()?)?,
            row: size(a.get(1)?)?,
            scrolled: boolean(a.get(2)?)?,
            sep: a.get(3).and_then(as_str).unwrap_or(" ").to_string(),
            zindex: a.get(4).and_then(int).unwrap_or(200),
            compindex: a.get(5).and_then(int).unwrap_or(0),
        },
        "win_viewport" => Event::WinViewport {
            grid: uint(a.first()?)?,
            win: a.get(1).and_then(handle),
            topline: int(a.get(2)?)?,
            botline: int(a.get(3)?)?,
            curline: int(a.get(4)?)?,
            curcol: int(a.get(5)?)?,
            line_count: int(a.get(6)?)?,
            // Missing from servers older than the client needs; read as no
            // scroll, which is right for animation.
            scroll_delta: a.get(7).and_then(int).unwrap_or(0),
        },
        "win_viewport_margins" => Event::WinViewportMargins {
            grid: uint(a.first()?)?,
            top: size(a.get(2)?)?,
            bottom: size(a.get(3)?)?,
            left: size(a.get(4)?)?,
            right: size(a.get(5)?)?,
        },
        "mode_info_set" => Event::ModeInfoSet {
            enabled: boolean(a.first()?)?,
            modes: a.get(1)?.as_array()?.iter().map(mode_info).collect(),
        },
        "mode_change" => Event::ModeChange {
            name: as_str(a.first()?)?.to_string(),
            index: size(a.get(1)?)?,
        },
        "option_set" => Event::OptionSet {
            name: as_str(a.first()?)?.to_string(),
            value: match a.get(1)? {
                ValueRef::Boolean(b) => OptionValue::Bool(*b),
                v @ ValueRef::Integer(_) => OptionValue::Int(int(v)?),
                v @ ValueRef::String(_) => OptionValue::Str(as_str(v)?.to_string()),
                _ => OptionValue::Other,
            },
        },
        "busy_start" => Event::BusyStart,
        "busy_stop" => Event::BusyStop,
        "mouse_on" => Event::MouseOn,
        "mouse_off" => Event::MouseOff,
        "bell" => Event::Bell,
        "visual_bell" => Event::VisualBell,
        "flush" => Event::Flush,
        "set_title" => Event::SetTitle(as_str(a.first()?)?.to_string()),
        "ui_send" => Event::UiSend(bytes(a.first()?)?.to_vec()),
        "error_exit" => Event::ErrorExit(int(a.first()?)?),
        // Known and ignored: the terminal has nowhere for an icon title;
        // directories, menus and screenshots are a GUI's; `suspend` has no
        // shell to return to (see `pty::check_child`); `connect` and `restart`
        // would leave the session nvmux chose; `win_extmark` and
        // `win_external_pos` answer requests this client never makes.
        "set_icon" | "chdir" | "update_menu" | "screenshot" | "suspend" | "connect" | "restart"
        | "win_extmark" | "win_external_pos" => return Some(None),
        _ => return None,
    }))
}

/// The cells of a `grid_line`: `[text]`, `[text, hl]` or `[text, hl, repeat]`
/// each. A left-out highlight is `None`: the run before's carries over.
fn line_cells(v: &ValueRef<'_>) -> Option<Vec<LineCell>> {
    let runs = v.as_array()?;
    let mut cells = Vec::with_capacity(runs.len());
    for run in runs {
        let run = run.as_array()?;
        let text = match run.first()? {
            ValueRef::String(s) => match s.as_str() {
                Some(s) => Text::new(s),
                // Not UTF-8: shown as what it would decode to, which is a
                // replacement character rather than a dropped cell.
                None => Text::new(&String::from_utf8_lossy(s.as_bytes())),
            },
            _ => return None,
        };
        let hl = match run.get(1) {
            Some(v) => Some(u32::try_from(uint(v)?).ok()?),
            None => None,
        };
        // A repeat of 0 is sent, and means no cells: the highlight still
        // carries over.
        let repeat = match run.get(2) {
            Some(v) => size(v)?,
            None => 1,
        };
        cells.push(LineCell { text, hl, repeat });
    }
    Some(cells)
}

/// A highlight's attributes, from either of the two maps `hl_attr_define`
/// carries. Keys the client has no use for — `nocombine`, and anything newer —
/// are passed over.
pub fn attrs(v: &ValueRef<'_>) -> Option<Attrs> {
    let mut a = Attrs::default();
    for (key, value) in v_map(v)? {
        let Some(key) = as_str(key) else { continue };
        let on = boolean(value).unwrap_or(false);
        match key {
            "foreground" => a.fg = colour(value),
            "background" => a.bg = colour(value),
            "special" => a.sp = colour(value),
            "reverse" | "standout" => a.reverse |= on,
            "bold" => a.bold = on,
            "italic" => a.italic = on,
            "strikethrough" => a.strikethrough = on,
            "altfont" => a.altfont = on,
            "dim" => a.dim = on,
            "blink" => a.blink = on,
            "conceal" => a.conceal = on,
            "overline" => a.overline = on,
            "underline" if on => a.underline = Underline::Line,
            "undercurl" if on => a.underline = Underline::Curl,
            "underdouble" if on => a.underline = Underline::Double,
            "underdotted" if on => a.underline = Underline::Dotted,
            "underdashed" if on => a.underline = Underline::Dashed,
            "blend" => a.blend = int(value).unwrap_or(0).clamp(0, 100) as u8,
            "url" => a.url = as_str(value).map(Into::into),
            _ => {}
        }
    }
    Some(a)
}

/// One mode of a `mode_info_set`. A field left out is the mode not having it.
fn mode_info(v: &ValueRef<'_>) -> ModeInfo {
    let mut m = ModeInfo::default();
    let Some(pairs) = v_map(v) else {
        return m;
    };
    for (key, value) in pairs {
        let Some(key) = as_str(key) else { continue };
        let n = || uint(value).unwrap_or(0);
        match key {
            "name" => m.name = as_str(value).unwrap_or_default().to_string(),
            "cursor_shape" => {
                m.shape = match as_str(value) {
                    Some("block") => Some(Shape::Block),
                    Some("horizontal") => Some(Shape::Horizontal),
                    Some("vertical") => Some(Shape::Vertical),
                    _ => None,
                }
            }
            "cell_percentage" => m.percentage = n().min(100) as u8,
            "blinkwait" => m.blinkwait = n(),
            "blinkon" => m.blinkon = n(),
            "blinkoff" => m.blinkoff = n(),
            "attr_id" => m.attr_id = u32::try_from(n()).unwrap_or(0),
            _ => {}
        }
    }
    m
}

fn v_map<'a, 'b>(v: &'b ValueRef<'a>) -> Option<&'b [(ValueRef<'a>, ValueRef<'a>)]> {
    match v {
        ValueRef::Map(pairs) => Some(pairs),
        _ => None,
    }
}

pub(super) fn as_str<'a>(v: &'a ValueRef<'_>) -> Option<&'a str> {
    match v {
        ValueRef::String(s) => s.as_str(),
        _ => None,
    }
}

/// The bytes of a `str` or a `bin`, whether or not they are UTF-8.
fn bytes<'a>(v: &'a ValueRef<'_>) -> Option<&'a [u8]> {
    match v {
        ValueRef::String(s) => Some(s.as_bytes()),
        ValueRef::Binary(b) => Some(b),
        _ => None,
    }
}

pub(super) fn int(v: &ValueRef<'_>) -> Option<i64> {
    match v {
        ValueRef::Integer(n) => n.as_i64(),
        _ => None,
    }
}

pub(super) fn uint(v: &ValueRef<'_>) -> Option<u64> {
    v.as_u64()
}

fn size(v: &ValueRef<'_>) -> Option<usize> {
    usize::try_from(uint(v)?).ok()
}

fn boolean(v: &ValueRef<'_>) -> Option<bool> {
    match v {
        ValueRef::Boolean(b) => Some(*b),
        _ => None,
    }
}

/// A colour: `0xRRGGBB`, or a cterm index, with -1 for none.
fn colour(v: &ValueRef<'_>) -> Option<u32> {
    int(v).filter(|n| *n >= 0).map(|n| n as u32)
}

/// A window handle: an `ext` whose payload is the msgpack integer, or a plain
/// integer from a peer that sends one.
pub(super) fn handle(v: &ValueRef<'_>) -> Option<i64> {
    match v {
        ValueRef::Ext(_, data) => {
            let mut rd = *data;
            int(&rmpv::decode::read_value_ref(&mut rd).ok()?)
        }
        other => int(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmpv::Value;

    fn events(batches: Value) -> (Vec<Event>, usize) {
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &batches).expect("encode");
        let mut rd = &buf[..];
        let v = rmpv::decode::read_value_ref(&mut rd).expect("decode");
        let mut out = Vec::new();
        let dropped = decode(&v, &mut out);
        // The fast path reads the same bytes the same way.
        let mut fast = Vec::new();
        let fast_dropped = decode_bytes(&buf, &mut fast);
        assert_eq!(
            (&fast, fast_dropped),
            (&out, dropped),
            "decode_bytes differs"
        );
        (out, dropped)
    }

    /// Random mutations of a real batch read the same from the bytes as from
    /// the tree.
    #[test]
    fn decode_bytes_matches_decode_on_mutated_batches() {
        let line = |hl: Value, text: Value, rep: Value| {
            arr(vec![
                Value::from(2),
                Value::from(3),
                Value::from(4),
                arr(vec![
                    arr(vec![Value::from("a"), Value::from(7)]),
                    arr(vec![text, hl, rep, Value::from("extra")]),
                    arr(vec![Value::from("é")]),
                ]),
                Value::from(false),
                Value::from("more"),
            ])
        };
        let odd = [
            Value::Nil,
            Value::from(-1),
            Value::from(i64::from(u32::MAX) + 1),
            Value::from(1.5),
            Value::from("x"),
            Value::Binary(vec![b'a']),
            // Patched to bytes that are not UTF-8 below, the one byte long
            // as well: past what is read as one ASCII character.
            Value::from("\u{1}\u{2}"),
            Value::from("\u{4}"),
            Value::from(5),
            arr(vec![]),
        ];
        let mut cases = Vec::new();
        for a in &odd {
            cases.push(line(a.clone(), Value::from("b"), Value::from(2)));
            cases.push(line(Value::from(1), a.clone(), Value::from(2)));
            cases.push(line(Value::from(1), Value::from("b"), a.clone()));
            cases.push(arr(vec![
                a.clone(),
                Value::from(0),
                Value::from(0),
                arr(vec![]),
            ]));
            cases.push(a.clone());
        }
        cases.push(arr(vec![Value::from(1), Value::from(0)]));
        // A line whose last run has a highlight and no repeat: what is read
        // of it is all there is to read before the next line.
        cases.push(arr(vec![
            Value::from(1),
            Value::from(2),
            Value::from(0),
            arr(vec![arr(vec![Value::from("b"), Value::from(3)])]),
        ]));
        cases.push(arr(vec![
            Value::from(1),
            Value::from(0),
            Value::from(0),
            arr(vec![arr(vec![])]),
        ]));
        let mut b = vec![Value::from("grid_line")];
        b.extend(cases);
        let batches = arr(vec![
            Value::Array(b),
            Value::from(3),
            arr(vec![]),
            arr(vec![Value::from(4), arr(vec![])]),
            batch(
                "grid_cursor_goto",
                vec![arr(vec![Value::from(1), Value::from(2), Value::from(3)])],
            ),
            batch("flush", vec![arr(vec![])]),
        ]);
        let (_, dropped) = events(batches.clone());
        assert!(dropped > 0);
        // And with text that is not UTF-8, in a cell and as a name.
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &batches).expect("encode");
        let mut patched = Vec::new();
        let mut i = 0;
        while i < buf.len() {
            if buf[i..].starts_with(&[0xa2, 0x01, 0x02]) {
                patched.extend_from_slice(&[0xa2, 0xff, 0xfe]);
                i += 3;
            } else if buf[i..].starts_with(&[0xa1, 0x04]) {
                patched.extend_from_slice(&[0xa1, 0x80]);
                i += 2;
            } else {
                patched.push(buf[i]);
                i += 1;
            }
        }
        assert_ne!(patched, buf);
        let mut rd = &patched[..];
        let v = rmpv::decode::read_value_ref(&mut rd).expect("decode");
        let mut out = Vec::new();
        let dropped = decode(&v, &mut out);
        let mut fast = Vec::new();
        let fast_dropped = decode_bytes(&patched, &mut fast);
        assert_eq!((fast, fast_dropped), (out, dropped));
    }

    fn encoded(v: &Value) -> Vec<u8> {
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, v).expect("encode");
        buf
    }

    /// However msgpack encodes the header; nothing else is taken for one.
    #[test]
    fn a_redraw_is_known_from_its_bytes() {
        let params = arr(vec![batch("flush", vec![arr(vec![])])]);
        let frame = encoded(&arr(vec![
            Value::from(2),
            Value::from("redraw"),
            params.clone(),
        ]));
        assert_eq!(redraw_params(&frame), Some(&encoded(&params)[..]));
        // The kind as a signed byte, as a peer may write it.
        let mut signed = frame.clone();
        signed.splice(1..2, [0xd0, 0x02]);
        assert_eq!(redraw_params(&signed), Some(&encoded(&params)[..]));
        for other in [
            arr(vec![
                Value::from(2),
                Value::from("nvmux_client"),
                params.clone(),
            ]),
            arr(vec![Value::from(1), Value::from("redraw"), params.clone()]),
            arr(vec![Value::from(2), Value::from("redraw")]),
            arr(vec![Value::from(-2), Value::from("redraw"), params.clone()]),
            Value::from("redraw"),
        ] {
            assert_eq!(redraw_params(&encoded(&other)), None, "{other}");
        }
    }

    /// Whatever integer width the id uses; a non-response has none.
    #[test]
    fn a_response_is_known_from_its_bytes() {
        let answer =
            |id: Value| encoded(&arr(vec![Value::from(1), id, Value::Nil, Value::from(5)]));
        assert_eq!(response_id(&answer(Value::from(7))), Some(7));
        assert_eq!(response_id(&answer(Value::from(70_000))), Some(70_000));
        assert_eq!(response_id(&answer(Value::from(-1))), None);
        assert_eq!(response_id(&answer(Value::from("7"))), None);
        let note = encoded(&arr(vec![Value::from(2), Value::from(7), Value::Nil]));
        assert_eq!(response_id(&note), None);
        assert_eq!(response_id(&encoded(&Value::from(1))), None);
    }

    fn batch(name: &str, calls: Vec<Value>) -> Value {
        let mut b = vec![Value::from(name)];
        b.extend(calls);
        Value::Array(b)
    }

    fn arr(items: Vec<Value>) -> Value {
        Value::Array(items)
    }

    /// The window handle as Neovim sends it: ext type 1 around a msgpack int.
    fn win(n: u16) -> Value {
        let mut payload = vec![0xcd];
        payload.extend_from_slice(&n.to_be_bytes());
        Value::Ext(1, payload)
    }

    /// A left-out highlight carries over; a repeat of 0 is no cells.
    #[test]
    fn a_grid_line_keeps_each_runs_text_highlight_and_repeat() {
        let line = arr(vec![
            Value::from(2),
            Value::from(3),
            Value::from(4),
            arr(vec![
                arr(vec![Value::from("a"), Value::from(7)]),
                arr(vec![Value::from("中")]),
                arr(vec![Value::from("")]),
                arr(vec![Value::from(" "), Value::from(9), Value::from(12)]),
                arr(vec![Value::from(" "), Value::from(0), Value::from(0)]),
            ]),
            Value::from(false),
        ]);
        let (out, dropped) = events(arr(vec![batch("grid_line", vec![line])]));
        assert_eq!(dropped, 0);
        assert_eq!(
            out,
            vec![Event::GridLine {
                grid: 2,
                row: 3,
                col: 4,
                cells: vec![
                    LineCell {
                        text: Text::Char('a'),
                        hl: Some(7),
                        repeat: 1
                    },
                    LineCell {
                        text: Text::Char('中'),
                        hl: None,
                        repeat: 1
                    },
                    LineCell {
                        text: Text::Half,
                        hl: None,
                        repeat: 1
                    },
                    LineCell {
                        text: Text::Char(' '),
                        hl: Some(9),
                        repeat: 12
                    },
                    LineCell {
                        text: Text::Char(' '),
                        hl: Some(0),
                        repeat: 0
                    },
                ],
            }]
        );
    }

    /// The position is read from the last two of its eleven parameters.
    #[test]
    fn a_float_is_placed_where_neovim_resolved_it() {
        let call = arr(vec![
            Value::from(4),
            win(1001),
            Value::from("NW"),
            Value::from(1),
            Value::F64(3.0),
            Value::F64(10.0),
            Value::from(true),
            Value::from(50),
            Value::from(2),
            Value::from(3),
            Value::from(10),
        ]);
        let (out, _) = events(arr(vec![batch("win_float_pos", vec![call])]));
        assert_eq!(
            out,
            vec![Event::WinFloatPos {
                grid: 4,
                win: Some(1001),
                mouse: true,
                zindex: 50,
                compindex: 2,
                row: 3,
                col: 10,
                anchor: Some(1),
            }]
        );
    }

    /// The popup menu's window handle is -1, sent as an ext like any other.
    #[test]
    fn a_negative_handle_decodes() {
        let v = Value::Ext(1, vec![0xff]);
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &v).expect("encode");
        let mut rd = &buf[..];
        let v = rmpv::decode::read_value_ref(&mut rd).expect("decode");
        assert_eq!(handle(&v), Some(-1));
    }

    /// A bad call's neighbours, in its batch and the next, still arrive; an
    /// unknown name likewise.
    #[test]
    fn what_cannot_be_read_is_dropped_alone() {
        let good = arr(vec![Value::from(1), Value::from(5), Value::from(6)]);
        let bad = arr(vec![Value::from("one"), Value::from(5)]);
        let (out, dropped) = events(arr(vec![
            batch("grid_cursor_goto", vec![bad, good]),
            batch("some_future_event", vec![arr(vec![])]),
            batch("flush", vec![arr(vec![])]),
        ]));
        assert_eq!(
            out,
            vec![
                Event::GridCursor {
                    grid: 1,
                    row: 5,
                    col: 6
                },
                Event::Flush
            ]
        );
        assert_eq!(dropped, 2);
    }

    #[test]
    fn extra_arguments_are_read_past() {
        let call = arr(vec![
            Value::from(1),
            Value::from(80),
            Value::from(24),
            Value::from("from the future"),
        ]);
        let (out, dropped) = events(arr(vec![batch("grid_resize", vec![call])]));
        assert_eq!(dropped, 0);
        assert_eq!(
            out,
            vec![Event::GridResize {
                grid: 1,
                width: 80,
                height: 24
            }]
        );
    }

    /// An absent colour is none, and so is -1.
    #[test]
    fn a_highlight_carries_colours_attributes_and_blend() {
        let rgb = Value::Map(vec![
            (Value::from("foreground"), Value::from(0x112233)),
            (Value::from("background"), Value::from(-1)),
            (Value::from("bold"), Value::from(true)),
            (Value::from("undercurl"), Value::from(true)),
            (Value::from("special"), Value::from(0xff0000)),
            (Value::from("blend"), Value::from(30)),
            (Value::from("nocombine"), Value::from(true)),
            (Value::from("dim"), Value::from(true)),
            (Value::from("overline"), Value::from(true)),
            (Value::from("url"), Value::from("https://neovim.io")),
        ]);
        let cterm = Value::Map(vec![(Value::from("foreground"), Value::from(10))]);
        let call = arr(vec![Value::from(59), rgb, cterm, arr(vec![])]);
        let (out, _) = events(arr(vec![batch("hl_attr_define", vec![call])]));
        let Event::HlAttr { id, rgb, cterm } = &out[0] else {
            panic!("{out:?}")
        };
        assert_eq!(*id, 59);
        assert_eq!(rgb.fg, Some(0x112233));
        assert_eq!(rgb.bg, None);
        assert_eq!(rgb.sp, Some(0xff0000));
        assert!(rgb.bold && !rgb.italic);
        assert_eq!(rgb.underline, Underline::Curl);
        assert_eq!(rgb.blend, 30);
        assert!(rgb.dim && rgb.overline && !rgb.blink && !rgb.conceal);
        assert_eq!(rgb.url.as_deref(), Some("https://neovim.io"));
        assert_eq!(cterm.fg, Some(10));
    }

    /// `default_colors_set` sends cterm colours 1-based, with 0 for none.
    #[test]
    fn default_cterm_colours_are_one_based() {
        let call = arr(vec![
            Value::from(0xe0e2ea),
            Value::from(-1),
            Value::from(-1),
            Value::from(0),
            Value::from(1),
        ]);
        let (out, _) = events(arr(vec![batch("default_colors_set", vec![call])]));
        assert_eq!(
            out,
            vec![Event::DefaultColors(DefaultColors {
                rgb_fg: Some(0xe0e2ea),
                rgb_bg: None,
                rgb_sp: None,
                cterm_fg: None,
                cterm_bg: Some(0),
            })]
        );
    }

    /// A mode without a cursor of its own says so by leaving the shape out.
    #[test]
    fn modes_read_their_cursor() {
        let normal = Value::Map(vec![
            (Value::from("name"), Value::from("normal")),
            (Value::from("cursor_shape"), Value::from("vertical")),
            (Value::from("cell_percentage"), Value::from(25)),
            (Value::from("blinkwait"), Value::from(700)),
            (Value::from("blinkon"), Value::from(250)),
            (Value::from("blinkoff"), Value::from(400)),
            (Value::from("attr_id"), Value::from(11)),
        ]);
        let hover = Value::Map(vec![(Value::from("name"), Value::from("vsep_hover"))]);
        let call = arr(vec![Value::from(true), arr(vec![normal, hover])]);
        let (out, _) = events(arr(vec![batch("mode_info_set", vec![call])]));
        let Event::ModeInfoSet { enabled, modes } = &out[0] else {
            panic!("{out:?}")
        };
        assert!(enabled);
        assert_eq!(modes[0].shape, Some(Shape::Vertical));
        assert_eq!(modes[0].percentage, 25);
        assert!(modes[0].blinks());
        assert_eq!(modes[0].attr_id, 11);
        assert_eq!(modes[1].shape, None);
        assert!(!modes[1].blinks());
    }

    #[test]
    fn ui_send_keeps_its_bytes() {
        let call = arr(vec![Value::from("\x1b]52;c;aGk=\x1b\\")]);
        let (out, _) = events(arr(vec![batch("ui_send", vec![call])]));
        assert_eq!(out, vec![Event::UiSend(b"\x1b]52;c;aGk=\x1b\\".to_vec())]);
    }
}
