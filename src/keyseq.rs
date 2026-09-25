//! How a terminal spells a key as an escape sequence, and whether that key is
//! the prefix.
//!
//! In its plainest form a `Ctrl` chord is one control byte, and for a long
//! time that was the only spelling [`crate::keys`] had to know. But
//! Neovim's TUI asks every terminal for a richer keyboard encoding when it
//! starts: the kitty keyboard protocol where the terminal answers its
//! `CSI ? u` query (`CSI > 1 u`; `CSI > 3 u` from Neovim 0.12, which asks for
//! key-repeat and key-release reports too), and xterm's `modifyOtherKeys`
//! (`CSI > 4 ; 2 m`) otherwise. A terminal that grants either one then spells
//! the same chord as an escape sequence — and since the `--remote-ui` client
//! runs that TUI, and nvmux relays its negotiation untouched (see
//! [`crate::pty`]), the terminal's spelling is what arrives on nvmux's stdin:
//!
//! ```text
//! Ctrl-Space, legacy:          0x00
//! Ctrl-Space, kitty protocol:  ESC [ 32 ; 5 u       32 = ' ', 5 = 1 + ctrl
//! Ctrl-Space, modifyOtherKeys: ESC [ 27 ; 5 ; 32 ~
//! ```
//!
//! A remapped `Ctrl-<letter>` prefix is spelled the same three ways under its
//! own code: `Ctrl-t` is `0x14`, `ESC [ 116 ; 5 u`, `ESC [ 27 ; 5 ; 116 ~`.
//!
//! Windows Terminal (from 1.25), kitty, Ghostty, foot, Alacritty, iTerm2 and
//! Rio all speak the first; xterm, and WezTerm unless `enable_kitty_keyboard`
//! is set, the second; and a terminal that speaks neither keeps sending the
//! byte. The prefix machine has to accept all three, or `<prefix> d` reaches
//! the editor as the prefix chord and a pending delete.
//!
//! [`classify`] is deliberately narrow. It decides whether a sequence *is the
//! prefix* (pressed or held down); whether it reports a key being *released* —
//! which the machine must not mistake for the next key, since a terminal asked
//! for release reports sends one for the very key the machine swallowed; whether
//! it is the terminal's *reply* to one of the editor's queries — not a
//! keystroke either, and one that can land between a prefix and its command,
//! because the editor queries at every start and resume; or whether it is
//! something else. No sequence is decoded any further than that, and none is
//! ever rewritten.
//!
//! Replies are the `CSI` sequences with a private marker (`?`, `>` or `=`)
//! first, which no key ever has: the kitty and `modifyOtherKeys` answers, DA1,
//! DA2 and DECRPM. Replies in string form (DCS, OSC) are not recognised; they
//! go through as ordinary input.
//!
//! # The kitty form
//!
//! `CSI key[:shifted[:base]] ; modifiers[:event] [; text] u`. `key` is the
//! unshifted codepoint — 32 for the space bar, and always lowercase for a
//! letter; `modifiers` is one plus a bitmask (shift 1, alt 2, ctrl 4, super 8,
//! hyper 16, meta 32, caps lock 64, num lock 128); `event` is 1 for a press,
//! 2 for a repeat and 3 for a release,
//! and is omitted for a press. The protocol withholds the lock bits only from
//! keys that produce text, and a chord produces none, so a terminal with Caps
//! Lock on may well report them; they carry no meaning for the chord, so they
//! are ignored rather than refused. Any other modifier bit makes it a
//! different chord: `Ctrl-Shift-Space` is not the prefix.
//!
//! # The xterm form
//!
//! `CSI 27 ; modifiers ; key ~`, with the same modifier arithmetic (shift 1,
//! alt 2, ctrl 4, meta 8) and no event types.

/// The byte every escape sequence starts with.
pub const ESC: u8 = 0x1b;

/// Longer than this and it is not a key report or a reply, whatever it is. The
/// longest spelling of a chord — every optional field present — is about
/// twenty bytes; a DA1 reply from a terminal proud of its features runs to
/// forty.
const MAX_LEN: usize = 64;

const CTRL: u32 = 4;
const CAPS_LOCK: u32 = 64;
const NUM_LOCK: u32 = 128;

/// What a run of bytes beginning with [`ESC`] has turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sequence {
    /// Not finished: more bytes could still make it any of the others.
    Partial,
    /// A complete report of the prefix chord being pressed or auto-repeated.
    Prefix,
    /// A complete report of the key with this code being released. Not a
    /// keystroke, whatever the key.
    Release(u32),
    /// A complete reply from the terminal to one of the editor's queries. Not
    /// a keystroke either.
    Reply,
    /// The first `n` bytes are a whole key that is not the prefix — or a
    /// sequence that is no key report at all — and any byte after them is
    /// ordinary input to be handled afresh. `n` is at least 1: a lone `ESC`
    /// followed by something that cannot continue it is the Escape key.
    Other(usize),
}

/// Classify `seq`, which must start with [`ESC`], as a spelling of the `Ctrl`
/// chord a terminal reports under the key code `code` — see
/// `keys::prefix_code`.
///
/// Meant to be called again after every byte: the answer for a prefix of a
/// sequence is [`Sequence::Partial`] until the byte that decides it arrives, and
/// a decision never changes with more bytes because the decided sequence is
/// consumed.
pub fn classify(seq: &[u8], code: u8) -> Sequence {
    debug_assert_eq!(seq.first(), Some(&ESC), "not an escape sequence");
    match seq.get(1) {
        None => return Sequence::Partial,
        Some(b'[') => {}
        // The Escape key, the ESC that spells Alt in a legacy chord, or the
        // start of an `ESC O` (SS3) key. Only a CSI sequence can spell the
        // prefix, so the bytes after this one stand on their own, and go
        // through as they are.
        Some(_) => return Sequence::Other(1),
    }

    // A private marker first is a reply; both spellings of a key start with
    // a number instead.
    let reply = matches!(seq.get(2), Some(b'?' | b'>' | b'='));
    for (i, &b) in seq.iter().enumerate().skip(2) {
        match b {
            // A final byte: the sequence is complete, and this is its end
            // whatever it says.
            0x40..=0x7e if reply => return Sequence::Reply,
            0x40..=0x7e => {
                return complete(&seq[2..i], b, code).unwrap_or(Sequence::Other(i + 1));
            }
            b'?' | b'>' | b'=' if i == 2 => {}
            b'0'..=b'9' => {}
            b';' | b':' if i > 2 => {}
            // An intermediate byte, as in a DECRPM reply's `$`.
            0x20..=0x2f if reply => {}
            // A separator first, or any other parameter or intermediate byte,
            // means a sequence that is neither a key report nor a reply — a
            // mouse event, say. It is passed on as it stands, and its
            // remaining bytes are plain text.
            0x20..=0x3f => return Sequence::Other(i + 1),
            // A byte no escape sequence contains: a control byte, `DEL`, or the
            // start of a multi-byte character. The sequence ended before it.
            _ => return Sequence::Other(i),
        }
    }

    if seq.len() >= MAX_LEN {
        Sequence::Other(seq.len())
    } else {
        Sequence::Partial
    }
}

/// Decide a complete sequence: `params` is everything between `ESC [` and the
/// final byte. `None` means it is some other key or none.
fn complete(params: &[u8], fin: u8, code: u8) -> Option<Sequence> {
    match fin {
        b'u' => kitty(params, code),
        b'~' => xterm(params, code),
        _ => None,
    }
}

/// `key[:shifted[:base]] ; modifiers[:event] [; text]`.
fn kitty(params: &[u8], code: u8) -> Option<Sequence> {
    let mut fields = params.split(|&b| b == b';');
    let key = fields.next()?;
    let reported = number(subfield(key, 0)?)?;
    let (modifiers, event) = match fields.next() {
        None => (1, 1),
        Some(f) => {
            let modifiers = number(subfield(f, 0)?)?;
            let event = match subfield(f, 1) {
                None => 1,
                Some(e) => number(e)?,
            };
            (modifiers, event)
        }
    };
    match event {
        3 => Some(Sequence::Release(reported)),
        1 | 2 => (reported == u32::from(code) && ctrl_only(modifiers)).then_some(Sequence::Prefix),
        _ => None,
    }
}

/// `27 ; modifiers ; key`.
fn xterm(params: &[u8], code: u8) -> Option<Sequence> {
    let mut fields = params
        .split(|&b| b == b';')
        .map(|f| subfield(f, 0).and_then(number));
    let (Some(Some(27)), Some(Some(modifiers)), Some(Some(reported))) =
        (fields.next(), fields.next(), fields.next())
    else {
        return None;
    };
    (reported == u32::from(code) && ctrl_only(modifiers)).then_some(Sequence::Prefix)
}

/// The `n`th colon-separated part of a field.
fn subfield(field: &[u8], n: usize) -> Option<&[u8]> {
    field.split(|&b| b == b':').nth(n)
}

/// A non-empty run of ASCII digits as a number. Nine digits at most, which is
/// more than any key report carries and never overflows.
fn number(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() || digits.len() > 9 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    digits.iter().try_fold(0u32, |n, &d| {
        n.checked_mul(10)?.checked_add(u32::from(d - b'0'))
    })
}

/// Ctrl and nothing else, lock keys aside.
fn ctrl_only(modifiers: u32) -> bool {
    modifiers >= 1 && (modifiers - 1) & !(CAPS_LOCK | NUM_LOCK) == CTRL
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default prefix's key code; `T` is a remapped `Ctrl-t`'s.
    const SPACE: u8 = b' ';
    const T: u8 = b't';

    /// Every proper prefix of a spelling is undecided, and the whole of it is
    /// the prefix. This is what lets the machine hold a sequence that a read
    /// boundary has cut in two.
    #[test]
    fn a_spelling_is_partial_until_its_last_byte() {
        for whole in [
            &b"\x1b[32;5u"[..],
            b"\x1b[32;5:1u",
            b"\x1b[32;5:2u",
            b"\x1b[32:32;5u",
            b"\x1b[32;133u",
            b"\x1b[27;5;32~",
        ] {
            for n in 1..whole.len() {
                assert_eq!(
                    classify(&whole[..n], SPACE),
                    Sequence::Partial,
                    "{:?} cut after {n} bytes",
                    String::from_utf8_lossy(whole)
                );
            }
            assert_eq!(
                classify(whole, SPACE),
                Sequence::Prefix,
                "{:?}",
                String::from_utf8_lossy(whole)
            );
        }
    }

    /// The lock keys are ignored, every other modifier makes a different chord,
    /// and a missing modifier field is a plain key.
    #[test]
    fn only_ctrl_and_the_lock_keys_spell_the_prefix() {
        for m in 1u32..=256 {
            let bits = m - 1;
            let seq = format!("\x1b[32;{m}u");
            let got = classify(seq.as_bytes(), SPACE);
            if bits & !(CAPS_LOCK | NUM_LOCK) == CTRL {
                assert_eq!(got, Sequence::Prefix, "modifiers {m}");
            } else {
                assert_eq!(got, Sequence::Other(seq.len()), "modifiers {m}");
            }
        }
        assert_eq!(classify(b"\x1b[32u", SPACE), Sequence::Other(5));
        // Ctrl-Shift-Space, Ctrl-Alt-Space, Ctrl-Super-Space: not the prefix.
        for seq in [&b"\x1b[32;6u"[..], b"\x1b[32;7u", b"\x1b[32;13u"] {
            assert_eq!(classify(seq, SPACE), Sequence::Other(seq.len()));
        }
    }

    /// A release is a release whatever the key — the prefix's own, another
    /// key's, an unmodified letter's — and it says which key.
    #[test]
    fn a_release_event_names_its_key() {
        assert_eq!(classify(b"\x1b[32;5:3u", SPACE), Sequence::Release(32));
        assert_eq!(classify(b"\x1b[32;1:3u", SPACE), Sequence::Release(32));
        assert_eq!(classify(b"\x1b[100;1:3u", SPACE), Sequence::Release(100));
        assert_eq!(classify(b"\x1b[27;1:3u", SPACE), Sequence::Release(27));
        // An event type that is neither press, repeat nor release is nothing.
        assert_eq!(classify(b"\x1b[32;5:4u", SPACE), Sequence::Other(9));
    }

    #[test]
    fn another_key_is_another_key() {
        // Ctrl-d, Ctrl-t, Escape, Ctrl-Escape.
        for seq in [
            &b"\x1b[100;5u"[..],
            b"\x1b[116;5u",
            b"\x1b[27u",
            b"\x1b[27;5u",
        ] {
            assert_eq!(classify(seq, SPACE), Sequence::Other(seq.len()), "{seq:?}");
        }
        // And the code follows the machine's prefix, not the default: for a
        // remapped Ctrl-t the letter's code is the prefix and Space's is not.
        assert_eq!(classify(b"\x1b[116;5u", T), Sequence::Prefix);
        assert_eq!(classify(b"\x1b[27;5;116~", T), Sequence::Prefix);
        assert_eq!(classify(b"\x1b[32;5u", T), Sequence::Other(7));
        // An uppercase code is not what the protocol sends for the chord.
        assert_eq!(classify(b"\x1b[84;5u", T), Sequence::Other(7));
    }

    #[test]
    fn the_xterm_form_needs_all_three_numbers_in_order() {
        assert_eq!(classify(b"\x1b[27;5;32~", SPACE), Sequence::Prefix);
        for seq in [
            &b"\x1b[27;5~"[..],
            b"\x1b[32;5;27~",
            b"\x1b[27;6;32~",
            b"\x1b[28;5;32~",
            b"\x1b[27;5;32u",
        ] {
            assert_eq!(classify(seq, SPACE), Sequence::Other(seq.len()), "{seq:?}");
        }
    }

    /// A lone `ESC` and a byte that cannot continue a CSI sequence: the `ESC`
    /// is the Escape key, or Alt, or the start of an `ESC O` key — it does
    /// not matter which, since none of them can be the prefix and all of them
    /// go through byte by byte — and that byte is handled on its own.
    #[test]
    fn an_escape_followed_by_anything_but_a_bracket_stands_alone() {
        assert_eq!(classify(b"\x1b", SPACE), Sequence::Partial);
        for b in [b'x', b':', 0x00, ESC, b'O', 0x7f, 0xc3] {
            assert_eq!(classify(&[ESC, b], SPACE), Sequence::Other(1), "{b:#04x}");
        }
    }

    /// A key report that is not the prefix is a whole unit, decided at its
    /// final byte; a sequence that cannot be a key report at all is decided
    /// at the parameter byte that gives it away, and what has arrived by then
    /// is the unit.
    #[test]
    fn other_sequences_are_decided_as_soon_as_they_can_be() {
        for (seq, unit) in [
            (&b"\x1b[A"[..], 3),      // up
            (b"\x1b[1;5A", 6),        // ctrl-up
            (b"\x1b[200~", 6),        // bracketed paste begins
            (b"\x1b[15~", 5),         // F5
            (b"\x1b[I", 3),           // focus in
            (b"\x1b[27;5;32;9u", 12), // nonsense
            (b"\x1b[<35;10;20M", 3),  // a mouse report
            (b"\x1b[;", 3),           // a separator first
            (b"\x1b[1 q", 4),         // an intermediate byte
        ] {
            assert_eq!(
                classify(seq, SPACE),
                Sequence::Other(unit),
                "{:?}",
                String::from_utf8_lossy(seq)
            );
        }
    }

    /// The terminal's answers to the editor's queries are whole units, and
    /// not keys: the reply to the kitty query, DA1 (short and long), DA2,
    /// DECRPM. Every proper prefix of one is still undecided.
    #[test]
    fn a_terminal_reply_is_recognised_whole() {
        for reply in [
            &b"\x1b[?0u"[..],
            b"\x1b[?62;22c",
            b"\x1b[?64;1;2;6;9;15;16;17;18;21;22;28;29c",
            b"\x1b[>0;95;0c",
            b"\x1b[?2026;2$y",
            b"\x1b[=0c",
        ] {
            for n in 1..reply.len() {
                assert_eq!(
                    classify(&reply[..n], SPACE),
                    Sequence::Partial,
                    "{:?} cut after {n} bytes",
                    String::from_utf8_lossy(reply)
                );
            }
            assert_eq!(
                classify(reply, SPACE),
                Sequence::Reply,
                "{:?}",
                String::from_utf8_lossy(reply)
            );
        }
        // A control byte still ends a reply early, like any sequence.
        assert_eq!(classify(b"\x1b[?6\x14", SPACE), Sequence::Other(4));
    }

    /// A control byte in the middle ends the sequence before itself: the
    /// prefix byte typed after a broken sequence must still be the prefix.
    #[test]
    fn a_byte_that_cannot_belong_ends_the_sequence_before_itself() {
        assert_eq!(classify(b"\x1b[11\x00", SPACE), Sequence::Other(4));
        assert_eq!(classify(b"\x1b[32;5\x1b", SPACE), Sequence::Other(6));
        assert_eq!(classify(b"\x1b[\x7f", SPACE), Sequence::Other(2));
    }

    #[test]
    fn a_sequence_that_never_ends_is_given_up_on() {
        let mut seq = b"\x1b[".to_vec();
        seq.extend(std::iter::repeat_n(b'1', MAX_LEN));
        assert_eq!(classify(&seq, SPACE), Sequence::Other(seq.len()));
        // But it takes the whole allowance to get there.
        assert_eq!(classify(&seq[..MAX_LEN - 1], SPACE), Sequence::Partial);
    }

    #[test]
    fn numbers_are_strict() {
        assert_eq!(number(b"116"), Some(116));
        assert_eq!(number(b""), None);
        assert_eq!(number(b"1a"), None);
        assert_eq!(number(b"4294967295"), None, "ten digits is too many");
        // An absurd modifier number must not panic or wrap into Ctrl.
        assert_eq!(classify(b"\x1b[32;999999999u", SPACE), Sequence::Other(15));
    }
}
