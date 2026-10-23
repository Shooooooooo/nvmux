//! Session identity and on-disk metadata.
//!
//! `<id>.sock`, `<id>.json` and `<id>.log` sit together on whichever host runs
//! the nvim process, so attaching from a second machine shows the same names.
//! `<id>` is a random token, never the name.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::SessionError;

/// Result of a liveness probe. Not a bool: "did not answer" must not be
/// confused with "is not there".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Liveness {
    /// Answered a deferred RPC call.
    Alive,
    /// Reachable but not serving deferred calls: running something blocking,
    /// or waiting at a prompt (which [`crate::rpc::probe`] detects at once via
    /// the mode). Never reaped.
    Busy,
    /// Nothing is listening; any socket file is stale.
    #[default]
    Dead,
}

/// Runtime state from probing; never serialised.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionState {
    pub liveness: Liveness,
    /// The number the picker shows and `<prefix> <n>` selects: the 1-based
    /// position in the listing, set by `transport::finish_listing` after
    /// sorting by the stored [`Session::num`]. Dense and unique even when
    /// stored ranks are missing or duplicated, and renumbers after a kill.
    ///
    /// Kept apart from `Session::num` because `SshTransport::rename_session`
    /// writes the in-memory session back to disk.
    pub num: u32,
}

/// A session, as listed by a [`crate::transport::Transport`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Random 8-character base32 token, stable for the session's life.
    pub id: String,
    /// Display name only; carries no identity.
    pub name: String,
    /// Unix epoch seconds.
    pub created: u64,
    /// The nvim server's pid: a hint, not an identity (pids are reused). `$!`
    /// from the spawn script may not be nvim's pid, since `setsid` forks when
    /// the caller leads a process group, so it is replaced by the server's own
    /// answer once reachable.
    pub pid: u32,
    /// Rank: orders the listing only. The displayed number is
    /// [`SessionState::num`]. Set once at creation (one past the host's
    /// highest) and changed only by a picker reorder, so kills leave holes.
    /// `0` means unranked (legacy metadata, orphans) and sorts last.
    #[serde(default)]
    pub num: u32,
    /// The launch command as [`crate::launch`] validated it, `{sock}` still in
    /// it. A record only: never re-run or shown. Empty for legacy metadata and
    /// orphans.
    #[serde(default)]
    pub command: String,
    /// Absolute start directory on the session host. A record only, like
    /// `command`; empty for legacy metadata and orphans.
    #[serde(default)]
    pub directory: String,

    /// Not serialised: the on-disk shape is exactly the seven keys above.
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
            command: String::new(),
            directory: String::new(),
            state: SessionState::default(),
        }
    }

    /// Not in `new`: orphans and sessions rebuilt from a listing have none.
    pub fn launched_with(mut self, command: &str) -> Self {
        self.command = command.to_string();
        self
    }

    pub fn started_in(mut self, directory: &str) -> Self {
        self.directory = directory.to_string();
        self
    }

    /// Parse `<id>.json` contents.
    pub fn from_json(bytes: &[u8], path: &Path) -> Result<Self, SessionError> {
        serde_json::from_slice(bytes).map_err(|error| SessionError::Metadata {
            path: path.to_path_buf(),
            error,
        })
    }

    pub fn to_json(&self) -> Result<String, SessionError> {
        serde_json::to_string(self).map_err(|error| SessionError::Metadata {
            path: std::path::PathBuf::from(format!("{}.json", self.id)),
            error,
        })
    }

    /// Write `<id>.json` via [`crate::paths::write_atomic`], so a concurrent
    /// listing never sees a half-written file.
    pub fn write_atomic(&self, path: &Path) -> Result<(), SessionError> {
        let mut json = self.to_json()?;
        json.push('\n');
        crate::paths::write_atomic(path, json.as_bytes()).map_err(|error| SessionError::Metadata {
            path: path.to_path_buf(),
            error: serde_json::Error::io(error),
        })
    }
}

/// Validate a user-supplied session name. Names never appear in a path, but
/// they reach a shell command line and the terminal.
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
    // Control characters could inject escape sequences into the terminal;
    // invisible formatting characters can reorder or hide listed text.
    if name.chars().any(is_unrenderable) {
        return invalid("must not contain control or invisible formatting characters");
    }
    Ok(())
}

/// Validate a user-supplied working directory and expand a leading `~` with
/// [`expand_tilde`] (shared with the prompt's completion, so both agree).
///
/// Returns an absolute path, so `scripts/spawn.sh` needs no shell to resolve
/// it. `home` is the session host's home: the local `$HOME`, or what
/// `scripts/hello.sh` reported for a remote host.
pub fn validate_directory(directory: &str, home: &str) -> Result<String, SessionError> {
    let invalid = |reason| {
        Err(SessionError::InvalidDirectory {
            directory: directory.to_string(),
            reason,
        })
    };

    let directory = directory.trim();
    if directory.is_empty() {
        return invalid("must not be empty");
    }
    // PATH_MAX on Linux; refuses a pasted runaway here rather than at execve.
    if directory.len() > 4096 {
        return invalid("must be 4096 bytes or fewer");
    }
    // Same rule as names: this is drawn on the prompt.
    if directory.chars().any(is_unrenderable) {
        return invalid("must not contain control or invisible formatting characters");
    }

    let expanded = match expand_tilde(directory, home) {
        Ok(expanded) => expanded,
        Err(reason) => return invalid(reason),
    };

    if !expanded.starts_with('/') {
        return invalid(
            "must be an absolute path — a relative one would be resolved against \
             whatever directory the session host's shell happened to be in",
        );
    }
    Ok(expanded)
}

/// Expand a leading `~` against the session host's `home` (not ours: over
/// ssh the paths differ). The only expansion nvmux does: `~` and `~/…` only,
/// no `~user`, globs or `$VAR`.
///
/// The `Err` is a plain reason: [`validate_directory`] reports it, while
/// [`crate::ui::complete`] ignores it (a typed `~r` may be `~root` half done).
pub fn expand_tilde(path: &str, home: &str) -> Result<String, &'static str> {
    match path.strip_prefix('~') {
        // Only the host can resolve `~user`; passing it through would name a
        // literal `~user` directory.
        Some(rest) if !rest.is_empty() && !rest.starts_with('/') => {
            Err("~user is not expanded — spell the path out")
        }
        Some(_) if home.is_empty() => {
            Err("~ cannot be expanded: the host reported no home directory")
        }
        Some(rest) => Ok(format!("{}{rest}", home.trim_end_matches('/'))),
        None => Ok(path.to_string()),
    }
}

/// A control character, or a zero-width or bidi formatting character that
/// changes how surrounding text reads without occupying a cell.
pub(crate) fn is_unrenderable(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200B}'..='\u{200F}' // zero-width space/joiners, LRM, RLM
            | '\u{202A}'..='\u{202E}' // bidi embeddings and overrides
            | '\u{2066}'..='\u{2069}' // bidi isolates
            | '\u{FEFF}' // zero-width no-break space
        )
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
    fn json_has_exactly_the_seven_documented_keys() {
        let s = Session::new("abcdefgh".into(), "dotfiles".into(), 4242, 3)
            .launched_with(crate::launch::DEFAULT)
            .started_in("/home/you/src");
        let v: serde_json::Value =
            serde_json::from_str(&s.to_json().expect("serialise")).expect("json");
        let obj = v.as_object().expect("object");
        let mut keys: Vec<_> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "command",
                "created",
                "directory",
                "id",
                "name",
                "num",
                "pid"
            ]
        );
        assert_eq!(v["command"], crate::launch::DEFAULT);
        assert_eq!(v["directory"], "/home/you/src");
    }

    #[test]
    fn metadata_from_before_these_fields_existed_still_loads() {
        let older = br#"{"id":"abcdefgh","name":"legacy","created":1,"pid":7}"#;
        let s = Session::from_json(older, std::path::Path::new("x.json")).expect("parse");
        assert_eq!(s.name, "legacy");
        assert!(s.command.is_empty());
        assert!(s.directory.is_empty());
        assert_eq!(s.num, 0);
    }

    #[test]
    fn a_relative_working_directory_is_refused_and_says_why() {
        let err = validate_directory("src", "/home/you").expect_err("relative");
        assert!(
            matches!(err, SessionError::InvalidDirectory { reason, .. } if reason.contains("absolute")),
            "{err}"
        );
        assert!(validate_directory("./src", "/home/you").is_err());
        assert!(validate_directory("", "/home/you").is_err(), "empty");
        assert!(
            validate_directory("   ", "/home/you").is_err(),
            "whitespace"
        );
        assert!(
            validate_directory("/home/you\u{7}", "/home/you").is_err(),
            "a control character must be refused as it is in a name"
        );
        assert!(
            validate_directory(&"/".repeat(5000), "/home/you").is_err(),
            "overlong"
        );
    }

    #[test]
    fn a_tilde_is_expanded_against_the_hosts_home_not_ours() {
        assert_eq!(validate_directory("~", "/home/them").unwrap(), "/home/them");
        assert_eq!(
            validate_directory("~/src/nvmux", "/home/them").unwrap(),
            "/home/them/src/nvmux"
        );
        // A trailing slash on the host's answer must not double up.
        assert_eq!(
            validate_directory("~/src", "/home/them/").unwrap(),
            "/home/them/src"
        );
        // An absolute path is passed through untouched, tilde or no tilde.
        assert_eq!(
            validate_directory("/srv/www", "/home/them").unwrap(),
            "/srv/www"
        );
        // A tilde inside the path is an ordinary character, not an expansion.
        assert_eq!(
            validate_directory("/tmp/~x", "/home/them").unwrap(),
            "/tmp/~x"
        );
        // Trimmed like every other typed value.
        assert_eq!(
            validate_directory("  /srv  ", "/home/them").unwrap(),
            "/srv"
        );
    }

    #[test]
    fn a_tilde_naming_another_user_is_refused_rather_than_passed_through() {
        let err = validate_directory("~root/src", "/home/them").expect_err("~user");
        assert!(
            matches!(err, SessionError::InvalidDirectory { reason, .. } if reason.contains("~user")),
            "{err}"
        );
    }

    #[test]
    fn a_tilde_is_refused_when_the_host_reported_no_home() {
        let err = validate_directory("~/src", "").expect_err("no home");
        assert!(
            matches!(err, SessionError::InvalidDirectory { reason, .. } if reason.contains("home")),
            "{err}"
        );
        assert!(
            validate_directory("/srv", "").is_ok(),
            "an absolute path never needed the home directory"
        );
    }

    /// A rename over ssh writes the in-memory session back to disk.
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
        s.state.num = 3;
        let json = s.to_json().expect("serialise");
        assert!(!json.contains("state"), "runtime state leaked into {json}");
        assert!(
            !json.contains("liveness"),
            "runtime state leaked into {json}"
        );
    }

    #[test]
    fn round_trips_through_json() {
        let s = Session::new("abcdefgh".into(), "api server".into(), 99, 7)
            .launched_with("nvim --clean --headless --listen {sock}");
        let json = s.to_json().expect("serialise");
        let back = Session::from_json(json.as_bytes(), Path::new("x.json")).expect("parse");
        assert_eq!(back.id, s.id);
        assert_eq!(back.name, s.name);
        assert_eq!(back.created, s.created);
        assert_eq!(back.pid, s.pid);
        assert_eq!(back.num, s.num);
        assert_eq!(back.command, s.command);
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
        // Likewise for metadata written before the command was recorded.
        assert_eq!(s.command, "");
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
        // Invisible formatting: a right-to-left override reverses how the
        // rest of the row reads, a zero-width space hides a word boundary.
        assert!(validate_name("abc\u{202E}def").is_err());
        assert!(validate_name("abc\u{200B}def").is_err());
        assert!(validate_name("\u{FEFF}abc").is_err());
        assert!(validate_name("abc\u{2066}def").is_err());
    }

    #[test]
    fn a_written_session_reads_back() {
        let dir = crate::test_support::scratch_dir("session-atomic");
        let path = dir.join("abcdefgh.json");
        let s = Session::new("abcdefgh".into(), "x".into(), 7, 1);
        s.write_atomic(&path).expect("write");

        let bytes = std::fs::read(&path).expect("read back");
        assert!(bytes.ends_with(b"\n"), "one line, newline-terminated");
        assert_eq!(Session::from_json(&bytes, &path).expect("parse").name, "x");
        std::fs::remove_dir_all(&dir).ok();
    }
}
