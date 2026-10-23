//! The screen shown while a session takes its client, between the picker (or
//! a `<prefix>` switch) and the session.
//!
//! The attach probe ([`crate::pty::Probe`]) ends with a deferred call, which a
//! session in `:!make` answers only when the make finishes. There is no
//! timeout; this screen shows a spinner by the session's name and offers
//! `Esc`. A session parked on a keypress ([`Answer::Blocked`]) is handed the
//! terminal at once, before [`GRACE`]: only a key from its client can unblock
//! it.
//!
//! Nothing is drawn or read for the first [`GRACE`]. Most attaches finish
//! sooner (ms locally, a few hundred over ssh), and keys typed meanwhile must
//! stay queued for the editor: a `:` typed right after `Enter` on the picker
//! opens its command line, and a poll would consume it. Unlike other screens,
//! this one never enables the mouse, so pointer reports stay out of that
//! queue too.
//!
//! Once the spinner is up, keys typed before it are dropped (an `Esc` typed
//! onto a blank screen was not a cancel), `Esc`/`Ctrl-c` cancel, and other
//! keys are forwarded to the session ([`Forward`]), since a long wait may be
//! one that only a key ends. So `Ctrl-c` is a key, not a signal: queued for
//! the editor during the grace, a cancel after.
//!
//! The loop waits on the probe's channel, so an answer is acted on at once;
//! keys are read at most a [`FRAME`] late.

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::layout::Rect;
use ratatui::Frame;

use super::app::Key;
use super::draw;
use crate::error::Result;
use crate::handoff::HandOff;
use crate::pty::{Answer, Probe};
use crate::rpc;

/// How long an attach may take before the screen appears and keys are read.
const GRACE: Duration = Duration::from_millis(300);

/// One spinner step, and the poll interval. Also the pace of the picker's
/// busy spinner ([`super::note`]).
pub(super) const FRAME: Duration = Duration::from_millis(80);

/// Braille, one column each, so the line's width does not change. Shared with
/// the picker.
pub(super) const GLYPHS: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

const CANCEL: &str = "esc cancel";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Hand the client the terminal.
    Ready,
    /// Hand over too, but the session waits for a key and draws nothing until
    /// the user presses one. Handing over is the recovery: the client's keys
    /// reach the server on the fast path, so the first key ends the wait. The
    /// caller says so on the notice, as the screen will be blank.
    Blocked,
    /// The user gave up. The caller retires the client.
    Cancelled,
}

impl Verdict {
    /// Whether the client gets the terminal (see
    /// [`super::Screen::close_for_attach`]).
    fn attaches(&self) -> bool {
        matches!(self, Self::Ready | Self::Blocked)
    }
}

/// Given the time rather than reading a clock, like [`crate::announce::Popup`],
/// so every frame is reachable from a test.
struct State {
    name: String,
    elapsed: Duration,
}

/// Wait for `probe` and say how it ended; a failed probe returns its error.
/// On an error or cancel the terminal is closed normally for the picker; on
/// [`Verdict::Ready`] or [`Verdict::Blocked`] it is handed back cleared for
/// the relay. Dropping the probe ends its worker (see [`Probe`]).
///
/// The grace is waited out without taking the screen, so a fast answer gets a
/// terminal untouched since the caller, and a name the picker left up
/// (`hand_off`, see [`crate::handoff`]) stays. Once the spinner is due, the
/// name is taken down first.
pub fn run(
    mut probe: Probe,
    name: &str,
    sock: &Path,
    hand_off: &mut Option<HandOff>,
) -> Result<Verdict> {
    let started = Instant::now();
    if let Some(verdict) = wait_out_grace(&mut probe, started)? {
        return Ok(verdict);
    }
    if let Some(h) = hand_off.take() {
        h.take_down();
    }
    // No mouse: see the module docs.
    super::owning_for_attach(Verdict::attaches, false, |terminal| {
        run_on(terminal, probe, name, sock, started)
    })
}

/// Wait for the probe until [`GRACE`], in raw mode so keys typed meanwhile
/// are neither echoed nor read. `None` if the grace ran out first.
fn wait_out_grace(probe: &mut Probe, started: Instant) -> Result<Option<Verdict>> {
    let _raw = crate::term::RawMode::enter()?;
    loop {
        let left = GRACE.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Ok(None);
        }
        if let Some(answer) = probe.wait(left.min(FRAME)) {
            return Ok(Some(verdict(answer?)));
        }
    }
}

fn verdict(answer: Answer) -> Verdict {
    match answer {
        Answer::Ready => Verdict::Ready,
        Answer::Blocked { mode } => {
            tracing::info!(mode = %mode, "attach: handing over a session waiting for a key");
            Verdict::Blocked
        }
    }
}

fn run_on(
    terminal: &mut ratatui::DefaultTerminal,
    mut probe: Probe,
    name: &str,
    sock: &Path,
    started: Instant,
) -> Result<Verdict> {
    let mut state = State {
        name: name.to_string(),
        elapsed: Duration::ZERO,
    };
    // Set once the input queue is drained, at `GRACE`.
    let mut listening = false;
    let mut forward = Forward::to(sock);

    loop {
        if let Some(answer) = probe.wait(FRAME) {
            return Ok(verdict(answer?));
        }
        state.elapsed = started.elapsed();

        // No reads before `GRACE`, not even a poll: the bytes are the
        // editor's.
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

        terminal.draw(|f| {
            draw(f, &state);
            crate::theme::paint(f.buffer_mut());
        })?;
    }
}

/// Sends keys typed during a visible wait to the session, since the wait may
/// be one that only a key ends. Uses its own connection: the probe's is
/// blocked in a read on its worker thread.
///
/// `nvim_input` is a fast call, answered from the socket's read callback, so
/// it lands whatever the editor is in. Measured: during `:!sleep 4`, where
/// `nvim_get_mode` never answers, `nvim_input` answers in 0.1 ms (0.12.5). So
/// a send does not stall the loop, and one failure means the session is
/// unreachable.
///
/// Only keys [`notation`] spells are sent; `Esc` and `Ctrl-c` never get here.
struct Forward<'a> {
    sock: &'a Path,
    /// Opened on the first key; most attaches see none.
    client: Option<rpc::Client<UnixStream>>,
    /// Set on the first failure, so a long wait does not redial on every key.
    /// Not reported: the user can still `Esc`.
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

/// `key` in Neovim's notation, or `None` for a key not sent. [`Key`] is lossy
/// (chords and function keys arrive as [`Key::Other`]), so the list is short
/// rather than invent keystrokes; any key ends a wait anyway. `<` opens every
/// named key, so it is spelled `<lt>`.
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
        Key::Home | Key::End | Key::BackTab | Key::CtrlN | Key::CtrlP | Key::Other => None,
        // Never reached: handled as a cancel first.
        Key::Esc | Key::CtrlC => None,
    }
}

/// Discard what was typed before the offer appeared. Called once, at
/// [`GRACE`].
fn drain() -> Result<()> {
    while event::poll(Duration::ZERO)? {
        event::read()?;
    }
    Ok(())
}

/// The next queued key press, without blocking. Other events are skipped.
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

/// Status where the picker draws its list, the way out on the last row, both
/// dim like every hint row. Nothing before [`GRACE`].
fn draw(frame: &mut Frame, state: &State) {
    if state.elapsed < GRACE {
        return;
    }
    draw::screen(frame, CANCEL, true, |frame, body| {
        draw_status(frame, state, body)
    });
}

/// Drawn like the empty picker's line: centred, truncated from the right
/// rather than wrapped. With no room above the hint row, it is dropped and
/// the way out kept.
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

/// Truncated by the caller, independently of the hint row.
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

    /// The middle of the area above the hint row, computed here rather than
    /// by calling `draw::centre_vertically`, so tests check where it should be.
    fn status_row(h: u16) -> usize {
        let body = h - 1;
        usize::from((body - 1) / 2)
    }

    fn spins(line: &str) -> bool {
        line.trim_start()
            .chars()
            .next()
            .is_some_and(|c| GLYPHS.contains(&c))
    }

    /// That no keys are read before `GRACE` is not covered here.
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

    /// The cancel is live as soon as it is drawn, so it shows on every frame.
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

    /// Every glyph is one column, so the line does not jitter.
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

    #[test]
    fn a_wide_name_is_fitted_by_column() {
        let lines = render(24, 4, &at(GRACE, "日本語のセッション"));
        let status = &lines[status_row(4)];
        assert!(status.width() <= 24, "{status:?}");
        assert!(status.contains("attaching to 日本"), "{status:?}");
        assert_eq!(lines[3].trim(), CANCEL, "{lines:?}");
    }

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

    /// A blocked session is attached to like any other: the key that ends its
    /// wait is pressed on the client this hands over.
    #[test]
    fn a_blocked_session_is_attached_to_and_only_a_cancel_is_not() {
        assert!(Verdict::Ready.attaches());
        assert!(Verdict::Blocked.attaches());
        assert!(!Verdict::Cancelled.attaches());
    }

    #[test]
    fn only_the_keys_nvmux_can_spell_are_sent_on() {
        assert_eq!(notation(Key::Char('a')).as_deref(), Some("a"));
        assert_eq!(notation(Key::Char('日')).as_deref(), Some("日"));
        assert_eq!(notation(Key::Enter).as_deref(), Some("<CR>"));
        assert_eq!(notation(Key::Backspace).as_deref(), Some("<BS>"));
        assert_eq!(notation(Key::Up).as_deref(), Some("<Up>"));

        // `<` opens every named key, so it cannot be sent as itself.
        assert_eq!(notation(Key::Char('<')).as_deref(), Some("<lt>"));

        for key in [Key::Other, Key::Esc, Key::CtrlC, Key::CtrlN, Key::CtrlP] {
            assert_eq!(notation(key), None, "{key:?} must not be guessed at");
        }
    }

    /// Not redialled per key, and never fails the attach: `Esc` still works.
    #[test]
    fn a_session_that_will_not_take_a_key_is_given_up_on_once() {
        let mut forward = Forward::to(Path::new("/nonexistent/nvmux-test.sock"));
        for _ in 0..3 {
            forward.send(Key::Char('x'));
        }
        assert!(forward.gave_up);
        assert!(forward.client.is_none());
    }

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
