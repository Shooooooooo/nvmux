//! Drawing no faster than the terminal reads: frames paced to the terminal.
//!
//! The client draws a frame whenever something changes, and while a scroll
//! slides, sixty and more of them a second, each the whole of a window. It
//! writes them to its pty and goes on; what reads them — nvmux's relay, and
//! past it the terminal, and on Windows a console host and WSL's plumbing
//! between them — reads them as fast as it can. Where that is slower than
//! the client draws, the frames do not go away: they wait, in every buffer
//! between the client and the screen, and every one of them is drawn, in
//! order, each as late as all the ones before it. A scroll is shown as many
//! frames late as the buffers hold, and goes on after the key is let go.
//! Nothing the client can see says so but its writes blocking, which happens
//! only once the first of those buffers, its own pty, is full, and that is
//! long after the rest have filled.
//!
//! So the client asks. After every frame it writes `CSI 5 n` — the status
//! question, which every terminal since the VT100 answers with `CSI 0 n` —
//! and the terminal answers it once it has read everything written before
//! it: the answer says that frame has reached it. A frame whose answer is
//! late is a frame that is queued behind others, and while one is, the
//! client draws nothing more ([`Pacer::holding`]). It goes on taking in what
//! Neovim sends and what is typed; when the answer comes, the next frame it
//! draws is the screen as it is then, not each of the ones it would have
//! drawn on the way. What the terminal falls behind by is a frame or so,
//! however much there is to buffer it — and over a link that a terminal far
//! away is reached by, it drops the frames it could not take in time, rather
//! than sending them all late.
//!
//! # Late
//!
//! Late is said against the quickest any answer has come ([`Pacer::floor`]):
//! the terminal with nothing ahead of the question, and the way there and
//! back — under a millisecond for a terminal on the same machine, the round
//! trip for one at the far end of an ssh. An answer later than that by more
//! than a little ([`slack`]) is one that waited behind other frames. The
//! quickest only ever goes down: an answer can be slowed by what is queued,
//! never sped up, and one that could go up would rise to what is queued
//! whenever the terminal stayed behind.
//!
//! # Answers that never come
//!
//! A question can go unanswered. A kept client is parked while another
//! session is in front, and what it writes is read but goes nowhere (see
//! `crate::pty::Attachment::park`); a terminal can simply not answer. A
//! question left unanswered for [`lost`] is given up on, and so is every
//! question out when the terminal is repainted or resized — how nvmux brings
//! a parked client back to the front — so that a client back in front never
//! waits on questions asked while it was away. Having given up, the client
//! draws freely, with one question out at a time, until an answer says the
//! terminal is answering again.
//!
//! An answer to a question given up on may come after all; it is told from
//! the server's own only by being expected, so for [`STRAYS_FOR`] that many
//! are taken as the client's. Any other `CSI 0 n` goes to the server, as
//! every answer the client did not ask for does.
//!
//! # Leaving
//!
//! An answer still owed when the client goes would be read by whatever reads
//! the terminal next — and the shell, after nvmux, would print it. So a
//! client leaving of its own accord takes its answers in first, for a while
//! ([`super::run`]); one nvmux hangs up has nobody passing it the terminal's
//! bytes any more, and nvmux takes in what that one is owed itself, on its
//! way back to the shell (`crate::pty::Attachment::take_owed_answers`).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The status question: "are you there?", answered `CSI 0 n`.
pub const PROBE: &[u8] = b"\x1b[5n";

/// How much later than the quickest an answer may be before its frame is
/// taken to be queued: a little for a terminal on this machine, where a
/// frame takes a millisecond or two to be read; a quarter of the round trip
/// for one further away, whose round trip wanders by more than that.
fn slack(floor: Duration) -> Duration {
    MIN_SLACK.max(floor / 4)
}

const MIN_SLACK: Duration = Duration::from_millis(4);

/// How long a question goes unanswered before it is given up on.
fn lost(floor: Duration) -> Duration {
    LOST_MIN.max(floor * 4)
}

const LOST_MIN: Duration = Duration::from_secs(1);

/// How long answers to questions given up on are watched for.
pub const STRAYS_FOR: Duration = Duration::from_secs(10);

/// The questions out, and what the answers have said.
#[derive(Debug, Default)]
pub struct Pacer {
    /// When each question not yet answered was asked, oldest first.
    out: VecDeque<Instant>,
    /// The quickest answer yet.
    floor: Option<Duration>,
    /// An answer has come since the client last gave up on its questions:
    /// the terminal is answering, and is paced.
    synced: bool,
    /// Questions given up on whose answers may yet come, and until when.
    strays: usize,
    strays_until: Option<Instant>,
}

impl Pacer {
    pub fn new() -> Self {
        Self::default()
    }

    /// A frame is written `now`: the question to follow it, if one is to.
    /// While the terminal is not known to answer, one at a time.
    pub fn sent(&mut self, now: Instant) -> Option<&'static [u8]> {
        if let (Some(&oldest), Some(floor)) = (self.out.front(), self.floor) {
            if now.saturating_duration_since(oldest) >= lost(floor) {
                self.forget(now);
            }
        } else if self.out.front().is_some_and(|&t| now >= t + LOST_MIN) {
            self.forget(now);
        }
        if !self.synced && !self.out.is_empty() {
            return None;
        }
        self.out.push_back(now);
        Some(PROBE)
    }

    /// A `CSI 0 n` came `now`. Whether it answered the client: otherwise it
    /// is the server's.
    pub fn answered(&mut self, now: Instant) -> bool {
        if let Some(asked) = self.out.pop_front() {
            let took = now.saturating_duration_since(asked);
            self.floor = Some(self.floor.map_or(took, |f| f.min(took)));
            self.synced = true;
            return true;
        }
        if self.strays > 0 && self.strays_until.is_some_and(|until| now < until) {
            self.strays -= 1;
            return true;
        }
        self.strays = 0;
        false
    }

    /// Whether a frame drawn now would queue behind one the terminal has
    /// not reached: the oldest question out is late, and not yet given up.
    pub fn holding(&self, now: Instant) -> bool {
        let (Some(floor), Some(&oldest)) = (self.floor, self.out.front()) else {
            return false;
        };
        let age = now.saturating_duration_since(oldest);
        self.synced && age > floor + slack(floor) && age < lost(floor)
    }

    /// When a hold ends if no answer does: the oldest question is given up
    /// on.
    pub fn until(&self) -> Option<Instant> {
        let floor = self.floor?;
        self.out.front().map(|&oldest| oldest + lost(floor))
    }

    /// Give up on every question out: the terminal has been repainted or
    /// resized, or the questions have gone unanswered too long.
    pub fn forget(&mut self, now: Instant) {
        if !self.out.is_empty() {
            self.strays += self.out.len();
            self.strays_until = Some(now + STRAYS_FOR);
            self.out.clear();
        }
        self.synced = false;
    }

    /// The quickest answer yet.
    pub fn floor(&self) -> Option<Duration> {
        self.floor
    }

    /// How many questions are out: answers the terminal still owes.
    pub fn owed(&self) -> usize {
        self.out.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A pacer that has had one answer, `rtt` after its question: the
    /// terminal is known to answer, that quickly.
    fn synced(t0: Instant, rtt: Duration) -> Pacer {
        let mut p = Pacer::new();
        assert_eq!(p.sent(t0), Some(PROBE));
        assert!(p.answered(t0 + rtt));
        p
    }

    #[test]
    fn nothing_is_held_before_the_terminal_has_answered() {
        let t0 = Instant::now();
        let mut p = Pacer::new();
        assert_eq!(p.sent(t0), Some(PROBE));
        // One question at a time until then.
        assert_eq!(p.sent(t0 + ms(10)), None);
        assert!(!p.holding(t0 + ms(500)));
    }

    #[test]
    fn a_late_answer_holds_the_next_frame_until_it_comes() {
        let t0 = Instant::now();
        let mut p = synced(t0, ms(1));
        let t1 = t0 + ms(100);
        assert_eq!(p.sent(t1), Some(PROBE));
        // Inside the quickest answer and the slack: still drawing.
        assert!(!p.holding(t1 + ms(4)));
        // Past it, the frame is queued: held.
        assert!(p.holding(t1 + ms(6)));
        assert_eq!(p.until(), Some(t1 + LOST_MIN));
        assert!(p.answered(t1 + ms(30)));
        assert!(!p.holding(t1 + ms(30)));
    }

    #[test]
    fn every_frame_asks_once_the_terminal_answers() {
        let t0 = Instant::now();
        let mut p = synced(t0, ms(1));
        let t1 = t0 + ms(100);
        assert_eq!(p.sent(t1), Some(PROBE));
        assert_eq!(p.sent(t1 + ms(2)), Some(PROBE));
        // The first answer is the first frame's: the second is still out,
        // and it is what is late or not.
        assert!(p.answered(t1 + ms(3)));
        assert!(!p.holding(t1 + ms(6)));
        assert!(p.holding(t1 + ms(8)));
    }

    #[test]
    fn the_quickest_answer_never_rises() {
        let t0 = Instant::now();
        let mut p = synced(t0, ms(2));
        let t1 = t0 + ms(100);
        p.sent(t1);
        assert!(p.answered(t1 + ms(50)));
        assert_eq!(p.floor(), Some(ms(2)));
        p.sent(t1 + ms(60));
        assert!(p.answered(t1 + ms(61)));
        assert_eq!(p.floor(), Some(ms(1)));
    }

    #[test]
    fn a_far_terminal_is_given_its_round_trip_and_a_quarter() {
        let t0 = Instant::now();
        let mut p = synced(t0, ms(200));
        let t1 = t0 + ms(1000);
        p.sent(t1);
        assert!(!p.holding(t1 + ms(249)));
        assert!(p.holding(t1 + ms(251)));
    }

    #[test]
    fn a_question_unanswered_too_long_is_given_up_on() {
        let t0 = Instant::now();
        let mut p = synced(t0, ms(1));
        let t1 = t0 + ms(100);
        p.sent(t1);
        assert!(p.holding(t1 + ms(999)));
        assert!(!p.holding(t1 + LOST_MIN));
        // The next frame gives it up, and asks afresh; unpaced until the
        // terminal answers again.
        let t2 = t1 + LOST_MIN;
        assert_eq!(p.sent(t2), Some(PROBE));
        assert_eq!(p.sent(t2 + ms(1)), None);
        assert!(!p.holding(t2 + ms(500)));
        assert!(p.answered(t2 + ms(600)));
        assert_eq!(p.sent(t2 + ms(601)), Some(PROBE));
        assert!(p.holding(t2 + ms(610)));
    }

    #[test]
    fn a_repaint_gives_up_the_questions_out() {
        let t0 = Instant::now();
        let mut p = synced(t0, ms(1));
        let t1 = t0 + ms(100);
        p.sent(t1);
        p.sent(t1 + ms(2));
        assert!(p.holding(t1 + ms(50)));
        p.forget(t1 + ms(50));
        assert!(!p.holding(t1 + ms(50)));
        // Their answers, should they come, are still the client's...
        assert!(p.answered(t1 + ms(60)));
        assert!(p.answered(t1 + ms(61)));
        // ...and no more than that.
        assert!(!p.answered(t1 + ms(62)));
    }

    #[test]
    fn answers_given_up_on_are_watched_for_a_while_only() {
        let t0 = Instant::now();
        let mut p = synced(t0, ms(1));
        let t1 = t0 + ms(100);
        p.sent(t1);
        p.forget(t1);
        assert!(!p.answered(t1 + STRAYS_FOR));
    }

    #[test]
    fn an_answer_nobody_asked_for_is_the_servers() {
        let t0 = Instant::now();
        let mut p = Pacer::new();
        assert!(!p.answered(t0));
    }
}
