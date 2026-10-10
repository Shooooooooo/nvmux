//! The picker: a centred list of session names and one dim hint line on the
//! last row. No borders, no popups — the hint line *is* the picker's help, and
//! the filter and kill confirm replace it in place.
//!
//! [`prompt`] and [`help`] are the other two screens. Both take the whole
//! terminal rather than drawing over anything, share the same vocabulary
//! (centred content, one dim hint row, no borders, no colour of their own),
//! and hand the same client back afterwards. `prompt::run` owns a terminal for
//! `<prefix> c`, which arrives with none; `prompt::run_on` borrows the
//! picker's — nesting the two would enter the alternate screen twice and leave
//! it once.
//!
//! [`attaching`] is a fourth, and the odd one out: not a screen the user works
//! on but the wait between the picker (or a `<prefix>` switch) and the session,
//! up only while the attach probe is in flight. It keeps the vocabulary — a
//! centred line where the picker draws its list, one dim hint row on the last
//! line, no borders, no colour of its own — and is the one screen that leaves
//! the mouse alone, for a reason given there.
//!
//! # There is no preview pane, and there must never be one
//!
//! Neovim sizes the global grid to the per-dimension **minimum** across every
//! attached UI (`ui_refresh()`, run unconditionally at the end of
//! `ui_attach_impl()`), so a small preview UI would shrink the grid of the
//! session being edited in and fire `VimResized`. Confirmed: attaching a 40x10
//! UI alongside a 120x40 one collapses both to 40x10. No `ui_option` opts out.
//! So the picker reads session state over plain RPC and never attaches a second
//! UI to a live session.
//!
//! # Colour
//!
//! Set no background and hardcode no palette. With no `[theme]` the screens
//! set no colour at all; a theme colours the roles they already have — the
//! selection, what is muted, a match, a note, what went wrong — and never a
//! background (see [`crate::theme`]). Do **not** rely on crossterm's own
//! `NO_COLOR` handling: it turns `SetForegroundColor(c)` into a bare `ESC[m`,
//! a full SGR reset that wipes bold, reverse and dim mid-line. Call
//! `force_color_output(true)`, test the variable directly, and set no colours
//! when it is present — which [`crate::theme::current`] does for the theme.
//!
//! Inheriting the palette has a cost the screens have to pay themselves.
//! Setting no colours means ratatui writes no SGR before its content, so a
//! screen is painted in whatever attributes the terminal was already in — and
//! coming back from a session, that is whatever the editor happened to be
//! drawing when the relay stopped. [`Screen::open`] therefore drops the
//! inherited attributes first, which is the one place that can: every screen is
//! taken through it. A theme does not make that redundant: a cell drawn in a
//! colour sets that colour and nothing else, and inherits the rest as a plain
//! one does.
//!
//! Beyond a theme's roles, the fade ([`crate::fade`]) and the picker's passing
//! effects ([`effects`]) are the only things that paint a colour on these
//! screens, and they do so as a post-pass over a finished frame, never inside
//! a `draw`: with no theme a screen's own drawing stays colourless, and its
//! tests say so. Each sets out from the colour a cell is drawn in
//! ([`crate::theme::ink`]), so in a theme a coloured cell fades from its own
//! colour.

pub mod app;
pub mod attaching;
pub mod backspace;
pub mod complete;
pub mod draw;
pub mod effects;
pub mod help;
pub mod landing;
pub mod note;
pub mod prompt;
pub mod setup;
pub mod sonar;
pub mod starfield;
pub mod swap;
#[cfg(test)]
pub(crate) mod test_support;

/// What the picker returned.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Attach to this session.
    Attach {
        session: Session,
        /// The listing the picker was showing — or, after a session created
        /// there was moved up to the highlighted row, the one listed once its
        /// place was stored (see `settle`).
        ///
        /// Carried out rather than re-listed, for the reason the picker already
        /// resolves its own keys against the list in hand: a listing is a script
        /// run — ~230 ms over SSH, and 88-110 ms on a local host whose forks are
        /// expensive — and the session loop needs the same two answers the
        /// picker had. The highest number, so the prefix machine knows when a
        /// digit can be acted on without waiting; and the rows themselves, so a
        /// `<prefix>` switch can name one without going back to disk.
        sessions: Vec<Session>,
        /// The session's name, left standing on the cleared screen for the
        /// session to take down as it dissolves in — or `None`, the screen
        /// left empty (see [`crate::handoff`]).
        hand_off: Option<HandOff>,
    },
    /// The user quit.
    Quit,
}

impl Outcome {
    /// Whether this hands the terminal straight to a client spawn, and so must
    /// give it back cleared — see [`Screen::close_for_attach`].
    fn attaches(&self) -> bool {
        matches!(self, Self::Attach { .. })
    }

    /// What to leave on the screen that is given back cleared: the name being
    /// handed off, if there is one.
    fn leaves(&self) -> Vec<u8> {
        match self {
            Self::Attach {
                hand_off: Some(h), ..
            } => h.show(),
            _ => Vec::new(),
        }
    }
}

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};

use crate::config::Notes as NoteStyle;
use crate::error::Result;
use crate::handoff::HandOff;
use crate::session::Session;
use crate::transport::Transport;
use app::{App, Key, Mouse, Request};

/// A bounded poll rather than an indefinite read, so a resize is not stuck
/// behind an idle keyboard.
const TICK: Duration = Duration::from_millis(250);

/// The tick for a screen waiting on an answer it deliberately did not block
/// for — the create prompt's directory completion, and nothing else so far.
///
/// A quarter of a second is the right wait for a resize, and far too long for a
/// suggestion: a local listing comes back in about a millisecond, so on
/// [`TICK`] the ghost text would appear a fifth of a second after it was ready
/// and read as lag in the prompt rather than in the disk. Roughly one frame,
/// and only for the few frames an answer is actually in flight — an idle prompt
/// is back on [`TICK`] and costs nothing.
const BUSY_TICK: Duration = Duration::from_millis(16);

/// A ratatui screen that is guaranteed to be given back.
///
/// `ratatui::try_init` enables raw mode and enters the alternate screen, and
/// nothing undoes that on `Drop` — so every `?` between init and restore was a
/// way to leave the user's shell with no echo. This guard owns the terminal
/// for one screen; dropping it restores, whatever path led there. The picker,
/// the prompt, the help and the first-run screen all open through here.
pub(crate) struct Screen {
    terminal: ratatui::DefaultTerminal,
    /// Set by the explicit close so `Drop` does not restore a second time.
    closed: bool,
}

impl Screen {
    /// Take the terminal — and the mouse.
    ///
    /// Every screen turns mouse reporting on for itself and off again as it
    /// closes, whatever is behind it. The relay puts a resumed client's own
    /// setting back: a kept client's (`[client] per_session`, the default) from
    /// its ledger, as the client last wrote it (see [`crate::ledger`]), and a
    /// held one's by asking the client's server (see `pty::repaint`); a client
    /// spawned next enables its own. The screens set no colours of their own so
    /// as to inherit the terminal's palette, but a *mode* cannot be inherited the
    /// same way — a client enables the mouse once, at startup, and would never
    /// re-enable a mode nvmux had turned off — which is why this is restored
    /// rather than left alone.
    pub(crate) fn open() -> Result<Self> {
        Self::open_with(true)
    }

    /// [`Screen::open`], saying whether to take the mouse.
    ///
    /// Every screen the user works on does. The one that does not is
    /// [`attaching`], which reads nothing at all for the first seconds of its
    /// life so that keys typed ahead stay in the terminal's queue for the
    /// client it is waiting on — and mouse reporting would fill that same
    /// queue with motion reports, handed to the editor as input the moment
    /// the relay began. With it off nothing is generated, and the release on
    /// close is a harmless no-op.
    pub(crate) fn open_with(mouse: bool) -> Result<Self> {
        // Stops crossterm second-guessing us; see the module docs on colour.
        ratatui::crossterm::style::force_color_output(true);

        // Before the alternate screen and before the clear below, because both
        // erase with the *current* background colour, and the screen this takes
        // over from may be a Neovim client stopped mid-frame. See the module
        // docs on colour, and `term::reset_inherited_attributes`.
        crate::term::reset_inherited_attributes();

        let mut screen = Self {
            terminal: ratatui::try_init()?,
            closed: false,
        };
        // A second "enter alternate screen" is a no-op on xterm and kitty, and
        // ratatui's first draw only paints what differs from an empty buffer —
        // so without this the content lands in the middle of the editor's last
        // frame. From here on an error restores through `Drop`.
        screen.terminal.clear()?;
        if mouse {
            crate::term::enable_mouse();
        }
        Ok(screen)
    }

    /// Undo [`crate::term::enable_mouse`]. Before the modes and the screen
    /// switch, so nothing this writes can land after the one-write handover the
    /// attach path relies on.
    fn release_mouse(&mut self) {
        crate::term::disable_mouse();
    }

    pub(crate) fn terminal(&mut self) -> &mut ratatui::DefaultTerminal {
        &mut self.terminal
    }

    /// Give the terminal back, reporting a failure to do so.
    pub(crate) fn close(mut self) -> Result<()> {
        self.closed = true;
        self.release_mouse();
        ratatui::try_restore()?;
        Ok(())
    }

    /// Give the terminal back **cleared**, for a screen that ends by handing it
    /// to a client spawn.
    ///
    /// Not `ratatui::try_restore()`: that leaves the alternate screen and stops,
    /// which uncovers the screen this one was drawn over — the session being
    /// switched away from — and nothing erases it until the next relay begins.
    /// The spawn in between is two RPC round trips and a fresh `nvim` process,
    /// so that stale frame is what the user watches for the whole switch.
    ///
    /// `try_restore` is exactly `disable_raw_mode` plus `\e[?1049l`, and that
    /// escape is the first thing [`crate::term::leave_alt_screen_and_clear`]
    /// writes — so doing the modes here and the screen there loses nothing and
    /// puts the leave and the erase in one `write`.
    ///
    /// `keep` is drawn on the cleared screen in the same synchronized update
    /// as the clear: the name of the session being attached to, which the
    /// picker hands across (see [`crate::handoff`]). Empty, the screen is left
    /// empty, as it always was.
    pub(crate) fn close_for_attach(mut self, keep: &[u8]) -> Result<()> {
        self.closed = true;
        self.release_mouse();
        // Modes first, for the reason ratatui gives for the same order: dropping
        // raw mode has the wider side effects. The screen is put back either
        // way, so a failure to restore the modes cannot also strand the user on
        // the picker's alternate screen.
        let modes = ratatui::crossterm::terminal::disable_raw_mode();
        if keep.is_empty() {
            crate::term::leave_alt_screen_and_clear();
        } else {
            crate::term::leave_alt_screen_and_clear_keeping(keep);
        }
        modes?;
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        if !self.closed {
            // The error path: `restore` reports a failure on stderr and goes
            // on, which is all that can be done while an error is already in
            // flight.
            self.release_mouse();
            ratatui::restore();
        }
    }
}

/// Take the terminal for one screen, run `f` on it, and give it back.
///
/// The restore happens before the outcome propagates: an error that leaves the
/// terminal in raw mode with no echo is far worse than the error itself.
pub(crate) fn owning<T>(f: impl FnOnce(&mut ratatui::DefaultTerminal) -> Result<T>) -> Result<T> {
    let mut screen = Screen::open()?;
    let outcome = f(screen.terminal());
    screen.close()?;
    outcome
}

/// [`owning`], for a screen that can end by handing the terminal to a session:
/// `attaches` names that outcome, and it alone gives the terminal back through
/// [`Screen::close_for_attach`] rather than leaving the outgoing session's frame
/// on display for the length of the client spawn. `mouse` is whether the
/// screen takes the mouse — see [`Screen::open_with`].
pub(crate) fn owning_for_attach<T>(
    attaches: impl FnOnce(&T) -> bool,
    mouse: bool,
    f: impl FnOnce(&mut ratatui::DefaultTerminal) -> Result<T>,
) -> Result<T> {
    owning_for_attach_leaving(attaches, |_| Vec::new(), mouse, f)
}

/// [`owning_for_attach`], with `leaves` saying what to leave standing on the
/// screen it gives back cleared (see [`Screen::close_for_attach`]).
pub(crate) fn owning_for_attach_leaving<T>(
    attaches: impl FnOnce(&T) -> bool,
    leaves: impl FnOnce(&T) -> Vec<u8>,
    mouse: bool,
    f: impl FnOnce(&mut ratatui::DefaultTerminal) -> Result<T>,
) -> Result<T> {
    let mut screen = Screen::open_with(mouse)?;
    let outcome = f(screen.terminal());
    // The restore happens before the outcome propagates, as in `owning`, and an
    // error takes the ordinary close: there is no session coming, and the shell
    // the error is about to be printed to wants its screen back, not a cleared
    // one.
    match &outcome {
        Ok(v) if attaches(v) => screen.close_for_attach(&leaves(v))?,
        _ => screen.close()?,
    }
    outcome
}

/// Wait up to [`TICK`] for a keypress the screens understand.
///
/// `Ok(None)` means nothing happened and the caller should loop — either the
/// poll timed out or the event was not a key press. Press only: with the kitty
/// protocol pushed by Neovim, a release would otherwise count as a second
/// keypress.
///
/// [`setup`] deliberately does not use this: it needs the raw chord, since
/// [`translate`] discards every control chord but the three the picker binds.
pub(crate) fn poll_key() -> Result<Option<Key>> {
    poll_key_for(TICK)
}

/// The same, waiting only `tick` — for a caller with something else to check
/// when the keyboard is idle. See [`BUSY_TICK`].
pub(crate) fn poll_key_for(tick: Duration) -> Result<Option<Key>> {
    if !event::poll(tick)? {
        return Ok(None);
    }
    Ok(match event::read()? {
        Event::Key(k) if k.kind == KeyEventKind::Press => Some(translate(k)),
        _ => None,
    })
}

/// Run the picker until the user attaches or quits. `message` replaces the hint
/// row — how a failed attach reports itself without exiting the program.
/// `focused` is the session the caller came from, if any: the cursor starts on
/// it, so `<prefix> Space` opens the picker where the user already was.
///
/// `still_attached` says that session still has a client behind it, which is
/// what makes `Esc` a way back to it rather than a key that does nothing. It is
/// false where the picker is all there is: the first screen of the program, and
/// the trip back from a failed attach or a session whose child exited.
///
/// `seen` is the session the user was looking at until the picker came up, if
/// any: what it was keeping to report is not news to them, and is cleared
/// before the picker reads it (see [`crate::notes`]).
pub fn run(
    transport: &dyn Transport,
    message: Option<String>,
    focused: Option<&str>,
    still_attached: bool,
    seen: Option<&str>,
) -> Result<Outcome> {
    owning_for_attach_leaving(Outcome::attaches, Outcome::leaves, true, |terminal| {
        run_loop(terminal, transport, message, focused, still_attached, seen)
    })
}

/// How long, of one pass of the picker's loop, may go on handing sessions to
/// the reader of their notes. Locally every session fits in it; over ssh a
/// session given its first forward takes most of it — an `ssh -O forward`
/// to the master — so they go a few at a pass, between frames, rather than
/// all before the first.
const HAND_OVER: Duration = Duration::from_millis(8);

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    transport: &dyn Transport,
    message: Option<String>,
    focused: Option<&str>,
    still_attached: bool,
    seen: Option<&str>,
) -> Result<Outcome> {
    let mut app = App::new(transport.list_sessions()?);
    // What each session has to report: read on a thread of its own (see
    // `crate::notes::Poller`), each session handed to it once the loop has
    // its socket, and what they said taken up on every pass. With `[picker]
    // notes = "off"` the same thread takes the watcher out of them instead.
    let poller = crate::notes::Poller::start(app.note_style(), seen.map(str::to_string));
    let mut handed: HashSet<String> = HashSet::new();
    let mut unread: VecDeque<Session> = VecDeque::new();
    // Until the reader first says what the sessions have to report, the loop
    // waits a frame rather than a tick, so the notes come up with the picker
    // rather than a quarter of a second after it — no longer than a round of
    // reads can take, and not at all with the notes off, when it never says.
    let mut heard = app.note_style() == NoteStyle::Off;
    let opened = Instant::now();
    if let Some(id) = focused {
        app.select_session(id);
        if still_attached {
            app.set_came_from(id);
        }
    }
    if let Some(msg) = message {
        app.set_message(msg);
    }

    // Dissolve the picker up out of the background before taking any input.
    // In place of a first draw: the fade's last frame is the plain screen, so
    // the loop's own first `draw` repaints nothing.
    crate::fade::fade_in(terminal, |f| draw::draw(f, &app))?;

    // When a half-typed session number must be settled. `App` decides every
    // unambiguous digit on its own, so this is only ever set when one number is
    // a prefix of another — sessions 1 and 12 both present.
    let mut deadline: Option<Instant> = None;
    // When whatever is moving on the picker — a trail, a glow, a fading row —
    // was last moved on. The clock lives here for the same reason the digit
    // deadline does.
    let mut ticked = Instant::now();
    // What the effects paint with, if they paint in colour: asked once, since
    // neither the answer nor `NO_COLOR` changes for the run.
    let palette = effects::palette();
    // When the loop last drew, and the area it drew: `None` until its first
    // frame, which is always drawn.
    let mut drawn: Option<Instant> = None;
    let mut area = ratatui::layout::Rect::default();
    // Whether a frame that moves waits for the terminal to have taken it (see
    // `caught_up`). Given up for the rest of the run the first time the
    // terminal does not answer.
    let mut paced = true;

    loop {
        tick(&mut app, &mut ticked);

        // Every session the listing has that the reader has not been given —
        // all of them at first, and whatever a later listing adds — handed
        // over a few milliseconds' worth at a time; and whatever they said
        // since the last pass, taken up before it is drawn.
        for s in app.sessions() {
            if handed.insert(s.id.clone()) {
                unread.push_back(s.clone());
            }
        }
        hand_over(transport, &poller, &mut unread, app.note_style());
        if let Some(notes) = poller.take() {
            app.set_notes(notes);
            heard = true;
        }
        let awaiting = !heard && opened.elapsed() < crate::notes::POLL;

        // Input already waiting is taken before the screen is drawn again, so
        // a frame answers everything that came in while the last one was going
        // out rather than one key of it. One frame a key holds the picker to
        // the terminal's pace, and a held key repeats faster than some
        // terminals take a frame: the repeats queue behind the frames, and the
        // session in flight goes on moving after the key comes up, for longer
        // the longer it was held. Never more than a frame's time without a
        // draw, though, so input that does not pause — the pointer sweeping
        // the list — still sees the screen follow it.
        let waiting = event::poll(Duration::ZERO)?;
        if !waiting || drawn.is_none_or(|at| at.elapsed() >= crate::fade::FRAME) {
            // The area kept for the mouse: a click is resolved against the
            // screen as it was last drawn, which is the one the user clicked
            // on.
            area = terminal.draw(|f| frame(f, &app, palette))?.area;
            // While anything is moving a frame goes out every `FRAME` whether
            // or not a key asked for one, and the terminal has to keep up.
            if paced && app.animating() {
                paced = caught_up();
            }
            drawn = Some(Instant::now());
        }

        // A frame's wait while anything is moving, so it moves, and while the
        // sessions are still to be handed to the reader of their notes, or it
        // has yet to answer, so the notes come soon; a spinner's frame while
        // one turns beside a busy session; the rest of the time — and all the
        // time, with the effects switched off in `[effects]` and nothing busy
        // — the picker changes on a key and nothing else, and waits the
        // ordinary tick.
        let tick_for = if app.animating() || !unread.is_empty() || awaiting {
            crate::fade::FRAME
        } else if app.spinning() {
            note::SPIN
        } else {
            TICK
        };
        if !waiting && !event::poll(tick_for)? {
            // The clock lives here rather than in `App`, which stays pure —
            // the same split `pty::pump` uses for `keys::Prefix`.
            if deadline.is_some_and(|d| Instant::now() >= d) {
                deadline = None;
                if let Request::Attach(id) = app.resolve_pending() {
                    if let Some(session) = app.session(&id).cloned() {
                        return leave_to_session(terminal, &app, session);
                    }
                }
            }
            continue;
        }
        let event = event::read()?;
        // Up to the moment of the event, so what it starts — a glow, a fade —
        // starts from nothing rather than from however long the wait for it
        // took.
        tick(&mut app, &mut ticked);
        let request = match event {
            Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(translate(k)),
            Event::Mouse(m) => {
                match translate_mouse(m, draw::row_at(&app, area, m.column, m.row)) {
                    Some(mouse) => app.on_mouse(mouse),
                    None => continue,
                }
            }
            _ => continue,
        };
        // A session just put down lands before anything is written: the
        // renumber that follows blocks over ssh, and the landing is the
        // answer to the key, so it comes first. Any other request that blocks
        // is drawn first too, so the screen does not sit on the key's question
        // — a `[y/N]`, a session in flight — through the I/O.
        //
        // A key that asks for nothing blocks on nothing, and the loop draws
        // next. A frame here as well would be a second for the same key —
        // with a session in flight, every arrow — and a second chance for a
        // held key to fall behind the screen.
        if app.landing().is_some() || request != Request::None {
            play_out(terminal, &mut app, palette, |app| app.landing().is_some())?;
        }
        deadline = app
            .pending()
            .is_some()
            .then(|| Instant::now() + Duration::from_millis(crate::config::get().keys.timeout_ms));

        // Every request names a row the picker is showing, so it is resolved
        // against the list in hand rather than a fresh listing: over SSH each
        // listing is a script run (~230 ms), and the action itself finds out
        // soon enough if the session has gone in the meantime — an attach
        // pings before spawning, a kill reports "absent", a rename reports
        // "not found". The list is re-read only after something changed it.
        match request {
            Request::None => {}
            Request::Quit => return Ok(Outcome::Quit),

            Request::Attach(id) => {
                if let Some(session) = app.session(&id).cloned() {
                    return leave_to_session(terminal, &app, session);
                }
            }

            Request::NewSession => {
                // A gap opens where the session will go, and the prompt comes
                // up out of it (see `make_room`); with that off, the prompt
                // simply replaces the picker. A create dissolves the prompt
                // out on its way to the client spawn either way — that is the
                // prompt's own doing, since its screen is the one up.
                let arrival = make_room(terminal, &mut app, palette)?;
                let outcome = prompt::run_on(
                    terminal,
                    transport,
                    prompt::Task::Create {
                        listed: Some(app.sessions()),
                        number: Some(app.new_number()),
                    },
                    arrival,
                );
                // Whatever the prompt answered, the gap was only ever the
                // picker's way out: a created session is in the listing that
                // follows, and a cancelled one never was.
                app.close_room();
                match outcome? {
                    prompt::Outcome::Created(session) => {
                        let (session, sessions) = settle(&app, transport, session);
                        return Ok(Outcome::Attach {
                            session,
                            sessions,
                            hand_off: None,
                        });
                    }
                    prompt::Outcome::Cancelled => {
                        // The prompt dissolved out if it dissolved in, and the
                        // picker it closed onto comes back up the same way.
                        if arrival != prompt::Arrival::Cut {
                            crate::fade::fade_in(terminal, |f| draw::draw(f, &app))?;
                        }
                    }
                    prompt::Outcome::Renamed => refresh(&mut app, transport)?,
                }
            }

            Request::RenameSession(id) => {
                if let Some(session) = app.session(&id).cloned() {
                    let outcome = prompt::run_on(
                        terminal,
                        transport,
                        prompt::Task::Rename(&session),
                        prompt::Arrival::Cut,
                    )?;
                    if !matches!(outcome, prompt::Outcome::Cancelled) {
                        refresh(&mut app, transport)?;
                    }
                }
            }

            Request::Kill(id) => {
                if let Some(session) = app.session(&id).cloned() {
                    erase(terminal, &mut app, &id, palette)?;
                    if let Err(e) = transport.kill_session(&session) {
                        app.set_message(e.one_line());
                    }
                    refresh(&mut app, transport)?;
                }
            }

            Request::Reorder(order) => {
                let batch: Vec<Session> = order
                    .into_iter()
                    .filter_map(|(id, num)| {
                        app.session(&id).cloned().map(|mut s| {
                            s.num = num;
                            s
                        })
                    })
                    .collect();
                if let Err(e) = transport.renumber(&batch) {
                    app.set_message(e.one_line());
                }
                // Whether it wrote or failed, what the numbers now are is a
                // listing's answer and not ours. `set_sessions` restores the
                // selection by id, so the cursor stays on the session that was
                // just placed.
                refresh(&mut app, transport)?;
            }

            Request::Help => help::run_on(terminal, false)?,
        }
    }
}

/// Wait for the terminal to have got through everything written to it.
///
/// A terminal reads what it is sent as it arrives and works through it at its
/// own pace. While a session is in flight its trail sends a frame every
/// [`crate::fade::FRAME`], and a terminal that takes nearly that long over each
/// keeps up with the trail alone, but not with the extra frames a held key
/// adds. It falls behind during the hold and, the trail still coming, barely
/// gains on the backlog afterwards. The session goes on moving after the key
/// comes up, and every key after it waits its turn behind frames already
/// out of date — until `Enter` puts the session down and the frames stop.
/// Before the hold, the same terminal was keeping up and a key was instant.
///
/// Asking where the cursor is says when the terminal has caught up: it
/// answers in turn, once it has got that far through what it was sent. Waiting
/// for the answer after each frame keeps one frame in flight, so frames go no
/// faster than the terminal takes them and no key is ever more than a frame
/// behind. Keys that arrive meanwhile are set aside by crossterm and handed
/// back to the loop, which takes them all before its next frame.
///
/// Nothing new to ask of a terminal: [`Screen::open`] has already asked it
/// the same thing, through ratatui's `clear`, to get the picker on screen at
/// all. Still, false when it did not answer within crossterm's two seconds,
/// and it is not asked again: an unanswered question would cost that wait on
/// every frame, and an unpaced trail is only what the picker had before.
fn caught_up() -> bool {
    match ratatui::crossterm::cursor::position() {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(error = %e, "the terminal did not say where its cursor is; frames go unpaced");
            false
        }
    }
}

/// Move whatever is passing on the picker on to now.
fn tick(app: &mut App, ticked: &mut Instant) {
    let now = Instant::now();
    app.tick(now - *ticked);
    *ticked = now;
}

/// Give the reader of notes the sockets of the sessions in `unread`, for as
/// long as [`HAND_OVER`] allows, and at least one: locally that is all of
/// them, and over ssh as many as can be given a forward in the time (see
/// [`Transport::local_socket_for`], which makes one where there is none).
///
/// With `[picker] notes = "off"`, only the sessions the watcher has not been
/// taken out of this run, so a remote session is given a forward for it once,
/// not every time the picker comes up.
fn hand_over(
    transport: &dyn Transport,
    poller: &crate::notes::Poller,
    unread: &mut VecDeque<Session>,
    style: NoteStyle,
) {
    let started = Instant::now();
    while let Some(s) = unread.pop_front() {
        if style == NoteStyle::Off && !crate::notes::to_remove(&s.id) {
            continue;
        }
        match transport.local_socket_for(&s) {
            Ok(sock) => poller.watch(&s.id, sock),
            Err(e) => tracing::debug!(id = %s.id, error = %e, "notes: no socket to read"),
        }
        if started.elapsed() >= HAND_OVER {
            break;
        }
    }
}

/// One frame of the picker: the screen, then whatever is passing over it.
fn frame(f: &mut ratatui::Frame, app: &App, palette: Option<&crate::palette::Palette>) {
    draw::draw(f, app);
    effects::paint(f, app, palette);
}

/// Erase the row a kill was just confirmed for, and only then let the kill
/// run (see [`backspace`] for why not during it).
///
/// The row stays empty afterwards until the listing that follows the kill
/// replaces it — or, if the kill failed, puts it back.
fn erase(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    id: &str,
    palette: Option<&crate::palette::Palette>,
) -> Result<()> {
    if !app.start_backspace(id) {
        return Ok(());
    }
    play_out(terminal, app, palette, App::erasing)
}

/// Make room in the list for the session `c` is about to create, and close
/// the picker onto it: what the picker does between the key and the prompt,
/// and how the prompt should come up after it.
///
/// The rows under the cursor step down a line, and in the gap the session
/// stands as it will be listed — the number it will be given, and the name the
/// prompt will offer, numbered after it (see [`prompt::default_name`]), dim —
/// coming up out of the background over
/// [`effects::ROOM`] (see [`App::make_room`]). Then the picker closes in onto
/// that row, and the prompt opens out of its name field, where the same name
/// is waiting: [`prompt::Arrival::FromName`].
///
/// The close and the opening are the fade's, so with no fade — switched off,
/// `NO_COLOR`, or a terminal that never said what its colours are — the gap
/// is held, dim, for [`effects::ROOM_HELD`] and the prompt cuts in. With
/// `[effects.create]` off there is no gap, and the prompt cuts in at once, as
/// it always did. Keys pressed meanwhile wait in the terminal's queue for the
/// prompt, as they do behind every effect played out here.
fn make_room(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    palette: Option<&crate::palette::Palette>,
) -> Result<prompt::Arrival> {
    let name = prompt::default_name(app.sessions(), Some(app.new_number()));
    if !app.make_room(&name) {
        return Ok(prompt::Arrival::Cut);
    }
    let opened = Instant::now();
    play_out(terminal, app, palette, |app| app.room().is_some())?;
    let size = terminal.size()?;
    let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
    match draw::row_rect(app, area, app::GHOST_ID) {
        Some(row) if crate::fade::enabled() => {
            crate::fade::fade_out_onto(terminal, |f| draw::draw(f, app), row.y)?;
            Ok(prompt::Arrival::FromName)
        }
        // Not on screen at all — a terminal with no room for a list — or no
        // fade to close it with: shown for as long as it is worth reading,
        // and then the prompt cuts in over it.
        _ => {
            std::thread::sleep(effects::ROOM_HELD.saturating_sub(opened.elapsed()));
            Ok(prompt::Arrival::Cut)
        }
    }
}

/// Draw frames until `playing` says the effect is over — for the three that
/// must finish before what they precede, the backspace, the landing and the
/// gap a new session opens —
/// and then the screen it leaves. Keys pressed meanwhile wait in the
/// terminal's queue, as they would behind the I/O itself. Nothing playing,
/// that last frame is the only one: the screen as it stands, for a caller
/// about to block.
fn play_out(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    palette: Option<&crate::palette::Palette>,
    playing: impl Fn(&App) -> bool,
) -> Result<()> {
    let mut ticked = Instant::now();
    while playing(app) {
        terminal.draw(|f| frame(f, app, palette))?;
        std::thread::sleep(crate::fade::FRAME);
        tick(app, &mut ticked);
    }
    terminal.draw(|f| frame(f, app, palette))?;
    Ok(())
}

/// Leave the picker for `session`: dissolve the screen out, then hand back the
/// outcome that attaches. The picker's own two exits share this so neither
/// can forget the fade; the prompt's `Created` exit is not one of them, since
/// the screen up at that moment is the prompt's, and the prompt fades it.
/// Quitting does not come through here either — the shell wants its screen
/// back, not a dissolved one.
///
/// Where the hand-off is on (see [`crate::handoff`]), the session's name is
/// kept out of the dissolve, and comes off its bar as plain text where it
/// stood, to be left on the cleared screen for the session to take down.
fn leave_to_session(
    terminal: &mut ratatui::DefaultTerminal,
    app: &App,
    session: Session,
) -> Result<Outcome> {
    let size = terminal.size()?;
    let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
    let hand_off = HandOff::enabled()
        .then(|| hand_off_for(app, area, &session))
        .flatten();
    match &hand_off {
        Some(h) => {
            let (row, col) = h.at();
            let keep = ratatui::layout::Rect::new(col, row, h.width(), 1);
            crate::fade::fade_out_keeping(terminal, |f| draw::draw(f, app), keep)?;
        }
        None => crate::fade::fade_out(terminal, |f| draw::draw(f, app))?,
    }
    Ok(Outcome::Attach {
        session,
        sessions: app.sessions().to_vec(),
        hand_off,
    })
}

/// The name of `session` as the picker has it on screen, ready to be handed
/// off: where its row draws it, cut where the row cuts it. `None` when its row
/// is not on screen — a number typed for a session scrolled out of view.
fn hand_off_for(app: &App, area: ratatui::layout::Rect, session: &Session) -> Option<HandOff> {
    let name = draw::name_rect(app, area, &session.id)?;
    HandOff::new(&session.name, name.y, name.x, name.width)
}

/// Put a session just created from the picker after the row that was
/// highlighted, and hand back the session and the listing the caller attaches
/// with.
///
/// Created last, a session is numbered last. Moving it up is a renumber — the
/// batch a reorder writes, from [`App::placement_of`] — and then a listing,
/// since what the numbers now are is a listing's answer and not ours (the
/// reorder above says why). That is two round trips over ssh, paid only when
/// the highlight was not already on the last row.
///
/// Placed last, nothing is written and nothing listed: the picker's rows plus
/// the new one are the listing, as they always were — the rows are still
/// accurate and this is the one they are missing. Order does not matter to
/// either reader of it — one takes a maximum, the other looks up a number.
///
/// Neither step may lose the session, which exists by now. A renumber that
/// fails leaves it attached where it was created, last; a listing that fails
/// falls back to the rows in hand. Both are logged, since there is no screen
/// left to say so on — the next thing on it is the session.
fn settle(app: &App, transport: &dyn Transport, session: Session) -> (Session, Vec<Session>) {
    let mut appended = app.sessions().to_vec();
    appended.push(session.clone());

    let order = app.placement_of(&session.id);
    if order.is_empty() {
        return (session, appended);
    }
    let batch: Vec<Session> = order
        .into_iter()
        .filter_map(|(id, num)| {
            appended.iter().find(|s| s.id == id).cloned().map(|mut s| {
                s.num = num;
                s
            })
        })
        .collect();
    if let Err(e) = transport.renumber(&batch) {
        tracing::warn!(id = %session.id, error = %e, "could not place the new session");
        return (session, appended);
    }
    match transport.list_sessions() {
        Ok(listing) => {
            let placed = listing
                .iter()
                .find(|s| s.id == session.id)
                .cloned()
                .unwrap_or(session);
            (placed, listing)
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not list the sessions after placing one");
            (session, appended)
        }
    }
}

/// Re-list, restoring the selection by id (`set_sessions` does that).
fn refresh(app: &mut App, transport: &dyn Transport) -> Result<()> {
    app.set_sessions(transport.list_sessions()?);
    Ok(())
}

/// The largest resolved session number in a listing, or 0 for none.
pub fn highest_num(sessions: &[Session]) -> u32 {
    sessions.iter().map(|s| s.state.num).max().unwrap_or(0)
}

/// Reduce a crossterm event to the keys the screens understand.
///
/// A control chord is either one of the three that are bound (`Ctrl-c`,
/// `Ctrl-n`, `Ctrl-p`) or nothing at all — never the bare letter. The prefix
/// is configurable, and someone who reflexively types `<prefix>` on the picker
/// must not find that `Ctrl-x` opened the kill confirm or `Ctrl-q` quit; nor
/// should a chord typed on the help screen close it and forward its second key.
fn translate(k: KeyEvent) -> Key {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Char('c') if ctrl => Key::CtrlC,
        KeyCode::Char('n') if ctrl => Key::CtrlN,
        KeyCode::Char('p') if ctrl => Key::CtrlP,
        KeyCode::Char(_) if ctrl => Key::Other,
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Tab => Key::Tab,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        _ => Key::Other,
    }
}

/// Reduce a crossterm mouse event to the gestures the picker understands, with
/// `row` the visible row under the pointer as [`draw::row_at`] resolved it.
///
/// Plain motion, the left button and the wheel, and nothing else: the other
/// buttons and a horizontal wheel are nothing to the picker.
fn translate_mouse(m: MouseEvent, row: Option<usize>) -> Option<Mouse> {
    match m.kind {
        MouseEventKind::Moved => Some(Mouse::Hover(row)),
        MouseEventKind::Down(MouseButton::Left) => Some(Mouse::Press(row)),
        MouseEventKind::Drag(MouseButton::Left) => Some(Mouse::Drag(row)),
        MouseEventKind::Up(MouseButton::Left) => Some(Mouse::Release),
        MouseEventKind::ScrollUp => Some(Mouse::ScrollUp),
        MouseEventKind::ScrollDown => Some(Mouse::ScrollDown),
        MouseEventKind::Down(_)
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Motion, the left button and the wheel reach the picker, carrying the
    /// row the caller resolved; everything else is dropped before it can.
    #[test]
    fn only_motion_the_left_button_and_the_wheel_reach_the_picker() {
        assert_eq!(
            translate_mouse(mouse(MouseEventKind::Moved), Some(1)),
            Some(Mouse::Hover(Some(1)))
        );
        assert_eq!(
            translate_mouse(mouse(MouseEventKind::Down(MouseButton::Left)), Some(2)),
            Some(Mouse::Press(Some(2)))
        );
        assert_eq!(
            translate_mouse(mouse(MouseEventKind::Drag(MouseButton::Left)), None),
            Some(Mouse::Drag(None))
        );
        assert_eq!(
            translate_mouse(mouse(MouseEventKind::Up(MouseButton::Left)), Some(2)),
            Some(Mouse::Release)
        );
        assert_eq!(
            translate_mouse(mouse(MouseEventKind::ScrollUp), None),
            Some(Mouse::ScrollUp)
        );
        assert_eq!(
            translate_mouse(mouse(MouseEventKind::ScrollDown), None),
            Some(Mouse::ScrollDown)
        );
        for kind in [
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Right),
            MouseEventKind::Drag(MouseButton::Middle),
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            assert_eq!(translate_mouse(mouse(kind), Some(0)), None, "{kind:?}");
        }
    }

    /// The three bound chords are the only ones that survive as chords; a
    /// chord must never arrive as its bare letter (see `translate`).
    #[test]
    fn the_bound_chords_are_distinguished_from_their_plain_letters() {
        for (c, ctrl, want) in [
            ('c', true, Key::CtrlC),
            ('c', false, Key::Char('c')),
            ('n', true, Key::CtrlN),
            ('n', false, Key::Char('n')),
            ('p', true, Key::CtrlP),
            ('p', false, Key::Char('p')),
        ] {
            let mods = if ctrl {
                KeyModifiers::CONTROL
            } else {
                KeyModifiers::NONE
            };
            assert_eq!(
                translate(KeyEvent::new(KeyCode::Char(c), mods)),
                want,
                "{c:?} with ctrl={ctrl}"
            );
        }
    }

    /// Only Ctrl-c, Ctrl-n and Ctrl-p are keys of their own; every other
    /// chord is nothing. The prefix is one of these (Ctrl-Space by default,
    /// but configurable), and `Ctrl-x`, `Ctrl-q`, `Ctrl-r`, `Ctrl-y` all name
    /// picker commands as bare letters.
    ///
    /// The space bar is in the list for the default prefix's sake: typed with
    /// Ctrl it must not reach the filter as a space.
    #[test]
    fn other_control_chords_are_ignored_not_folded_to_letters() {
        for c in ['t', 'x', 'q', 'r', 'y', 'g', 'a', 'd', ' '] {
            assert_eq!(
                translate(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)),
                Key::Other,
                "Ctrl-{c:?} must not act as a plain {c:?}"
            );
        }
    }

    #[test]
    fn shifted_letters_arrive_as_uppercase() {
        // `G` must reach the app as 'G', not as 'g' plus a modifier, or
        // jump-to-last would be indistinguishable from jump-to-first.
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            Key::Char('G')
        );
    }

    /// Why [`Screen::open`] drops the inherited attributes before it draws.
    ///
    /// With no theme the screens set no colours on purpose, so ratatui's
    /// crossterm backend — which tracks fg and bg from `Color::Reset` and
    /// writes SGR only on a difference — emits nothing that would clear an
    /// attribute until the whole frame has been painted, and never writes a
    /// blank cell at all. Whatever the terminal was already in is what the
    /// screen is drawn in: coming back from a session, the editor's own
    /// colours, mid-frame.
    ///
    /// If ratatui ever leads a frame with a reset of its own this fails and the
    /// call in `Screen::open` can be revisited — the point of asserting it
    /// rather than merely writing it down.
    #[test]
    fn a_screen_paints_before_it_clears_anything() {
        let emitted = test_support::emitted(48, 6, |f| draw::draw(f, &App::new(Vec::new())));
        let content = emitted
            .find("no sessions")
            .expect("the empty-list line is the screen's first content");

        for clear in [
            "\x1b[0m", "\x1b[m", "\x1b[39m", "\x1b[49m", "\x1b[22m", "\x1b[27m",
        ] {
            assert!(
                !emitted[..content].contains(clear),
                "{clear:?} before the first glyph: the screen no longer inherits \
                 the terminal's attributes, so term::reset_inherited_attributes \
                 may be redundant"
            );
        }
        assert!(
            emitted.rfind("\x1b[0m").is_some_and(|at| at > content),
            "the only SGR reset in a frame comes after its content, which is \
             too late to undo anything inherited"
        );
    }

    /// In a theme the first thing a screen writes may be a colour — the muted
    /// empty-list line, here — and a colour sets only itself: still nothing
    /// before the content clears the bold, reverse or dim a session left, so
    /// the reset `Screen::open` writes is as needed with a theme as without.
    #[test]
    fn a_theme_clears_nothing_inherited_either() {
        let mut app = App::new(Vec::new());
        app.set_theme(crate::test_support::theme());
        let emitted = test_support::emitted(48, 6, |f| draw::draw(f, &app));
        let content = emitted
            .find("no sessions")
            .expect("the empty-list line is the screen's first content");
        assert!(
            emitted[..content].contains("38;5;8"),
            "the line is not in muted's colour: {emitted:?}"
        );
        for clear in [
            "\x1b[0m", "\x1b[m", "\x1b[39m", "\x1b[49m", "\x1b[22m", "\x1b[27m",
        ] {
            assert!(
                !emitted[..content].contains(clear),
                "{clear:?} before the first glyph in a theme"
            );
        }
    }

    /// Attaching is the outcome that gives the terminal back cleared; quitting
    /// must not, because the shell it returns to wants its own screen rather
    /// than a wiped one.
    #[test]
    fn only_attaching_hands_the_terminal_to_a_session() {
        let session = Session::new("id000000".into(), "a".into(), 100, 1);
        assert!(Outcome::Attach {
            session: session.clone(),
            sessions: vec![session],
            hand_off: None,
        }
        .attaches());
        assert!(!Outcome::Quit.attaches());
    }

    /// The name handed off is the one the picker drew, where it drew it: the
    /// selected row's, after its marker, number and gap.
    #[test]
    fn the_name_handed_off_is_where_the_picker_drew_it() {
        let a = test_support::picker(&["api-server", "dotfiles", "notes"]);
        let area = ratatui::layout::Rect::new(0, 0, 40, 8);
        let lines = test_support::render(40, 8, |f| draw::draw(f, &a));
        let session = a.session("id000001").unwrap().clone();
        let h = hand_off_for(&a, area, &session).expect("on screen");
        let (row, col) = h.at();
        assert_eq!(h.text(), "dotfiles");
        let line = &lines[usize::from(row)];
        let at = line.find("dotfiles").expect("drawn on that row");
        assert_eq!(line[..at].chars().count(), usize::from(col), "{line:?}");
    }

    /// Leaving to attach leaves the name drawn on the cleared screen; quitting
    /// leaves nothing.
    #[test]
    fn only_an_attach_with_a_name_leaves_one() {
        let session = Session::new("id000000".into(), "a".into(), 100, 1);
        let with = Outcome::Attach {
            session: session.clone(),
            sessions: vec![session.clone()],
            hand_off: HandOff::new("a", 3, 4, 10),
        };
        assert_eq!(with.leaves(), HandOff::new("a", 3, 4, 10).unwrap().show());
        let without = Outcome::Attach {
            session: session.clone(),
            sessions: vec![session],
            hand_off: None,
        };
        assert!(without.leaves().is_empty());
        assert!(Outcome::Quit.leaves().is_empty());
    }
}
