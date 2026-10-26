//! The picker as a panel floating over a session (`<prefix> Space`), drawn by
//! the relay like the settings and help (see [`super::float`] and
//! `crate::pty::panel`).
//!
//! It wraps the picker's [`App`] and [`super::draw`], drawn into the panel
//! instead of the terminal; its hint row is the full screen's, on the
//! screen's last row ([`Floating::draw_hints`]). Keys that lead elsewhere
//! become a [`Leave`] for the relay. Help, settings and prompts replace the
//! panel and return to it.
//!
//! It opens at once on the session loop's rows ([`hand`]) and lists in the
//! background (see [`super::Listing`]). A kill or move relists, and the loop
//! takes the new rows back once the relay is over ([`take_changed`]); until
//! then the next panel opens on them and `<prefix>` switches are resolved
//! among them ([`rows`]).
//!
//! `c` opens a gap after the cursor row, then leaves for the prompt
//! ([`Leave::New`], [`super::prompt::Panel`]); `r` renames the cursor row
//! ([`Leave::Rename`]). A prompt that switches nowhere (a rename, a cancelled
//! create) returns to the panel with the gap closed. With no room for the
//! prompt beside the session, it goes full screen and the picker floats again
//! afterwards (see `crate::pty::Attachment::float_the_picker`). With no room
//! for the panel, the picker is full screen.

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
use super::{draw, effects, note, Behind, Listing};
use crate::ids::Id;
use crate::notes::Poller;
use crate::session::Session;
use crate::transport::Shared;

/// Minimum session margin left visible around the panel, as for the help.
const SESSION_COLS: u16 = 2;
const SESSION_ROWS: u16 = 1;

/// Columns either side of the list, past its margin: for the signs and for
/// effects that fly past a row's ends.
const ROOM: u16 = 8;

/// Fits a typical note (Claude Code's permission request with its age) and
/// the panel's margin column either side. Longer notes are cut.
const MIN_WIDTH: u16 = 50;

/// Rows besides the list: two above, the list being centred; one and the
/// note below; and the last, where full screen has the hint row, left blank
/// (see [`float::inside`]).
const AROUND_LIST: u16 = 5;

/// Set once at startup: the relay that opens the panel has no transport.
static TRANSPORT: OnceLock<Shared> = OnceLock::new();

static IN_HAND: Mutex<Vec<Session>> = Mutex::new(Vec::new());

/// Cursor row for the next panel when not the session behind it: the row `c`
/// or `r` was pressed on, back from its full-screen prompt.
static ON: Mutex<Option<String>> = Mutex::new(None);

/// Rows as a panel changed them, for the session loop to take back; until
/// then, the rows as they stand (see [`rows`]).
static CHANGED: Mutex<Option<Vec<Session>>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Call once at startup.
pub fn lend(transport: &Shared) {
    let _ = TRANSPORT.set(std::sync::Arc::clone(transport));
}

/// Set the rows the next panel opens on, and its cursor row if not the session
/// behind it (back from a full-screen prompt). Clears any changed rows.
pub fn hand(rows: &[Session], on: Option<&str>) {
    *lock(&IN_HAND) = rows.to_vec();
    *lock(&ON) = on.map(str::to_string);
    lock(&CHANGED).take();
}

/// Rows a panel changed (kill, move, create, rename, or a newer listing)
/// since the last [`hand`].
pub fn take_changed() -> Option<Vec<Session>> {
    lock(&CHANGED).take()
}

/// For the floating prompt (see [`super::prompt::Panel`]).
pub(super) fn transport() -> Option<Shared> {
    TRANSPORT.get().cloned()
}

/// The changed rows if any, else the ones handed over: what the next panel
/// opens on, and what a `<prefix>` switch is resolved among during the relay
/// (see `main`).
pub fn rows() -> Vec<Session> {
    lock(&CHANGED)
        .clone()
        .unwrap_or_else(|| lock(&IN_HAND).clone())
}

/// Store rows after the prompt creates or renames a session, for the session
/// loop and for the panel to resume with.
pub(super) fn leave_rows(rows: &[Session]) {
    *lock(&CHANGED) = Some(rows.to_vec());
}

pub struct Panel {
    app: App,
    /// The session behind the panel; `Esc`, or `Enter` on its row, returns
    /// to it.
    behind: String,
    listing: Option<Behind>,
    /// The notes reader thread, the sessions handed to it, and those not yet.
    poller: Poller,
    handed: HashSet<String>,
    unread: VecDeque<Session>,
    /// Until the reader first answers, for at most one `notes::POLL` after
    /// opening, the panel wakes often so the signs appear with it.
    heard: bool,
    opened: Instant,
    /// A confirmed kill, run once its row's erase effect ends.
    killing: Option<String>,
    /// A reorder, written once its landing effect ends.
    placing: Option<Vec<(String, u32)>>,
    /// After `c`: the row the new session goes after, and when the gap opened.
    creating: Option<(Option<Id>, Instant)>,
    /// When a half-typed number resolves to the session it already names.
    deadline: Option<Instant>,
    /// Where to go once closed, if a key chose.
    leave: Option<Leave>,
    /// Close requested by a tick rather than a key.
    closing: bool,
    /// Where the picker was last drawn; clicks resolve against it.
    drawn: Rect,
    ticked: Instant,
    spun: usize,
    colours: Option<effects::Colours>,
}

impl Panel {
    /// Open over `behind` on the rows as they stand ([`rows`]), the cursor on
    /// the handed row or else `behind`, and start a listing. Not the rows in
    /// hand: after `Esc` the relay goes on, and changes the last panel made
    /// reach the session loop only once it ends.
    pub fn open(behind: &str) -> Self {
        let rows = rows();
        // Taken, so the next `<prefix> Space` panel opens on `behind` again.
        let on = lock(&ON).take();
        let (rows, listing) = match TRANSPORT.get().map(|t| Listing::in_hand(rows.clone(), t)) {
            Some(Listing::InHand(rows, answer)) => (rows, Some(answer)),
            Some(Listing::Fresh(rows)) => (rows, None),
            Some(Listing::Ask) | None => (rows, None),
        };
        let mut app = App::new(rows);
        app.select_session(on.as_deref().unwrap_or(behind));
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
            creating: None,
            deadline: None,
            leave: None,
            closing: false,
            drawn: Rect::default(),
            ticked: now,
            spun: 0,
            colours: effects::colours(),
        }
    }

    fn move_on(&mut self, now: Instant) {
        self.app.tick(now.saturating_duration_since(self.ticked));
        self.ticked = self.ticked.max(now);
    }

    /// Carry out a request; switching session or opening another screen
    /// closes the panel.
    fn answer(&mut self, request: Request) -> Did {
        match request {
            Request::None => Did::Stay,
            Request::Attach(id) if id == self.behind => Did::Close,
            Request::Attach(id) => match Id::new(&id) {
                Some(id) => self.go(Leave::Switch(id)),
                None => Did::Stay,
            },
            // Open the gap first, as full screen does (`super::make_room`);
            // leave once it is up.
            Request::NewSession => {
                let after = self.cursor();
                let number = Some(self.app.new_number());
                let name = super::prompt::default_name(self.app.sessions(), number);
                if self.app.make_room(&name) {
                    self.creating = Some((after, self.ticked));
                    Did::Stay
                } else {
                    self.go(Leave::New(after))
                }
            }
            Request::RenameSession(id) => match Id::new(&id) {
                Some(id) => self.go(Leave::Rename(id)),
                None => Did::Stay,
            },
            Request::Help => self.go(Leave::Help),
            Request::Settings => self.go(Leave::Settings),
            Request::Quit | Request::SaveAndQuit { .. } => self.go(Leave::Quit),
            // Erase the row first, as full screen does (`super::erase`), then
            // kill.
            Request::Kill(id) => {
                if self.app.start_backspace(&id) {
                    self.killing = Some(id);
                } else {
                    self.kill(&id);
                }
                Did::Stay
            }
            // Let the row land first, as full screen does, then write.
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

    fn cursor(&self) -> Option<Id> {
        let rows = self.app.visible();
        rows.get(self.app.selected_index())
            .and_then(|s| Id::new(&s.id))
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

    /// Relist after a kill or move, dropping any listing in flight: it
    /// predates the change and would undo it.
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

    /// Hand new sessions to the notes reader, `super::HAND_OVER` of work at a
    /// time, as the picker does.
    fn hand_over(&mut self, transport: &Shared) {
        let fresh: Vec<Session> = self
            .app
            .sessions()
            .iter()
            .filter(|s| self.handed.insert(s.id.clone()))
            .cloned()
            .collect();
        transport.reach_all(&fresh);
        self.unread.extend(fresh);
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

    /// Whether pending work needs frequent wakes.
    fn busy(&self) -> bool {
        self.listing.is_some()
            || !self.unread.is_empty()
            || (!self.heard && self.opened.elapsed() < crate::notes::POLL)
            || self.killing.is_some()
            || self.placing.is_some()
            || self.creating.is_some()
    }
}

impl Floating for Panel {
    /// Centred above the hint row; as wide as the list plus [`ROOM`] each
    /// side, at least [`MIN_WIDTH`]; as tall as all sessions, not just
    /// filtered ones, so it holds still while a filter is typed. Shrinks to
    /// keep the session margin; `None` if too small.
    fn area(&self, screen: Rect) -> Option<Rect> {
        let (room, _) = float::split(screen);
        let list = draw::list_width(&self.app);
        let rows = self.app.sessions().len().max(1) as u16;
        let width = (list + 2 * ROOM + 2)
            .max(MIN_WIDTH)
            .min(room.width.saturating_sub(2 * SESSION_COLS));
        let height = (rows + AROUND_LIST).min(room.height.saturating_sub(2 * SESSION_ROWS));
        if width < list + 6 || height < AROUND_LIST + 1 {
            return None;
        }
        Some(Rect::new(
            room.x + (room.width - width) / 2,
            room.y + (room.height - height) / 2,
            width,
            height,
        ))
    }

    /// Releases are ignored, as is every key while the `c` gap opens (as full
    /// screen does).
    fn on_key(&mut self, key: KeyEvent, now: Instant) -> Did {
        if key.kind == KeyEventKind::Release || self.creating.is_some() {
            return Did::Stay;
        }
        // Catch up first so animations the key starts begin fresh.
        self.move_on(now);
        let request = self.app.on_key(super::translate(key));
        self.deadline = self
            .app
            .pending()
            .is_some()
            .then(|| now + Duration::from_millis(crate::config::get().keys.timeout_ms));
        self.answer(request)
    }

    /// Resolved against the rows as last drawn; presses on the session are the
    /// relay's. Ignored while the `c` gap opens.
    fn on_mouse(&mut self, mouse: MouseEvent, now: Instant) -> Did {
        if self.creating.is_some() {
            return Did::Stay;
        }
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

    /// The tick that ends an animation also counts as a change: it draws the
    /// picker at rest, without which the row the cursor left stays part-lit.
    fn tick(&mut self, now: Instant) -> bool {
        let mut changed = self.app.animating();
        self.move_on(now);
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
        // Once the gap is up; without fades, held as long as full screen holds
        // it (see `super::make_room`).
        if let Some((after, opened)) = self.creating {
            let shown = crate::fade::enabled() || now >= opened + effects::ROOM_HELD;
            if self.app.room().is_none() && shown {
                self.creating = None;
                self.leave = Some(Leave::New(after));
                self.closing = true;
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

    /// Draw picker, theme and effects on an in-memory terminal the size of the
    /// panel's inside, then copy all but its hint row into a buffer of `area`.
    fn draw(&mut self, area: Rect) -> Buffer {
        let mut buf = Buffer::empty(area);
        let inside = float::inside(area);
        self.drawn = inside;
        if inside.width == 0 || inside.height == 0 {
            return buf;
        }
        // The picker draws on a `Frame`; a `TestBackend` gives one of any size.
        let Ok(mut terminal) = Terminal::new(TestBackend::new(inside.width, inside.height));
        let app = &self.app;
        let colours = self.colours.as_ref();
        let Ok(_) = terminal.draw(|f| {
            draw::draw(f, app);
            crate::theme::paint(f.buffer_mut());
            effects::paint(f, app, colours);
        });
        float::lay(&mut buf, inside, terminal.backend().buffer());
        buf
    }

    fn draw_hints(&self, row: Rect) -> Buffer {
        let mut buf = Buffer::empty(row);
        let (text, dim) = draw::hint_line(&self.app, row.width);
        draw::hint_row_on(&mut buf, row, &text, dim);
        crate::theme::paint(&mut buf);
        buf
    }

    fn close(&self) {}

    /// Return from help, settings or a prompt: reload effects, close the `c`
    /// gap, and take up rows a prompt changed (see [`App::reload_effects`],
    /// [`App::close_room`]). A listing in flight is dropped with them, since
    /// it predates the prompt's change.
    fn back(&mut self) {
        self.leave = None;
        self.closing = false;
        self.app.reload_effects();
        self.app.close_room();
        if let Some(rows) = lock(&CHANGED).clone() {
            self.listing = None;
            self.app.set_sessions(rows);
        }
    }

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
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    fn rows(ids: &[&str]) -> Vec<Session> {
        ids.iter()
            .zip(1..)
            .map(|(id, num)| {
                let mut s = Session::new(id.to_string(), format!("s{num}"), 100 + num, num);
                s.state.num = num;
                s
            })
            .collect()
    }

    fn cursor(panel: &Panel) -> String {
        panel.app.visible()[panel.app.selected_index()].id.clone()
    }

    fn ids(panel: &Panel) -> Vec<String> {
        panel.app.sessions().iter().map(|s| s.id.clone()).collect()
    }

    fn press(panel: &mut Panel, c: char) -> Did {
        key(panel, KeyCode::Char(c))
    }

    fn key(panel: &mut Panel, code: KeyCode) -> Did {
        let key = KeyEvent::new(code, KeyModifiers::NONE);
        panel.on_key(key, Instant::now())
    }

    /// `Esc` still returns to the session behind, and the next panel opens on
    /// that session again.
    #[test]
    fn r_renames_the_row_it_is_on_and_the_panel_comes_back_on_it() {
        let _turn = lock(&crate::ui::test_support::PANELS);
        let (other, behind) = ("aaaaaaaa", "bbbbbbbb");
        hand(&rows(&[other, behind]), None);
        let mut panel = Panel::open(behind);
        assert_eq!(cursor(&panel), behind);
        assert_eq!(press(&mut panel, 'k'), Did::Stay);
        assert_eq!(press(&mut panel, 'r'), Did::Close);
        let renamed = Id::new(other).expect("an id");
        assert_eq!(panel.leave(), Some(Leave::Rename(renamed)));

        hand(&rows(&[other, behind]), Some(other));
        let back = Panel::open(behind);
        assert_eq!(cursor(&back), other);
        let esc = back.app.came_from().map(|s| s.id.as_str());
        assert_eq!(esc, Some(behind), "Esc goes back to the session");
        assert_eq!(cursor(&Panel::open(behind)), behind);
        hand(&[], None);
    }

    /// The rows in hand stay stale until the relay ends; on them, a moved
    /// session showed where it had been until the listing came.
    #[test]
    fn a_panel_opened_again_opens_on_the_rows_the_last_one_left() {
        let _turn = lock(&crate::ui::test_support::PANELS);
        let (one, two, behind) = ("aaaaaaaa", "bbbbbbbb", "cccccccc");
        hand(&rows(&[one, two, behind]), None);
        let mut panel = Panel::open(behind);
        // As `place` leaves the rows: `behind` moved first and renumbered.
        let mut moved = rows(&[one, two, behind]);
        moved.rotate_right(1);
        for (s, num) in moved.iter_mut().zip(1..) {
            s.num = num;
            s.state.num = num;
        }
        panel.app.set_sessions(moved);
        panel.changed();
        assert_eq!(key(&mut panel, KeyCode::Esc), Did::Close);
        assert_eq!(panel.leave(), None, "back to the session");

        let again = Panel::open(behind);
        assert_eq!(ids(&again), [behind, one, two]);
        assert_eq!(cursor(&again), behind);
        hand(&[], None);
    }

    #[test]
    fn c_opens_the_gap_and_goes_to_put_the_session_after_the_row() {
        let _turn = lock(&crate::ui::test_support::PANELS);
        let (one, two, behind) = ("aaaaaaaa", "bbbbbbbb", "cccccccc");
        hand(&rows(&[one, two, behind]), None);
        let mut panel = Panel::open(behind);
        assert_eq!(press(&mut panel, 'k'), Did::Stay);
        assert_eq!(press(&mut panel, 'c'), Did::Stay, "the gap first");
        assert!(panel.app.has_room());
        assert_eq!(press(&mut panel, 'j'), Did::Stay);
        assert_eq!(cursor(&panel), two, "no key while the gap comes up");
        assert!(!panel.closing());

        let up = Instant::now() + effects::ROOM_HELD + Duration::from_millis(1);
        assert!(panel.tick(up));
        assert!(panel.closing());
        let after = Id::new(two).expect("an id");
        assert_eq!(panel.leave(), Some(Leave::New(Some(after))));
        hand(&[], None);
    }

    /// `Esc` afterwards returns to the session, not to the help.
    #[test]
    fn back_from_the_help_the_panel_is_as_it_was_left() {
        let _turn = lock(&crate::ui::test_support::PANELS);
        let (other, behind) = ("aaaaaaaa", "bbbbbbbb");
        hand(&rows(&[other, behind]), None);
        let mut panel = Panel::open(behind);
        assert_eq!(press(&mut panel, 'k'), Did::Stay);
        assert_eq!(press(&mut panel, '?'), Did::Close);
        assert_eq!(panel.leave(), Some(Leave::Help));

        panel.back();
        assert_eq!(panel.leave(), None);
        assert!(!panel.closing());
        assert_eq!(cursor(&panel), other);
        assert_eq!(key(&mut panel, KeyCode::Esc), Did::Close);
        assert_eq!(panel.leave(), None, "Esc goes back to the session");
        hand(&[], None);
    }

    #[test]
    fn back_from_a_prompt_the_panel_takes_up_what_it_left() {
        let _turn = lock(&crate::ui::test_support::PANELS);
        let (other, behind) = ("aaaaaaaa", "bbbbbbbb");
        hand(&rows(&[other, behind]), None);
        let mut panel = Panel::open(behind);
        assert_eq!(press(&mut panel, 'k'), Did::Stay);
        assert_eq!(press(&mut panel, 'r'), Did::Close);
        let mut renamed = rows(&[other, behind]);
        renamed[0].name = "blog".into();
        leave_rows(&renamed);
        panel.back();
        assert_eq!(panel.leave(), None);
        assert_eq!(panel.app.sessions()[0].name, "blog");
        assert_eq!(cursor(&panel), other);

        hand(&rows(&[other, behind]), None);
        let mut panel = Panel::open(behind);
        assert_eq!(press(&mut panel, 'c'), Did::Stay);
        assert!(panel.app.has_room());
        let up = Instant::now() + effects::ROOM_HELD + Duration::from_millis(1);
        assert!(panel.tick(up));
        assert!(panel.closing());
        panel.back();
        assert!(!panel.app.has_room(), "the gap closed");
        assert!(!panel.closing());
        hand(&[], None);
    }

    /// The last glow frame shows the row at rest, so the relay must repaint.
    #[test]
    fn the_tick_that_ends_the_glow_is_a_change() {
        let _turn = lock(&crate::ui::test_support::PANELS);
        let (other, behind) = ("aaaaaaaa", "bbbbbbbb");
        hand(&rows(&[other, behind]), None);
        let mut panel = Panel::open(behind);
        panel.app.set_effects(true);
        let now = Instant::now();
        let up = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(panel.on_key(up, now), Did::Stay);
        assert!(panel.moving(), "the row left glows");
        assert!(panel.tick(now + effects::AFTERGLOW / 2), "on its way back");
        assert!(panel.tick(now + effects::AFTERGLOW), "the last of the glow");
        assert!(!panel.moving());
        assert!(
            !panel.tick(now + effects::AFTERGLOW * 2),
            "and nothing after"
        );
        hand(&[], None);
    }

    fn rows_of(buf: &Buffer, area: Rect) -> Vec<String> {
        (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn the_panels_hint_row_is_the_full_screens() {
        let _turn = lock(&crate::ui::test_support::PANELS);
        let (other, behind) = ("aaaaaaaa", "bbbbbbbb");
        hand(&rows(&[other, behind]), None);
        let panel = Panel::open(behind);
        let mut full = App::new(rows(&[other, behind]));
        full.set_came_from(behind);
        for (width, height) in [(80, 24), (120, 36), (200, 50)] {
            let (_, row) = float::split(Rect::new(0, 0, width, height));
            assert_eq!(row, Rect::new(0, height - 1, width, 1));
            let floating = panel.draw_hints(row);
            let whole = crate::ui::test_support::buffer(width, height, |f| draw::draw(f, &full));
            for x in 0..width {
                assert_eq!(
                    floating[(x, row.y)],
                    whole[(x, row.y)],
                    "{width}x{height}, column {x}"
                );
            }
            let hints = &rows_of(&floating, row)[0];
            assert!(hints.contains("esc back"), "{width}x{height}: {hints:?}");
            if width == 80 {
                assert_eq!(
                    hints.trim(),
                    "↑↓ move  ⏎ attach  c new  x kill  ␣ order  / find  , settings  esc back"
                );
            } else {
                assert!(hints.contains("r rename") && hints.contains("q quit"));
            }
        }
        hand(&[], None);
    }

    #[test]
    fn the_panel_says_how_to_reach_the_settings() {
        let _turn = lock(&crate::ui::test_support::PANELS);
        let panel = Panel::open("abcd2345");
        for (width, height) in [(80, 24), (200, 50)] {
            let (_, row) = float::split(Rect::new(0, 0, width, height));
            let hints = &rows_of(&panel.draw_hints(row), row)[0];
            assert!(hints.contains(", settings"), "{width}x{height}: {hints:?}");
        }
    }

    #[test]
    fn the_panel_leaves_its_hint_row_to_the_bottom_of_the_screen() {
        let _turn = lock(&crate::ui::test_support::PANELS);
        let (other, behind) = ("aaaaaaaa", "bbbbbbbb");
        hand(&rows(&[other, behind]), None);
        let mut panel = Panel::open(behind);
        let screen = Rect::new(0, 0, 80, 24);
        let (_, row) = float::split(screen);
        let area = panel.area(screen).expect("room to float");
        assert!(area.bottom() + SESSION_ROWS <= row.y, "{area:?}");
        let rows = rows_of(&panel.draw(area), area);
        assert_eq!(rows.len(), 2 + AROUND_LIST as usize, "{rows:#?}");
        assert!(
            rows[2].contains("s1") && rows[3].contains("s2"),
            "{rows:#?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("move") || r.contains("esc")),
            "{rows:#?}"
        );
        assert!(rows[rows.len() - 1].trim().is_empty(), "{rows:#?}");
        hand(&[], None);
    }

    #[test]
    fn at_its_least_width_the_panel_says_a_note_whole() {
        use crate::notes::{Kind, Note};
        let _turn = lock(&crate::ui::test_support::PANELS);
        let (other, behind) = ("aaaaaaaa", "bbbbbbbb");
        hand(&rows(&[other, behind]), None);
        let mut panel = Panel::open(behind);
        let note = Note {
            kind: Kind::Notified,
            title: "Claude Code".into(),
            text: "Claude needs your permission".into(),
            age: Duration::from_secs(59 * 60),
            read_at: Instant::now(),
        };
        panel.app.set_notes(std::collections::HashMap::from([(
            behind.to_string(),
            note,
        )]));
        let area = panel.area(Rect::new(0, 0, 80, 24)).expect("room to float");
        assert_eq!(area.width, MIN_WIDTH, "a list narrower than its note");
        let buf = panel.draw(area);
        crate::ui::test_support::assert_corners_blank(&buf);
        let rows = rows_of(&buf, area);
        let caption = "Claude Code · Claude needs your permission · 59m";
        assert!(rows.iter().any(|r| r.contains(caption)), "{rows:#?}");
        hand(&[], None);
    }
}
