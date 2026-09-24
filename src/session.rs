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
    /// Reachable, but not serving deferred calls right now: running something
    /// blocking, or waiting for a key at a prompt — [`crate::rpc::probe`] asks
    /// the mode first, and the second is settled in a millisecond rather than
    /// a timeout. Never reaped.
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
    /// The number the picker shows and `<prefix> <n>` selects: this session's
    /// **position** in the listing, assigned by `transport::finish_listing`
    /// after it has ordered them by the stored [`Session::num`].
    ///
    /// Separate from the stored one on purpose. It is dense, starts at 1 and is
    /// duplicate-free across one listing, whatever the stored ranks were —
    /// legacy metadata and orphans have none at all, and two clients creating
    /// at once can store the same one. Being a position is also what makes it
    /// recalculate: kill the second of three and the third is second on the
    /// next listing, with nothing written anywhere.
    ///
    /// It must never reach disk. `SshTransport::rename_session` writes an
    /// in-memory session straight back through `write_meta.sh`, so a position
    /// kept in `Session::num` would be silently persisted as a rank by a
    /// rename.
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
    /// The session's **rank**: what orders the listing, and nothing else. Not
    /// what the picker shows — that is the position the rank sorts this
    /// session into, resolved into [`SessionState::num`] by
    /// `transport::finish_listing`, which is why killing a session above this
    /// one changes the number it displays but not this.
    ///
    /// Assigned once at creation, one past the highest on the host, and
    /// rewritten only by a reorder in the picker. Ranks are therefore *not*
    /// dense — a kill leaves a hole in them, and nothing closes it, because a
    /// hole in an ordering key is invisible. `0` means unranked: metadata
    /// written before numbering existed, and orphans, both of which sort last.
    #[serde(default)]
    pub num: u32,
    /// The command line this session's Neovim was launched with, as
    /// [`crate::launch`] validated it and with `{sock}` still in it. A record,
    /// not an instruction: nothing re-runs it, and the picker does not show it.
    /// Its `<id>.log` says what the editor printed and this says what produced
    /// it. Empty for metadata written before the field existed, and for orphans.
    #[serde(default)]
    pub command: String,
    /// The directory the session's Neovim was started in, absolute and on the
    /// host that runs it. A record in the same sense as `command`: nothing
    /// re-enters it, and the picker does not show it. Empty for metadata written
    /// before the field existed, and for orphans.
    #[serde(default)]
    pub directory: String,

    /// Probe results. Never serialised: the on-disk shape stays exactly the
    /// seven keys above.
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

    /// Record what launched it. Separate from `new` because an orphan and a
    /// session reconstructed from a listing have no command to record.
    pub fn launched_with(mut self, command: &str) -> Self {
        self.command = command.to_string();
        self
    }

    /// Record where it was launched, for the same reason and on the same terms.
    pub fn started_in(mut self, directory: &str) -> Self {
        self.directory = directory.to_string();
        self
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

    /// Write `<id>.json` atomically — temp file plus `rename(2)`, see
    /// [`crate::paths::write_atomic`] — so a concurrent listing sees the old
    /// name or the new one and never a half-written file.
    pub fn write_atomic(&self, path: &Path) -> Result<(), SessionError> {
        let mut json = self.to_json()?;
        json.push('\n');
        crate::paths::write_atomic(path, json.as_bytes()).map_err(|source| SessionError::Metadata {
            path: path.to_path_buf(),
            source: serde_json::Error::io(source),
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
    // escape sequences into the terminal via a session listing; the invisible
    // formatting characters can reorder or hide what the listing shows.
    if name.chars().any(is_unrenderable) {
        return invalid("must not contain control or invisible formatting characters");
    }
    Ok(())
}

/// Validate a user-supplied working directory, and expand a leading `~`.
///
/// Returns the path as it should be sent to the session host: absolute, so that
/// `scripts/spawn.sh` has nothing left to resolve and no shell is needed to
/// resolve it. `home` is the *session host's* home directory — the local `$HOME`
/// for a local session, and what `scripts/hello.sh` reported for a remote one.
/// A path on one machine means nothing on another, so expanding against ours
/// would be quietly wrong over ssh.
///
/// The `~` is [`expand_tilde`]'s, which the create prompt's completion also
/// calls — so what the menu lists inside `~/` and what enter finally creates
/// cannot mean two different directories.
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
    // Generous: this is a path, and PATH_MAX is 4096 on Linux. The cap is here
    // so that a pasted runaway is refused at the prompt rather than by execve.
    if directory.len() > 4096 {
        return invalid("must be 4096 bytes or fewer");
    }
    // The same rule a name and a command live by: this is drawn on the prompt
    // and could otherwise smuggle escape sequences into the terminal.
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

/// Expand a leading `~` against the session host's home directory.
///
/// This is the one expansion nvmux performs anywhere, and it is deliberately
/// narrow: `~` and `~/…` only, never `~user`, no globs, no `$VAR`. It is
/// possible here for the reason [`crate::launch`] says it is not possible for a
/// command line — nvmux *knows* this answer, so nothing has to be handed to a
/// shell to find it out.
///
/// `home` is the *session host's* home — the local `$HOME` for a local session,
/// and what `scripts/hello.sh` reported for a remote one. A path on one machine
/// means nothing on another, so expanding against ours would be quietly wrong
/// over ssh.
///
/// The `Err` is a reason rather than a type, because its two callers want
/// opposite things from it: [`validate_directory`] reports it and refuses, and
/// [`crate::ui::complete`] discards it and lists the text literally — at a
/// prompt, `~r` is not a mistake, it is `~root` half typed, and a keystroke on
/// the way to somewhere must not turn the field red.
pub fn expand_tilde(path: &str, home: &str) -> Result<String, &'static str> {
    match path.strip_prefix('~') {
        // `~user` is somebody else's home directory, which only the host can
        // resolve. Refused rather than passed through as a literal directory
        // named `~someone`, which is what would otherwise be created-or-missing.
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

/// A character that a session listing cannot show honestly: a control
/// character, or one of the zero-width and bidirectional-formatting characters
/// that change how the *surrounding* text reads without occupying a cell.
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

    /// Metadata written before either field existed still loads: both are
    /// `#[serde(default)]`, so an older session keeps working rather than
    /// becoming an unreadable file the picker refuses to list.
    #[test]
    fn metadata_from_before_these_fields_existed_still_loads() {
        let older = br#"{"id":"abcdefgh","name":"legacy","created":1,"pid":7}"#;
        let s = Session::from_json(older, std::path::Path::new("x.json")).expect("parse");
        assert_eq!(s.name, "legacy");
        assert!(s.command.is_empty());
        assert!(s.directory.is_empty());
        assert_eq!(s.num, 0);
    }

    /// A relative directory is the one rule a directory has that a name and a
    /// command do not, and it exists because of what would happen if it did
    /// not: the session would start somewhere nobody chose.
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

    /// `~` is expanded against the *session host's* home, which over ssh is not
    /// ours. Expanding against ours would send a path that means nothing there,
    /// and it would look right in the prompt while doing it.
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

    /// Only `~` and `~/…`. `~someone` is a question only the host can answer,
    /// and passing it through would name a directory that is almost certainly
    /// not there — reported as "not a directory", which explains nothing.
    #[test]
    fn a_tilde_naming_another_user_is_refused_rather_than_passed_through() {
        let err = validate_directory("~root/src", "/home/them").expect_err("~user");
        assert!(
            matches!(err, SessionError::InvalidDirectory { reason, .. } if reason.contains("~user")),
            "{err}"
        );
    }

    /// A host that could not say where home is must not have one invented for
    /// it. The prompt offers no default in that case, and a typed `~` is a
    /// question nvmux cannot answer either.
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

    /// What reaches disk reads back as the session that was written; that no
    /// temp file is left beside it is `paths::write_atomic`'s own test.
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
