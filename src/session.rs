//! Session identity and on-disk metadata.
//!
//! `<id>.sock`, `<id>.json` and `<id>.log` sit together on whichever host runs
//! the nvim process, so attaching from a second machine shows the same names.
//! `<id>` is a random token, never the name.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::SessionError;

/// What a liveness probe concluded about a session. Three states rather than a
/// bool, because "did not answer" and "is not there" must not be confused —
/// see [`crate::rpc`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Liveness {
    /// Answered a deferred RPC call. Definitely serving.
    Alive,
    /// Reachable, but did not finish a deferred call in time. Probably running
    /// something blocking. Never reaped.
    Busy,
    /// Nothing is listening. The socket file, if present, is stale.
    #[default]
    Dead,
}

/// Runtime state, discovered by probing rather than read from disk. Kept out of
/// the JSON entirely.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionState {
    pub liveness: Liveness,
    /// Number of attached UIs, when known.
    pub attached_uis: Option<usize>,
    /// The number the picker shows and `Ctrl-t <n>` selects, resolved by
    /// [`crate::transport::finish_listing`] from the stored [`Session::num`].
    ///
    /// Separate from the stored one on purpose. It is dense and duplicate-free
    /// across one listing — legacy metadata and orphans have no stored number,
    /// and two clients creating at once can store the same one — and it must
    /// never reach disk. `SshTransport::rename_session` writes an in-memory
    /// session straight back through `write_meta.sh`, so a derived number kept
    /// in `Session::num` would be silently persisted by a rename.
    pub num: u32,
}

/// A session, as listed by a [`crate::transport::Transport`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Random 8-character base32 token. Stable for the session's life.
    pub id: String,
    /// Display name. Mutable; carries no identity.
    pub name: String,
    /// Unix epoch seconds, rather than RFC3339, so this file needs no date
    /// library at all.
    pub created: u64,
    /// The nvim server's pid — a hint, not an identity. `$!` from the spawn
    /// script is not reliably nvim's own pid, since `setsid` forks when the
    /// caller is already a process group leader, so this is overwritten with the
    /// server's own answer once it is reachable. Liveness never rests on it:
    /// pids are reused.
    pub pid: u32,
    /// The session's number, assigned once at creation and kept for life, so a
    /// number a user memorised keeps naming the same session. `0` means
    /// unnumbered: metadata written before numbering existed, and orphans.
    /// [`crate::transport::finish_listing`] resolves that into `state.num`.
    #[serde(default)]
    pub num: u32,

    /// Probe results. Never serialised: the on-disk shape stays exactly the
    /// five keys above.
    #[serde(skip)]
    pub state: SessionState,
}

impl Session {
    pub fn new(id: String, name: String, pid: u32, num: u32) -> Self {
        Self {
            id,
            name,
            created: now_secs(),
            pid,
            num,
            state: SessionState::default(),
        }
    }

    pub fn is_alive(&self) -> bool {
        matches!(self.state.liveness, Liveness::Alive | Liveness::Busy)
    }

    /// Parse `<id>.json` contents.
    pub fn from_json(bytes: &[u8], path: &Path) -> Result<Self, SessionError> {
        serde_json::from_slice(bytes).map_err(|source| SessionError::Metadata {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn to_json(&self) -> Result<String, SessionError> {
        serde_json::to_string(self).map_err(|source| SessionError::Metadata {
            path: std::path::PathBuf::from(format!("{}.json", self.id)),
            source,
        })
    }

    /// Write `<id>.json` atomically: temp file plus `rename(2)`, which within
    /// one directory is atomic, so a concurrent listing sees the old name or the
    /// new one and never a half-written file.
    pub fn write_atomic(&self, path: &Path) -> Result<(), SessionError> {
        use std::io::Write;
        let json = self.to_json()?;
        let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
        let write = || -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(json.as_bytes())?;
            f.write_all(b"\n")?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)
        };
        write().map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            SessionError::Metadata {
                path: path.to_path_buf(),
                source: serde_json::Error::io(e),
            }
        })
    }
}

/// Validate a user-supplied session name.
///
/// Names are display metadata and never appear in a path, but they do reach a
/// shell command line and the terminal, so they are checked at the boundary
/// rather than trusted.
pub fn validate_name(name: &str) -> Result<(), SessionError> {
    let invalid = |reason| {
        Err(SessionError::InvalidName {
            name: name.to_string(),
            reason,
        })
    };
    if name.is_empty() {
        return invalid("must not be empty");
    }
    if name.len() > 64 {
        return invalid("must be 64 bytes or fewer");
    }
    if name.trim() != name {
        return invalid("must not start or end with whitespace");
    }
    // Control characters would corrupt the picker's rendering and could smuggle
    // escape sequences into the terminal via a session listing.
    if name.chars().any(|c| c.is_control()) {
        return invalid("must not contain control characters");
    }
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_has_exactly_the_five_documented_keys() {
        let s = Session::new("abcdefgh".into(), "dotfiles".into(), 4242, 3);
        let v: serde_json::Value =
            serde_json::from_str(&s.to_json().expect("serialise")).expect("json");
        let obj = v.as_object().expect("object");
        let mut keys: Vec<_> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["created", "id", "name", "num", "pid"]);
    }

    /// The resolved number is display state, and a rename over ssh writes an
    /// in-memory session straight back to disk — so it must not serialise.
    #[test]
    fn the_resolved_number_never_reaches_disk() {
        let mut s = Session::new("abcdefgh".into(), "x".into(), 1, 2);
        s.state.num = 9;
        let v: serde_json::Value =
            serde_json::from_str(&s.to_json().expect("serialise")).expect("json");
        assert_eq!(v["num"], 2, "the stored number is what is written");
    }

    #[test]
    fn state_never_reaches_disk() {
        let mut s = Session::new("abcdefgh".into(), "x".into(), 1, 1);
        s.state.liveness = Liveness::Alive;
        s.state.attached_uis = Some(3);
        let json = s.to_json().expect("serialise");
        assert!(!json.contains("state"), "runtime state leaked into {json}");
        assert!(
            !json.contains("liveness"),
            "runtime state leaked into {json}"
        );
    }

    #[test]
    fn round_trips_through_json() {
        let s = Session::new("abcdefgh".into(), "api server".into(), 99, 7);
        let json = s.to_json().expect("serialise");
        let back = Session::from_json(json.as_bytes(), Path::new("x.json")).expect("parse");
        assert_eq!(back.id, s.id);
        assert_eq!(back.name, s.name);
        assert_eq!(back.created, s.created);
        assert_eq!(back.pid, s.pid);
        assert_eq!(back.num, s.num);
        // A freshly parsed session has been probed by nobody.
        assert_eq!(back.state, SessionState::default());
    }

    #[test]
    fn parses_metadata_written_by_hand() {
        let raw = br#"{"id":"abcdefgh","name":"scratch","created":1700000000,"pid":1234}"#;
        let s = Session::from_json(raw, Path::new("x.json")).expect("parse");
        assert_eq!(s.name, "scratch");
        assert_eq!(s.created, 1_700_000_000);
        assert_eq!(s.pid, 1234);
        // Written before numbering existed: unnumbered, not a parse failure.
        assert_eq!(s.num, 0);
    }

    #[test]
    fn rejects_truncated_metadata() {
        let raw = br#"{"id":"abcdefgh","name":"scr"#;
        assert!(Session::from_json(raw, Path::new("x.json")).is_err());
    }

    #[test]
    fn names_with_spaces_and_quotes_are_allowed() {
        // These must survive: the spawn path shell-escapes rather than rejecting.
        validate_name("my project").expect("spaces are fine");
        validate_name("it's \"quoted\"").expect("quotes are fine");
        validate_name("a$b`c").expect("shell metacharacters are fine");
        validate_name("日本語").expect("non-ascii is fine");
    }

    #[test]
    fn names_that_would_corrupt_the_display_are_rejected() {
        assert!(validate_name("").is_err());
        assert!(validate_name(" leading").is_err());
        assert!(validate_name("trailing ").is_err());
        assert!(validate_name("two\nlines").is_err());
        assert!(validate_name("esc\x1b[31m").is_err());
        assert!(validate_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("nvmux-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("abcdefgh.json");
        let s = Session::new("abcdefgh".into(), "x".into(), 7, 1);
        s.write_atomic(&path).expect("write");

        let bytes = std::fs::read(&path).expect("read back");
        assert_eq!(Session::from_json(&bytes, &path).expect("parse").name, "x");

        let strays: Vec<_> = std::fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(strays.is_empty(), "left temp files behind: {strays:?}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
