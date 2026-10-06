//! nvmux's own Neovim client: `[client] ui = "nvmux"`.
//!
//! nvmux draws a session with `nvim --remote-ui` unless told otherwise, and
//! says why in [`crate::pty`]: that client is Neovim's own, it negotiates with
//! the terminal itself, and everything the terminal can do reaches the editor
//! untouched. This is the other choice, for the one thing that client will not
//! do — move. Neovide animates the cursor, the scroll and the windows of a
//! Neovim it draws in a window of its own; this client draws in the terminal,
//! and animates as much of the same as a grid of character cells allows:
//!
//! - the cursor travels between cells instead of jumping, its leading edge
//!   ahead of its trailing one, so a long jump leaves a smear behind it, drawn
//!   in block elements at a quarter of a cell or finer ([`anim::smear`]);
//! - sparks, rings and outlines off the cursor as it moves — Neovide's
//!   `railgun`, `torpedo`, `pixiedust`, `sonicboom`, `ripple` and `wireframe`
//!   ([`anim::vfx`]);
//! - a scroll slides the text through the window a row at a time rather than
//!   redrawing it in place ([`anim::scroll`]);
//! - a window or a float that moves slides to where it is going
//!   ([`anim::motion`]);
//! - a blinking cursor fades in and out ([`anim::blink`]);
//! - and a float casts a shadow on what is under it ([`compose`]).
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
//! 1, and what is lost is what needs windows of their own — their moving, and
//! the shadows. An nvmux client says in its client info that it wants
//! `ext_multigrid`, so that one only passing through grid 1 does not send the
//! rest there with it.
//!
//! A UI attaching with `ext_multigrid` to a session another UI has drawn is
//! sent no window at all, besides: no placement, and nothing drawn on its
//! grid. Neovim keeps a window's grid from one UI to the next and sends its
//! cells only when they change, and nothing has. So the client asks where
//! every window it is told of is ([`LAYOUT_LUA`]), and has Neovim draw the
//! windows afresh by having it put them on grid 1 and back: a second
//! connection attaches a UI without `ext_multigrid` for as long as Neovim
//! takes to draw for it, and goes ([`refresh`]). The windows of a tab page
//! the client has not seen are the same, the first time it is shown. Drawing
//! waits for both, for up to [`HOLD_LIMIT`], so that what the terminal shows is
//! the editor whole.
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

pub mod anim;
pub mod compose;
pub mod grid;
pub mod input;
pub mod model;
pub mod redraw;
pub mod screen;
pub mod style;
pub mod term;
pub mod wire;

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use rmpv::ValueRef;

use self::anim::{Animator, Effects};
use self::input::{Input, Mouse, Parser, Reply};
use self::model::{Changes, Model, Place};
use self::redraw::Event;
use self::screen::Screen;
use self::wire::{Arg, Inbox, Outbox};
use crate::palette::{Palette, Rgb};

/// The environment variable a parent nvmux hands the terminal's colours down
/// in (see [`crate::palette::Palette::to_env`]): the client is on a pty and
/// cannot ask the terminal itself without the answers racing the user's keys.
pub const PALETTE_ENV: &str = "NVMUX_PALETTE";

/// The least time between two frames drawn for Neovim's flushes: a burst of
/// them — a `:terminal` running something — is drawn at most this often, the
/// last of them always.
const MIN_FRAME_GAP: Duration = Duration::from_millis(4);

/// How long drawing waits for what a multigrid attach lacks — where its
/// windows are, what is on them, the other UIs to settle — before drawing
/// what there is. See the module docs.
const HOLD_LIMIT: Duration = Duration::from_millis(500);

/// How long a terminal's answer part way through is waited for, from the last
/// of it to come, before what has come of it is dropped.
const REPLY_WAIT: Duration = Duration::from_secs(1);

/// How soon to ask who is attached again, having found a UI only passing
/// through grid 1: another nvmux client's refresh, or one attaching again.
const RECHECK: Duration = Duration::from_millis(30);

/// How long a refresh is given to be answered before it is given up, and how
/// long before one that came to nothing is tried again.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(2);

/// How many times a window is refreshed for, at most, before it is drawn
/// blank: Neovim does not draw for a UI attaching while it waits for Enter.
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
/// gone — `{ 1, row, col, top }` for a window in the layout, and
/// `{ 2, row, col, top, bottom, left, right, zindex, mouse }` for a float,
/// with the margins `win_viewport_margins` would have said: a winbar, a
/// border.
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
      return { 1, pos[1], pos[2], bar }
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
    return { 2, pos[1], pos[2], bar + side(2), side(6), side(8), side(4), cfg.zindex or 50, mouse }
  end)
  out[i] = ok and r or { 0 }
end
return out";

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
    /// The window grids to be seen with nothing on them, as of the last
    /// flush, and the refreshes asked for them.
    lacking: Vec<u64>,
    refreshed: HashMap<u64, Refreshed>,
    /// A refresh is to be started: see [`App::take_refresh`].
    refresh: bool,
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
    /// This attach has had its first flush: before it, nothing animates or
    /// is drawn.
    flushed: bool,
    /// What to write to the terminal before the next frame — the bell, the
    /// title, the server's own bytes — in order.
    say: Vec<u8>,
    /// The server is gone, or said to go.
    done: bool,
}

impl App {
    pub fn new(term: Palette, effects: Effects, size: (usize, usize)) -> Self {
        Self {
            model: Model::new(term),
            screen: Screen::new(),
            multigrid_wanted: effects.want_multigrid(),
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
            hold_since: None,
            escape_since: None,
            pressed: None,
            mouse: (false, false),
            dirty: false,
            last_frame: None,
            flushed: false,
            say: Vec::new(),
            done: false,
        }
    }

    /// Say who this client is, and attach — once it is known who else is
    /// attached, where that decides how.
    pub fn start(&mut self) {
        let id = self.outbox.request("nvim_get_api_info", &[]);
        self.asked.insert(id, Ask::ApiInfo);
        self.client_info();
        if self.multigrid_wanted {
            self.ask_uis();
        } else {
            self.attach_as(false);
        }
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
    }

    /// Detach, to attach again as `multigrid` says once Neovim has done it.
    fn leave(&mut self, multigrid: bool) {
        tracing::info!(multigrid, "client: attaching again for the UIs there are");
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
                    // Between a detach and its answer, what is drawn is the
                    // old attach's, and the next one starts from nothing.
                    (Some("redraw"), Some(_)) if matches!(self.attach, Attach::Leaving { .. }) => {}
                    (Some("redraw"), Some(params)) => {
                        redraw::decode(params, &mut self.batch);
                        self.take_flushes(now);
                    }
                    (Some("nvim_error_event"), Some(params)) => {
                        tracing::warn!(error = %params.to_owned(), "client: the server reported an error");
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
                match self.asked.remove(&id) {
                    Some(Ask::ApiInfo) => {
                        self.chan = result
                            .and_then(ValueRef::as_array)
                            .and_then(|a| a.first())
                            .and_then(ValueRef::as_u64);
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
                        }
                        self.settle(now);
                    }
                    Some(Ask::Detach) => {
                        if let Attach::Leaving { multigrid } = self.attach {
                            self.reset();
                            self.attach_as(multigrid);
                        }
                    }
                    None => {}
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
            Attach::Attached { multigrid: true } if foreign => self.leave(false),
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
                self.leave(true);
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
            match at(0) {
                1 => {
                    events.push(Event::WinPos {
                        grid,
                        win: Some(win),
                        row: cells(1),
                        col: cells(2),
                        width,
                        height,
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
                    events.push(Event::WinFloatPos {
                        grid,
                        win: Some(win),
                        mouse: at(8) != 0,
                        zindex: at(7),
                        compindex,
                        row: at(1),
                        col: at(2),
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
            self.apply(events, false, now);
        }
    }

    /// Forget the editor: a fresh attach is about to describe it again. The
    /// screen is kept as the terminal has it, for the next frame to be drawn
    /// over.
    fn reset(&mut self) {
        let term = self.model.colors().term;
        self.model = Model::new(term);
        self.batch.clear();
        self.anim.reset();
        self.flushed = false;
        self.unsettled = false;
        self.placing = 0;
        self.lacking.clear();
        self.refreshed.clear();
        self.hold_since = None;
        self.pressed = None;
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
        let animate = self.flushed && !self.holding(now);
        self.apply(batch, animate, now);
        self.flushed = true;
        if self.multigrid() {
            self.inspect(now);
        }
        self.settle(now);
    }

    /// Take a batch into the model, and set off what it starts — if
    /// `animate`, or simply put everything where it now is.
    fn apply(&mut self, batch: Vec<Event>, animate: bool, now: Instant) {
        let multigrid = self.multigrid();
        let before = self.anim.before(&self.model, &batch, multigrid);
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
        self.anim
            .after(before, &self.model, &changes, multigrid, animate, now);
        self.dirty = true;
    }

    /// What a multigrid attach still lacks after a flush: windows with
    /// nothing drawn on them, which a refresh is for, and windows nowhere,
    /// which a layout is asked for. See the module docs.
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
            tracing::debug!(grids = ?self.lacking, "client: windows with nothing on them; refreshing");
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
    /// after the last of it came — or [`REPLY_WAIT`] for a terminal's answer
    /// already part way through (see [`input`]).
    fn escape_deadline(&self) -> Option<Instant> {
        let since = self.escape_since?;
        if self.parser.in_reply() {
            return Some(since + REPLY_WAIT);
        }
        let o = &self.model.options;
        let wait = if o.ttimeout { o.ttimeoutlen } else { 0 };
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
            match input {
                Input::Keys(keys) => {
                    self.outbox.notify("nvim_input", &[Arg::str(&keys)]);
                    self.anim.typed(now);
                }
                Input::Mouse(m) => self.mouse_input(m),
                Input::Paste { phase, data } => self.outbox.notify(
                    "nvim_paste",
                    &[Arg::Str(&data), Arg::Bool(true), Arg::Int(phase)],
                ),
                Input::Focus(on) => self.outbox.notify("nvim_ui_set_focus", &[Arg::Bool(on)]),
                Input::Reply(reply) => self.reply(reply),
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
    fn mouse_input(&mut self, m: Mouse) {
        let (grid, row, col) = if !self.multigrid() {
            (0, m.row as i64, m.col as i64)
        } else {
            let pressed = self
                .pressed
                .filter(|_| matches!(m.action, "drag" | "release"));
            let grid = pressed.unwrap_or_else(|| self.grid_at(m.row as i64, m.col as i64));
            let (r0, c0) = self.origin_of(grid);
            (grid, m.row as i64 - r0, m.col as i64 - c0)
        };
        match m.action {
            "press" => self.pressed = Some(grid),
            "release" => self.pressed = None,
            _ => {}
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

    /// Where a grid is, as the editor laid it out.
    fn origin_of(&self, grid: u64) -> (i64, i64) {
        self.model
            .layout
            .get(&grid)
            .map_or((0, 0), model::Placement::origin)
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
        self.dirty = true;
    }

    /// The terminal's size, as last told.
    pub fn size(&self) -> (usize, usize) {
        self.size
    }

    /// Anything the client is to do on the clock: ask after the UIs again.
    pub fn tick(&mut self, now: Instant) {
        if self.recheck_at.is_some_and(|at| at <= now) {
            self.recheck_at = None;
            if self.checking {
                self.recheck = true;
            } else {
                self.ask_uis();
            }
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
            self.hold_since.map(|since| since + HOLD_LIMIT)
        } else if self.dirty {
            Some(
                self.last_frame
                    .map_or(now, |last| (last + MIN_FRAME_GAP).max(now)),
            )
        } else {
            self.anim.next_frame(&self.model, now)
        };
        [self.escape_deadline(), frame, self.recheck_at]
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
            return self
                .last_frame
                .is_none_or(|last| now >= last + MIN_FRAME_GAP);
        }
        self.anim
            .next_frame(&self.model, now)
            .is_some_and(|at| at <= now)
    }

    /// The next frame's bytes, with whatever was waiting to be said ahead of
    /// it.
    pub fn frame(&mut self, now: Instant) -> Vec<u8> {
        self.anim.advance(now);
        let (w, h) = self.size;
        let shadows = self.multigrid() && self.anim.shadows();
        let mut frame = compose::compose(&self.model, &self.anim, shadows, w, h);
        let cursor = self.anim.paint(&mut frame, &self.model, now);
        frame.mend();
        let sync = self.model.options.termsync;
        let mut out = std::mem::take(&mut self.say);
        out.extend_from_slice(&self.screen.draw(frame, &cursor, sync, self.model.links()));
        self.dirty = false;
        self.last_frame = Some(now);
        out
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
    let settings = crate::config::load().unwrap_or_default();
    let effects = Effects::from_settings(&settings);
    let term = palette_from_env().unwrap_or(FALLBACK);

    let mut stream =
        UnixStream::connect(sock).with_context(|| format!("connecting to {}", sock.display()))?;
    stream.set_nonblocking(true)?;

    let _raw = term::Raw::enter()?;
    let hangup = term::Hangup::install()?;
    let winch = crate::winch::Winch::install()?;
    crate::term::write_stdout(term::OPENING)?;

    let mut app = App::new(term, effects, term::size());
    app.start();
    let mut inbox = Inbox::default();
    let stdin = std::io::stdin();
    let mut buf = vec![0u8; 64 * 1024];

    let outcome = loop {
        app.tick(Instant::now());
        if app.take_refresh() {
            refresh(sock, app.size());
        }
        if let Err(e) = app.outbox.send(&mut stream) {
            break Err(anyhow::Error::from(e).context("writing to the session"));
        }
        let said = app.take_said();
        if !said.is_empty() {
            crate::term::write_stdout(&said)?;
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
                    Ok(Some(frame)) => app.message(&frame, now),
                    Ok(None) => break,
                    Err(e) => {
                        return Err(anyhow::Error::from(e).context("reading from the session"))
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
        if app.due(now) {
            let bytes = app.frame(now);
            crate::term::write_stdout(&bytes)?;
        }
        if app.done {
            break Ok(());
        }
    };
    // Whatever ended it, the terminal is handed back as it was.
    let _ = app.outbox.send(&mut stream);
    let _ = crate::term::write_stdout(term::CLOSING);
    outcome
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

/// A refresh's own connection: blocking, and given until `deadline`.
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
        let timed_out = || std::io::Error::from(std::io::ErrorKind::TimedOut);
        while !self.out.is_empty() {
            if Instant::now() > self.deadline {
                return Err(timed_out());
            }
            self.out.send(&mut self.stream)?;
        }
        loop {
            while let Some(frame) = self
                .inbox
                .take_frame()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
            {
                let mut rd = &frame[..];
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
            if Instant::now() > self.deadline {
                return Err(timed_out());
            }
            if !self.inbox.fill(&mut self.stream)? {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
        }
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
    use rmpv::Value;

    type Call = (Option<u64>, String, Vec<Value>);

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
            windows: multigrid.then_some(0.15),
            ..Effects::none()
        };
        App::new(palette(), effects, (20, 6))
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
                0.into(),
                Value::Array(cells),
                false.into(),
            ],
        )
    }

    fn resize(grid: u64, width: u64, height: u64) -> (&'static str, Vec<Value>) {
        (
            "grid_resize",
            vec![grid.into(), width.into(), height.into()],
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
            vec![line(2, 0, "right"), line(4, 0, "bar"), line(4, 1, "left")],
            t0,
        );
        assert!(a.due(t0));
        assert!(!a.take_refresh(), "nothing lacking now");
        let f = compose::compose(&a.model, &a.anim, false, 20, 6);
        let text = f.text();
        let rows: Vec<&str> = text.lines().take(2).collect();
        assert_eq!(rows, ["bar        right    ", "left                "]);
        assert_eq!(a.model.margins(4).top, 1, "the winbar, as the layout said");
        assert_eq!(a.model.cursor_on_screen(), (1, 0));
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
        redraw(
            &mut a,
            vec![
                resize(2, 10, 5),
                line(2, 0, "x"),
                viewport(2, 1000),
                (
                    "win_pos",
                    vec![
                        2.into(),
                        1000.into(),
                        0.into(),
                        0.into(),
                        10.into(),
                        5.into(),
                    ],
                ),
            ],
            t0,
        );
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
        redraw(&mut a, vec![resize(6, 20, 4), line(6, 0, "tab two")], t0);
        assert!(a.due(t0));
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

    /// An escape held is let go `'ttimeoutlen'` after it came, whatever else
    /// wakes the loop in between.
    #[test]
    fn an_escape_is_let_go_on_time() {
        let t0 = Instant::now();
        let mut a = app(false);
        a.keys(b"\x1b", t0);
        let at = t0 + Duration::from_millis(50);
        assert_eq!(a.wake_at(t0 + Duration::from_millis(20)), Some(at));
        assert!(!a.escape_due(at - Duration::from_millis(1)));
        assert!(a.escape_due(at));
        a.escape_timeout(at);
        let sent = calls(&mut a);
        assert!(sent
            .iter()
            .any(|(_, m, args)| m == "nvim_input" && args[0] == Value::from("<Esc>")));
        assert_eq!(a.wake_at(at), None);
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
}
