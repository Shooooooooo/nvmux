//! The picker: a centred list of session names and one dim hint line on the
//! last row. No borders, no popups — the hint line *is* the picker's help, and
//! the filter and kill confirm replace it in place.
//!
//! [`prompt`] and [`help`] are the other two screens. Both take the whole
//! terminal rather than drawing over anything, share the same vocabulary
//! (centred content, one dim hint row, no borders, no colour), and hand the
//! same client back afterwards. `prompt::run` owns a terminal for `<prefix> c`,
//! which arrives with none; `prompt::run_on` borrows the picker's — nesting the
//! two would enter the alternate screen twice and leave it once.
//!
//! [`attaching`] is a fourth, and the odd one out: not a screen the user works
//! on but the wait between the picker (or a `<prefix>` switch) and the session,
//! up only while the attach probe is in flight. It keeps the vocabulary — one
//! dim row, no borders, no colour — and is the one screen that leaves the
//! mouse alone, for a reason given there.
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
//! Set no background and hardcode no palette. Do **not** rely on crossterm's
//! own `NO_COLOR` handling: it turns `SetForegroundColor(c)` into a bare
//! `ESC[m`, a full SGR reset that wipes bold, reverse and dim mid-line. Call
//! `force_color_output(true)`, test the variable directly, and set no colours
//! when it is present.
//!
//! Inheriting the palette has a cost the screens have to pay themselves.
//! Setting no colours means ratatui writes no SGR before its content, so a
//! screen is painted in whatever attributes the terminal was already in — and
//! coming back from a session, that is whatever the editor happened to be
//! drawing when the relay stopped. [`Screen::open`] therefore drops the
//! inherited attributes first, which is the one place that can: every screen is
//! taken through it.
//!
//! The fade ([`crate::fade`]) is the one thing that paints a colour on these
//! screens, and it does so as a post-pass over a finished frame, never inside
//! a `draw`: a screen's own drawing stays colourless, and its tests say so.

pub mod app;
pub mod attaching;
pub mod complete;
pub mod draw;
pub mod help;
pub mod prompt;
pub mod setup;
#[cfg(test)]
pub(crate) mod test_support;

/// What the picker returned.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Attach to this session.
    Attach {
        session: Session,
        /// The listing the picker was showing.
        ///
        /// Carried out rather than re-listed, for the reason the picker already
        /// resolves its own keys against the list in hand: a listing is a script
        /// run — ~230 ms over SSH, and 88-110 ms on a local host whose forks are
        /// expensive — and the session loop needs the same two answers the
        /// picker had. The highest number, so the prefix machine knows when a
        /// digit can be acted on without waiting; and the rows themselves, so a
        /// `<prefix>` switch can name one without going back to disk.
        sessions: Vec<Session>,
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
}

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};

use crate::error::Result;
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
    /// closes, whatever is behind it. Where that is a held client, the relay
    /// puts the client's own setting back as it resumes it, having asked the
    /// client's server what that was (see `pty::repaint`); a client spawned
    /// next enables its own. The screens set no colours so as to inherit the
    /// terminal's palette, but a *mode* cannot be inherited the same way — a
    /// `--remote-ui` client enables the mouse once, at startup, and would
    /// never re-enable a mode nvmux had turned off — which is why this is
    /// restored by asking rather than left alone.
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
    pub(crate) fn close_for_attach(mut self) -> Result<()> {
        self.closed = true;
        self.release_mouse();
        // Modes first, for the reason ratatui gives for the same order: dropping
        // raw mode has the wider side effects. The screen is put back either
        // way, so a failure to restore the modes cannot also strand the user on
        // the picker's alternate screen.
        let modes = ratatui::crossterm::terminal::disable_raw_mode();
        crate::term::leave_alt_screen_and_clear();
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
    let mut screen = Screen::open_with(mouse)?;
    let outcome = f(screen.terminal());
    // The restore happens before the outcome propagates, as in `owning`, and an
    // error takes the ordinary close: there is no session coming, and the shell
    // the error is about to be printed to wants its screen back, not a cleared
    // one.
    match &outcome {
        Ok(v) if attaches(v) => screen.close_for_attach()?,
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
pub fn run(
    transport: &dyn Transport,
    message: Option<String>,
    focused: Option<&str>,
    still_attached: bool,
) -> Result<Outcome> {
    owning_for_attach(Outcome::attaches, true, |terminal| {
        run_loop(terminal, transport, message, focused, still_attached)
    })
}

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    transport: &dyn Transport,
    message: Option<String>,
    focused: Option<&str>,
    still_attached: bool,
) -> Result<Outcome> {
    let mut sessions = transport.list_sessions()?;
    let mut highest = highest_num(&sessions);
    let mut app = App::new(std::mem::take(&mut sessions));
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

    loop {
        // The area kept for the mouse: a click is resolved against the screen
        // as it was last drawn, which is the one the user clicked on.
        let area = terminal.draw(|f| draw::draw(f, &app))?.area;

        if !event::poll(TICK)? {
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
        let request = match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(translate(k)),
            Event::Mouse(m) => {
                match translate_mouse(m, draw::row_at(&app, area, m.column, m.row)) {
                    Some(mouse) => app.on_mouse(mouse),
                    None => continue,
                }
            }
            _ => continue,
        };
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
                // Not animated: the picker is already up, and takes over
                // again if the prompt is cancelled. A create still dissolves
                // the prompt out on its way to the client spawn — that is
                // the prompt's own doing, since its screen is the one up.
                match prompt::run_on(terminal, transport, prompt::Task::Create, false)? {
                    prompt::Outcome::Created(session) => {
                        // Appended rather than re-listed: the picker's rows are
                        // still accurate and this is the one row they are
                        // missing, so the caller gets a listing that includes
                        // what it is about to attach to without another script
                        // run. Order does not matter to either reader — one
                        // takes a maximum, the other looks up a number.
                        let mut sessions = app.sessions().to_vec();
                        sessions.push(session.clone());
                        return Ok(Outcome::Attach { session, sessions });
                    }
                    prompt::Outcome::Cancelled => {}
                    prompt::Outcome::Renamed => refresh(&mut app, &mut highest, transport)?,
                }
            }

            Request::RenameSession(id) => {
                if let Some(session) = app.session(&id).cloned() {
                    let outcome =
                        prompt::run_on(terminal, transport, prompt::Task::Rename(&session), false)?;
                    if !matches!(outcome, prompt::Outcome::Cancelled) {
                        refresh(&mut app, &mut highest, transport)?;
                    }
                }
            }

            Request::Kill(id) => {
                if let Some(session) = app.session(&id).cloned() {
                    if let Err(e) = transport.kill_session(&session) {
                        app.set_message(one_line(&e));
                    }
                    refresh(&mut app, &mut highest, transport)?;
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
                    app.set_message(one_line(&e));
                }
                // Whether it wrote or failed, what the numbers now are is a
                // listing's answer and not ours. `set_sessions` restores the
                // selection by id, so the cursor stays on the session that was
                // just placed.
                refresh(&mut app, &mut highest, transport)?;
            }

            Request::Help => help::run_on(terminal, false)?,
        }
    }
}

/// Leave the picker for `session`: dissolve the screen out, then hand back the
/// outcome that attaches. The picker's own two exits share this so neither
/// can forget the fade; the prompt's `Created` exit is not one of them, since
/// the screen up at that moment is the prompt's, and the prompt fades it.
/// Quitting does not come through here either — the shell wants its screen
/// back, not a dissolved one.
fn leave_to_session(
    terminal: &mut ratatui::DefaultTerminal,
    app: &App,
    session: Session,
) -> Result<Outcome> {
    crate::fade::fade_out(terminal, |f| draw::draw(f, app))?;
    Ok(Outcome::Attach {
        session,
        sessions: app.sessions().to_vec(),
    })
}

/// Re-list, keeping the highest number in step with what is on screen.
fn refresh(app: &mut App, highest: &mut u32, transport: &dyn Transport) -> Result<()> {
    let sessions = transport.list_sessions()?;
    *highest = highest_num(&sessions);
    app.set_sessions(sessions);
    Ok(())
}

/// The largest resolved session number in a listing, or 0 for none.
pub fn highest_num(sessions: &[Session]) -> u32 {
    sessions.iter().map(|s| s.state.num).max().unwrap_or(0)
}

/// Collapse an error to something that fits on one line.
///
/// The bottom row is exactly one row; a multi-line error would be truncated at
/// the first newline and lose the part that explains itself.
fn one_line(e: &crate::error::NvmuxError) -> String {
    e.to_string().lines().collect::<Vec<_>>().join(" — ")
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
        KeyCode::BackTab => Key::BackTab,
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
    /// The screens set no colours on purpose, so ratatui's crossterm backend —
    /// which tracks fg and bg from `Color::Reset` and writes SGR only on a
    /// difference — emits nothing that would clear an attribute until the whole
    /// frame has been painted, and never writes a blank cell at all. Whatever
    /// the terminal was already in is what the screen is drawn in: coming back
    /// from a session, the editor's own colours, mid-frame.
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

    /// Attaching is the outcome that gives the terminal back cleared; quitting
    /// must not, because the shell it returns to wants its own screen rather
    /// than a wiped one.
    #[test]
    fn only_attaching_hands_the_terminal_to_a_session() {
        let session = Session::new("id000000".into(), "a".into(), 100, 1);
        assert!(Outcome::Attach {
            session: session.clone(),
            sessions: vec![session],
        }
        .attaches());
        assert!(!Outcome::Quit.attaches());
    }

    #[test]
    fn errors_are_flattened_to_one_line() {
        let e = crate::error::NvmuxError::Session(crate::error::SessionError::NotReady {
            name: "x".into(),
            timeout: Duration::from_secs(1),
            log: "x.log".into(),
            log_tail: "a\nmultiline\nthing".into(),
        });
        assert!(
            e.to_string().contains('\n'),
            "the fixture must be multi-line"
        );
        assert!(!one_line(&e).contains('\n'));
    }
}
