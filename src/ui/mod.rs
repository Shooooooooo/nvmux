//! The picker: a centred list of session names and one dim hint line on the
//! last row, which the filter and kill confirm replace in place. No borders or
//! popups.
//!
//! [`prompt`], [`help`] and [`settings`] are the other screens, in the same
//! style (centred, one dim hint row, no borders, no colour); each takes the
//! whole terminal and hands the same client back. `prompt::run` owns a
//! terminal (for `<prefix> c`), `prompt::run_on` borrows the picker's: nesting
//! would enter the alternate screen twice and leave it once. Help and settings
//! work the same way. [`attaching`] is the wait while the attach probe runs,
//! and the one screen that leaves the mouse alone (see there).
//!
//! # No preview pane, ever
//!
//! Neovim sizes the grid to the per-dimension minimum over all attached UIs
//! (`ui_refresh()` at the end of `ui_attach_impl()`): a 40x10 UI beside a
//! 120x40 one shrinks both to 40x10 and fires `VimResized`, and no `ui_option`
//! opts out. So the picker reads state over plain RPC and never attaches.
//!
//! # Colour
//!
//! Set no background and hardcode no palette. Do not rely on crossterm's
//! `NO_COLOR` handling: it turns `SetForegroundColor(c)` into `ESC[m`, a full
//! SGR reset that wipes bold, reverse and dim mid-line. Call
//! `force_color_output(true)`, test the variable directly, and set no colours
//! when it is present.
//!
//! With no SGR written, a screen inherits the terminal's current attributes
//! (e.g. the editor's, mid-frame), so [`Screen::open`] resets them first.
//!
//! Only the fade ([`crate::fade`]), the effects ([`effects`]) and the theme
//! ([`crate::theme`], from `[style] color`) paint colour, as post-passes over a
//! finished frame, never inside `draw`. The theme pass runs first on every
//! frame (fade frames included), inking full-strength cells and leaving dim
//! ones, so later passes fade each cell from the colour it is drawn in.

pub mod app;
pub mod attaching;
pub mod backspace;
pub mod complete;
pub mod draw;
pub mod effects;
pub mod float;
pub mod help;
pub mod landing;
pub mod note;
pub mod picker;
pub mod prompt;
pub mod settings;
pub mod setup;
pub mod sonar;
pub mod starfield;
pub mod swap;
#[cfg(test)]
pub(crate) mod test_support;

#[derive(Debug, Clone)]
pub enum Outcome {
    Attach {
        session: Session,
        /// The listing the picker showed (or, after a new session was moved
        /// below the highlighted row, the one taken once its place was stored;
        /// see `settle`). Passed on rather than relisted, as a listing costs
        /// ~230 ms over SSH and 88-110 ms on hosts with slow forks. The
        /// session loop needs the highest number (to act on a digit without
        /// waiting) and the rows (to name `<prefix>` switch targets).
        sessions: Vec<Session>,
        /// The session's name, left on the cleared screen for the session to
        /// remove as it dissolves in; `None` leaves it empty (see
        /// [`crate::handoff`]).
        hand_off: Option<HandOff>,
        /// Whether the settings float over the session as it returns: the
        /// picker's `,` sets this when it came up over this session (see
        /// [`settings::Outcome::Float`]).
        float_settings: bool,
    },
    Quit,
}

impl Outcome {
    /// Whether this hands the terminal to a client spawn, so it must be given
    /// back cleared (see [`Screen::close_for_attach`]).
    fn attaches(&self) -> bool {
        matches!(self, Self::Attach { .. })
    }

    /// What to leave on the cleared screen: the handed-off name, if any.
    fn leaves(&self) -> Vec<u8> {
        match self {
            Self::Attach {
                hand_off: Some(h), ..
            } => h.show(),
            _ => Vec::new(),
        }
    }
}

/// What the picker opens on: the rows for its first frame, and whether a
/// listing is still to come.
///
/// Over SSH a listing is a round trip; a first frame waiting for it leaves the
/// screen blank for a third of a second or more after `<prefix> Space`.
/// Returning from a session, the session loop already has the rows (see
/// `main`), stale only for changes made outside this nvmux.
#[derive(Debug, Default)]
pub enum Listing {
    /// No rows in hand: list, and wait for it before the first frame.
    #[default]
    Ask,
    /// A listing just taken (the one clients were started ahead from, see
    /// `main`), which relisting would only repeat.
    Fresh(Vec<Session>),
    /// The last known rows, drawn at once, and a background listing that
    /// replaces them once it answers and the picker is at rest (see
    /// [`take_listing`]).
    InHand(Vec<Session>, Behind),
}

/// The answer of a background listing; see [`Listing::InHand`].
pub type Behind = std::sync::mpsc::Receiver<Result<Vec<Session>>>;

impl Listing {
    /// `rows` at once, plus a listing on a new thread. If no thread can be
    /// spawned the rows stay: the hung-up channel reads as no news.
    pub fn in_hand(rows: Vec<Session>, transport: &crate::transport::Shared) -> Self {
        let (tell, answer) = std::sync::mpsc::channel();
        let transport = std::sync::Arc::clone(transport);
        let spawned = std::thread::Builder::new()
            .name("nvmux-listing".into())
            .spawn(move || {
                // If the picker is gone, nothing is lost: the next one lists.
                let _ = tell.send(transport.list_sessions());
            });
        if let Err(e) = spawned {
            tracing::debug!(error = %e, "no thread to list the sessions behind the picker");
        }
        Self::InHand(rows, answer)
    }
}

use std::collections::{HashSet, VecDeque};
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};

use crate::error::Result;
use crate::handoff::HandOff;
use crate::session::Session;
use crate::transport::Transport;
use app::{App, Key, Mode, Mouse, Request};

/// A bounded poll rather than an indefinite read, so a resize is not stuck
/// behind an idle keyboard.
const TICK: Duration = Duration::from_millis(250);

/// The tick while a screen awaits a non-blocking answer (only the create
/// prompt's directory completion so far). A local listing takes ~1 ms, so on
/// [`TICK`] the ghost text would visibly lag; this is about one frame, used
/// only while an answer is in flight.
const BUSY_TICK: Duration = Duration::from_millis(16);

/// A ratatui screen that is guaranteed to be given back.
///
/// [`init`] enables raw mode and the alternate screen, and nothing undoes that
/// on `Drop`, so any `?` before restore could leave the shell with no echo.
/// This guard restores on drop. The picker, prompt, help and first-run screen
/// all open through it.
pub(crate) struct Screen {
    terminal: ratatui::DefaultTerminal,
    /// Set by the explicit close so `Drop` does not restore a second time.
    closed: bool,
}

impl Screen {
    /// Take the terminal and the mouse.
    ///
    /// Each screen enables mouse reporting and disables it on close. The relay
    /// restores a resumed client's setting: a kept client's from its ledger
    /// ([`crate::ledger`]), a held one's by asking its server (`pty::repaint`);
    /// a new client enables its own. Unlike colours, the mode cannot be left
    /// alone: a client enables the mouse once, at startup.
    pub(crate) fn open() -> Result<Self> {
        Self::open_with(true)
    }

    /// [`Screen::open`], optionally without the mouse. Only [`attaching`] goes
    /// without: it reads nothing at first so typed-ahead keys stay queued for
    /// the client, and mouse reports would fill that queue with motion the
    /// editor would then receive as input.
    pub(crate) fn open_with(mouse: bool) -> Result<Self> {
        // Stops crossterm second-guessing us; see the module docs on colour.
        ratatui::crossterm::style::force_color_output(true);

        // Before the alternate screen and the clear: both erase with the
        // current background, which a client stopped mid-frame may have set.
        crate::term::reset_inherited_attributes();

        let mut screen = Self {
            terminal: init()?,
            closed: false,
        };
        // A second "enter alternate screen" is a no-op on xterm and kitty, and
        // ratatui's first draw only paints what differs from an empty buffer,
        // so without this the content lands on the editor's last frame. From
        // here on an error restores through `Drop`.
        screen.terminal.clear()?;
        if mouse {
            crate::term::enable_mouse();
        }
        Ok(screen)
    }

    /// Undo [`crate::term::enable_mouse`]. Called before the mode and screen
    /// switch so nothing lands after the attach path's one-write handover.
    fn release_mouse(&mut self) {
        crate::term::disable_mouse();
    }

    pub(crate) fn terminal(&mut self) -> &mut ratatui::DefaultTerminal {
        &mut self.terminal
    }

    pub(crate) fn close(mut self) -> Result<()> {
        self.closed = true;
        self.release_mouse();
        ratatui::try_restore()?;
        Ok(())
    }

    /// Give the terminal back cleared, for a screen ending in a client spawn.
    ///
    /// `ratatui::try_restore()` would uncover the previous session's frame for
    /// the whole spawn (two RPC round trips and a new `nvim`). It is just
    /// `disable_raw_mode` plus `\e[?1049l`, which
    /// [`crate::term::leave_alt_screen_and_clear`] writes first, so the leave
    /// and the erase go out in one `write`.
    ///
    /// `keep` (the attached session's name, see [`crate::handoff`]) is drawn on
    /// the cleared screen in the same synchronized update; empty leaves it
    /// blank.
    pub(crate) fn close_for_attach(mut self, keep: &[u8]) -> Result<()> {
        self.closed = true;
        self.release_mouse();
        // Modes first, as ratatui orders it. The screen is restored even if
        // this fails, so the user is not stranded on the alternate screen.
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
            // Error path: `restore` reports failure on stderr and carries on.
            self.release_mouse();
            ratatui::restore();
        }
    }
}

/// `ratatui::try_init` without its panic hook, which stacks a new hook per
/// call and also fires for panics nvmux catches. nvmux's own hook
/// ([`crate::panics`]) is installed here for callers that skip `main`.
fn init() -> std::io::Result<ratatui::DefaultTerminal> {
    crate::panics::install();
    ratatui::crossterm::terminal::enable_raw_mode()?;
    ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::terminal::EnterAlternateScreen
    )?;
    ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))
}

/// Run `f` on a [`Screen`], restoring before the outcome propagates: raw mode
/// with no echo is worse than any error.
pub(crate) fn owning<T>(f: impl FnOnce(&mut ratatui::DefaultTerminal) -> Result<T>) -> Result<T> {
    let mut screen = Screen::open()?;
    let outcome = f(screen.terminal());
    screen.close()?;
    outcome
}

/// [`owning`] for a screen that can end in an attach: outcomes matching
/// `attaches` close through [`Screen::close_for_attach`]. For `mouse` see
/// [`Screen::open_with`].
pub(crate) fn owning_for_attach<T>(
    attaches: impl FnOnce(&T) -> bool,
    mouse: bool,
    f: impl FnOnce(&mut ratatui::DefaultTerminal) -> Result<T>,
) -> Result<T> {
    owning_for_attach_leaving(attaches, |_| Vec::new(), mouse, f)
}

/// [`owning_for_attach`], with `leaves` giving what to keep on the cleared
/// screen.
pub(crate) fn owning_for_attach_leaving<T>(
    attaches: impl FnOnce(&T) -> bool,
    leaves: impl FnOnce(&T) -> Vec<u8>,
    mouse: bool,
    f: impl FnOnce(&mut ratatui::DefaultTerminal) -> Result<T>,
) -> Result<T> {
    let mut screen = Screen::open_with(mouse)?;
    let outcome = f(screen.terminal());
    // Errors take the ordinary close: no session is coming, and the shell that
    // prints the error wants its screen back.
    match &outcome {
        Ok(v) if attaches(v) => screen.close_for_attach(&leaves(v))?,
        _ => screen.close()?,
    }
    outcome
}

/// Wait up to [`TICK`] for a keypress; `Ok(None)` on timeout or a non-press
/// event. Presses only: with the kitty protocol pushed by Neovim, a release
/// would count as a second press.
///
/// [`setup`] does not use this: it needs raw chords, and [`translate`] drops
/// every control chord but the three the picker binds.
pub(crate) fn poll_key() -> Result<Option<Key>> {
    poll_key_for(TICK)
}

/// [`poll_key`] waiting only `tick` (see [`BUSY_TICK`]).
pub(crate) fn poll_key_for(tick: Duration) -> Result<Option<Key>> {
    if !event::poll(tick)? {
        return Ok(None);
    }
    Ok(match event::read()? {
        Event::Key(k) if k.kind == KeyEventKind::Press => Some(translate(k)),
        _ => None,
    })
}

/// Run the picker until the user attaches or quits.
///
/// - `listing`: what it opens on (see [`Listing`]).
/// - `message`: replaces the hint row, e.g. to report a failed attach.
/// - `focused`: the session the caller came from; the cursor starts on it.
/// - `still_attached`: that session still has a client, so `Esc` returns to
///   it. False on the first screen and after a failed attach or exited child.
/// - `seen`: the session the user was viewing; its notes are cleared as
///   already seen (see [`crate::notes`]).
/// - `notifier`: given each session's socket, so the others can be heard from
///   once one is in front (see [`crate::tell`]).
pub fn run(
    transport: &dyn Transport,
    listing: Listing,
    message: Option<String>,
    focused: Option<&str>,
    still_attached: bool,
    seen: Option<&str>,
    notifier: &crate::tell::Notifier,
) -> Result<Outcome> {
    owning_for_attach_leaving(Outcome::attaches, Outcome::leaves, true, |terminal| {
        let (app, behind) = open(transport, listing, message, focused, still_attached)?;
        run_loop(terminal, transport, app, behind, seen, notifier)
    })
}

/// The picker's initial state, plus any background listing: taken up at rest
/// once it answers ([`take_listing`]), dropped by any listing the picker makes
/// itself ([`refresh`]).
fn open(
    transport: &dyn Transport,
    listing: Listing,
    message: Option<String>,
    focused: Option<&str>,
    still_attached: bool,
) -> Result<(App, Option<Behind>)> {
    let (rows, behind) = match listing {
        Listing::Ask => (transport.list_sessions()?, None),
        Listing::Fresh(rows) => (rows, None),
        Listing::InHand(rows, answer) => (rows, Some(answer)),
    };
    let mut app = App::new(rows);
    if let Some(id) = focused {
        app.select_session(id);
        if still_attached {
            app.set_came_from(id);
        }
    }
    if let Some(msg) = message {
        app.set_message(msg);
    }
    Ok((app, behind))
}

/// Time per loop pass for handing sessions to the notes reader. Locally all
/// fit; over SSH a session's first forward (`ssh -O forward`) takes most of
/// it, so they go a few per pass, between frames.
const HAND_OVER: Duration = Duration::from_millis(8);

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    transport: &dyn Transport,
    mut app: App,
    mut behind: Option<Behind>,
    seen: Option<&str>,
    notifier: &crate::tell::Notifier,
) -> Result<Outcome> {
    app.set_unsaved(unsaved());
    // Notes are read on their own thread (`crate::notes::Poller`); each
    // session is handed over once its socket is known.
    let poller = crate::notes::Poller::start(seen.map(str::to_string));
    let mut handed: HashSet<String> = HashSet::new();
    let mut unread: VecDeque<Session> = VecDeque::new();
    // Until the reader first answers (at most one `notes::POLL`), wait a frame
    // rather than a tick, so notes appear with the picker.
    let mut heard = false;
    let opened = Instant::now();

    // Fade in before taking input. The fade's last frame is the plain screen,
    // so the first `draw` repaints nothing.
    crate::fade::fade_in(terminal, |f| draw::draw(f, &app))?;

    // When a half-typed number must resolve. Only set when one number prefixes
    // another (sessions 1 and 12); `App` decides the rest itself.
    let mut deadline: Option<Instant> = None;
    // When animations were last advanced; the clock lives here, like the
    // deadline.
    let mut ticked = Instant::now();
    // Queried once: neither the palette nor `NO_COLOR` changes during the run.
    let colours = effects::colours();
    let colours = colours.as_ref();
    // Last draw time and area; `None` until the first frame, always drawn.
    let mut drawn: Option<Instant> = None;
    let mut area = ratatui::layout::Rect::default();
    // Whether animated frames wait for the terminal to take them (see
    // `caught_up`); dropped for the run the first time it does not answer.
    let mut paced = true;

    loop {
        tick(&mut app, &mut ticked);

        // Before the hand-over, so sessions it adds are handed over this pass.
        take_listing(&mut app, &mut behind);

        // Make new sessions reachable together, hand them over a few ms at a
        // time, and take up their notes before drawing.
        let fresh: Vec<Session> = app
            .sessions()
            .iter()
            .filter(|s| handed.insert(s.id.clone()))
            .cloned()
            .collect();
        transport.reach_all(&fresh);
        unread.extend(fresh);
        hand_over(transport, &poller, notifier, &mut unread);
        if let Some(notes) = poller.take() {
            app.set_notes(notes);
            heard = true;
        }
        let awaiting = !heard && opened.elapsed() < crate::notes::POLL;

        // Take waiting input before redrawing, so one frame answers all of it.
        // A frame per key would tie the picker to the terminal's pace, and a
        // held key repeats faster than some terminals draw, so a session in
        // flight would keep moving after release. Still draw every `FRAME`,
        // so continuous input (a pointer sweep) sees the screen follow.
        let waiting = event::poll(Duration::ZERO)?;
        if !waiting || drawn.is_none_or(|at| at.elapsed() >= crate::fade::FRAME) {
            // Clicks resolve against the area as last drawn.
            area = terminal.draw(|f| frame(f, &app, colours))?.area;
            if paced && app.animating() {
                paced = caught_up();
            }
            drawn = Some(Instant::now());
        }

        // A frame's wait while animating, handing over sessions, awaiting
        // notes or a background listing; a spinner frame while one spins;
        // otherwise only a key changes anything, so the ordinary tick.
        let tick_for = if app.animating() || !unread.is_empty() || awaiting || behind.is_some() {
            crate::fade::FRAME
        } else if app.spinning() {
            note::SPIN
        } else {
            TICK
        };
        if !waiting && !event::poll(tick_for)? {
            // The clock lives here so `App` stays pure, as `pty::pump` does
            // for `keys::Prefix`.
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
        // Advance to the event first, so animations it starts begin fresh.
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
        // Before a request blocks (e.g. a renumber over SSH), finish the
        // landing and draw, so the screen does not show the key's question
        // through the I/O. Skipped with neither: the loop draws next, and a
        // second frame per key would let a held key fall behind.
        if app.landing().is_some() || request != Request::None {
            play_out(terminal, &mut app, colours, |app| app.landing().is_some())?;
        }
        deadline = app
            .pending()
            .is_some()
            .then(|| Instant::now() + Duration::from_millis(crate::config::get().keys.timeout_ms));

        // Resolve requests against the list in hand, not a fresh listing
        // (~230 ms over SSH): each action detects a vanished session itself
        // (attach pings, kill reports "absent", rename "not found"). Relist
        // only after a change.
        match request {
            Request::None => {}
            Request::Quit => return Ok(Outcome::Quit),

            Request::Attach(id) => {
                if let Some(session) = app.session(&id).cloned() {
                    return leave_to_session(terminal, &app, session);
                }
            }

            Request::NewSession => {
                // Open a gap and bring the prompt up out of it (`make_room`),
                // or just replace the picker if that is off. On create, the
                // prompt fades itself out.
                let arrival = make_room(terminal, &mut app, colours)?;
                let outcome = prompt::run_on(
                    terminal,
                    transport,
                    prompt::Task::Create {
                        listed: Some(app.sessions()),
                        number: Some(app.new_number()),
                    },
                    arrival,
                );
                // Close the gap either way: a created session shows up in the
                // listing that follows.
                app.close_room();
                match outcome? {
                    prompt::Outcome::Created(session) => {
                        let (session, sessions) = settle(&app, transport, session);
                        return Ok(Outcome::Attach {
                            session,
                            sessions,
                            hand_off: None,
                            float_settings: false,
                        });
                    }
                    prompt::Outcome::Cancelled => {
                        // The prompt faded out if it faded in; fade the
                        // picker back in to match.
                        if arrival != prompt::Arrival::Cut {
                            crate::fade::fade_in(terminal, |f| draw::draw(f, &app))?;
                        }
                    }
                    prompt::Outcome::Renamed(_) => refresh(&mut app, transport, &mut behind)?,
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
                        refresh(&mut app, transport, &mut behind)?;
                    }
                }
            }

            Request::Kill(id) => {
                if let Some(session) = app.session(&id).cloned() {
                    erase(terminal, &mut app, &id, colours)?;
                    if let Err(e) = transport.kill_session(&session) {
                        app.set_message(e.one_line());
                    }
                    refresh(&mut app, transport, &mut behind)?;
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
                // Relist either way: only a listing knows the numbers now.
                // `set_sessions` keeps the cursor on the placed session by id.
                refresh(&mut app, transport, &mut behind)?;
            }

            Request::Help => help::run_on(terminal, false)?,

            // Afterwards, reload the picker's copies of the effects settings;
            // the rest is read live.
            Request::Settings => {
                let remote = matches!(transport.location(), crate::transport::Location::Ssh(_));
                let behind = app.came_from().cloned();
                match (
                    settings::run_on(terminal, false, remote, behind.is_some())?,
                    behind,
                ) {
                    // Switched to floating, with a session behind: return to
                    // it with the settings floating over it. They have already
                    // faded out.
                    (settings::Outcome::Float, Some(session)) => {
                        return Ok(Outcome::Attach {
                            session,
                            sessions: app.sessions().to_vec(),
                            hand_off: None,
                            float_settings: true,
                        });
                    }
                    _ => {
                        app.reload_effects();
                        app.set_unsaved(unsaved());
                    }
                }
            }

            Request::SaveAndQuit { overwrite } => {
                let live = crate::config::get();
                match crate::config::save(&live, &crate::config::saved(), overwrite) {
                    Ok(_) => {
                        crate::config::set_saved((*live).clone());
                        return Ok(Outcome::Quit);
                    }
                    Err(e) => app.not_saved(e.to_string(), e.changed_in_file().is_some()),
                }
            }
        }
    }
}

/// Wait until the terminal has processed everything written to it.
///
/// A session in flight sends a frame every [`crate::fade::FRAME`]. A terminal
/// that barely keeps up falls behind when a held key adds frames, and the
/// backlog persists, so the session keeps moving after release and later keys
/// wait behind stale frames. A cursor position query is answered only once the
/// terminal gets that far, so waiting for it keeps one frame in flight. Keys
/// arriving meanwhile are buffered by crossterm for the loop. ([`Screen::open`]
/// already relies on this query via ratatui's `clear`.)
///
/// False if there is no answer within crossterm's 2 s; the caller then stops
/// asking, since that wait every frame is worse than unpaced frames.
fn caught_up() -> bool {
    match ratatui::crossterm::cursor::position() {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(error = %e, "the terminal did not say where its cursor is; frames go unpaced");
            false
        }
    }
}

/// Number of unsaved setting changes, for the quit prompt.
fn unsaved() -> usize {
    crate::settings::changed(&crate::config::get(), &crate::config::saved()).len()
}

fn tick(app: &mut App, ticked: &mut Instant) {
    let now = Instant::now();
    app.tick(now - *ticked);
    *ticked = now;
}

/// Hand sockets from `unread` to the notes reader and the notifier: at least
/// one, then until [`HAND_OVER`] runs out. Over SSH,
/// [`Transport::local_socket_for`] may have to create a forward for each.
fn hand_over(
    transport: &dyn Transport,
    poller: &crate::notes::Poller,
    notifier: &crate::tell::Notifier,
    unread: &mut VecDeque<Session>,
) {
    let started = Instant::now();
    while let Some(s) = unread.pop_front() {
        match transport.local_socket_for(&s) {
            Ok(sock) => {
                notifier.watch(&s.id, &s.name, sock.clone());
                poller.watch(&s.id, sock);
            }
            Err(e) => tracing::debug!(id = %s.id, error = %e, "notes: no socket to read"),
        }
        if started.elapsed() >= HAND_OVER {
            break;
        }
    }
}

fn frame(f: &mut ratatui::Frame, app: &App, colours: Option<&effects::Colours>) {
    draw::draw(f, app);
    crate::theme::paint(f.buffer_mut());
    effects::paint(f, app, colours);
}

/// Play the erase of a confirmed kill's row before the kill runs (see
/// [`backspace`] for why not during it). The row stays empty until the next
/// listing replaces it, or restores it if the kill failed.
fn erase(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    id: &str,
    colours: Option<&effects::Colours>,
) -> Result<()> {
    if !app.start_backspace(id) {
        return Ok(());
    }
    play_out(terminal, app, colours, App::erasing)
}

/// Open a gap for the session `c` will create and close the picker onto it;
/// returns how the prompt should arrive.
///
/// Rows below the cursor step down and the new session rises dim in the gap
/// over [`effects::ROOM`] ([`App::make_room`]), with its future number and the
/// prompt's default name ([`prompt::default_name`]). The picker then fades out
/// onto that row and the prompt opens from its name field
/// ([`prompt::Arrival::FromName`]).
///
/// Without a fade (off, `NO_COLOR`, or no palette) the gap is held for
/// [`effects::ROOM_HELD`] and the prompt cuts in; with `[effects.create]` off
/// there is no gap. Keys pressed meanwhile stay queued for the prompt.
fn make_room(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    colours: Option<&effects::Colours>,
) -> Result<prompt::Arrival> {
    let name = prompt::default_name(app.sessions(), Some(app.new_number()));
    if !app.make_room(&name) {
        return Ok(prompt::Arrival::Cut);
    }
    let opened = Instant::now();
    play_out(terminal, app, colours, |app| app.room().is_some())?;
    let size = terminal.size()?;
    let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
    match draw::row_rect(app, area, app::GHOST_ID) {
        Some(row) if crate::fade::enabled() => {
            crate::fade::fade_out_onto(terminal, |f| draw::draw(f, app), row.y)?;
            Ok(prompt::Arrival::FromName)
        }
        // Off screen (no room for a list) or no fade: hold it long enough to
        // read, then cut to the prompt.
        _ => {
            std::thread::sleep(effects::ROOM_HELD.saturating_sub(opened.elapsed()));
            Ok(prompt::Arrival::Cut)
        }
    }
}

/// Draw frames until `playing` returns false, then one final frame: for
/// effects that must finish first (backspace, landing, the new-session gap).
/// Keys stay queued meanwhile. With nothing playing, it just draws the current
/// screen before the caller blocks.
fn play_out(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    colours: Option<&effects::Colours>,
    playing: impl Fn(&App) -> bool,
) -> Result<()> {
    let mut ticked = Instant::now();
    while playing(app) {
        terminal.draw(|f| frame(f, app, colours))?;
        std::thread::sleep(crate::fade::FRAME);
        tick(app, &mut ticked);
    }
    terminal.draw(|f| frame(f, app, colours))?;
    Ok(())
}

/// Fade the picker out and return the attaching outcome. Shared by the
/// picker's two exits so neither forgets the fade; the prompt's `Created` exit
/// fades its own screen, and quitting does not fade.
///
/// With the hand-off on ([`crate::handoff`]), the session's name is kept out
/// of the fade, left as plain text for the session to remove.
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
        float_settings: false,
    })
}

/// The session's name as drawn (position and truncation), for the hand-off;
/// `None` if its row is off screen (a number typed for a scrolled-out row).
fn hand_off_for(app: &App, area: ratatui::layout::Rect, session: &Session) -> Option<HandOff> {
    let name = draw::name_rect(app, area, &session.id)?;
    HandOff::new(&session.name, name.y, name.x, name.width)
}

/// Move a session just created from the picker to after the highlighted row;
/// returns the session and the listing to attach with.
///
/// New sessions are numbered last, so this is a renumber
/// ([`App::placement_of`]) then a listing (only a listing knows the numbers):
/// two round trips over SSH, skipped when the highlight is already last. In
/// that case the rows plus the new one serve as the listing; order does not
/// matter to its readers.
///
/// Neither step may lose the session: a failed renumber leaves it last, a
/// failed listing falls back to the rows in hand. Both are only logged, as the
/// session is what the screen shows next.
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

/// Relist (keeping the selection by id) and drop any background listing: it
/// predates the change just made and would undo it.
fn refresh(app: &mut App, transport: &dyn Transport, behind: &mut Option<Behind>) -> Result<()> {
    *behind = None;
    app.set_sessions(transport.list_sessions()?);
    Ok(())
}

/// Take up the background listing ([`Listing::InHand`]) if it has answered and
/// the picker is at rest (browsing or filtering, nothing animating). A move, a
/// `[y/N]` or a fading row leaves it queued, so rows never change under a
/// question or mid-effect. [`App::set_sessions`] keeps the cursor on its
/// session. A failed listing leaves the rows as they are.
fn take_listing(app: &mut App, behind: &mut Option<Behind>) {
    let Some(answer) = behind.as_ref() else {
        return;
    };
    if !matches!(app.mode(), Mode::Normal | Mode::Filter) || app.animating() {
        return;
    }
    match answer.try_recv() {
        Err(TryRecvError::Empty) => return,
        Ok(Ok(fresh)) => app.set_sessions(fresh),
        Ok(Err(e)) => tracing::debug!(error = %e, "could not list the sessions behind the picker"),
        Err(TryRecvError::Disconnected) => {}
    }
    *behind = None;
}

/// The largest resolved session number in a listing, or 0 for none.
pub fn highest_num(sessions: &[Session]) -> u32 {
    sessions.iter().map(|s| s.state.num).max().unwrap_or(0)
}

/// Reduce a crossterm event to the keys the screens understand.
///
/// Control chords other than `Ctrl-c`, `Ctrl-n` and `Ctrl-p` become
/// `Key::Other`, never the bare letter: the prefix is configurable, and a
/// reflexive `<prefix>` must not open the kill confirm (`x`) or quit (`q`),
/// nor close the help and forward its second key.
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
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        _ => Key::Other,
    }
}

/// Reduce a mouse event to the picker's gestures (motion, left button,
/// vertical wheel); `row` is the row under the pointer per [`draw::row_at`].
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

    /// The prefix (default Ctrl-Space) is a control chord, and `x`, `q`, `r`,
    /// `y` are picker commands. Ctrl-Space must not reach the filter as a
    /// space.
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
        // Otherwise `G` (last) would be indistinguishable from `g` (first).
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            Key::Char('G')
        );
    }

    /// Why [`Screen::open`] resets inherited attributes: ratatui's crossterm
    /// backend writes SGR only on a change from `Color::Reset`, so a colourless
    /// screen is drawn in the terminal's current attributes. If ratatui starts
    /// leading frames with a reset, this fails and that call can go.
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

    /// Dim text (numbers, hint row) keeps the terminal's foreground; no
    /// background is set; glyphs and modifiers are unchanged.
    #[test]
    fn a_themed_picker_is_the_plain_one_with_its_ink_where_it_is_not_dim() {
        use ratatui::style::{Color, Modifier};

        let ink = crate::palette::Rgb(0x7a, 0xa2, 0xf7);
        let a = test_support::picker(&["api-server", "dotfiles", "notes"]);
        let plain = test_support::buffer(40, 8, |f| draw::draw(f, &a));
        let themed = test_support::buffer(40, 8, |f| {
            draw::draw(f, &a);
            crate::theme::paint_in(f.buffer_mut(), ink);
        });
        let mut dim = 0;
        for (cell, was) in themed.content.iter().zip(&plain.content) {
            let want = if was.modifier.contains(Modifier::DIM) {
                dim += 1;
                Color::Reset
            } else {
                Color::Rgb(ink.0, ink.1, ink.2)
            };
            assert_eq!(cell.fg, want, "{cell:?}");
            assert_eq!(cell.bg, Color::Reset, "{cell:?}");
            assert_eq!((cell.symbol(), cell.modifier), (was.symbol(), was.modifier));
        }
        assert!(dim > 0, "the fixture draws something dim");
    }

    /// Quitting must not clear: the shell wants its own screen back.
    #[test]
    fn only_attaching_hands_the_terminal_to_a_session() {
        let session = Session::new("id000000".into(), "a".into(), 100, 1);
        assert!(Outcome::Attach {
            session: session.clone(),
            sessions: vec![session],
            hand_off: None,
            float_settings: false,
        }
        .attaches());
        assert!(!Outcome::Quit.attaches());
    }

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

    #[test]
    fn only_an_attach_with_a_name_leaves_one() {
        let session = Session::new("id000000".into(), "a".into(), 100, 1);
        let with = Outcome::Attach {
            session: session.clone(),
            sessions: vec![session.clone()],
            hand_off: HandOff::new("a", 3, 4, 10),
            float_settings: false,
        };
        assert_eq!(with.leaves(), HandOff::new("a", 3, 4, 10).unwrap().show());
        let without = Outcome::Attach {
            session: session.clone(),
            sessions: vec![session],
            hand_off: None,
            float_settings: false,
        };
        assert!(without.leaves().is_empty());
        assert!(Outcome::Quit.leaves().is_empty());
    }

    /// Answers every listing with `rows` after `wait`, counting listings.
    struct Lister {
        location: crate::transport::Location,
        rows: Vec<Session>,
        wait: Duration,
        asked: std::sync::atomic::AtomicUsize,
    }

    impl Lister {
        fn new(rows: Vec<Session>, wait: Duration) -> Self {
            Self {
                location: crate::transport::Location::Local,
                rows,
                wait,
                asked: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn asked(&self) -> usize {
            self.asked.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Transport for Lister {
        fn location(&self) -> &crate::transport::Location {
            &self.location
        }
        fn list_sessions(&self) -> Result<Vec<Session>> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(self.wait);
            Ok(self.rows.clone())
        }
        fn create_session(&self, _: &str, _: &crate::launch::Launch, _: &str) -> Result<Session> {
            unimplemented!()
        }
        fn kill_session(&self, _: &Session) -> Result<()> {
            unimplemented!()
        }
        fn rename_session(&self, _: &Session, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn renumber(&self, _: &[Session]) -> Result<()> {
            unimplemented!()
        }
        fn home(&self) -> &str {
            unimplemented!()
        }
        fn dir_source(&self) -> crate::dirs::DirSource {
            unimplemented!()
        }
        fn local_socket_for(&self, _: &Session) -> Result<std::path::PathBuf> {
            unimplemented!()
        }
    }

    /// The rows `test_support::picker` opens on, ids included.
    fn rows(names: &[&str]) -> Vec<Session> {
        test_support::picker(names).sessions().to_vec()
    }

    fn names(app: &App) -> Vec<String> {
        app.sessions().iter().map(|s| s.name.clone()).collect()
    }

    fn cursor(app: &App) -> String {
        app.visible()[app.selected_index()].id.clone()
    }

    /// Poll [`take_listing`] as the loop would, for up to `within`.
    fn take_up(app: &mut App, behind: &mut Option<Behind>, within: Duration) {
        let deadline = Instant::now() + within;
        while behind.is_some() && Instant::now() < deadline {
            take_listing(app, behind);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The cursor stays on the session it came from throughout.
    #[test]
    fn the_picker_opens_on_the_rows_in_hand_and_takes_the_listing_when_it_comes() {
        let wait = Duration::from_millis(300);
        let host = std::sync::Arc::new(Lister::new(rows(&["one", "two", "three"]), wait));
        let shared: crate::transport::Shared = host.clone();
        let started = Instant::now();
        let (mut app, mut behind) = open(
            host.as_ref(),
            Listing::in_hand(rows(&["one", "two"]), &shared),
            None,
            Some("id000001"),
            true,
        )
        .expect("open");
        assert!(
            started.elapsed() < wait,
            "opened {:?} after it was asked, the host's time",
            started.elapsed()
        );
        assert_eq!(names(&app), ["one", "two"]);
        assert_eq!(cursor(&app), "id000001");

        take_up(&mut app, &mut behind, Duration::from_secs(10));
        assert!(
            behind.is_none(),
            "the listing behind the rows never answered"
        );
        assert_eq!(names(&app), ["one", "two", "three"]);
        assert_eq!(cursor(&app), "id000001", "the cursor left its session");
        assert_eq!(host.asked(), 1);
    }

    /// The picker opens on the rows in hand minus the exited session, cursor on
    /// the first row. If the listing still has it (client ended by `:detach`),
    /// the cursor returns to it; if not, it stays. A key pressed meanwhile
    /// wins.
    #[test]
    fn the_cursor_goes_back_to_a_session_whose_client_exited_if_it_is_still_there() {
        let wait = Duration::from_millis(300);
        let all = rows(&["one", "two", "three"]);
        let without_two =
            || -> Vec<Session> { all.iter().filter(|s| s.id != "id000001").cloned().collect() };
        let opened = |listed: Vec<Session>| {
            let host = std::sync::Arc::new(Lister::new(listed, wait));
            let shared: crate::transport::Shared = host.clone();
            let started = Instant::now();
            let picker = open(
                host.as_ref(),
                Listing::in_hand(without_two(), &shared),
                None,
                Some("id000001"),
                false,
            )
            .expect("open");
            assert!(
                started.elapsed() < wait,
                "opened {:?} after it was asked, the host's time",
                started.elapsed()
            );
            picker
        };

        let (mut app, mut behind) = opened(all.clone());
        assert_eq!(names(&app), ["one", "three"]);
        assert_eq!(cursor(&app), "id000000");
        take_up(&mut app, &mut behind, Duration::from_secs(10));
        assert_eq!(names(&app), ["one", "two", "three"]);
        assert_eq!(cursor(&app), "id000001", "the listing brought it back");

        let (mut app, mut behind) = opened(without_two());
        take_up(&mut app, &mut behind, Duration::from_secs(10));
        assert_eq!(names(&app), ["one", "three"]);
        assert_eq!(cursor(&app), "id000000", "gone, and the cursor stays");

        let (mut app, mut behind) = opened(all.clone());
        app.on_key(Key::Down);
        app.on_key(Key::Up);
        // Rows are taken up only at rest, after the glow ends.
        app.tick(Duration::from_secs(5));
        take_up(&mut app, &mut behind, Duration::from_secs(10));
        assert_eq!(names(&app), ["one", "two", "three"]);
        assert_eq!(
            cursor(&app),
            "id000000",
            "the cursor was taken over, and taken from its owner"
        );
    }

    /// With no rows in hand, the picker lists and opens on the answer.
    #[test]
    fn a_fresh_listing_is_opened_on_without_asking_the_host() {
        let host = Lister::new(rows(&["one", "two", "three"]), Duration::ZERO);
        let (app, behind) = open(
            &host,
            Listing::Fresh(rows(&["one", "two"])),
            None,
            None,
            false,
        )
        .expect("open");
        assert_eq!(names(&app), ["one", "two"]);
        assert!(behind.is_none());
        assert_eq!(host.asked(), 0, "a fresh listing was asked for again");

        let (app, behind) = open(&host, Listing::Ask, None, None, false).expect("open");
        assert_eq!(names(&app), ["one", "two", "three"]);
        assert!(behind.is_none());
        assert_eq!(host.asked(), 1);
    }

    /// A kill's `[y/N]` or a picked-up session holds the answer until it is
    /// over and the picker is still.
    #[test]
    fn a_question_about_a_row_keeps_the_listing_waiting() {
        for (what, ask, done) in [
            ("a kill's [y/N]", Key::Char('x'), Key::Char('n')),
            ("a session picked up", Key::Char(' '), Key::Esc),
        ] {
            let mut app = test_support::picker(&["one", "two"]);
            let (tell, answer) = std::sync::mpsc::channel();
            let mut behind = Some(answer);
            app.on_key(ask);
            // "one" gone, killed somewhere else.
            tell.send(Ok(app.sessions()[1..].to_vec())).expect("send");
            take_listing(&mut app, &mut behind);
            assert!(behind.is_some(), "{what}: taken up under the question");
            assert_eq!(names(&app), ["one", "two"], "{what}");

            app.on_key(done);
            app.tick(Duration::from_secs(5));
            assert!(!app.animating(), "{what}: still moving");
            take_listing(&mut app, &mut behind);
            assert!(behind.is_none(), "{what}: left waiting once it was over");
            assert_eq!(names(&app), ["two"], "{what}");
        }
    }

    /// A failed listing or a hung-up thread; the listing is no longer awaited.
    #[test]
    fn no_answer_leaves_the_rows_as_they_were() {
        for what in ["a failed listing", "a thread that went"] {
            let mut app = test_support::picker(&["one", "two"]);
            app.on_key(Key::Down);
            app.tick(Duration::from_secs(5));
            let (tell, answer) = std::sync::mpsc::channel();
            if what == "a failed listing" {
                let gone = crate::error::NvmuxError::Io(std::io::Error::other("the link went"));
                tell.send(Err(gone)).expect("send");
            } else {
                drop(tell);
            }
            let mut behind = Some(answer);
            take_listing(&mut app, &mut behind);
            assert!(behind.is_none(), "{what}: still waited for");
            assert_eq!(names(&app), ["one", "two"], "{what}");
            assert_eq!(cursor(&app), "id000001", "{what}");
        }
    }

    /// After a kill, the stale background listing would bring the session back.
    #[test]
    fn a_listing_of_the_pickers_own_lets_go_of_the_one_behind() {
        let mut app = test_support::picker(&["one", "two"]);
        let host = Lister::new(app.sessions()[1..].to_vec(), Duration::ZERO);
        let (tell, answer) = std::sync::mpsc::channel();
        tell.send(Ok(app.sessions().to_vec())).expect("send");
        let mut behind = Some(answer);
        refresh(&mut app, &host, &mut behind).expect("refresh");
        assert!(
            behind.is_none(),
            "the listing behind outlived the picker's own"
        );
        take_listing(&mut app, &mut behind);
        assert_eq!(names(&app), ["two"]);
    }
}
