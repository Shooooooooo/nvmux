//! The picker floating over a session: `<prefix> Space`, in a panel the relay
//! draws and gives its keys to (see [`super::float`] and `crate::pty::panel`),
//! the way the settings and the help float — rather than on a screen of its
//! own, the session dissolving out under it.
//!
//! It is the picker: its [`App`], drawn by [`super::draw`] — rows, signs, the
//! note under the list, the effects — into the panel rather than onto the
//! terminal, on its own keys and the mouse. What changes is where it is drawn,
//! and so how much of its hint row there is room for ([`HINTS`]); and where a
//! key that leads off it goes — another session, the create prompt, the help,
//! the settings, out of nvmux — which is the relay's to follow ([`Leave`]).
//!
//! # Rows in hand
//!
//! It opens on the rows the session loop has in hand ([`hand`]), at once, and
//! lists behind them on a thread, as the picker does after `<prefix> Space`
//! (see [`super::Listing`]). A kill or a move made on it lists again, and the
//! session loop takes the rows that leaves back ([`take_changed`]), so a
//! `<prefix>` switch afterwards names what is there.
//!
//! # What it does not do floating
//!
//! A rename asks for the name on a screen of its own: the panel goes, and the
//! picker comes up full screen, as `<prefix> Space` used to bring it
//! ([`Leave::Picker`]). On a terminal with no room for the panel and the
//! session round it, `<prefix> Space` has the picker full screen too.
//!
//! # A prototype
//!
//! It floats only with `NVMUX_FLOAT_PICKER=1` in nvmux's environment
//! ([`floats`]); otherwise `<prefix> Space` brings the picker full screen, as
//! it always has.

use std::collections::{HashSet, VecDeque};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyEvent, KeyEventKind, MouseEvent};
use ratatui::layout::Rect;
use ratatui::Terminal;

use super::app::{App, Request};
use super::float::{self, Did, Floating, Leave};
use super::{draw, effects, note, settings, Behind, Listing};
use crate::ids::Id;
use crate::notes::Poller;
use crate::session::Session;
use crate::transport::Shared;

/// The picker's hints as the panel has room for them: the full screen's, in
/// its order, and `esc back` after them, each ranked by how much it is needed
/// — the panel keeps as many as its width takes, dropping the least needed
/// whole (see [`settings::fit`]). At the panel's least width, the seven that
/// rank under seven.
const HINTS: [(&str, u8); 10] = [
    ("↑↓ move", 0),
    ("⏎ attach", 1),
    ("c new", 3),
    ("r rename", 8),
    ("x kill", 5),
    ("␣ order", 6),
    ("/ find", 4),
    (", settings", 7),
    ("q quit", 9),
    ("esc back", 2),
];

/// The least of the session the panel leaves showing round it: columns either
/// side, and rows above and below. The help's.
const SESSION_COLS: u16 = 2;
const SESSION_ROWS: u16 = 1;

/// Columns of the panel either side of the list, past its margin: room for the
/// signs left of the list, and for what flies off the ends of a row in flight.
const ROOM: u16 = 8;

/// The narrowest the panel is: room for the seven hints that matter most, and
/// for most of what a session's note says under the list — Claude Code's
/// asking for a permission, whole, and how long ago. A longer note is cut, as
/// the picker's is on a terminal too narrow for it.
const MIN_WIDTH: u16 = 64;

/// The panel's rows that are not the list's: one over it; one, and the note,
/// under it; one more, the hint row and the panel's last.
const AROUND_LIST: u16 = 7;

/// What lists, kills and renumbers the sessions, lent once as nvmux starts:
/// the panel is opened by the relay, which has no transport of its own.
static TRANSPORT: OnceLock<Shared> = OnceLock::new();

/// The rows the session loop has in hand as it starts each relay.
static IN_HAND: Mutex<Vec<Session>> = Mutex::new(Vec::new());

/// The rows as a panel left them, where it changed them, for the session loop
/// to take back.
static CHANGED: Mutex<Option<Vec<Session>>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether `<prefix> Space` floats the picker over the session: with
/// `NVMUX_FLOAT_PICKER=1` in nvmux's environment, read once.
pub fn floats() -> bool {
    static FLOATS: OnceLock<bool> = OnceLock::new();
    *FLOATS.get_or_init(|| std::env::var_os("NVMUX_FLOAT_PICKER").is_some_and(|v| v == "1"))
}

/// Lend the panel the transport, once, as nvmux starts.
pub fn lend(transport: &Shared) {
    let _ = TRANSPORT.set(std::sync::Arc::clone(transport));
}

/// The rows the next panel opens on: the session loop's, as it starts a relay.
pub fn hand(rows: &[Session]) {
    *lock(&IN_HAND) = rows.to_vec();
    lock(&CHANGED).take();
}

/// The rows a panel left, if it changed them — a kill, a move, or a listing
/// that found something new — since the session loop last handed them over.
pub fn take_changed() -> Option<Vec<Session>> {
    lock(&CHANGED).take()
}

/// The picker floating over a session (see the module docs).
pub struct Panel {
    app: App,
    /// The session the panel floats over: `Esc`, and `Enter` on its own row,
    /// go back to it.
    behind: String,
    /// The listing under way behind the rows in hand.
    listing: Option<Behind>,
    /// What each session has to say, read on a thread of its own as the
    /// picker reads it; the sessions it has been given, and the ones it has
    /// yet to be.
    poller: Poller,
    handed: HashSet<String>,
    unread: VecDeque<Session>,
    /// Whether the reader has said anything yet, and when the panel opened:
    /// until it has, or for as long as a round of reads can take, the panel
    /// asks to be woken often, so the signs come up with it.
    heard: bool,
    opened: Instant,
    /// A kill confirmed, made once its row has been erased.
    killing: Option<String>,
    /// A move put down, written once its landing is over.
    placing: Option<Vec<(String, u32)>>,
    /// When a half-typed number settles for the session it already names.
    deadline: Option<Instant>,
    /// Where it goes once it is down, if a key has said.
    leave: Option<Leave>,
    /// Whether it has asked to be taken down with no key to say so.
    closing: bool,
    /// Where the picker was last drawn in the panel: what a click is resolved
    /// against, the screen the user clicked on.
    drawn: Rect,
    /// When what moves on it was last moved on, and the spinner's frame then.
    ticked: Instant,
    spun: usize,
    /// What the effects paint with, asked once.
    colours: Option<effects::Colours>,
}

impl Panel {
    /// The picker over session `behind`, on the rows in hand, the cursor on
    /// that session — and a listing begun behind them.
    pub fn open(behind: &str) -> Self {
        let rows = lock(&IN_HAND).clone();
        let (rows, listing) = match TRANSPORT.get().map(|t| Listing::in_hand(rows.clone(), t)) {
            Some(Listing::InHand(rows, answer)) => (rows, Some(answer)),
            Some(Listing::Fresh(rows)) => (rows, None),
            Some(Listing::Ask) | None => (rows, None),
        };
        let mut app = App::new(rows);
        app.select_session(behind);
        app.set_came_from(behind);
        let now = Instant::now();
        Self {
            app,
            behind: behind.to_string(),
            listing,
            poller: Poller::start(Some(behind.to_string())),
            handed: HashSet::new(),
            unread: VecDeque::new(),
            heard: false,
            opened: now,
            killing: None,
            placing: None,
            deadline: None,
            leave: None,
            closing: false,
            drawn: Rect::default(),
            ticked: now,
            spun: 0,
            colours: effects::colours(),
        }
    }

    /// What moves on the picker moved on to `now`.
    fn move_on(&mut self, now: Instant) {
        self.app.tick(now.saturating_duration_since(self.ticked));
        self.ticked = self.ticked.max(now);
    }

    /// What the picker asked for, done or handed on: a session to go to, or a
    /// screen to come up in the panel's place, takes the panel down.
    fn answer(&mut self, request: Request) -> Did {
        match request {
            Request::None => Did::Stay,
            Request::Attach(id) if id == self.behind => Did::Close,
            Request::Attach(id) => match Id::new(&id) {
                Some(id) => self.go(Leave::Switch(id)),
                None => Did::Stay,
            },
            Request::NewSession => self.go(Leave::New),
            Request::RenameSession(_) => self.go(Leave::Picker),
            Request::Help => self.go(Leave::Help),
            Request::Settings => self.go(Leave::Settings),
            Request::Quit | Request::SaveAndQuit { .. } => self.go(Leave::Quit),
            // Erased first, as the picker erases a row before the kill runs
            // (see `super::erase`): made once the erase is over.
            Request::Kill(id) => {
                if self.app.start_backspace(&id) {
                    self.killing = Some(id);
                } else {
                    self.kill(&id);
                }
                Did::Stay
            }
            // Landed first, as the picker lands a row before it writes.
            Request::Reorder(order) => {
                if self.app.landing().is_some() {
                    self.placing = Some(order);
                } else {
                    self.place(order);
                }
                Did::Stay
            }
        }
    }

    fn go(&mut self, to: Leave) -> Did {
        self.leave = Some(to);
        Did::Close
    }

    fn kill(&mut self, id: &str) {
        let (Some(transport), Some(session)) = (TRANSPORT.get(), self.app.session(id).cloned())
        else {
            return;
        };
        if let Err(e) = transport.kill_session(&session) {
            self.app.set_message(e.one_line());
        }
        self.relist(transport);
    }

    fn place(&mut self, order: Vec<(String, u32)>) {
        let Some(transport) = TRANSPORT.get() else {
            return;
        };
        let batch: Vec<Session> = order
            .into_iter()
            .filter_map(|(id, num)| {
                self.app.session(&id).cloned().map(|mut s| {
                    s.num = num;
                    s
                })
            })
            .collect();
        if let Err(e) = transport.renumber(&batch) {
            self.app.set_message(e.one_line());
        }
        self.relist(transport);
    }

    /// Listed again, after a kill or a move — and the listing under way let
    /// go of: asked before either, it would put back what they changed.
    fn relist(&mut self, transport: &Shared) {
        self.listing = None;
        match transport.list_sessions() {
            Ok(rows) => {
                self.app.set_sessions(rows);
                self.changed();
            }
            Err(e) => self.app.set_message(e.one_line()),
        }
    }

    fn changed(&self) {
        *lock(&CHANGED) = Some(self.app.sessions().to_vec());
    }

    /// The sessions the reader of notes has not been given, given it — a few
    /// milliseconds' worth at a time, as the picker hands them over.
    fn hand_over(&mut self, transport: &Shared) {
        for s in self.app.sessions() {
            if self.handed.insert(s.id.clone()) {
                self.unread.push_back(s.clone());
            }
        }
        let started = Instant::now();
        while let Some(s) = self.unread.pop_front() {
            match transport.local_socket_for(&s) {
                Ok(sock) => self.poller.watch(&s.id, sock),
                Err(e) => tracing::debug!(id = %s.id, error = %e, "notes: no socket to read"),
            }
            if started.elapsed() >= super::HAND_OVER {
                break;
            }
        }
    }

    /// Whether anything is under way that wants the panel woken soon: a
    /// listing, sessions still to be handed to the reader, its first answer,
    /// a kill or a move waiting on what is passing over its row.
    fn busy(&self) -> bool {
        self.listing.is_some()
            || !self.unread.is_empty()
            || (!self.heard && self.opened.elapsed() < crate::notes::POLL)
            || self.killing.is_some()
            || self.placing.is_some()
    }

    /// The hint row's text at rest, as much of it as `width` takes.
    fn hints(width: u16) -> String {
        let entries = HINTS.map(|(text, rank)| (Some(text), rank));
        settings::fit(&entries, usize::from(width.saturating_sub(2)))
    }
}

impl Floating for Panel {
    /// In the middle: as wide as the list and room either side of it, or
    /// [`MIN_WIDTH`]; as tall as every session and the rows round them —
    /// every session, not the ones a filter leaves, so the panel holds still
    /// as one is typed. Smaller where that leaves too little of the session
    /// round it, and nowhere where even that is too small.
    fn area(&self, screen: Rect) -> Option<Rect> {
        let list = draw::list_width(&self.app);
        let rows = self.app.sessions().len().max(1) as u16;
        let width = (list + 2 * ROOM + 2)
            .max(MIN_WIDTH)
            .min(screen.width.saturating_sub(2 * SESSION_COLS));
        let height = (rows + AROUND_LIST).min(screen.height.saturating_sub(2 * SESSION_ROWS));
        if width < list + 6 || height < AROUND_LIST + 1 {
            return None;
        }
        Some(Rect::new(
            screen.x + (screen.width - width) / 2,
            screen.y + (screen.height - height) / 2,
            width,
            height,
        ))
    }

    /// A key, as the picker takes it; releases are no keys.
    fn on_key(&mut self, key: KeyEvent, now: Instant) -> Did {
        if key.kind == KeyEventKind::Release {
            return Did::Stay;
        }
        // Up to the key, so what it starts starts from nothing.
        self.move_on(now);
        let request = self.app.on_key(super::translate(key));
        self.deadline = self
            .app
            .pending()
            .is_some()
            .then(|| now + Duration::from_millis(crate::config::get().keys.timeout_ms));
        self.answer(request)
    }

    /// A gesture, as the picker takes it, against the rows where they were
    /// last drawn; a press outside the panel is the relay's.
    fn on_mouse(&mut self, mouse: MouseEvent, now: Instant) -> Did {
        let local = Rect::new(0, 0, self.drawn.width, self.drawn.height);
        let row = mouse
            .column
            .checked_sub(self.drawn.x)
            .zip(mouse.row.checked_sub(self.drawn.y))
            .and_then(|(col, row)| draw::row_at(&self.app, local, col, row));
        let Some(gesture) = super::translate_mouse(mouse, row) else {
            return Did::Stay;
        };
        self.move_on(now);
        let request = self.app.on_mouse(gesture);
        self.answer(request)
    }

    /// A half-typed number's wait, a turn of the spinner, and while anything
    /// is under way, the picker's own tick.
    fn wake_at(&self) -> Option<Instant> {
        let tick = if self.busy() {
            crate::fade::FRAME * 3
        } else {
            super::TICK
        };
        [
            self.deadline,
            self.app.spinning().then(|| self.ticked + note::SPIN),
            Some(self.ticked + tick),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Moved on to `now`: the listing behind the rows taken up if it has come
    /// and the picker is at rest, the sessions handed to the reader of their
    /// notes and what they said taken up, a kill or a move made once what was
    /// passing over its row is over, and a half-typed number settled once its
    /// wait has run out.
    fn tick(&mut self, now: Instant) -> bool {
        self.move_on(now);
        let mut changed = false;
        if self.listing.is_some() {
            super::take_listing(&mut self.app, &mut self.listing);
            if self.listing.is_none() {
                self.changed();
                changed = true;
            }
        }
        if let Some(transport) = TRANSPORT.get() {
            self.hand_over(transport);
        }
        if let Some(notes) = self.poller.take() {
            self.app.set_notes(notes);
            self.heard = true;
            changed = true;
        }
        if self.killing.is_some() && !self.app.erasing() {
            if let Some(id) = self.killing.take() {
                self.kill(&id);
                changed = true;
            }
        }
        if self.placing.is_some() && self.app.landing().is_none() {
            if let Some(order) = self.placing.take() {
                self.place(order);
                changed = true;
            }
        }
        if self.deadline.is_some_and(|d| now >= d) {
            self.deadline = None;
            let request = self.app.resolve_pending();
            if self.answer(request) == Did::Close {
                self.closing = true;
            }
            changed = true;
        }
        let spun = self.app.spin_frame();
        if self.app.spinning() && spun != self.spun {
            changed = true;
        }
        self.spun = spun;
        changed
    }

    fn moving(&self) -> bool {
        self.app.animating()
    }

    /// The picker drawn into `area`: as it draws itself on a terminal of the
    /// panel's inside, with the theme's colour on it and whatever is passing
    /// over it, and laid into a buffer of `area`.
    fn draw(&mut self, area: Rect) -> Buffer {
        let mut buf = Buffer::empty(area);
        let inside = float::inside(area);
        self.drawn = inside;
        if inside.width == 0 || inside.height == 0 {
            return buf;
        }
        let hints = Self::hints(inside.width);
        // A terminal of the inside's size, in memory: the picker draws on a
        // frame, and this is the one it can have of a size of nvmux's choosing.
        let Ok(mut terminal) = Terminal::new(TestBackend::new(inside.width, inside.height));
        let app = &self.app;
        let colours = self.colours.as_ref();
        let Ok(_) = terminal.draw(|f| {
            draw::draw_with(f, app, &hints);
            crate::theme::paint(f.buffer_mut());
            effects::paint(f, app, colours);
        });
        let drawn = terminal.backend().buffer();
        for y in 0..inside.height {
            for x in 0..inside.width {
                buf[(inside.x + x, inside.y + y)] = drawn[(x, y)].clone();
            }
        }
        buf
    }

    fn close(&self) {}

    fn leave(&self) -> Option<Leave> {
        self.leave
    }

    fn closing(&self) -> bool {
        self.closing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    /// At its least width the panel keeps the seven hints that matter most, in
    /// the full screen's order, and `esc back` after them; given the room, all
    /// of them.
    #[test]
    fn the_panels_hints_are_the_seven_that_matter_most() {
        let inside = MIN_WIDTH - 2;
        assert_eq!(
            Panel::hints(inside),
            "↑↓ move  ⏎ attach  c new  x kill  ␣ order  / find  esc back"
        );
        let all = HINTS.map(|(t, _)| t).join("  ");
        assert_eq!(Panel::hints(all.width() as u16 + 2), all);
    }
}
