//! What nvmux remembers between runs.
//!
//! One thing so far: the command each host's sessions were last launched with,
//! so the create prompt can offer it again rather than making the user retype a
//! path to a nightly build every time.
//!
//! ```toml
//! # ~/.local/state/nvmux/state.toml — written by nvmux; safe to delete.
//! [commands]
//! local = "nvim --headless --listen {sock}"
//! "ssh:myhost" = "/opt/nvim-nightly/bin/nvim --headless --listen {sock}"
//! ```
//!
//! # The mirror image of [`crate::config`]
//!
//! A config file is written by a person, so it is strict and loud: an unknown
//! key is a startup error and a bad value stops nvmux before it draws anything.
//! This file is written by nvmux, so it is the opposite on every count —
//! **best-effort and silent**. Absent, unreadable, malformed, or holding a
//! command that no longer parses: each of those is a debug log line and a
//! fallback to the configured default, never an error. It is a convenience, and
//! nothing about running sessions depends on it.
//!
//! Two consequences worth stating plainly. There is no `deny_unknown_fields`, so
//! a file written by a newer nvmux still loads here; and a rewrite by this
//! version keeps only the keys this version knows, so the newer one's extras are
//! dropped. Losing a remembered hint is the whole of the damage.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::transport::Location;

/// The whole file. Absent tables and keys are simply nothing remembered.
#[derive(Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
struct State {
    /// Launch command by host key — see [`key`].
    commands: BTreeMap<String, String>,
}

/// How a location is spelled in the file.
///
/// The `ssh:` prefix is not decoration: without it a host actually named `local`
/// would share an entry with this machine.
fn key(location: &Location) -> String {
    match location {
        Location::Local => "local".to_string(),
        Location::Ssh(host) => format!("ssh:{host}"),
    }
}

/// Resolve the state path from the relevant environment, purely.
///
/// Precedence mirrors [`crate::config`]: `$NVMUX_STATE` (an exact path) >
/// `$XDG_STATE_HOME/nvmux/` > `$HOME/.local/state/nvmux/`. Unlike the config
/// there is no explicit/default distinction, because a missing file is never an
/// error here whichever named it.
fn resolve_state_path(
    nvmux_state: Option<&OsStr>,
    xdg_state_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<PathBuf> {
    if let Some(explicit) = nonempty(nvmux_state) {
        return Some(PathBuf::from(explicit));
    }
    if let Some(xdg) = nonempty(xdg_state_home) {
        return Some(Path::new(xdg).join("nvmux").join("state.toml"));
    }
    nonempty(home).map(|home| {
        Path::new(home)
            .join(".local")
            .join("state")
            .join("nvmux")
            .join("state.toml")
    })
}

/// An empty value is treated as unset, matching how a shell exports a variable
/// that was never really set — and matching [`crate::config`], which takes the
/// same view of the same kind of variable.
fn nonempty(value: Option<&OsStr>) -> Option<&OsStr> {
    value.filter(|s| !s.is_empty())
}

fn path() -> Option<PathBuf> {
    let nvmux_state = std::env::var_os("NVMUX_STATE");
    let xdg = std::env::var_os("XDG_STATE_HOME");
    let home = std::env::var_os("HOME");
    resolve_state_path(nvmux_state.as_deref(), xdg.as_deref(), home.as_deref())
}

/// Read the file, or nothing at all. Never an error: see the module docs.
fn load(path: &Path) -> State {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(path = %path.display(), error = %e, "could not read the state file");
            }
            return State::default();
        }
    };
    toml::from_str(&contents).unwrap_or_else(|e| {
        tracing::debug!(path = %path.display(), error = %e, "ignoring a malformed state file");
        State::default()
    })
}

/// The command last used on this host, if it is still one nvmux could run.
///
/// Validated on the way out rather than trusted: the file is editable, and a
/// remembered command that no longer parses would otherwise be offered as a
/// default and then refused the moment the user pressed enter.
pub fn remembered(location: &Location) -> Option<String> {
    let path = path()?;
    let line = load(&path).commands.remove(&key(location))?;
    match crate::launch::Launch::parse(&line) {
        Ok(_) => Some(line),
        Err(e) => {
            tracing::debug!(error = %e, "ignoring an unusable remembered command");
            None
        }
    }
}

/// Remember the command used on this host, keeping every other host's.
///
/// Failure is logged and dropped: nothing the user did has gone wrong, and the
/// session they just created is running. A value that is already what is stored
/// writes nothing at all, so the common case touches no disk.
pub fn remember(location: &Location, line: &str) {
    let Some(path) = path() else {
        return;
    };
    let mut state = load(&path);
    let key = key(location);
    if state
        .commands
        .get(&key)
        .is_some_and(|stored| stored == line)
    {
        return;
    }
    state.commands.insert(key, line.to_string());

    if let Err(e) = write_atomic(&path, &state) {
        tracing::debug!(path = %path.display(), error = %e, "could not record the launch command");
    }
}

/// Temp file plus `rename(2)`, as everything else nvmux writes — see
/// [`crate::config::write_default`], whose gentle `DirBuilder` this shares for
/// the same reason.
fn write_atomic(path: &Path, state: &State) -> std::io::Result<()> {
    use std::io::Write;

    let body = toml::to_string(state).map_err(std::io::Error::other)?;
    let contents =
        format!("# nvmux remembers things here. Written by nvmux; safe to delete.\n\n{body}");

    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }

    let tmp = path.with_extension(format!("toml.tmp{}", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // --- keys and path resolution (pure) -----------------------------------

    /// A host named `local` must not share this machine's entry.
    #[test]
    fn a_remote_host_cannot_collide_with_the_local_entry() {
        assert_eq!(key(&Location::Local), "local");
        assert_eq!(key(&Location::Ssh("local".into())), "ssh:local");
        assert_eq!(key(&Location::Ssh("me@box".into())), "ssh:me@box");
    }

    #[test]
    fn resolve_prefers_nvmux_state_then_xdg_then_home() {
        let explicit = OsStr::new("/explicit.toml");
        let xdg = OsStr::new("/xdg");
        let home = OsStr::new("/home/u");

        assert_eq!(
            resolve_state_path(Some(explicit), Some(xdg), Some(home)),
            Some(PathBuf::from("/explicit.toml"))
        );
        assert_eq!(
            resolve_state_path(None, Some(xdg), Some(home)),
            Some(PathBuf::from("/xdg/nvmux/state.toml"))
        );
        assert_eq!(
            resolve_state_path(None, None, Some(home)),
            Some(PathBuf::from("/home/u/.local/state/nvmux/state.toml"))
        );
        assert_eq!(resolve_state_path(None, None, None), None);
    }

    #[test]
    fn an_empty_env_value_is_treated_as_unset() {
        let empty = OsStr::new("");
        assert_eq!(
            resolve_state_path(Some(empty), Some(empty), Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/home/u/.local/state/nvmux/state.toml"))
        );
    }

    // --- the file (touches process-global env; guarded) --------------------

    /// `remembered`/`remember` read `NVMUX_STATE`, which is process-global.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Point `$NVMUX_STATE` at a scratch file for the duration of a test, and
    /// put the environment back on drop even if the test panics.
    struct Scratch {
        dir: PathBuf,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let saved = ["NVMUX_STATE", "XDG_STATE_HOME", "HOME"]
                .into_iter()
                .map(|k| (k, std::env::var_os(k)))
                .collect::<Vec<_>>();
            for (k, _) in &saved {
                std::env::remove_var(k);
            }
            let dir =
                std::env::temp_dir().join(format!("nvmux-state-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            std::env::set_var("NVMUX_STATE", dir.join("state.toml"));
            Self { dir, saved }
        }

        fn file(&self) -> PathBuf {
            self.dir.join("state.toml")
        }

        fn write(&self, contents: &str) {
            std::fs::write(self.file(), contents).expect("write");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const CLEAN: &str = "nvim --clean --headless --listen {sock}";

    #[test]
    fn a_command_survives_a_round_trip() {
        let _lock = guard();
        let _scratch = Scratch::new("round-trip");

        assert_eq!(remembered(&Location::Local), None, "nothing yet");
        remember(&Location::Local, CLEAN);
        assert_eq!(remembered(&Location::Local), Some(CLEAN.to_string()));
    }

    /// Each host is asked about separately, so each is remembered separately —
    /// a nightly build on one box says nothing about another.
    #[test]
    fn hosts_are_remembered_independently() {
        let _lock = guard();
        let scratch = Scratch::new("per-host");

        remember(&Location::Local, CLEAN);
        remember(
            &Location::Ssh("myhost".into()),
            "nvim -u NONE --listen {sock}",
        );

        assert_eq!(remembered(&Location::Local), Some(CLEAN.to_string()));
        assert_eq!(
            remembered(&Location::Ssh("myhost".into())),
            Some("nvim -u NONE --listen {sock}".to_string())
        );
        assert_eq!(remembered(&Location::Ssh("other".into())), None);

        // And a later write must not lose one of them.
        remember(&Location::Local, "nvim --listen {sock}");
        assert_eq!(
            remembered(&Location::Ssh("myhost".into())),
            Some("nvim -u NONE --listen {sock}".to_string()),
            "another host's entry must survive a write: {}",
            std::fs::read_to_string(scratch.file()).unwrap_or_default()
        );
    }

    /// The file is a convenience. Every way it can be broken has to end in the
    /// caller getting `None` and carrying on, never an error.
    #[test]
    fn a_broken_file_is_ignored_rather_than_fatal() {
        let _lock = guard();
        let scratch = Scratch::new("broken");

        for (what, body) in [
            ("not toml at all", "{{{ nonsense"),
            ("the wrong shape", "commands = 3\n"),
            ("an empty file", ""),
            ("a table we do not know", "[future]\nthing = 1\n"),
        ] {
            scratch.write(body);
            assert_eq!(remembered(&Location::Local), None, "{what}");
        }
    }

    /// The file is editable, and a command that no longer parses would be
    /// offered as a default and then refused the instant enter was pressed.
    #[test]
    fn an_unusable_remembered_command_is_not_offered() {
        let _lock = guard();
        let scratch = Scratch::new("unusable");

        scratch.write("[commands]\nlocal = \"nvim --headless\"\n");
        assert_eq!(
            remembered(&Location::Local),
            None,
            "a command with no {{sock}} could never start a session"
        );
    }

    /// A newer nvmux's file must still load here — the keys this version does
    /// not know are simply not read.
    #[test]
    fn a_file_from_a_newer_version_still_yields_what_it_can() {
        let _lock = guard();
        let scratch = Scratch::new("forward");

        scratch.write(&format!(
            "[commands]\nlocal = \"{CLEAN}\"\n\n[layouts]\nlast = \"grid\"\n"
        ));
        assert_eq!(remembered(&Location::Local), Some(CLEAN.to_string()));
    }

    #[test]
    fn writing_leaves_no_temp_file_behind() {
        let _lock = guard();
        let scratch = Scratch::new("no-temps");

        remember(&Location::Local, CLEAN);
        let strays: Vec<_> = std::fs::read_dir(&scratch.dir)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(strays.is_empty(), "left temp files behind: {strays:?}");
    }

    /// The common case is creating a session with the same command as last
    /// time; that must not rewrite the file.
    #[test]
    fn remembering_what_is_already_stored_writes_nothing() {
        let _lock = guard();
        let scratch = Scratch::new("no-op");

        remember(&Location::Local, CLEAN);
        // A mark nvmux would never write. A rewrite replaces the whole file, so
        // its survival is the evidence that nothing was written.
        let marked = format!(
            "# touched by hand\n{}",
            std::fs::read_to_string(scratch.file()).expect("read")
        );
        scratch.write(&marked);

        remember(&Location::Local, CLEAN);
        assert_eq!(
            std::fs::read_to_string(scratch.file()).expect("read"),
            marked,
            "an unchanged value must not be rewritten"
        );
    }

    /// Nothing about running sessions may depend on this file being writable.
    ///
    /// The parent is a regular file, so the `mkdir` cannot succeed for anybody
    /// — a merely absent directory would be created, and by root at that, which
    /// is who CI runs as.
    #[test]
    fn an_unwritable_path_is_survivable() {
        let _lock = guard();
        let scratch = Scratch::new("unwritable");
        let blocked = scratch.dir.join("a-file-not-a-directory");
        std::fs::write(&blocked, "").expect("write");
        std::env::set_var("NVMUX_STATE", blocked.join("state.toml"));

        remember(&Location::Local, CLEAN);
        assert_eq!(remembered(&Location::Local), None);
    }

    /// The file says what it is, so someone who finds it knows they may delete
    /// it, and reads back as its own state.
    #[test]
    fn the_written_file_explains_itself_and_reloads() {
        let _lock = guard();
        let scratch = Scratch::new("self-describing");

        remember(&Location::Ssh("box".into()), CLEAN);
        let body = std::fs::read_to_string(scratch.file()).expect("read back");
        assert!(body.starts_with('#'), "no explanation in {body:?}");
        assert!(body.contains("safe to delete"), "{body:?}");
        assert!(body.contains("[commands]"), "{body:?}");
        assert_eq!(
            remembered(&Location::Ssh("box".into())),
            Some(CLEAN.to_string())
        );
    }
}
