//! What a session drawn by nvmux's own client does with a terminal wider
//! than its windows need, as it stands while nvmux runs: what `[client]
//! centre` says to begin with, and then wherever `<prefix> w` last went (see
//! [`crate::config::Centre`], and [`crate::client::wide`] for what each
//! does). One for every session: a client is started with it, and one coming
//! back to the front is told it, having perhaps been parked through a change.

use std::sync::atomic::{AtomicU8, Ordering};

use crate::config::Centre;

/// Nothing yet: [`current`] is the config's.
const UNSET: u8 = u8::MAX;

static NOW: AtomicU8 = AtomicU8::new(UNSET);

fn code(c: Centre) -> u8 {
    match c {
        Centre::Off => 0,
        Centre::Editor => 1,
        Centre::Windows => 2,
    }
}

fn of(code: u8) -> Option<Centre> {
    match code {
        0 => Some(Centre::Off),
        1 => Some(Centre::Editor),
        2 => Some(Centre::Windows),
        _ => None,
    }
}

/// As it stands.
pub fn current() -> Centre {
    of(NOW.load(Ordering::Relaxed)).unwrap_or_else(|| crate::config::get().client.centre)
}

/// `<prefix> w`: on to the next way, which stands from now on.
pub fn cycle() -> Centre {
    let next = current().next();
    NOW.store(code(next), Ordering::Relaxed);
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The config's to begin with, then round the three and back, a key at a
    /// time. One test, as the state is the process's.
    #[test]
    fn the_key_goes_round_from_the_configs() {
        let first = current();
        assert_eq!(first, crate::config::get().client.centre);
        let seen: Vec<Centre> = (0..3).map(|_| cycle()).collect();
        assert_eq!(seen, [first.next(), first.next().next(), first]);
        assert_eq!(current(), first);
        for c in [Centre::Off, Centre::Editor, Centre::Windows] {
            assert_eq!(of(code(c)), Some(c));
        }
    }
}
