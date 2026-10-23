//! Session ids and host tokens. Ids are short because socket paths are (see
//! [`crate::paths`] for the byte budget).

/// RFC 4648 base32, lowercase, no padding: filename- and shell-safe, and never
/// looks like a flag.
const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// Encode 40 bits as 8 base32 characters (no padding case).
fn b32_40(bytes: &[u8; 5]) -> String {
    let n = u64::from(bytes[0]) << 32
        | u64::from(bytes[1]) << 24
        | u64::from(bytes[2]) << 16
        | u64::from(bytes[3]) << 8
        | u64::from(bytes[4]);
    // MSB first, so ids sort like their bit patterns.
    (0..8)
        .map(|i| ALPHABET[((n >> (35 - i * 5)) & 0x1f) as usize] as char)
        .collect()
}

/// Eight random base32 characters (40 bits). The crate's only randomness: used
/// for session ids and the [`crate::proc::Shell`] output-framing secret.
pub fn nonce() -> std::io::Result<String> {
    let mut bytes = [0u8; 5];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    Ok(b32_40(&bytes))
}

/// Ids are read from `<id>.json` on disk and become paths (unlink, kill
/// script, socket connect), so anything malformed like `../..` is rejected.
pub fn is_valid_id(id: &str) -> bool {
    id.len() == ID_LEN && id.bytes().all(|b| ALPHABET.contains(&b))
}

/// Session id length in characters and bytes (the alphabet is ASCII).
pub const ID_LEN: usize = 8;

/// A validated session id that is `Copy`, e.g. for `pty::Target`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Id([u8; ID_LEN]);

impl Id {
    pub fn new(id: &str) -> Option<Self> {
        let bytes: [u8; ID_LEN] = id.as_bytes().try_into().ok()?;
        is_valid_id(id).then_some(Self(bytes))
    }

    pub fn as_str(&self) -> &str {
        // `new` checked it is ASCII.
        std::str::from_utf8(&self.0).unwrap_or_default()
    }
}

/// A stable, filename-safe token for a host, naming the local end of an SSH
/// forward.
///
/// Not `DefaultHasher`, which may change across Rust releases and orphan live
/// sockets. FNV-1a with the 64-bit offset basis, but the multiplier is **not**
/// the FNV-64 prime (`0x100000001b3`; it has an extra nibble). Do not fix it:
/// forwarded socket names on disk depend on it.
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
    fn an_id_is_a_well_formed_one_and_reads_back() {
        let id = Id::new("abcd2345").expect("well formed");
        assert_eq!(id.as_str(), "abcd2345");
        assert_eq!(Id::new("abcd234"), None, "short");
        assert_eq!(Id::new("ABCD2345"), None, "not the alphabet");
        assert_eq!(Id::new("../../x1"), None);
    }

    #[test]
    fn ids_are_eight_chars_from_the_alphabet() {
        for _ in 0..256 {
            let id = nonce().expect("getrandom");
            assert_eq!(id.len(), 8, "id {id:?} is not 8 chars");
            assert!(
                id.bytes().all(|b| ALPHABET.contains(&b)),
                "id {id:?} has characters outside the base32 alphabet"
            );
        }
    }

    #[test]
    fn ids_do_not_obviously_collide() {
        let ids: std::collections::HashSet<_> = (0..1000).filter_map(|_| nonce().ok()).collect();
        assert_eq!(ids.len(), 1000, "duplicate ids from nonce()");
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
            assert!(is_valid_id(&nonce().expect("getrandom")));
        }
        assert!(is_valid_id(&host_token("myhost")));
    }

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

    /// A failure here renames every live session's forwarded socket.
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
