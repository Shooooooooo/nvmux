//! The wait for a session to take its client: the screen between the picker
//! (or a `<prefix>` switch) and the session itself.
//!
//! An attach probes the session before its client is let onto the terminal
//! (see [`crate::pty::Probe`]), and the probe's last call is a deferred one: a
//! session in the middle of `:!make` answers it when the make is done. That
//! wait used to be three seconds and then a refusal. Now it is as long as it
//! takes, and this screen is what makes that bearable — it names the session
//! being waited for, with a spinner beside the name so the wait is visibly the
//! session's and not a hang, and it offers `Esc` to give up on it.
//!
//! The one wait that is *not* waited out is a session parked on a keypress,
//! which the probe reports as [`Answer::Blocked`] rather than asking anything
//! deferred of it. There is nothing to spin for there: the session will not
//! move until a key reaches it, and the client this screen is holding back is
//! how the user presses one. So that answer hands the terminal over at once —
//! before `GRACE`, so this screen never appears for it at all.
//!
//! # The shape is the picker's
//!
//! One centred line where the picker draws its list, and one dim hint row on
//! the last line: [`draw::screen`], the same call every other screen makes.
//! The line is the picker's
//! empty-list line by another name — a single dim sentence standing in for a
//! list that is not there — so it is drawn the same way, through
//! [`draw::centre_vertically`].
//!
//! # The grace period
//!
//! Nothing is drawn, and nothing is read, for the first [`GRACE`]. An attach
//! to an idle session is milliseconds locally and a few hundred over ssh, and
//! a spinner that flashed on every one of those would be noise on the very
//! transition it exists to explain. Not *reading* matters more: whatever the
//! user types while an attach is in flight sits in the terminal's input queue,
//! and a fast attach hands all of it to the editor as the relay begins — a `:`
//! typed straight after `Enter` on the picker opens the command line. A screen
//! that polled the keyboard would take those bytes out of the queue.
//!
//! Once the spinner is up, that reverses: the screen is what the user is
//! typing into. It reads keys from then on, and `Esc` or `Ctrl-c` ends the
//! attach — the offer on the hint row is live from the moment it appears,
//! which is the whole reason that row is unconditional. What was typed
//! *before* the spinner appeared is drained and dropped at that moment rather
//! than acted on: an `Esc` typed a tenth of a second after `Enter`, onto a
//! blank screen, was not an answer to an offer nobody had seen yet.
//!
//! Any other key is sent on to the session (see [`Forward`]), which is the one
//! thing this screen does that is not drawing or giving up. Dropping them was
//! the obvious reading — the session is not answering, so there is nowhere to
//! send it — and it was wrong twice over: `nvim_input` is a fast call, so a key
//! lands whatever the editor is parked in, and the waits long enough for this
//! screen to appear include the ones that end *only* when a key arrives. A
//! screen that drew a spinner while swallowing the keystroke that would stop it
//! is the shape of the bug this was written against.
//!
//! One consequence worth knowing: the probe used to run on a cooked terminal,
//! where `Ctrl-c` was a signal that ended nvmux. Here it is a key — queued for
//! the editor while the screen is blank, a cancel once the spinner is up.
//!
//! # No mouse
//!
//! The one screen that does not take the mouse, and the grace period is why:
//! with reporting on, a pointer moved during those first milliseconds would
//! leave SGR reports in the queue the editor is about to read.
//!
//! # The tick
//!
//! The wait is on the probe's channel, not on the keyboard: the probe's answer
//! wakes the loop at once, so a fast attach pays nothing for the screen, and a
//! key is read on the way round, at most a [`FRAME`] late.

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::layout::Rect;
use ratatui::Frame;

use super::app::Key;
use super::draw;
use crate::error::Result;
use crate::pty::{Answer, Probe};
use crate::rpc;

/// How long an attach may take before the screen appears — and, with it,
/// before a single key is read. The one threshold this screen has; see the
/// module docs for both halves of what it gates.
const GRACE: Duration = Duration::from_millis(300);

/// One turn of the spinner, and the wait between polls.
const FRAME: Duration = Duration::from_millis(80);

/// The spinner's frames. Braille, one column each, so the line's width does
/// not change as it turns.
const GLYPHS: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The way out, in the grammar the hint rows use: key, then action. Two words
/// on the last row, like the help screen's `esc back`.
const CANCEL: &str = "esc cancel";

/// How the wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The session will take the client: hand it the terminal.
    Ready,
    /// The same — hand it the terminal — but the session is waiting for a key
    /// nvmux will not press, so it will draw nothing until the user presses
    /// one. Handing over *is* the recovery: the client's keys reach the server
    /// on the fast path whatever it is waiting in, so the first key ends the
    /// wait and releases the paint queued behind it. The caller says so on the
    /// notice, since the screen itself will be blank.
    Blocked,
    /// The user gave up on the session. The client is the caller's to retire.
    Cancelled,
}

impl Verdict {
    /// Whether this hands the terminal to the client — see
    /// [`super::Screen::close_for_attach`].
    fn attaches(&self) -> bool {
        matches!(self, Self::Ready | Self::Blocked)
    }
}

/// What the screen knows: which session, and how long it has been waiting.
///
/// Told the time rather than reading a clock, like [`crate::announce::Popup`],
/// so every frame it can draw is reachable from a test.
struct State {
    name: String,
    elapsed: Duration,
}

/// Wait for `probe` on a screen of its own, and say how it ended.
///
/// A failed probe is the error it failed with. The terminal goes back through
/// the ordinary close then, as on a cancel: the picker comes next and takes
/// the screen for itself. The two answers that have a client to hand —
/// [`Verdict::Ready`] and [`Verdict::Blocked`] — give it back cleared instead,
/// for the relay. The probe is dropped on every way out, which is what ends
/// its worker (see [`Probe`]).
pub fn run(probe: Probe, name: &str, sock: &Path) -> Result<Verdict> {
    // No mouse: see the module docs.
    super::owning_for_attach(Verdict::attaches, false, |terminal| {
        run_on(terminal, probe, name, sock)
    })
}

fn run_on(
    terminal: &mut ratatui::DefaultTerminal,
    mut probe: Probe,
    name: &str,
    sock: &Path,
) -> Result<Verdict> {
    let started = Instant::now();
    let mut state = State {
        name: name.to_string(),
        elapsed: Duration::ZERO,
    };
    // Whether the queue has been drained, which happens once, at `GRACE`.
    let mut listening = false;
    let mut forward = Forward::to(sock);

    loop {
        if let Some(answer) = probe.wait(FRAME) {
            return Ok(match answer? {
                Answer::Ready => Verdict::Ready,
                Answer::Blocked { mode } => {
                    tracing::info!(mode = %mode, "attach: handing over a session waiting for a key");
                    Verdict::Blocked
                }
            });
        }
        state.elapsed = started.elapsed();

        // Not a key before `GRACE` — not even a poll, which would take bytes
        // out of the queue the editor is going to read.
        if state.elapsed >= GRACE {
            if !listening {
                listening = true;
                drain()?;
            }
            while let Some(key) = next_key()? {
                if matches!(key, Key::Esc | Key::CtrlC) {
                    return Ok(Verdict::Cancelled);
                }
                forward.send(key);
            }
        }

        terminal.draw(|f| draw(f, &state))?;
    }
}

/// Where a key typed during a visible wait goes.
///
/// It used to go nowhere. What the module docs above call the cost of the
/// offer — "the keys typed *during* a visible wait, which used to reach the
/// editor and no longer do" — turned out to have a sharper edge than losing a
/// keystroke: the waits long enough to draw on include the ones the session is
/// in *because* it is waiting for a key, and dropping the keys there meant
/// nvmux was eating the one thing that would end the wait it was drawing.
///
/// So they are sent on, over a connection of this screen's own: `nvim_input`
/// is a fast call, answered from the socket's read callback, so a key lands
/// whatever the editor is parked in — which is the whole reason it is worth
/// sending. The probe's own connection cannot carry them, being parked in a
/// blocking read on its worker thread.
///
/// "Whatever it is parked in" is stronger than the rest of this crate's
/// fast-call caution, and is measured rather than assumed. During a `:!cmd`,
/// where `nvim_get_mode` is queued like any deferred call and answers nothing
/// at all, `nvim_input` still answers in 0.1 ms (0.12.5, `:!sleep 4`). So a
/// send here does not stall the loop it is drawn from, in the one state where
/// it might have been expected to — and a failure really does mean the session
/// is unreachable, which is why one is enough to stop trying.
///
/// Only the keys nvmux can spell without guessing are sent (see [`notation`]).
/// `Esc` and `Ctrl-c` never reach here: they are the offer on the hint row, and
/// answering it is not typing.
struct Forward<'a> {
    sock: &'a Path,
    /// Opened on the first key, so an attach nobody types during — which is
    /// nearly all of them — costs no connection at all.
    client: Option<rpc::Client<UnixStream>>,
    /// Set once the session could not be reached or would not take a key, so
    /// it is not dialled again on every keystroke of a long wait. Nothing is
    /// reported: a key that did not arrive is exactly as bad as the key that
    /// was dropped before this existed, and the wait is still the user's to
    /// end with `Esc`.
    gave_up: bool,
}

impl<'a> Forward<'a> {
    fn to(sock: &'a Path) -> Self {
        Self {
            sock,
            client: None,
            gave_up: false,
        }
    }

    fn send(&mut self, key: Key) {
        let Some(keys) = notation(key) else {
            return;
        };
        if self.gave_up {
            return;
        }
        if self.client.is_none() {
            match rpc::Client::connect(self.sock, rpc::CONNECT_TIMEOUT) {
                Ok(client) => self.client = Some(client),
                Err(e) => {
                    tracing::debug!(error = %e, "attaching: no connection for the keys typed");
                    self.gave_up = true;
                    return;
                }
            }
        }
        let client = self.client.as_mut().expect("just connected");
        if let Err(e) = client.input(&keys) {
            tracing::debug!(error = %e, "attaching: the session would not take a key");
            self.gave_up = true;
        }
    }
}

/// This key in Neovim's own notation, or `None` for one nvmux will not guess at.
///
/// Deliberately a short list. [`Key`] is lossy — everything it does not name
/// arrives as [`Key::Other`], and a chord, a function key or an arrow cannot be
/// recovered from that — so anything beyond these would be nvmux inventing a
/// keystroke and typing it into somebody's editor, which is the failure the
/// rest of this change exists to avoid. What is here is enough for the job:
/// ending a wait takes any key at all, and these are the ones a person leans on
/// when a screen looks stuck.
///
/// `<` is the one character that is not itself in this notation, being the
/// opening of every named key; `<lt>` is how Neovim spells it.
fn notation(key: Key) -> Option<String> {
    match key {
        Key::Char('<') => Some("<lt>".into()),
        Key::Char(c) => Some(c.to_string()),
        Key::Enter => Some("<CR>".into()),
        Key::Backspace => Some("<BS>".into()),
        Key::Tab => Some("<Tab>".into()),
        Key::Up => Some("<Up>".into()),
        Key::Down => Some("<Down>".into()),
        Key::Left => Some("<Left>".into()),
        Key::Right => Some("<Right>".into()),
        Key::Home | Key::End | Key::CtrlN | Key::CtrlP | Key::Other => None,
        // Never reached: both are the way out, taken before this is asked.
        Key::Esc | Key::CtrlC => None,
    }
}

/// Discard whatever was typed before the offer appeared. Once, at [`GRACE`].
fn drain() -> Result<()> {
    while event::poll(Duration::ZERO)? {
        event::read()?;
    }
    Ok(())
}

/// The next key already waiting, without waiting for one. `None` once there
/// is nothing left; events that are not key presses are stepped over.
fn next_key() -> Result<Option<Key>> {
    while event::poll(Duration::ZERO)? {
        if let Event::Key(k) = event::read()? {
            if k.kind == KeyEventKind::Press {
                return Ok(Some(super::translate(k)));
            }
        }
    }
    Ok(None)
}

/// The screen: the spinner and what it is waiting on where the picker draws
/// its list, the way out on the last row. Nothing at all before [`GRACE`].
///
/// Both rows are dim, like every hint row: a status and an offer, neither of
/// them something the user is being asked to answer.
fn draw(frame: &mut Frame, state: &State) {
    if state.elapsed < GRACE {
        return;
    }
    draw::screen(frame, CANCEL, true, |frame, body| {
        draw_status(frame, state, body)
    });
}

/// The status line, drawn the way the empty picker draws "no sessions": one
/// dim sentence centred on both axes, truncated from the right rather than
/// wrapped, so the hint row stays where it is — a hint row by another name,
/// and drawn through the same function.
///
/// A terminal with no room above the hint row gets no status: the way out is
/// worth more than the name of what it gets out of.
fn draw_status(frame: &mut Frame, state: &State, area: Rect) {
    if area.height == 0 {
        return;
    }
    draw::draw_hint_row(
        frame,
        draw::centre_vertically(area, 1),
        &status(state),
        true,
    );
}

/// The spinner and the session it is waiting for.
///
/// Fitted by the caller rather than here: the two rows truncate
/// independently, each from the right, as every other row in this UI does.
fn status(state: &State) -> String {
    let turn = state.elapsed.as_millis() / FRAME.as_millis();
    let glyph = GLYPHS[(turn % GLYPHS.len() as u128) as usize];
    format!("{glyph} attaching to {}", state.name)
}

#[cfg(test)]
mod tests {
    use super::super::test_support;
    use super::*;
    use ratatui::style::Modifier;
    use unicode_width::UnicodeWidthStr;

    fn at(elapsed: Duration, name: &str) -> State {
        State {
            name: name.into(),
            elapsed,
        }
    }

    fn render(w: u16, h: u16, state: &State) -> Vec<String> {
        test_support::render(w, h, |f| draw(f, state))
    }

    /// Which line the status lands on, given the terminal's height: the middle
    /// of everything above the hint row. Worked out the way
    /// `draw::centre_vertically` does, so a test says where the line *should*
    /// be rather than agreeing with wherever it went.
    fn status_row(h: u16) -> usize {
        let body = h - 1;
        usize::from((body - 1) / 2)
    }

    /// Does this line open with a spinner glyph?
    fn spins(line: &str) -> bool {
        line.trim_start()
            .chars()
            .next()
            .is_some_and(|c| GLYPHS.contains(&c))
    }

    /// A fast attach — every local one, most remote ones — shows nothing at
    /// all, so the transition it sits in is not decorated with a flash. And
    /// nothing is read either, which is the half this cannot see; see the
    /// module docs.
    #[test]
    fn nothing_is_drawn_before_the_grace_period() {
        let lines = render(40, 8, &at(GRACE - Duration::from_millis(1), "dotfiles"));
        assert!(
            lines.iter().all(String::is_empty),
            "drew before GRACE: {lines:?}"
        );

        let lines = render(40, 8, &at(GRACE, "dotfiles"));
        assert!(
            !lines[status_row(8)].is_empty() && !lines[7].is_empty(),
            "GRACE must bring up both rows at once: {lines:?}"
        );
    }

    /// The picker's shape: the status where the list goes, centred on both
    /// axes, and the way out alone on the last row. Nothing anywhere else —
    /// two rows, and the screen is otherwise the terminal's own background.
    #[test]
    fn the_status_is_centred_and_the_way_out_is_on_the_last_row() {
        let lines = render(40, 8, &at(GRACE, "dotfiles"));
        let occupied: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.is_empty())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            occupied,
            vec![status_row(8), 7],
            "want the centred status row and the hint row: {lines:?}"
        );

        let status = &lines[status_row(8)];
        assert!(spins(status), "{status:?}");
        assert!(status.contains("attaching to dotfiles"), "{status:?}");
        let (left, right) = test_support::padding(status, 40);
        assert!(
            left.abs_diff(right) <= 2,
            "the status is not centred: {left} left, {right} right, {status:?}"
        );

        let hint = &lines[7];
        assert_eq!(hint.trim(), CANCEL);
        let (left, right) = test_support::padding(hint, 40);
        assert!(
            left.abs_diff(right) <= 2,
            "the hint is not centred: {left} left, {right} right, {hint:?}"
        );
    }

    /// The offer is live from the moment it is drawn (see the module docs), so
    /// the row carries it on every frame the screen is up — never a hint that
    /// has to be waited out, and never one the status line has to make room
    /// for.
    #[test]
    fn the_way_out_is_offered_for_as_long_as_the_screen_is_up() {
        let before = render(40, 6, &at(GRACE - Duration::from_millis(1), "dotfiles"));
        assert!(before[5].is_empty(), "offered before GRACE: {before:?}");

        for elapsed in [GRACE, Duration::from_secs(1), Duration::from_secs(300)] {
            let lines = render(40, 6, &at(elapsed, "dotfiles"));
            assert_eq!(lines[5].trim(), CANCEL, "at {elapsed:?}: {lines:?}");
            assert!(
                !lines[status_row(6)].contains(CANCEL),
                "the hint belongs to the last row alone: {lines:?}"
            );
        }
    }

    /// One glyph per frame, round and round, so the spinner visibly turns
    /// however long the wait is — and every glyph is one column, so the line
    /// does not jitter as it goes.
    #[test]
    fn the_spinner_turns_one_glyph_per_frame_and_wraps() {
        for k in 0..25u32 {
            let text = status(&at(FRAME * k, "x"));
            let want = GLYPHS[k as usize % GLYPHS.len()];
            assert!(
                text.starts_with(want),
                "frame {k}: want {want:?}, got {text:?}"
            );
        }
        let widths: Vec<usize> = (0..GLYPHS.len() as u32)
            .map(|k| status(&at(FRAME * k, "dotfiles")).width())
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
    }

    /// Two rows, two independent truncations: a name too long for the screen
    /// loses its tail like any other row in this UI, and takes nothing from
    /// the row that says how to get out.
    #[test]
    fn a_long_name_is_truncated_and_the_way_out_is_untouched() {
        let long = "a".repeat(60);
        let lines = render(30, 3, &at(GRACE, &long));

        let status = &lines[status_row(3)];
        assert!(status.width() <= 30, "{status:?}");
        assert!(spins(status), "{status:?}");
        assert!(status.contains("attaching to a"), "{status:?}");
        assert_eq!(lines[2].trim(), CANCEL, "{lines:?}");
    }

    /// Two columns per character, so a Japanese name is fitted to what it
    /// occupies rather than to how many characters it has.
    #[test]
    fn a_wide_name_is_fitted_by_column() {
        let lines = render(24, 4, &at(GRACE, "日本語のセッション"));
        let status = &lines[status_row(4)];
        assert!(status.width() <= 24, "{status:?}");
        assert!(status.contains("attaching to 日本"), "{status:?}");
        assert_eq!(lines[3].trim(), CANCEL, "{lines:?}");
    }

    /// Down to one row, where the hint row is the only row there is.
    #[test]
    fn every_tiny_size_survives() {
        for &(w, h) in test_support::TINY_SIZES {
            for elapsed in [Duration::ZERO, GRACE, Duration::from_secs(300)] {
                let lines = render(w, h, &at(elapsed, "dotfiles"));
                for line in &lines {
                    assert!(
                        line.width() <= usize::from(w),
                        "{w}x{h} at {elapsed:?} overflowed: {line:?}"
                    );
                }
            }
        }
    }

    /// Both answers that have a client to hand give it the terminal. A
    /// blocked session is attached to like any other — that is the whole
    /// recovery, since the key that ends its wait is one the user presses on
    /// the client this hands over — and only the notice differs.
    #[test]
    fn a_blocked_session_is_attached_to_and_only_a_cancel_is_not() {
        assert!(Verdict::Ready.attaches());
        assert!(Verdict::Blocked.attaches());
        assert!(!Verdict::Cancelled.attaches());
    }

    /// What may be typed at a session on the user's behalf, and what may not.
    ///
    /// The `None`s are the point. [`Key`] collapses everything it does not
    /// name into `Key::Other`, so a chord or a function key cannot be
    /// recovered from it, and spelling one anyway would be nvmux inventing a
    /// keystroke for somebody's editor. `Esc` and `Ctrl-c` are the offer on
    /// the hint row rather than text, and never reach this at all.
    #[test]
    fn only_the_keys_nvmux_can_spell_are_sent_on() {
        assert_eq!(notation(Key::Char('a')).as_deref(), Some("a"));
        assert_eq!(notation(Key::Char('日')).as_deref(), Some("日"));
        assert_eq!(notation(Key::Enter).as_deref(), Some("<CR>"));
        assert_eq!(notation(Key::Backspace).as_deref(), Some("<BS>"));
        assert_eq!(notation(Key::Up).as_deref(), Some("<Up>"));

        // `<` opens every named key in this notation, so it is the one
        // character that cannot be sent as itself.
        assert_eq!(notation(Key::Char('<')).as_deref(), Some("<lt>"));

        for key in [Key::Other, Key::Esc, Key::CtrlC, Key::CtrlN, Key::CtrlP] {
            assert_eq!(notation(key), None, "{key:?} must not be guessed at");
        }
    }

    /// A session that cannot be reached is not dialled again on every
    /// keystroke, and never fails the attach: the wait is still the user's to
    /// end with `Esc`, exactly as it was when the keys went nowhere at all.
    #[test]
    fn a_session_that_will_not_take_a_key_is_given_up_on_once() {
        let mut forward = Forward::to(Path::new("/nonexistent/nvmux-test.sock"));
        for _ in 0..3 {
            forward.send(Key::Char('x'));
        }
        assert!(forward.gave_up);
        assert!(forward.client.is_none());
    }

    /// A key nvmux will not spell must not open a connection either: an
    /// attach nobody types anything sendable during pays for nothing.
    #[test]
    fn an_unspellable_key_costs_no_connection() {
        let mut forward = Forward::to(Path::new("/nonexistent/nvmux-test.sock"));
        forward.send(Key::Other);
        assert!(
            !forward.gave_up,
            "an unsendable key must not count as a failure"
        );
        assert!(forward.client.is_none());
    }

    #[test]
    fn nothing_sets_a_colour() {
        test_support::assert_no_colour(40, 6, |f| draw(f, &at(GRACE, "dotfiles")));
    }

    #[test]
    fn nothing_draws_a_border() {
        test_support::assert_no_borders(&render(40, 6, &at(GRACE, "dotfiles")));
    }

    /// Dim throughout, both rows: the screen is telling the user what it is
    /// waiting for and offering a key, and neither is something being asked
    /// or typed — the distinction the hint row's `dim` flag carries
    /// everywhere else.
    #[test]
    fn both_rows_are_dim() {
        let buf = test_support::buffer(40, 4, |f| draw(f, &at(GRACE, "dotfiles")));

        let mut seen = 0;
        for y in [status_row(4) as u16, 3] {
            for x in 0..40 {
                let cell = &buf[(x, y)];
                if cell.symbol().trim().is_empty() {
                    continue;
                }
                seen += 1;
                assert!(
                    cell.modifier.contains(Modifier::DIM),
                    "cell ({x},{y}) is not dim: {:?}",
                    cell.symbol()
                );
            }
        }
        assert!(seen > 0, "both rows were blank");
    }
}
