//! The `Ctrl-t` prefix state machine.
//!
//! This sits in the stdin half of the PTY proxy and is the *only* thing that
//! inspects the byte stream on the way to Neovim. The other direction — child
//! to terminal — is never parsed at all, which is what lets bracketed paste,
//! the kitty keyboard protocol, truecolor, undercurl, OSC 52 and DA1/XTGETTCAP
//! round-trips work: the child negotiates directly with the real terminal.
//!
//! Deliberately pure. Timing is the caller's job (it calls [`Prefix::timeout`]
//! when its read times out), so the whole table below is unit-testable without
//! a PTY, a terminal, or a clock.
//!
//! | Sequence           | Action                                              |
//! |--------------------|-----------------------------------------------------|
//! | `C-t` `t`          | back to the picker; the child keeps running         |
//! | `C-t` `d`          | detach: kill the local UI, leave the server running |
//! | `C-t` `c`          | create a new session and attach to it               |
//! | `C-t` `C-t`        | send one literal `C-t` to Neovim                    |
//! | `C-t` *other*      | send `C-t` then that byte                           |
//! | `C-t` then silence | send `C-t`                                          |
//!
//! Note what is *not* here: `Ctrl-z` (0x1a) is not special-cased. It is
//! forwarded like any other byte and Neovim receives it as a key.

/// `Ctrl-t`.
pub const PREFIX: u8 = 0x14;

/// How long to wait for the second byte of a prefix sequence before deciding
/// the user meant a literal `Ctrl-t`.
pub const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Something the proxy must do instead of forwarding bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `C-t t` — suspend the relay and show the picker. The child stays alive.
    Picker,
    /// `C-t d` — terminate the local UI and exit, leaving the server running.
    Detach,
    /// `C-t c` — create a new session and attach to it.
    Create,
}

/// One instruction from the machine, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Write these bytes to the PTY master, verbatim.
    Forward(Vec<u8>),
    /// Do this.
    Act(Action),
}

/// The prefix state machine.
#[derive(Debug, Default)]
pub struct Prefix {
    /// True between seeing `C-t` and resolving what follows it.
    armed: bool,
}

impl Prefix {
    pub fn new() -> Self {
        Self { armed: false }
    }

    /// True if a prefix byte has been swallowed and we are waiting to see what
    /// follows. The caller uses this to decide whether to arm a [`TIMEOUT`].
    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// Feed a chunk of bytes read from the user's terminal.
    ///
    /// Runs of ordinary bytes are coalesced into a single [`Step::Forward`], so
    /// a paste of 8 KB does not become 8192 writes.
    pub fn feed(&mut self, input: &[u8]) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut pending: Vec<u8> = Vec::new();

        for &b in input {
            if self.armed {
                self.armed = false;
                match b {
                    // C-t C-t: one literal prefix byte reaches Neovim.
                    PREFIX => pending.push(PREFIX),
                    b't' => {
                        flush(&mut steps, &mut pending);
                        steps.push(Step::Act(Action::Picker));
                    }
                    b'd' => {
                        flush(&mut steps, &mut pending);
                        steps.push(Step::Act(Action::Detach));
                    }
                    b'c' => {
                        flush(&mut steps, &mut pending);
                        steps.push(Step::Act(Action::Create));
                    }
                    // Anything else was not a command, so the user's original
                    // keystrokes are replayed in order and nothing is eaten.
                    other => {
                        pending.push(PREFIX);
                        pending.push(other);
                    }
                }
            } else if b == PREFIX {
                self.armed = true;
            } else {
                pending.push(b);
            }
        }

        flush(&mut steps, &mut pending);
        steps
    }

    /// Called when no byte arrived within [`TIMEOUT`] of arming.
    ///
    /// Resolves a lone `C-t` into a literal one. Idempotent, so a caller that
    /// fires its timer spuriously does no harm.
    pub fn timeout(&mut self) -> Vec<Step> {
        if self.armed {
            self.armed = false;
            vec![Step::Forward(vec![PREFIX])]
        } else {
            Vec::new()
        }
    }
}

fn flush(steps: &mut Vec<Step>, pending: &mut Vec<u8>) {
    if !pending.is_empty() {
        steps.push(Step::Forward(std::mem::take(pending)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concatenate everything the machine would forward, dropping actions.
    fn forwarded(steps: &[Step]) -> Vec<u8> {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Forward(b) => Some(b.clone()),
                Step::Act(_) => None,
            })
            .flatten()
            .collect()
    }

    fn actions(steps: &[Step]) -> Vec<Action> {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Act(a) => Some(*a),
                Step::Forward(_) => None,
            })
            .collect()
    }

    #[test]
    fn ordinary_bytes_pass_through_untouched() {
        let mut p = Prefix::new();
        let steps = p.feed(b"hello world");
        assert_eq!(forwarded(&steps), b"hello world");
        assert!(actions(&steps).is_empty());
        assert!(!p.is_armed());
    }

    #[test]
    fn a_run_of_bytes_is_one_write() {
        let mut p = Prefix::new();
        let steps = p.feed(b"abcdef");
        assert_eq!(steps.len(), 1, "should coalesce: {steps:?}");
    }

    #[test]
    fn prefix_alone_is_swallowed_and_arms() {
        let mut p = Prefix::new();
        let steps = p.feed(&[PREFIX]);
        assert!(
            forwarded(&steps).is_empty(),
            "prefix must not reach nvim yet"
        );
        assert!(p.is_armed());
    }

    #[test]
    fn doubled_prefix_sends_one_literal() {
        let mut p = Prefix::new();
        let steps = p.feed(&[PREFIX, PREFIX]);
        assert_eq!(forwarded(&steps), vec![PREFIX]);
        assert!(!p.is_armed());
    }

    #[test]
    fn commands_produce_actions_and_no_bytes() {
        for (byte, want) in [
            (b't', Action::Picker),
            (b'd', Action::Detach),
            (b'c', Action::Create),
        ] {
            let mut p = Prefix::new();
            let steps = p.feed(&[PREFIX, byte]);
            assert_eq!(actions(&steps), vec![want], "for {:?}", byte as char);
            assert!(
                forwarded(&steps).is_empty(),
                "command bytes must not reach nvim: {:?}",
                byte as char
            );
            assert!(!p.is_armed());
        }
    }

    #[test]
    fn unknown_command_replays_both_bytes_in_order() {
        let mut p = Prefix::new();
        let steps = p.feed(&[PREFIX, b'x']);
        assert_eq!(forwarded(&steps), vec![PREFIX, b'x']);
        assert!(actions(&steps).is_empty());
    }

    #[test]
    fn timeout_resolves_a_lone_prefix_to_a_literal() {
        let mut p = Prefix::new();
        p.feed(&[PREFIX]);
        assert!(p.is_armed());
        assert_eq!(forwarded(&p.timeout()), vec![PREFIX]);
        assert!(!p.is_armed());
    }

    #[test]
    fn timeout_when_not_armed_does_nothing() {
        let mut p = Prefix::new();
        assert!(p.timeout().is_empty());
        // Idempotent: a spurious second timer must not inject a stray byte.
        p.feed(&[PREFIX]);
        p.timeout();
        assert!(p.timeout().is_empty());
    }

    /// The prefix may be the last byte of one `read()` and the command the
    /// first byte of the next. This is the common case at a human typing speed,
    /// not an edge case.
    #[test]
    fn state_survives_a_chunk_boundary() {
        let mut p = Prefix::new();
        let first = p.feed(b"ab\x14");
        assert_eq!(forwarded(&first), b"ab");
        assert!(p.is_armed());

        let second = p.feed(b"d");
        assert_eq!(actions(&second), vec![Action::Detach]);
        assert!(forwarded(&second).is_empty());
    }

    #[test]
    fn ctrl_z_is_not_special() {
        // 0x1a must reach Neovim as an ordinary key. Whether the server then
        // emits a `suspend` UI event is the child TUI's business, not ours.
        let mut p = Prefix::new();
        let steps = p.feed(&[0x1a]);
        assert_eq!(forwarded(&steps), vec![0x1a]);
        assert!(actions(&steps).is_empty());
    }

    #[test]
    fn ctrl_c_and_ctrl_s_are_not_special() {
        // These only reach us as bytes because the terminal is in raw mode with
        // ISIG off and IXON cleared; the machine itself must not intercept them.
        let mut p = Prefix::new();
        let steps = p.feed(&[0x03, 0x13, 0x1c]);
        assert_eq!(forwarded(&steps), vec![0x03, 0x13, 0x1c]);
    }

    #[test]
    fn escape_sequences_are_never_parsed() {
        let mut p = Prefix::new();
        // A bracketed paste wrapper plus a kitty keyboard query.
        let raw = b"\x1b[200~pasted\x1b[201~\x1b[?u";
        let steps = p.feed(raw);
        assert_eq!(forwarded(&steps), raw);
    }

    #[test]
    fn a_prefix_inside_a_larger_chunk_splits_correctly() {
        let mut p = Prefix::new();
        let steps = p.feed(b"before\x14tafter");
        assert_eq!(forwarded(&steps), b"beforeafter");
        assert_eq!(actions(&steps), vec![Action::Picker]);
        // Ordering matters: the bytes before the command must be written first.
        assert!(matches!(steps[0], Step::Forward(_)));
        assert!(matches!(steps[1], Step::Act(Action::Picker)));
        assert!(matches!(steps[2], Step::Forward(_)));
    }

    #[test]
    fn several_commands_in_one_chunk() {
        let mut p = Prefix::new();
        let steps = p.feed(b"\x14t\x14d\x14c");
        assert_eq!(
            actions(&steps),
            vec![Action::Picker, Action::Detach, Action::Create]
        );
        assert!(forwarded(&steps).is_empty());
    }

    #[test]
    fn triple_prefix_is_a_literal_then_arms_again() {
        let mut p = Prefix::new();
        let steps = p.feed(&[PREFIX, PREFIX, PREFIX]);
        assert_eq!(forwarded(&steps), vec![PREFIX]);
        assert!(p.is_armed(), "the third byte should re-arm");
    }

    #[test]
    fn empty_input_is_a_no_op() {
        let mut p = Prefix::new();
        assert!(p.feed(b"").is_empty());
        p.feed(&[PREFIX]);
        assert!(p.feed(b"").is_empty());
        assert!(p.is_armed(), "an empty read must not disarm");
    }

    /// Every byte that is not a command must survive a prefix unchanged.
    #[test]
    fn exhaustive_second_byte_table() {
        for b in 0u8..=255 {
            let mut p = Prefix::new();
            let steps = p.feed(&[PREFIX, b]);
            match b {
                b't' | b'd' | b'c' => {
                    assert_eq!(actions(&steps).len(), 1, "byte {b:#04x} should act");
                    assert!(forwarded(&steps).is_empty(), "byte {b:#04x} leaked bytes");
                }
                PREFIX => assert_eq!(forwarded(&steps), vec![PREFIX]),
                _ => assert_eq!(
                    forwarded(&steps),
                    vec![PREFIX, b],
                    "byte {b:#04x} must be replayed after the prefix"
                ),
            }
            assert!(!p.is_armed(), "byte {b:#04x} left the machine armed");
        }
    }
}
