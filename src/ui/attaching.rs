//! The wait for a session to take its client: the screen between the picker
//! (or a `<prefix>` switch) and the session itself.
//!
//! An attach probes the session before its client is let onto the terminal
//! (see [`crate::pty::Probe`]), and the probe's last call is a deferred one: a
//! session in the middle of `:!make` answers it when the make is done. That
//! wait used to be three seconds and then a refusal. Now it is as long as it
//! takes, and this screen is what makes that bearable — a spinner in the
//! bottom-right corner, so the wait is visibly the session's and not a hang,
//! and, once it has gone on long enough, `Esc` to give up on it.
//!
//! # Two clocks
//!
//! Nothing is drawn for the first [`GRACE`]. An attach to an idle session is
//! milliseconds locally and a few hundred over ssh, and a spinner that flashed
//! on every one of those would be noise on the transition it is meant to
//! explain.
//!
//! Nothing is *read* for the first [`PATIENCE`], and this is the one that
//! matters. Whatever the user types while an attach is in flight sits in the
//! terminal's input queue, and every byte of it has always reached the editor
//! once the relay began: a `:` typed straight after `Enter` on the picker
//! opens the command line. That has to stay true, and a screen that read its
//! keys would break it — the keys would be gone from the queue, and one of
//! them might be the `Esc` a vim user types by reflex, which must not cancel
//! an attach that was about to succeed. So the screen leaves the queue alone
//! until the wait has gone on for longer than the old budget, which is the
//! point past which no attach used to succeed at all: nothing typed by then
//! was ever going to be delivered. From there the row says `esc cancel`, what
//! was typed ahead is drained and dropped (an `Esc` typed before the offer
//! existed was not an answer to it), and `Esc` or `Ctrl-c` ends the attach.
//! Any other key is dropped too: the session is not answering, and there is
//! nowhere to send it.
//!
//! One consequence worth knowing: the probe used to run on a cooked terminal,
//! where `Ctrl-c` was a signal that ended nvmux. Here it is a key — queued for
//! the editor before [`PATIENCE`], a cancel after.
//!
//! # No mouse
//!
//! The one screen that does not take the mouse, for the reason above: with
//! reporting on, every movement of the pointer during the wait would be a
//! report in the same queue, handed to the editor as input the moment the
//! relay began.
//!
//! # The tick
//!
//! The wait is on the probe's channel, not on the keyboard: the probe's answer
//! wakes the loop at once, so a fast attach pays nothing for the screen, and a
//! key is read on the way round, at most a [`FRAME`] late.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::layout::Alignment;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::app::Key;
use super::draw;
use crate::error::Result;
use crate::pty::Probe;

/// How long an attach may take before anything is shown.
const GRACE: Duration = Duration::from_millis(300);

/// How long an attach may take before the user is offered a way out — and
/// before this screen reads a single key. The old probe budget, deliberately:
/// see the module docs.
const PATIENCE: Duration = Duration::from_secs(3);

/// One turn of the spinner, and the wait between polls.
const FRAME: Duration = Duration::from_millis(80);

/// The spinner's frames. Braille, one column each, so the row's width does
/// not change as it turns.
const GLYPHS: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The way out, in the grammar the hint rows use: key, then action.
const CANCEL: &str = "esc cancel";

/// How the wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The session will take the client: hand it the terminal.
    Ready,
    /// The user gave up on the session. The client is the caller's to retire.
    Cancelled,
}

impl Verdict {
    /// Whether this hands the terminal to the client — see
    /// [`super::Screen::close_for_attach`].
    fn attaches(&self) -> bool {
        matches!(self, Self::Ready)
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
/// the screen for itself. Only [`Verdict::Ready`] gives it back cleared, for
/// the relay. The probe is dropped on every way out, which is what ends its
/// worker (see [`Probe`]).
pub fn run(probe: Probe, name: &str) -> Result<Verdict> {
    // No mouse: see the module docs.
    super::owning_for_attach(Verdict::attaches, false, |terminal| {
        run_on(terminal, probe, name)
    })
}

fn run_on(
    terminal: &mut ratatui::DefaultTerminal,
    mut probe: Probe,
    name: &str,
) -> Result<Verdict> {
    let started = Instant::now();
    let mut state = State {
        name: name.to_string(),
        elapsed: Duration::ZERO,
    };
    // Whether the queue has been drained, which happens once, at `PATIENCE`.
    let mut listening = false;

    loop {
        if let Some(verdict) = probe.wait(FRAME) {
            verdict?;
            return Ok(Verdict::Ready);
        }
        state.elapsed = started.elapsed();

        // Not a key before `PATIENCE` — not even a poll, which would take
        // bytes out of the queue the editor is going to read.
        if state.elapsed >= PATIENCE {
            if !listening {
                listening = true;
                drain()?;
            }
            while let Some(key) = next_key()? {
                if matches!(key, Key::Esc | Key::CtrlC) {
                    return Ok(Verdict::Cancelled);
                }
            }
        }

        terminal.draw(|f| draw(f, &state))?;
    }
}

/// Discard everything typed so far. Once, at `PATIENCE`.
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

/// The bottom-right corner: the spinner and what it is waiting on, and after
/// [`PATIENCE`] the way out. Nothing before [`GRACE`].
///
/// Right-aligned on the last row, the one every screen keeps for its hint, and
/// dim like a hint: it is a status, not something the user is being asked.
fn draw(frame: &mut Frame, state: &State) {
    let area = frame.area();
    if area.height == 0 || area.width == 0 || state.elapsed < GRACE {
        return;
    }
    let (_, bottom) = draw::split_hint_row(area);
    let text = row(state, usize::from(bottom.width));
    let para = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().add_modifier(Modifier::DIM),
    )))
    .alignment(Alignment::Right);
    frame.render_widget(para, bottom);
}

/// The row's text, fitted to `width` columns.
///
/// The name is what gives way on a narrow terminal, and the words that
/// introduce it go with it once there is no room for any of it: the glyph
/// says a wait is on, and the hint says how to end one. Only a terminal too
/// narrow for the hint itself cuts the hint.
fn row(state: &State, width: usize) -> String {
    let turn = state.elapsed.as_millis() / FRAME.as_millis();
    let glyph = GLYPHS[(turn % GLYPHS.len() as u128) as usize];
    let tail = if state.elapsed >= PATIENCE {
        format!("  {CANCEL}")
    } else {
        String::new()
    };
    let lead = format!("{glyph} attaching to ");
    let room = width.saturating_sub(lead.width() + tail.width());
    let name = draw::truncate(&state.name, room);
    let text = if name.is_empty() {
        format!("{glyph}{tail}")
    } else {
        format!("{lead}{name}{tail}")
    };
    draw::truncate(&text, width)
}

#[cfg(test)]
mod tests {
    use super::super::test_support;
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn at(elapsed: Duration, name: &str) -> State {
        State {
            name: name.into(),
            elapsed,
        }
    }

    fn render(w: u16, h: u16, state: &State) -> Vec<String> {
        test_support::render(w, h, |f| draw(f, state))
    }

    /// A fast attach — every local one, most remote ones — shows nothing at
    /// all, so the transition it sits in is not decorated with a flash.
    #[test]
    fn nothing_is_drawn_before_the_grace_period() {
        let lines = render(40, 8, &at(GRACE - Duration::from_millis(1), "dotfiles"));
        assert!(
            lines.iter().all(String::is_empty),
            "drew before GRACE: {lines:?}"
        );
        let lines = render(40, 8, &at(GRACE, "dotfiles"));
        assert!(!lines[7].is_empty(), "drew nothing at GRACE: {lines:?}");
    }

    /// The last row, flush against the right edge, and nothing anywhere else.
    #[test]
    fn the_spinner_sits_in_the_bottom_right_corner() {
        let lines = render(40, 8, &at(GRACE, "dotfiles"));
        for (i, line) in lines.iter().enumerate().take(7) {
            assert!(line.is_empty(), "row {i} is not blank: {line:?}");
        }
        let last = &lines[7];
        assert!(last.contains("attaching to dotfiles"), "{last:?}");
        assert_eq!(
            last.width(),
            40,
            "the row does not reach the right edge: {last:?}"
        );
        assert!(
            GLYPHS.contains(&last.trim_start().chars().next().expect("a glyph")),
            "the row does not start with a spinner glyph: {last:?}"
        );
    }

    /// `esc cancel` appears exactly when the screen starts listening for it,
    /// and not a frame before: an offer the screen would not honour yet is a
    /// lie on the hint row.
    #[test]
    fn the_way_out_is_offered_only_after_patience() {
        let before = render(40, 4, &at(PATIENCE - Duration::from_millis(1), "dotfiles"));
        assert!(!before[3].contains(CANCEL), "{before:?}");
        let after = render(40, 4, &at(PATIENCE, "dotfiles"));
        assert!(after[3].contains(CANCEL), "{after:?}");
        assert!(
            after[3].ends_with(CANCEL),
            "the hint is not last: {after:?}"
        );
    }

    /// One glyph per frame, round and round, so the spinner visibly turns
    /// however long the wait is.
    #[test]
    fn the_spinner_turns_one_glyph_per_frame_and_wraps() {
        for k in 0..25u32 {
            let text = row(&at(FRAME * k, "x"), 80);
            let want = GLYPHS[k as usize % GLYPHS.len()];
            assert!(
                text.starts_with(want),
                "frame {k}: want {want:?}, got {text:?}"
            );
        }
        // And the row is the same width from one frame to the next.
        let widths: Vec<usize> = (0..GLYPHS.len() as u32)
            .map(|k| row(&at(FRAME * k, "dotfiles"), 80).width())
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
    }

    /// Once the row offers a way out, the offer has to be legible: a long
    /// name is what gives way, then the words around it, never the hint.
    #[test]
    fn a_long_name_gives_way_before_the_cancel_hint() {
        let long = "a".repeat(60);
        let lines = render(30, 3, &at(PATIENCE, &long));
        let last = &lines[2];
        assert!(last.ends_with(CANCEL), "{last:?}");
        assert!(last.width() <= 30, "{last:?}");
        assert!(last.contains("attaching to a"), "{last:?}");

        // Narrower still: the name and its introduction go, the hint stays.
        let lines = render(16, 3, &at(PATIENCE, &long));
        let last = &lines[2];
        assert!(last.ends_with(CANCEL), "{last:?}");
        assert!(!last.contains("attaching"), "{last:?}");
        assert!(
            GLYPHS.contains(&last.trim_start().chars().next().expect("a glyph")),
            "{last:?}"
        );
    }

    /// Before the hint exists the name has the whole row less the glyph and
    /// its introduction, and a wide name is measured in columns.
    #[test]
    fn a_wide_name_is_fitted_by_column() {
        let lines = render(24, 2, &at(GRACE, "日本語のセッション"));
        let last = &lines[1];
        assert!(last.width() <= 24, "{last:?}");
        assert!(last.contains("attaching to 日本"), "{last:?}");
    }

    #[test]
    fn every_tiny_size_survives() {
        for &(w, h) in test_support::TINY_SIZES {
            for elapsed in [Duration::ZERO, GRACE, PATIENCE] {
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

    #[test]
    fn nothing_sets_a_colour() {
        test_support::assert_no_colour(40, 6, |f| draw(f, &at(PATIENCE, "dotfiles")));
    }

    #[test]
    fn nothing_draws_a_border() {
        test_support::assert_no_borders(&render(40, 6, &at(PATIENCE, "dotfiles")));
    }

    /// Dim throughout, like the hint it shares a row with on the other
    /// screens: a status, not a message.
    #[test]
    fn the_row_is_dim() {
        let mut terminal = Terminal::new(TestBackend::new(40, 4)).expect("terminal");
        terminal
            .draw(|f| draw(f, &at(PATIENCE, "dotfiles")))
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        let mut seen = 0;
        for x in 0..40 {
            let cell = &buf[(x, 3)];
            if cell.symbol().trim().is_empty() {
                continue;
            }
            seen += 1;
            assert!(
                cell.modifier.contains(Modifier::DIM),
                "cell {x} on the row is not dim: {:?}",
                cell.symbol()
            );
        }
        assert!(seen > 0, "the row was blank");
    }
}
