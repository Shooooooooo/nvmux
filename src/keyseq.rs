//! Recognising the prefix chord in a terminal's escape sequences.
//!
//! Neovim's TUI (and nvmux's own client, see [`crate::client`]) requests the
//! kitty keyboard protocol if the terminal answers its `CSI ? u` query
//! (`CSI > 1 u`; `CSI > 3 u` from Neovim 0.12, adding repeat and release
//! events), else xterm's `modifyOtherKeys` (`CSI > 4 ; 2 m`). nvmux relays
//! that negotiation untouched (see [`crate::pty`]), so stdin may carry any of
//! three spellings:
//!
//! ```text
//! Ctrl-Space, legacy:          0x00
//! Ctrl-Space, kitty protocol:  ESC [ 32 ; 5 u       32 = ' ', 5 = 1 + ctrl
//! Ctrl-Space, modifyOtherKeys: ESC [ 27 ; 5 ; 32 ~
//! ```
//!
//! A remapped `Ctrl-<letter>` prefix uses its own code: `Ctrl-t` is `0x14`,
//! `ESC [ 116 ; 5 u`, `ESC [ 27 ; 5 ; 116 ~`. Windows Terminal (1.25+), kitty,
//! Ghostty, foot, Alacritty, iTerm2 and Rio use kitty; xterm, and WezTerm
//! without `enable_kitty_keyboard`, use `modifyOtherKeys`. All three must be
//! accepted, or `<prefix> d` reaches the editor as a pending delete.
//!
//! [`classify`] only decides whether a sequence is the prefix (press or
//! repeat), a key release (which must not count as the next key; the
//! terminal sends one for the swallowed prefix), a terminal reply (which can
//! land between a prefix and its command, as the editor queries on every
//! start and resume), or something else. Nothing is rewritten.
//!
//! Replies are `CSI` sequences starting with a private marker (`?`, `>`,
//! `=`), which no key has: kitty and `modifyOtherKeys` answers, DA1, DA2,
//! DECRPM. String replies (DCS, OSC) pass through as ordinary input.
//!
//! # The kitty form
//!
//! `CSI key[:shifted[:base]] ; modifiers[:event] [; text] u`. `key` is the
//! unshifted codepoint (lowercase for letters); `modifiers` is 1 + a bitmask
//! (shift 1, alt 2, ctrl 4, super 8, hyper 16, meta 32, caps lock 64, num lock
//! 128); `event` is 1 press (default), 2 repeat, 3 release. Lock bits may be
//! reported for a chord and are ignored; any other extra modifier makes a
//! different chord.
//!
//! # The xterm form
//!
//! `CSI 27 ; modifiers ; key ~`, same modifier arithmetic (shift 1, alt 2,
//! ctrl 4, meta 8), no event types.

pub const ESC: u8 = 0x1b;

/// Longest sequence still considered a key report or reply. A chord with
/// every field is ~20 bytes; a long DA1 reply ~40.
const MAX_LEN: usize = 64;

const CTRL: u32 = 4;
const CAPS_LOCK: u32 = 64;
const NUM_LOCK: u32 = 128;

/// What a run of bytes beginning with [`ESC`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sequence {
    /// Incomplete; more bytes could make it any of the others.
    Partial,
    /// The prefix chord, pressed or repeated.
    Prefix,
    /// Release of the key with this code; not a keystroke.
    Release(u32),
    /// A terminal reply to an editor query; not a keystroke.
    Reply,
    /// The first `n` bytes (at least 1) are a non-prefix key or non-key
    /// sequence; later bytes are ordinary input. A lone `ESC` followed by a
    /// byte that cannot continue it is `Other(1)`.
    Other(usize),
}

/// Classify `seq` (starting with [`ESC`]) against the `Ctrl` chord whose key
/// code is `code` (see `keys::prefix_code`).
///
/// Call again after each byte: it returns [`Sequence::Partial`] until the
/// deciding byte arrives.
pub fn classify(seq: &[u8], code: u8) -> Sequence {
    debug_assert_eq!(seq.first(), Some(&ESC), "not an escape sequence");
    match seq.get(1) {
        None => return Sequence::Partial,
        Some(b'[') => {}
        // Escape, legacy Alt, or an SS3 key. Only CSI can spell the prefix,
        // so the following bytes are handled on their own.
        Some(_) => return Sequence::Other(1),
    }

    // A private marker first means a reply; no key report starts with one.
    let reply = matches!(seq.get(2), Some(b'?' | b'>' | b'='));
    for (i, &b) in seq.iter().enumerate().skip(2) {
        match b {
            // Final byte: the sequence is complete.
            0x40..=0x7e if reply => return Sequence::Reply,
            0x40..=0x7e => {
                return complete(&seq[2..i], b, code).unwrap_or(Sequence::Other(i + 1));
            }
            b'?' | b'>' | b'=' if i == 2 => {}
            b'0'..=b'9' => {}
            b';' | b':' if i > 2 => {}
            // Intermediate byte, as in DECRPM's `$`.
            0x20..=0x2f if reply => {}
            // A leading separator or any other parameter/intermediate byte:
            // neither key nor reply (e.g. a mouse event). Its remaining bytes
            // are plain text.
            0x20..=0x3f => return Sequence::Other(i + 1),
            // Control byte, `DEL` or UTF-8 lead byte: the sequence ended
            // before it.
            _ => return Sequence::Other(i),
        }
    }

    if seq.len() >= MAX_LEN {
        Sequence::Other(seq.len())
    } else {
        Sequence::Partial
    }
}

/// Decide a complete sequence; `params` is everything between `ESC [` and
/// the final byte. `None` means some other key or none.
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

fn subfield(field: &[u8], n: usize) -> Option<&[u8]> {
    field.split(|&b| b == b':').nth(n)
}

/// Parse 1 to 9 ASCII digits; nine is enough for any key report and cannot
/// overflow.
fn number(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() || digits.len() > 9 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    digits.iter().try_fold(0u32, |n, &d| {
        n.checked_mul(10)?.checked_add(u32::from(d - b'0'))
    })
}

/// Ctrl and nothing else, ignoring lock keys.
fn ctrl_only(modifiers: u32) -> bool {
    modifiers >= 1 && (modifiers - 1) & !(CAPS_LOCK | NUM_LOCK) == CTRL
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default prefix's key code; `T` is a remapped `Ctrl-t`'s.
    const SPACE: u8 = b' ';
    const T: u8 = b't';

    /// Lets the machine hold a sequence split across reads.
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
        // A missing modifier field is a plain key.
        assert_eq!(classify(b"\x1b[32u", SPACE), Sequence::Other(5));
        // Ctrl-Shift-Space, Ctrl-Alt-Space, Ctrl-Super-Space: not the prefix.
        for seq in [&b"\x1b[32;6u"[..], b"\x1b[32;7u", b"\x1b[32;13u"] {
            assert_eq!(classify(seq, SPACE), Sequence::Other(seq.len()));
        }
    }

    #[test]
    fn a_release_event_names_its_key() {
        assert_eq!(classify(b"\x1b[32;5:3u", SPACE), Sequence::Release(32));
        assert_eq!(classify(b"\x1b[32;1:3u", SPACE), Sequence::Release(32));
        assert_eq!(classify(b"\x1b[100;1:3u", SPACE), Sequence::Release(100));
        assert_eq!(classify(b"\x1b[27;1:3u", SPACE), Sequence::Release(27));
        // Unknown event type.
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
        // The code follows the configured prefix.
        assert_eq!(classify(b"\x1b[116;5u", T), Sequence::Prefix);
        assert_eq!(classify(b"\x1b[27;5;116~", T), Sequence::Prefix);
        assert_eq!(classify(b"\x1b[32;5u", T), Sequence::Other(7));
        // The protocol never sends an uppercase code for the chord.
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

    #[test]
    fn an_escape_followed_by_anything_but_a_bracket_stands_alone() {
        assert_eq!(classify(b"\x1b", SPACE), Sequence::Partial);
        for b in [b'x', b':', 0x00, ESC, b'O', 0x7f, 0xc3] {
            assert_eq!(classify(&[ESC, b], SPACE), Sequence::Other(1), "{b:#04x}");
        }
    }

    /// A non-prefix key is decided at its final byte; a non-key sequence at
    /// the first byte that rules out a key report.
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

    /// Kitty query reply, DA1 (short and long), DA2, DECRPM.
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

    /// A prefix byte typed after a broken sequence must still be the prefix.
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
        // Not before reaching `MAX_LEN`.
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
