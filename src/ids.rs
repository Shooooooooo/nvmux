//! Session ids and host tokens.
//!
//! Ids are short because socket paths are short — see [`crate::paths`] for the
//! byte budget.

use crate::error::Result;

/// RFC 4648 base32, lowercased, no padding: unambiguous in a filename, safe in
/// a shell word without quoting, and never looks like a flag.
const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// Encode exactly 40 bits (5 bytes) as 8 base32 characters. 40 divides evenly
/// by 5, so there is no padding case to get wrong.
fn b32_40(bytes: &[u8; 5]) -> String {
    let n = u64::from(bytes[0]) << 32
        | u64::from(bytes[1]) << 24
        | u64::from(bytes[2]) << 16
        | u64::from(bytes[3]) << 8
        | u64::from(bytes[4]);
    // Most significant group first, so ids sort the same as their bit patterns.
    (0..8)
        .map(|i| ALPHABET[((n >> (35 - i * 5)) & 0x1f) as usize] as char)
        .collect()
}

/// A fresh random session id.
pub fn new_id() -> Result<String> {
    let mut bytes = [0u8; 5];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    Ok(b32_40(&bytes))
}

/// Reject anything that is not a well-formed session id.
///
/// Ids are also read out of `<id>.json` on disk, and an id becomes a *path*
/// passed to unlink, to a kill script and to a socket connect — so
/// `../../elsewhere/precious` would escape the runtime directory entirely.
pub fn is_valid_id(id: &str) -> bool {
    id.len() == ID_LEN && id.bytes().all(|b| ALPHABET.contains(&b))
}

/// The length of a session id, in characters and bytes (the alphabet is ASCII).
pub const ID_LEN: usize = 8;

/// A short, stable, filename-safe token for a host string, naming the local end
/// of an SSH forward.
///
/// A fixed hash rather than `DefaultHasher`, which is documented as unstable
/// across Rust releases: a toolchain upgrade must not orphan a live session's
/// socket. The shape is FNV-1a's (xor, then multiply, over the bytes), with
/// FNV's 64-bit offset basis — but the multiplier is **not** the FNV-64 prime
/// (`0x100000001b3`): it has an extra nibble, and has had since the first
/// release. It is not being corrected, because every forwarded socket name on
/// disk depends on it; `host_token_is_stable_forever` pins the values.
pub fn host_token(host: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x1000_0000_01b3;
    let mut h = OFFSET;
    for b in host.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(PRIME);
    }
    b32_40(&[
        (h >> 32) as u8,
        (h >> 24) as u8,
        (h >> 16) as u8,
        (h >> 8) as u8,
        h as u8,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_eight_chars_from_the_alphabet() {
        for _ in 0..256 {
            let id = new_id().expect("getrandom");
            assert_eq!(id.len(), 8, "id {id:?} is not 8 chars");
            assert!(
                id.bytes().all(|b| ALPHABET.contains(&b)),
                "id {id:?} has characters outside the base32 alphabet"
            );
        }
    }

    #[test]
    fn ids_do_not_obviously_collide() {
        let ids: std::collections::HashSet<_> = (0..1000).filter_map(|_| new_id().ok()).collect();
        assert_eq!(ids.len(), 1000, "duplicate ids from new_id()");
    }

    #[test]
    fn b32_is_msb_first_and_covers_the_alphabet() {
        assert_eq!(b32_40(&[0, 0, 0, 0, 0]), "aaaaaaaa");
        assert_eq!(b32_40(&[0xff; 5]), "77777777");
        // 0b00000_00001_00010_00011_00100_00101_00110_00111
        assert_eq!(b32_40(&[0x00, 0x44, 0x32, 0x14, 0xc7]), "abcdefgh");
    }

    #[test]
    fn generated_ids_are_valid() {
        for _ in 0..256 {
            assert!(is_valid_id(&new_id().expect("getrandom")));
        }
        assert!(is_valid_id(&host_token("myhost")));
    }

    /// Everything here would become a path if it were accepted.
    #[test]
    fn ids_that_would_escape_the_runtime_directory_are_rejected() {
        for bad in [
            "",
            "short",
            "toolongtobevalid",
            "../../etc",
            "..",
            ".",
            "/etc/passwd",
            "abcdefg/",
            "abcdefg.",
            "ABCDEFGH", // uppercase is outside the alphabet
            "abcdefg1", // 0/1/8/9 are not in base32
            "abcdefg-",
            "abcd efg",
            "abcdefg\n",
            "abcdef\u{0}h",
        ] {
            assert!(!is_valid_id(bad), "{bad:?} should be rejected");
        }
    }

    /// If this test ever fails, every live session's forwarded socket path
    /// changes name. That is the whole reason FNV is spelled out here rather
    /// than delegated to `DefaultHasher`.
    #[test]
    fn host_token_is_stable_forever() {
        assert_eq!(host_token("myhost"), "gtqu2ip5");
        assert_eq!(host_token("user@myhost"), "yiolvso2");
        assert_eq!(host_token(""), "4scceizf");
    }

    #[test]
    fn host_token_distinguishes_similar_hosts() {
        assert_ne!(host_token("myhost"), host_token("myhost "));
        assert_ne!(host_token("a@h"), host_token("b@h"));
    }
}
